pub mod artifact_cache;
pub mod engine;
pub mod error;
pub mod ingress;
pub mod spec;
pub mod synthesizer;
pub mod verifier;

use clap::Parser;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use artifact_cache::ArtifactCache;
use engine::http_envelope::RequestEnvelope;
use engine::quickjs::{QuickJsEngine, DEFAULT_COMPONENT_FUEL};
use ingress::{ActiveGeneration, IngressState};
use spec::ServiceSpec;
use synthesizer::{ContractAssertion, LlmConfig, SynthesisOutput, Synthesizer};
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

    /// Address to listen on. Defaults to loopback; use 0.0.0.0 explicitly for external access.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: IpAddr,

    /// Enable detailed generation-pipeline and HTTP request diagnostics.
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    pub verbose: bool,

    /// Directory for content-addressed, locally cached verified artifacts.
    #[arg(long, default_value = "./.nosrc-cache")]
    pub cache_dir: std::path::PathBuf,

    /// Bypass artifact cache reads and writes and always synthesize a new handler.
    #[arg(long)]
    pub no_cache: bool,
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

/// Validate synthesis output before it is compiled or loaded from cache.
fn validate_synthesis_output(output: &SynthesisOutput) -> anyhow::Result<()> {
    if output.code_format != "javascript" {
        anyhow::bail!(
            "Synthesizer returned unsupported code format '{}'; expected 'javascript'",
            output.code_format
        );
    }
    if output.handler_code.trim().is_empty() {
        anyhow::bail!("Synthesizer returned an empty JavaScript handler");
    }
    Ok(())
}

