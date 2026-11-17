use arc_swap::ArcSwap;
use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::{any, get};
use axum::{Json, Router};
use axum::http::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use wasmtime::Module;

use crate::engine::host_abi::HostCapabilities;
use crate::engine::http_envelope::{ErrorEnvelope, RequestEnvelope};
use crate::engine::WasmEngine;
use crate::error::EngineError;

/// Active generation representing the currently deployed, shadow-verified Wasm module
pub struct ActiveGeneration {
    pub generation_id: u64,
    pub module_hash: String,
    pub module: Arc<Module>,
    pub capabilities: HostCapabilities,
    pub fuel_budget: u64,
}

/// Shared ingress state holding the atomic pointer to the active generation
pub struct IngressState {
    pub engine: Arc<WasmEngine>,
    pub active_generation: ArcSwap<Option<ActiveGeneration>>,
}

impl IngressState {
    pub fn new(engine: Arc<WasmEngine>) -> Self {
        Self {
            engine,
            active_generation: ArcSwap::from_pointee(None),
        }
    }

    /// Atomically swaps the active Wasm module generation without locking
    pub fn hot_swap(&self, new_gen: ActiveGeneration) {
        tracing::info!(
            generation = new_gen.generation_id,
            hash = %new_gen.module_hash,
            "Performing atomic zero-downtime hot-swap"
        );
        self.active_generation.store(Arc::new(Some(new_gen)));
    }
}

/// Creates the Axum router for the live HTTP data ingress
#[derive(Debug, Serialize)]
struct RuntimeStatus {
    status: &'static str,
    active_generation: Option<u64>,
    module_hash: Option<String>,
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn runtime_status(State(state): State<Arc<IngressState>>) -> Json<RuntimeStatus> {
    let active = state.active_generation.load();
    let (active_generation, module_hash) = match active.as_ref() {
        Some(generation) => (Some(generation.generation_id), Some(generation.module_hash.clone())),
        None => (None, None),
    };
    Json(RuntimeStatus { status: "running", active_generation, module_hash })
}

pub fn build_ingress_router(state: Arc<IngressState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/_nosrc/status", get(runtime_status))
        .fallback(any(handle_http_request))
        .with_state(state)
}

/// Catch-all HTTP handler that dispatches incoming requests to the active Wasm generation
async fn handle_http_request(
    State(state): State<Arc<IngressState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. Wait-free load of active generation
    let guard = state.active_generation.load();
    let Some(active) = guard.as_ref() else {
        return ErrorEnvelope {
            error: "No active Wasm module generation deployed".to_string(),
            error_type: "NoActiveGeneration".to_string(),
            status: 503,
        }
        .into_axum_response();
    };

    // 2. Serialize incoming request envelope
    let envelope = RequestEnvelope::from_parts(&method, &uri, &headers, body);

    // 3. Dispatch to sandboxed Wasm execution
    let engine = state.engine.clone();
    let module = active.module.clone();
    let capabilities = active.capabilities.clone();
    let fuel_budget = active.fuel_budget;

    // Run execution off the main Tokio reactor thread
    let execution_result = tokio::task::spawn_blocking(move || {
        engine.invoke_handler(&module, capabilities, &envelope, fuel_budget)
    })
    .await;

    match execution_result {
        Ok(Ok(response_env)) => response_env.into_axum_response(),
        Ok(Err(engine_err)) => {
            tracing::warn!(error = %engine_err, "Wasm request execution trapped or failed");
            ErrorEnvelope::from_engine_error(&engine_err).into_axum_response()
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "Tokio task join error during Wasm execution");
            ErrorEnvelope::from_engine_error(&EngineError::Internal(join_err.to_string()))
                .into_axum_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::DEFAULT_FUEL_BUDGET;
    use axum::body::Body;
    use axum::http::Request;
    use std::collections::HashSet;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_ingress_wait_free_hot_swap() {
        let engine = Arc::new(WasmEngine::new().unwrap());
        let ingress_state = Arc::new(IngressState::new(engine.clone()));
        let app = build_ingress_router(ingress_state.clone());

        // 1. When no generation is loaded -> 503 Service Unavailable
        let req1 = Request::builder()
            .method("GET")
            .uri("/status")
            .body(Body::empty())
            .unwrap();
        let resp1 = app.clone().oneshot(req1).await.unwrap();
        assert_eq!(resp1.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);

        // 2. Prepare Generation 1: returns status 200 "gen 1".
        // Keep the response data separate from the engine's default request buffer at 1024.
        let wat_gen1 = r#"
            (module
                (import "nosrc_host" "host_set_response" (func $set_resp (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 4096) "{\"status\":200,\"headers\":{},\"body\":\"gen 1\"}")

                (func (export "handle_request") (param i32 i32) (result i32)
                    (call $set_resp (i32.const 4096) (i32.const 42))
                )
            )
        "#;
        let mod_gen1 = Arc::new(engine.compile(wat_gen1.as_bytes()).unwrap());
        ingress_state.hot_swap(ActiveGeneration {
            generation_id: 1,
            module_hash: "hash_gen1".to_string(),
            module: mod_gen1,
            capabilities: HostCapabilities::new_external(HashSet::new()),
            fuel_budget: DEFAULT_FUEL_BUDGET,
        });

        let req2 = Request::builder()
            .method("GET")
            .uri("/test")
            .body(Body::empty())
            .unwrap();
        let resp2 = app.clone().oneshot(req2).await.unwrap();
        let resp2_status = resp2.status();
        let resp2_body = axum::body::to_bytes(resp2.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            resp2_status,
            axum::http::StatusCode::OK,
            "generation 1 returned an unexpected response: {}",
            String::from_utf8_lossy(&resp2_body)
        );
        assert_eq!(resp2_body.as_ref(), b"gen 1");

        // 3. Hot-swap to Generation 2: returns status 201 "gen 2".
        let wat_gen2 = r#"
            (module
                (import "nosrc_host" "host_set_response" (func $set_resp (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 4096) "{\"status\":201,\"headers\":{},\"body\":\"gen 2\"}")

                (func (export "handle_request") (param i32 i32) (result i32)
                    (call $set_resp (i32.const 4096) (i32.const 42))
                )
            )
        "#;
        let mod_gen2 = Arc::new(engine.compile(wat_gen2.as_bytes()).unwrap());
        ingress_state.hot_swap(ActiveGeneration {
            generation_id: 2,
            module_hash: "hash_gen2".to_string(),
            module: mod_gen2,
            capabilities: HostCapabilities::new_external(HashSet::new()),
            fuel_budget: DEFAULT_FUEL_BUDGET,
        });

        // Immediately verify new generation is serving traffic without restart
        let req3 = Request::builder()
            .method("GET")
            .uri("/test")
            .body(Body::empty())
            .unwrap();
        let resp3 = app.clone().oneshot(req3).await.unwrap();
        let resp3_status = resp3.status();
        let resp3_body = axum::body::to_bytes(resp3.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            resp3_status,
            axum::http::StatusCode::CREATED,
            "generation 2 returned an unexpected response: {}",
            String::from_utf8_lossy(&resp3_body)
        );
        assert_eq!(resp3_body.as_ref(), b"gen 2");
    }
}
