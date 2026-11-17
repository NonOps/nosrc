pub mod engine;
pub mod error;
pub mod ingress;
pub mod spec;
pub mod synthesizer;
pub mod verifier;

use clap::Parser;
use std::sync::{Arc, Mutex};

use engine::http_envelope::RequestEnvelope;
use engine::quickjs::{QuickJsEngine, DEFAULT_COMPONENT_FUEL};
use ingress::{ActiveGeneration, IngressState};
use spec::ServiceSpec;
use synthesizer::{ContractAssertion, LlmConfig, Synthesizer};
use verifier::ShadowVerifier;

#[derive(Parser, Debug)]
#[command(
    name = "nosrc",
    about = "Specification-driven JavaScript service runtime"
)]
pub struct Cli {
    #[arg(short, long, default_value = "./SPEC.md")]
    pub spec: std::path::PathBuf,

    #[arg(short, long, default_value = "8080")]
    pub port: u16,
}

fn validate_runtime_spec(spec: &ServiceSpec) -> anyhow::Result<()> {
    if spec.capabilities.state != "volatile" {
        anyhow::bail!(
            "SPEC state mode '{}' is not supported yet; this runtime supports only volatile, generation-scoped JavaScript state",
            spec.capabilities.state
        );
    }
    if !spec.capabilities.egress.is_empty() {
        anyhow::bail!(
            "SPEC requests outbound egress ({}) but network capabilities are not available in this runtime",
            spec.capabilities.egress.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if !spec.schema.is_empty() {
        anyhow::bail!(
            "SPEC declares database schema, but SQL capabilities are not available in this runtime"
        );
    }

    spec.validate_verification()
        .map_err(|error| anyhow::anyhow!("Invalid trusted verification suite: {error}"))?;

    Ok(())
}

/// Synthesize, compile, verify, and instantiate one production generation.
///
/// Keeping this lifecycle in one function lets the executable and the offline
/// end-to-end test exercise exactly the same startup path.
async fn prepare_generation(
    spec: &ServiceSpec,
    synthesizer: &Synthesizer,
) -> anyhow::Result<(ActiveGeneration, usize, usize)> {
    validate_runtime_spec(spec)?;

    let output = synthesizer.synthesize(spec, &[]).await?;
    if output.code_format != "javascript" {
        anyhow::bail!(
            "Synthesizer returned unsupported code format '{}'; expected 'javascript'",
            output.code_format
        );
    }
    if output.handler_code.trim().is_empty() {
        anyhow::bail!("Synthesizer returned an empty JavaScript handler");
    }

    let engine = QuickJsEngine::new()?;
    let compiled = engine.compile_handler(&output.handler_code).await?;

    // SPEC-authored cases are independent of the model that generated the handler.
    // Separate candidate instances prevent the supplemental suite from mutating trusted-test state.
    let trusted_assertions: Vec<ContractAssertion> = spec
        .verification
        .iter()
        .map(|case| {
            Ok(ContractAssertion {
                name: format!("SPEC {} {}", case.method, case.path),
                request: RequestEnvelope {
                    method: case.method.clone(),
                    path: case.path.clone(),
                    query: std::collections::HashMap::new(),
                    headers: case.headers.clone(),
                    body: case.body.clone(),
                },
                expected_status: case.expected_status.ok_or_else(|| {
                    anyhow::anyhow!(
                        "verification case {} {} has no response status",
                        case.method,
                        case.path
                    )
                })?,
                body_contains: case.body_contains.clone(),
                header_matches: None,
            })
        })
        .collect::<anyhow::Result<_>>()?;

    let mut trusted_candidate = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let trusted_results =
        ShadowVerifier::new().verify_handler(&mut trusted_candidate, &trusted_assertions)?;
    drop(trusted_candidate);

    let mut generated_candidate = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let generated_results =
        ShadowVerifier::new().verify_handler(&mut generated_candidate, &output.assertions)?;
    drop(generated_candidate);

    let active_handler = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let generation = ActiveGeneration {
        generation_id: 1,
        module_hash: compiled.module_hash,
        handler: Arc::new(Mutex::new(active_handler)),
    };

    Ok((generation, trusted_results.len(), generated_results.len()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let spec_text = tokio::fs::read_to_string(&cli.spec)
        .await
        .map_err(|error| {
            anyhow::anyhow!("Failed to read SPEC at {}: {error}", cli.spec.display())
        })?;
    let spec = ServiceSpec::parse(&spec_text).map_err(|error| {
        anyhow::anyhow!("Failed to parse SPEC at {}: {error}", cli.spec.display())
    })?;

    tracing::info!(
        service = %spec.name,
        endpoints = spec.endpoints.len(),
        spec = %cli.spec.display(),
        "Loaded service specification"
    );

    let synthesizer = Synthesizer::new(LlmConfig::from_env()?);
    let (generation, trusted_assertions, generated_assertions) =
        prepare_generation(&spec, &synthesizer).await?;

    tracing::info!(
        trusted_assertions,
        generated_assertions,
        module_hash = %generation.module_hash,
        "Candidate compiled and passed contract verification"
    );

    let state = Arc::new(IngressState::new());
    state.hot_swap(generation);

    let app = ingress::build_ingress_router(state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cli.port)).await?;

    tracing::info!(
        address = %listener.local_addr()?,
        "nosrc is listening with a verified JavaScript handler"
    );

    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    async fn mock_chat_completions(
        State(request_count): State<Arc<AtomicUsize>>,
    ) -> Json<Value> {
        request_count.fetch_add(1, Ordering::SeqCst);

        let synthesis = json!({
            "handler_code": r#"
                export function handle(requestJson) {
                    const request = JSON.parse(requestJson);
                    if (request.method === "GET" && request.path === "/hello") {
                        return JSON.stringify({
                            status: 200,
                            headers: { "content-type": "application/json" },
                            body: JSON.stringify({ message: "hello" })
                        });
                    }
                    return JSON.stringify({
                        status: 404,
                        headers: { "content-type": "text/plain" },
                        body: "not found"
                    });
                }
            "#,
            "code_format": "javascript",
            "assertions": [{
                "name": "generated hello assertion",
                "request": {
                    "method": "GET",
                    "path": "/hello",
                    "query": {},
                    "headers": {},
                    "body": null
                },
                "expected_status": 200,
                "body_contains": "hello"
            }],
            "rationale": "Deterministic handler returned by the test provider"
        });

        Json(json!({
            "choices": [{
                "message": {
                    "content": serde_json::to_string(&synthesis).unwrap()
                }
            }]
        }))
    }

    #[tokio::test]
    async fn startup_synthesizes_verifies_activates_and_serves_without_external_llm() {
        let request_count = Arc::new(AtomicUsize::new(0));
        let mock_provider = Router::new()
            .route("/v1/chat/completions", post(mock_chat_completions))
            .with_state(request_count.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let provider_address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let provider_task = tokio::spawn(async move {
            axum::serve(listener, mock_provider)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let spec = ServiceSpec::parse(include_str!("../examples/minimal/SPEC.md")).unwrap();
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: "test-key".to_string(),
            base_url: format!("http://{provider_address}/v1"),
            model: "nosrc-test-model".to_string(),
            temperature: 0.0,
        });

        let (generation, trusted_count, generated_count) =
            prepare_generation(&spec, &synthesizer).await.unwrap();

        assert_eq!(trusted_count, 1);
        assert_eq!(generated_count, 1);
        assert_eq!(request_count.load(Ordering::SeqCst), 1);

        let state = Arc::new(IngressState::new());
        state.hot_swap(generation);
        let app = ingress::build_ingress_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/hello")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({ "message": "hello" })
        );

        let _ = shutdown_tx.send(());
        provider_task.await.unwrap();
    }
}