/// Synthesize (or load), compile, verify, and instantiate one production generation.
///
/// Cache hits skip provider synthesis and componentization, but are always recompiled
/// by the current Wasmtime engine and rerun through both isolated verification suites.
async fn prepare_generation(
    spec: &ServiceSpec,
    synthesizer: &Synthesizer,
    cache: Option<&ArtifactCache>,
) -> anyhow::Result<(ActiveGeneration, usize, usize, Option<String>)> {
    let validation_started = Instant::now();
    tracing::debug!("Validating runtime capabilities and trusted SPEC verification suite");
    validate_runtime_spec(spec)?;
    tracing::debug!(
        elapsed_ms = validation_started.elapsed().as_millis(),
        "SPEC validation completed"
    );

    let engine = QuickJsEngine::new()?;
    let cache_key = synthesizer.artifact_cache_key(spec);
    let cached = cache.and_then(|artifact_cache| artifact_cache.load(&cache_key));

    let (output, compiled, component_bytes, cache_hit) = match cached {
        Some(cached_artifact) => match engine.compile_component(&cached_artifact.component_bytes) {
            Ok(compiled) => {
                tracing::info!(cache_key = %cache_key, "Loaded component artifact from cache");
                (
                    cached_artifact.synthesis,
                    compiled,
                    cached_artifact.component_bytes,
                    true,
                )
            }
            Err(error) => {
                tracing::warn!(cache_key = %cache_key, %error, "Cached component could not be loaded; regenerating");
                let synthesis_started = Instant::now();
                let output = synthesizer.synthesize(spec, &[]).await?;
                validate_synthesis_output(&output)?;
                tracing::debug!(
                    elapsed_ms = synthesis_started.elapsed().as_millis(),
                    code_format = %output.code_format,
                    handler_bytes = output.handler_code.len(),
                    generated_assertions = output.assertions.len(),
                    "Handler synthesis completed"
                );
                let compilation_started = Instant::now();
                let (compiled, component_bytes) = engine
                    .compile_handler_with_artifact(&output.handler_code)
                    .await?;
                tracing::debug!(
                    elapsed_ms = compilation_started.elapsed().as_millis(),
                    module_hash = %compiled.module_hash,
                    "Wasm component compilation completed"
                );
                (output, compiled, component_bytes, false)
            }
        },
        None => {
            let synthesis_started = Instant::now();
            let output = synthesizer.synthesize(spec, &[]).await?;
            validate_synthesis_output(&output)?;
            tracing::debug!(
                elapsed_ms = synthesis_started.elapsed().as_millis(),
                code_format = %output.code_format,
                handler_bytes = output.handler_code.len(),
                generated_assertions = output.assertions.len(),
                "Handler synthesis completed"
            );
            let compilation_started = Instant::now();
            tracing::debug!("Initializing QuickJS/Wasm engine and compiling generated JavaScript");
            let (compiled, component_bytes) = engine
                .compile_handler_with_artifact(&output.handler_code)
                .await?;
            tracing::debug!(
                elapsed_ms = compilation_started.elapsed().as_millis(),
                module_hash = %compiled.module_hash,
                "Wasm component compilation completed"
            );
            (output, compiled, component_bytes, false)
        }
    };

    // SPEC-authored cases are independent of the model that generated the handler.
    // Separate candidate instances prevent the supplemental suite from mutating trusted-test state.
    let trusted_assertions = trusted_assertions_from_spec(spec)?;

    let trusted_started = Instant::now();
    tracing::debug!(
        assertions = trusted_assertions.len(),
        "Running SPEC-authored trusted verification suite"
    );
    let mut trusted_candidate = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let trusted_results =
        match ShadowVerifier::new().verify_handler(&mut trusted_candidate, &trusted_assertions) {
            Ok(results) => results,
            Err(error) => {
                tracing::error!(
                    suite = "trusted",
                    "Contract verification failed; detailed error returned to caller"
                );
                return Err(error.into());
            }
        };
    tracing::debug!(
        passed = trusted_results.len(),
        total = trusted_assertions.len(),
        elapsed_ms = trusted_started.elapsed().as_millis(),
        "Trusted verification suite passed"
    );
    drop(trusted_candidate);

    let generated_started = Instant::now();
    tracing::debug!(
        assertions = output.assertions.len(),
        "Running model-generated supplemental verification suite"
    );
    let mut generated_candidate = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let generated_results =
        match ShadowVerifier::new().verify_handler(&mut generated_candidate, &output.assertions) {
            Ok(results) => results,
            Err(error) => {
                tracing::error!(
                    suite = "generated",
                    "Contract verification failed; detailed error returned to caller"
                );
                return Err(error.into());
            }
        };
    tracing::debug!(
        passed = generated_results.len(),
        total = output.assertions.len(),
        elapsed_ms = generated_started.elapsed().as_millis(),
        "Generated verification suite passed"
    );
    drop(generated_candidate);

    tracing::debug!("Creating a fresh production handler instance after verification");
    let active_handler = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let generation = ActiveGeneration {
        generation_id: 1,
        module_hash: compiled.module_hash,
        handler: Arc::new(Mutex::new(active_handler)),
    };

    if !cache_hit {
        if let Some(artifact_cache) = cache {
            if let Err(error) = artifact_cache.store(&cache_key, &output, &component_bytes) {
                tracing::warn!(cache_key = %cache_key, %error, "Could not persist verified artifact cache entry");
            } else {
                tracing::info!(cache_key = %cache_key, "Stored verified component artifact in cache");
            }
        }
    }

    Ok((
        generation,
        trusted_results.len(),
        generated_results.len(),
        output.rationale,
    ))
}

