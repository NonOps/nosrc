use arc_swap::ArcSwap;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, OriginalUri, State};
use axum::http::{header::CONTENT_LENGTH, HeaderMap, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::engine::http_envelope::{ErrorEnvelope, RequestEnvelope};
use crate::engine::quickjs::QuickJsHandler;
use crate::error::EngineError;

const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// Active generation representing a compiled and contract-verified JavaScript handler.
pub struct ActiveGeneration {
    pub generation_id: u64,
    pub module_hash: String,
    pub handler: Arc<Mutex<QuickJsHandler>>,
}

/// Shared ingress state holding the active handler generation.
pub struct IngressState {
    pub active_generation: ArcSwap<Option<ActiveGeneration>>,
}

impl IngressState {
    pub fn new() -> Self {
        Self {
            active_generation: ArcSwap::from_pointee(None),
        }
    }

    /// Atomically activates a candidate only after compilation and verification succeed.
    pub fn hot_swap(&self, new_generation: ActiveGeneration) {
        tracing::info!(
            generation = new_generation.generation_id,
            hash = %new_generation.module_hash,
            "Activating verified JavaScript handler generation"
        );
        self.active_generation.store(Arc::new(Some(new_generation)));
    }
}

impl Default for IngressState {
    fn default() -> Self {
        Self::new()
    }
}

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
        Some(generation) => (
            Some(generation.generation_id),
            Some(generation.module_hash.clone()),
        ),
        None => (None, None),
    };

    Json(RuntimeStatus {
        status: "running",
        active_generation,
        module_hash,
    })
}

/// Logs safe request metadata without recording query strings, headers, or bodies.
async fn log_http_request(request: Request<axum::body::Body>, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let request_bytes = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let started = Instant::now();
    let response = next.run(request).await;
    let response_bytes = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    tracing::debug!(
        %method,
        %path,
        request_bytes = ?request_bytes,
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis(),
        response_bytes = ?response_bytes,
        "HTTP request completed"
    );
    response
}

/// Creates the Axum router for health, status, and application ingress.
pub fn build_ingress_router(state: Arc<IngressState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/_nosrc/status", get(runtime_status))
        .fallback(any(handle_http_request))
        .layer(middleware::from_fn(log_http_request))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
}

async fn handle_http_request(
    State(state): State<Arc<IngressState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let generation = state.active_generation.load();
    let Some(active) = generation.as_ref() else {
        return ErrorEnvelope {
            error: "No active verified JavaScript handler generation".to_string(),
            error_type: "NoActiveGeneration".to_string(),
            status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        }
        .into_axum_response();
    };

    let envelope = match RequestEnvelope::try_from_parts(&method, &uri, &headers, body) {
        Ok(envelope) => envelope,
        Err(_) => {
            return ErrorEnvelope {
                error: "Request body must be valid UTF-8 for this handler ABI".to_string(),
                error_type: "InvalidRequestBody".to_string(),
                status: StatusCode::BAD_REQUEST.as_u16(),
            }
            .into_axum_response();
        }
    };
    let handler = active.handler.clone();

    let execution = tokio::task::spawn_blocking(move || {
        let mut handler = handler
            .lock()
            .map_err(|_| EngineError::Internal("Handler execution lock was poisoned".into()))?;
        handler.invoke(&envelope)
    })
    .await;

    match execution {
        Ok(Ok(response)) => response.into_axum_response(),
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "JavaScript handler execution failed");
            ErrorEnvelope::from_engine_error(&error).into_axum_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "Handler execution task failed");
            ErrorEnvelope::from_engine_error(&EngineError::Internal(error.to_string()))
                .into_axum_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::quickjs::{test_support, DEFAULT_COMPONENT_FUEL};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_endpoint_reports_process_health() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_endpoint_reports_no_active_generation_at_startup() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .uri("/_nosrc/status")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["status"], "running");
        assert!(status["active_generation"].is_null());
        assert!(status["module_hash"].is_null());
    }

    #[tokio::test]
    async fn oversized_request_body_is_rejected_before_handler_dispatch() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(Body::from(vec![120; MAX_REQUEST_BODY_BYTES + 1]))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn application_routes_return_503_until_a_generation_is_active() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .uri("/hello")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn ingress_dispatches_requests_to_the_active_quickjs_handler() {
        let (engine, compiled) = test_support::compiled().await;
        let handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let state = Arc::new(IngressState::new());
        state.hot_swap(ActiveGeneration {
            generation_id: 1,
            module_hash: "test-component".to_string(),
            handler: Arc::new(Mutex::new(handler)),
        });

        let app = build_ingress_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/hello")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"GET /hello");
    }
}