fn trusted_assertions_from_spec(spec: &ServiceSpec) -> anyhow::Result<Vec<ContractAssertion>> {
    spec.verification
        .iter()
        .map(|case| {
            Ok(ContractAssertion {
                name: format!("SPEC {} {}", case.method, case.path),
                request: RequestEnvelope {
                    method: case.method.clone(),
                    path: case.path.clone(),
                    query: case.query.clone(),
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
                header_matches: if case.response_headers.is_empty() {
                    None
                } else {
                    Some(case.response_headers.clone())
                },
            })
        })
        .collect()
}

fn default_log_filter(verbose: bool) -> &'static str {
    if verbose {
        "warn,nosrc=debug"
    } else {
        "warn,nosrc=info"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_log_filter(cli.verbose)));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

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
    let cache = if cli.no_cache {
        tracing::info!("Artifact cache is disabled");
        None
    } else {
        Some(ArtifactCache::new(cli.cache_dir.clone()))
    };
    let (generation, trusted_assertions, generated_assertions, _rationale) =
        prepare_generation(&spec, &synthesizer, cache.as_ref()).await?;

    tracing::info!(
        trusted_assertions,
        generated_assertions,
        module_hash = %generation.module_hash,
        "Candidate compiled and passed contract verification"
    );

    let state = Arc::new(IngressState::new());
    state.hot_swap(generation);

    let app = ingress::build_ingress_router(state);
    let listener = tokio::net::TcpListener::bind((cli.bind, cli.port)).await?;

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

    #[test]
    fn verbose_flag_enables_debug_diagnostics() {
        let cli = Cli::try_parse_from(["nosrc", "--verbose"]).unwrap();
        assert!(cli.verbose);
    }

    #[test]
    fn debug_diagnostics_are_disabled_by_default() {
        let cli = Cli::try_parse_from(["nosrc"]).unwrap();
        assert!(!cli.verbose);
    }

    #[test]
    fn artifact_cache_is_enabled_by_default_and_can_be_bypassed() {
        let cli = Cli::try_parse_from(["nosrc"]).unwrap();
        assert!(!cli.no_cache);
        assert_eq!(cli.cache_dir, std::path::PathBuf::from("./.nosrc-cache"));

        let cli = Cli::try_parse_from(["nosrc", "--no-cache"]).unwrap();
        assert!(cli.no_cache);
    }

    #[test]
    fn listener_defaults_to_loopback_and_allows_explicit_external_bind() {
        let cli = Cli::try_parse_from(["nosrc"]).unwrap();
        assert_eq!(cli.bind, "127.0.0.1".parse::<IpAddr>().unwrap());

        let cli = Cli::try_parse_from(["nosrc", "--bind", "0.0.0.0"]).unwrap();
        assert_eq!(cli.bind, "0.0.0.0".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_spec_cases_preserve_query_and_expected_response_headers() {
        let markdown = r#"
# Service: Search
## Endpoints
### `GET /search`
- Response `200`: results
## Verification
### `GET /search?q=rust%20wasm`
- Header: accept=application/json
- Response header: content-type=application/json
- Response `200`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        spec.validate_verification().unwrap();
        let assertions = trusted_assertions_from_spec(&spec).unwrap();
        assert_eq!(assertions.len(), 1);
        assert_eq!(assertions[0].request.path, "/search");
        assert_eq!(
            assertions[0].request.query.get("q").map(String::as_str),
            Some("rust wasm")
        );
        assert_eq!(
            assertions[0]
                .request
                .headers
                .get("accept")
                .map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            assertions[0]
                .header_matches
                .as_ref()
                .unwrap()
                .get("content-type")
                .map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn default_log_filters_scope_application_verbosity() {
        assert_eq!(default_log_filter(false), "warn,nosrc=info");
        assert_eq!(default_log_filter(true), "warn,nosrc=debug");
        tracing_subscriber::EnvFilter::try_new(default_log_filter(false)).unwrap();
        tracing_subscriber::EnvFilter::try_new(default_log_filter(true)).unwrap();
    }
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    async fn mock_chat_completions(State(request_count): State<Arc<AtomicUsize>>) -> Json<Value> {
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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

        let cache_directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(cache_directory.path());
        let (generation, trusted_count, generated_count, rationale) =
            prepare_generation(&spec, &synthesizer, Some(&cache))
                .await
                .unwrap();

        assert_eq!(trusted_count, 1);
        assert_eq!(generated_count, 1);
        assert_eq!(
            rationale.as_deref(),
            Some("Deterministic handler returned by the test provider")
        );
        assert_eq!(request_count.load(Ordering::SeqCst), 1);

        // A second startup must load the cached component, rerun both suites,
        // and avoid a second request to the model provider.
        let (cached_generation, cached_trusted, cached_generated, cached_rationale) =
            prepare_generation(&spec, &synthesizer, Some(&cache)).await.unwrap();
        assert_eq!(cached_trusted, trusted_count);
        assert_eq!(cached_generated, generated_count);
        assert_eq!(cached_rationale, rationale);
        assert_eq!(cached_generation.module_hash, generation.module_hash);
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
