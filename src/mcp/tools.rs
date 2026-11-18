use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars::JsonSchema;
use rmcp::{tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::engine::http_envelope::RequestEnvelope;
use crate::mcp::lifecycle::{AppLifecycle, GenerationId, PreparationError, PreparedGeneration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum PrepareSource {
    Spec,
    DraftSpec,
    DirectJs,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct PrepareAppParams {
    /// Whether to synthesize from the operator SPEC or accept direct JavaScript.
    source: PrepareSource,
    /// ES module source exporting handle(requestJson).
    js_code: Option<String>,
    /// Agent-authored draft contract used as synthesis input, never as operator policy.
    draft_spec: Option<String>,
    /// Optional informational assertions; these never replace SPEC assertions.
    supplemental_assertions: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct ActivateAppParams {
    /// Opaque generation ID returned by prepare_app.
    generation_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct ExportAppParams {
    /// Opaque generation ID returned by prepare_app.
    generation_id: String,
    /// New or absent destination directory for the complete offline bundle.
    out_dir: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct InvokeAppParams {
    /// Absolute request path, optionally including a query string.
    path: String,
    /// HTTP method; defaults to GET.
    method: Option<String>,
    /// Request headers.
    headers: Option<HashMap<String, String>>,
    /// UTF-8 request body.
    body: Option<String>,
}

#[derive(Clone)]
pub struct NosrcMcpServer {
    pub lifecycle: Arc<Mutex<AppLifecycle>>,
}

async fn prepare_and_publish<F>(
    lifecycle: Arc<Mutex<AppLifecycle>>,
    timeout_duration: Duration,
    preparation: F,
) -> CallToolResult
where
    F: Future<Output = Result<PreparedGeneration, PreparationError>>,
{
    let operation = async {
        let candidate = preparation.await?;
        let mut lifecycle = lifecycle.lock().await;
        let id = lifecycle.insert_prepared(candidate);
        let status = lifecycle.get_status();
        let generation = status
            .generations
            .iter()
            .find(|item| item.generation_id == id.as_str());
        Ok::<CallToolResult, PreparationError>(CallToolResult::structured(json!({
            "generation_id": id.as_str(),
            "state": "ready",
            "module_hash": generation.map(|item| item.module_hash.as_str()),
            "trusted_assertions_passed": generation.map(|item| item.trusted_assertions_passed).unwrap_or(0),
            "model_assertions_passed": generation.map(|item| item.model_assertions_passed).unwrap_or(0),
            "model_assertions_failed": generation.map(|item| item.model_assertions_failed).unwrap_or(0),
            "supplemental_assertions_passed": generation.map(|item| item.supplemental_assertions_passed).unwrap_or(0),
            "supplemental_assertions_failed": generation.map(|item| item.supplemental_assertions_failed).unwrap_or(0)
        })))
    };

    match tokio::time::timeout(timeout_duration, operation).await {
        Ok(Ok(result)) => result,
        Ok(Err(PreparationError::Verification(results))) => {
            let failed_assertions: Vec<_> = results
                .iter()
                .map(|result| {
                    json!({
                        "name": result.name.as_str(),
                        "expected_status": result.expected_status,
                        "actual_status": result.actual_status,
                        "error": result.error_message.as_deref()
                    })
                })
                .collect();
            CallToolResult::structured_error(json!({
                "error": "verification_failed",
                "failed_assertions": failed_assertions
            }))
        }
        Ok(Err(PreparationError::Runtime(error))) => CallToolResult::structured_error(json!({
            "error": "preparation_failed",
            "message": error.to_string()
        })),
        Err(_) => CallToolResult::structured_error(json!({
            "error": "preparation_timeout",
            "message": format!(
                "prepare_app exceeded its {} second time limit",
                timeout_duration.as_secs()
            )
        })),
    }
}

#[tool_router]
impl NosrcMcpServer {
    #[tool(
        description = "Compile and verify a JavaScript handler against the operator-controlled SPEC without activating it. Preparation never activates a generation."
    )]
    async fn prepare_app(
        &self,
        Parameters(params): Parameters<PrepareAppParams>,
    ) -> CallToolResult {
        let source = params.source;
        let js_code = params.js_code;
        let draft_spec = params.draft_spec;
        if source == PrepareSource::DirectJs && js_code.is_none() {
            return CallToolResult::structured_error(json!({
                "error": "missing_js_code",
                "message": "js_code is required when source is 'direct_js'"
            }));
        }
        if source != PrepareSource::DirectJs && js_code.is_some() {
            return CallToolResult::structured_error(json!({
                "error": "unexpected_js_code",
                "message": "js_code is accepted only when source is 'direct_js'"
            }));
        }
        if source == PrepareSource::DraftSpec && draft_spec.is_none() {
            return CallToolResult::structured_error(json!({
                "error": "missing_draft_spec",
                "message": "draft_spec is required when source is 'draft_spec'"
            }));
        }
        if source != PrepareSource::DraftSpec && draft_spec.is_some() {
            return CallToolResult::structured_error(json!({
                "error": "unexpected_draft_spec",
                "message": "draft_spec is accepted only when source is 'draft_spec'"
            }));
        }
        let assertions = match params.supplemental_assertions {
            Some(values) => match values
                .into_iter()
                .map(serde_json::from_value)
                .collect::<Result<Vec<crate::synthesizer::ContractAssertion>, _>>()
            {
                Ok(assertions) => assertions,
                Err(error) => {
                    return CallToolResult::structured_error(json!({
                        "error": "invalid_supplemental_assertions",
                        "message": error.to_string()
                    }))
                }
            },
            None => Vec::new(),
        };
        let context = {
            let lifecycle = self.lifecycle.lock().await;
            lifecycle.preparation_context()
        };
        let preparation = async move {
            match source {
                PrepareSource::DirectJs => match js_code {
                    Some(code) => context.prepare_from_js(code, assertions).await,
                    None => Err(PreparationError::Runtime(
                        crate::error::EngineError::Internal(
                            "js_code is required when source is 'direct_js'".into(),
                        ),
                    )),
                },
                PrepareSource::Spec => context.prepare_from_spec(assertions).await,
                PrepareSource::DraftSpec => match draft_spec {
                    Some(spec) => context.prepare_from_draft_spec(spec, assertions).await,
                    None => Err(PreparationError::Runtime(
                        crate::error::EngineError::Internal("draft_spec is required".into()),
                    )),
                },
            }
        };
        prepare_and_publish(
            self.lifecycle.clone(),
            Duration::from_secs(120),
            preparation,
        )
        .await
    }

    #[tool(
        description = "Activate a previously prepared and verified generation. Only a generation in ready state can be activated."
    )]
    async fn activate_app(
        &self,
        Parameters(params): Parameters<ActivateAppParams>,
    ) -> CallToolResult {
        let mut lifecycle = self.lifecycle.lock().await;
        match lifecycle.activate(&GenerationId::from_string(params.generation_id.clone())) {
            Ok(()) => CallToolResult::structured(json!({
                "generation_id": params.generation_id,
                "state": "active"
            })),
            Err(error) => CallToolResult::structured_error(json!({
                "error": "activation_failed",
                "message": error.to_string()
            })),
        }
    }

    #[tool(
        description = "Export a prepared generation using the same atomic artifact pipeline as the CLI. The resulting bundle can be served offline with 'nosrc serve --bundle' without provider credentials."
    )]
    async fn export_app(
        &self,
        Parameters(params): Parameters<ExportAppParams>,
    ) -> CallToolResult {
        let export = {
            let lifecycle = self.lifecycle.lock().await;
            match lifecycle.export_generation(&GenerationId::from_string(params.generation_id.clone())) {
                Ok(export) => export,
                Err(error) => {
                    return CallToolResult::structured_error(json!({
                        "error": "export_unavailable",
                        "message": error.to_string()
                    }));
                }
            }
        };
        let out_dir = params.out_dir;
        let policy_mode = export.policy_mode.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::export::export_artifacts_with_policy(
                &out_dir,
                &export.spec,
                &export.spec_text,
                &export.output,
                &export.component_bytes,
                &export.module_hash,
                &export.trusted_assertions,
                &export.policy_mode,
            )
        })
        .await;
        match result {
            Ok(Ok(path)) => CallToolResult::structured(json!({
                "generation_id": params.generation_id,
                "bundle_path": path.display().to_string(),
                "policy_mode": policy_mode
            })),
            Ok(Err(error)) => CallToolResult::structured_error(json!({
                "error": "export_failed",
                "message": error.to_string()
            })),
            Err(error) => CallToolResult::structured_error(json!({
                "error": "export_task_failed",
                "message": error.to_string()
            })),
        }
    }

    #[tool(
        description = "Restore the immediately previous active generation. The current active generation becomes the rollback target."
    )]
    async fn rollback_app(&self) -> CallToolResult {
        let mut lifecycle = self.lifecycle.lock().await;
        match lifecycle.rollback() {
            Ok(id) => CallToolResult::structured(json!({
                "generation_id": id.as_str(),
                "state": "active",
                "operation": "rollback"
            })),
            Err(error) => CallToolResult::structured_error(json!({
                "error": "rollback_failed",
                "message": error.to_string()
            })),
        }
    }

    #[tool(
        description = "Invoke the currently active application handler with an HTTP-style request. An active generation is required."
    )]
    async fn invoke_app(&self, Parameters(params): Parameters<InvokeAppParams>) -> CallToolResult {
        if params.path.len() > 8192 {
            return CallToolResult::structured_error(json!({
                "error": "invalid_path",
                "message": "request URI exceeds the 8 KiB limit"
            }));
        }
        let parsed: axum::http::Uri = match params.path.parse() {
            Ok(uri) => uri,
            Err(error) => {
                return CallToolResult::structured_error(json!({
                    "error": "invalid_path",
                    "message": format!("invalid request URI: {error}")
                }))
            }
        };
        if !parsed.path().starts_with('/') {
            return CallToolResult::structured_error(json!({
                "error": "invalid_path",
                "message": "request path must be absolute"
            }));
        }
        let method = params
            .method
            .unwrap_or_else(|| "GET".to_string())
            .to_uppercase();
        if axum::http::Method::from_bytes(method.as_bytes()).is_err() {
            return CallToolResult::structured_error(json!({
                "error": "invalid_method",
                "message": "method is not a valid HTTP token"
            }));
        }
        if params
            .body
            .as_ref()
            .is_some_and(|value| value.len() > 1024 * 1024)
        {
            return CallToolResult::structured_error(json!({
                "error": "request_too_large",
                "message": "request body exceeds the 1 MiB HTTP ingress limit"
            }));
        }
        let supplied_headers = params.headers.unwrap_or_default();
        let header_bytes = supplied_headers.iter().fold(0usize, |total, (key, value)| {
            total.saturating_add(key.len()).saturating_add(value.len())
        });
        if supplied_headers.len() > 100 || header_bytes > 64 * 1024 {
            return CallToolResult::structured_error(json!({
                "error": "headers_too_large",
                "message": "request headers exceed the MCP ingress limits"
            }));
        }
        let mut headers = HashMap::new();
        for (name, value) in supplied_headers {
            let normalized_name = name.to_ascii_lowercase();
            if axum::http::HeaderName::try_from(normalized_name.as_str()).is_err()
                || axum::http::HeaderValue::try_from(value.as_str()).is_err()
            {
                return CallToolResult::structured_error(json!({
                    "error": "invalid_header",
                    "message": "request contains an invalid HTTP header name or value"
                }));
            }
            if headers.insert(normalized_name, value).is_some() {
                return CallToolResult::structured_error(json!({
                    "error": "duplicate_header",
                    "message": "request contains header names that differ only by case"
                }));
            }
        }
        let mut query = HashMap::new();
        if let Some(query_string) = parsed.query() {
            for (key, value) in form_urlencoded::parse(query_string.as_bytes()) {
                let key = key.into_owned();
                // Match HTTP ingress: repeated query keys collapse to the last value.
                query.insert(key, value.into_owned());
            }
        }
        let request = RequestEnvelope {
            method,
            path: parsed.path().to_string(),
            query,
            headers,
            body: params.body,
        };

        let handler = {
            let lifecycle = self.lifecycle.lock().await;
            match lifecycle.active_handler() {
                Ok(handler) => handler,
                Err(error) => {
                    return CallToolResult::structured_error(json!({
                        "error": "invocation_failed",
                        "message": error.to_string()
                    }))
                }
            }
        };
        let invocation = tokio::task::spawn_blocking(move || {
            let mut handler = handler
                .lock()
                .map_err(|_| "Active handler lock was poisoned".to_string())?;
            handler.invoke(&request).map_err(|error| error.to_string())
        });
        match tokio::time::timeout(Duration::from_secs(30), invocation).await {
            Ok(Ok(Ok(response))) => CallToolResult::structured(json!({
                "status": response.status,
                "headers": response.headers,
                "body": response.body
            })),
            Ok(Ok(Err(error))) => CallToolResult::structured_error(json!({
                "error": "invocation_failed",
                "message": error
            })),
            Ok(Err(error)) => CallToolResult::structured_error(json!({
                "error": "invocation_task_failed",
                "message": error.to_string()
            })),
            Err(_) => CallToolResult::structured_error(json!({
                "error": "invocation_timeout",
                "message": "invoke_app exceeded its 30 second time limit"
            })),
        }
    }

    #[tool(
        description = "Return the active generation, retained candidates, and trusted and supplemental verification summaries."
    )]
    async fn get_app_status(&self) -> CallToolResult {
        let lifecycle = self.lifecycle.lock().await;
        match serde_json::to_value(lifecycle.get_status()) {
            Ok(status) => CallToolResult::structured(status),
            Err(error) => CallToolResult::structured_error(json!({
                "error": "status_serialization_failed",
                "message": error.to_string()
            })),
        }
    }
}

#[tool_handler]
impl rmcp::ServerHandler for NosrcMcpServer {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "nosrc",
            env!("CARGO_PKG_VERSION"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_tools_complete_prepare_status_activate_invoke_flow() {
        use crate::engine::quickjs::test_support::HANDLER_SOURCE;
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-tool-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n- Body contains: `healthy`\n",
        ).unwrap();
        let lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let server = NosrcMcpServer {
            lifecycle: Arc::new(Mutex::new(lifecycle)),
        };

        let prepared = server
            .prepare_app(Parameters(PrepareAppParams {
                source: PrepareSource::DirectJs,
                js_code: Some(HANDLER_SOURCE.to_string()),
                draft_spec: None,
                supplemental_assertions: None,
            }))
            .await;
        assert_eq!(prepared.is_error, Some(false));
        let generation_id = prepared.structured_content.unwrap()["generation_id"]
            .as_str()
            .unwrap()
            .to_string();

        let status = server.get_app_status().await;
        assert_eq!(
            status.structured_content.unwrap()["generations"][0]["state"],
            "ready"
        );

        let activated = server
            .activate_app(Parameters(ActivateAppParams {
                generation_id: generation_id.clone(),
            }))
            .await;
        assert_eq!(activated.structured_content.unwrap()["state"], "active");

        // MCP activation publishes the selected generation to the HTTP request path.
        let ingress_state = server.lifecycle.lock().await.ingress_state();
        let app = crate::ingress::build_ingress_router(ingress_state);
        let http_response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .uri("/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(http_response.status(), axum::http::StatusCode::OK);

        let invoked = server
            .invoke_app(Parameters(InvokeAppParams {
                path: "/health".into(),
                method: None,
                headers: None,
                body: None,
            }))
            .await;
        assert_eq!(invoked.structured_content.unwrap()["body"], "healthy");

        let final_status = server.get_app_status().await;
        assert_eq!(
            final_status.structured_content.unwrap()["active_generation"].as_str(),
            Some(generation_id.as_str())
        );

        let echoed = server
            .invoke_app(Parameters(InvokeAppParams {
                path: "/echo?tag=first&tag=last".into(),
                method: None,
                headers: Some(HashMap::from([("X-Test".to_string(), "yes".to_string())])),
                body: None,
            }))
            .await;
        let echoed_body = echoed.structured_content.unwrap()["body"]
            .as_str()
            .unwrap()
            .to_string();
        let envelope: serde_json::Value = serde_json::from_str(&echoed_body).unwrap();
        assert_eq!(envelope["headers"]["x-test"], "yes");
        assert_eq!(envelope["query"]["tag"], "last");
    }

    #[tokio::test]
    async fn mcp_first_mode_prepares_direct_javascript_without_operator_policy() {
        use crate::engine::quickjs::test_support::HANDLER_SOURCE;

        let lifecycle = AppLifecycle::new_mcp_first(None).unwrap();
        let server = NosrcMcpServer {
            lifecycle: Arc::new(Mutex::new(lifecycle)),
        };
        let prepared = server
            .prepare_app(Parameters(PrepareAppParams {
                source: PrepareSource::DirectJs,
                js_code: Some(HANDLER_SOURCE.to_string()),
                draft_spec: None,
                supplemental_assertions: None,
            }))
            .await;
        assert_eq!(prepared.is_error, Some(false));
        assert_eq!(
            prepared.structured_content.unwrap()["trusted_assertions_passed"],
            0
        );
    }

    #[tokio::test]
    async fn rollback_tool_restores_previous_generation() {
        use crate::engine::quickjs::test_support;
        use crate::mcp::lifecycle::{prepare_compiled, SourceType};
        use crate::mcp::policy::AcceptancePolicy;
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-rollback-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n- Body contains: `healthy`\n",
        )
        .unwrap();
        let (engine, compiled) = test_support::compiled().await;
        let policy = AcceptancePolicy::from_spec(&spec).unwrap();

        let first_engine = engine.clone();
        let first_compiled = compiled.clone();
        let first_policy = policy.clone();
        let first = tokio::task::spawn_blocking(move || {
            prepare_compiled(
                first_engine,
                first_policy,
                first_compiled,
                test_support::HANDLER_SOURCE.to_string(),
                SourceType::DirectJs,
                Vec::new(),
                Vec::new(),
            )
        })
        .await
        .unwrap()
        .unwrap();

        let second_engine = engine.clone();
        let second_compiled = compiled.clone();
        let second_policy = policy.clone();
        let second = tokio::task::spawn_blocking(move || {
            prepare_compiled(
                second_engine,
                second_policy,
                second_compiled,
                test_support::HANDLER_SOURCE.to_string(),
                SourceType::DirectJs,
                Vec::new(),
                Vec::new(),
            )
        })
        .await
        .unwrap()
        .unwrap();

        let mut lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let first_id = lifecycle.insert_prepared(first);
        let second_id = lifecycle.insert_prepared(second);
        let server = NosrcMcpServer {
            lifecycle: Arc::new(Mutex::new(lifecycle)),
        };

        server
            .activate_app(Parameters(ActivateAppParams {
                generation_id: first_id.as_str().to_string(),
            }))
            .await;
        server
            .activate_app(Parameters(ActivateAppParams {
                generation_id: second_id.as_str().to_string(),
            }))
            .await;

        let rolled_back = server.rollback_app().await;
        assert_eq!(rolled_back.is_error, Some(false));
        assert_eq!(
            rolled_back.structured_content.unwrap()["generation_id"],
            first_id.as_str()
        );
        let status = server.get_app_status().await;
        assert_eq!(
            status.structured_content.unwrap()["active_generation"],
            first_id.as_str()
        );
    }

    #[tokio::test]
    async fn timed_out_blocking_preparation_cannot_publish_after_worker_finishes() {
        use crate::engine::quickjs::test_support;
        use crate::mcp::lifecycle::prepare_compiled;
        use crate::mcp::policy::AcceptancePolicy;
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-timeout-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n- Body contains: `healthy`\n",
        )
        .unwrap();
        let lifecycle = Arc::new(Mutex::new(AppLifecycle::new(&spec, None).unwrap()));
        let (engine, compiled) = test_support::compiled().await;
        let engine = engine.clone();
        let compiled = compiled.clone();
        let policy = AcceptancePolicy::from_spec(&spec).unwrap();
        let candidate = tokio::task::spawn_blocking(move || {
            prepare_compiled(
                engine,
                policy,
                compiled,
                test_support::HANDLER_SOURCE.to_string(),
                crate::mcp::lifecycle::SourceType::DirectJs,
                Vec::new(),
                Vec::new(),
            )
        })
        .await
        .unwrap()
        .unwrap();

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            let _ = finished_tx.send(());
            candidate
        });
        started_rx.await.unwrap();

        let preparation = async move {
            worker.await.map_err(|error| {
                PreparationError::Runtime(crate::error::EngineError::Internal(format!(
                    "Test preparation worker failed: {error}"
                )))
            })
        };
        let result =
            prepare_and_publish(lifecycle.clone(), Duration::from_millis(20), preparation).await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "preparation_timeout"
        );

        release_tx.send(()).unwrap();
        finished_rx.await.unwrap();

        let status = lifecycle.lock().await.get_status();
        assert!(status.generations.is_empty());
        assert!(status.active_generation.is_none());
    }

    #[tokio::test]
    async fn stdio_tools_list_exposes_rollback_app() {
        use crate::spec::ServiceSpec;
        use rmcp::ServiceExt;

        let spec = ServiceSpec::parse(
            "# Service: mcp-tools-list-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n",
        )
        .unwrap();
        let server = NosrcMcpServer {
            lifecycle: Arc::new(Mutex::new(AppLifecycle::new(&spec, None).unwrap())),
        };
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server.serve(server_stream).await.unwrap();
            service.waiting().await.unwrap();
        });

        let client = ().serve(client_stream).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        let mut names: Vec<String> = tools
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "activate_app",
                "get_app_status",
                "invoke_app",
                "prepare_app",
                "rollback_app",
            ]
        );

        client.cancel().await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_javascript_returns_an_mcp_error() {
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-invalid-js\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n",
        ).unwrap();
        let lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let server = NosrcMcpServer {
            lifecycle: Arc::new(Mutex::new(lifecycle)),
        };

        let result = server
            .prepare_app(Parameters(PrepareAppParams {
                source: PrepareSource::DirectJs,
                js_code: Some("export function handle(".into()),
                draft_spec: None,
                supplemental_assertions: None,
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "preparation_failed"
        );
    }

    #[tokio::test]
    async fn supplemental_assertions_cannot_bypass_operator_spec() {
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-trust-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n- Body contains: `healthy`\n",
        ).unwrap();
        let lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let server = NosrcMcpServer {
            lifecycle: Arc::new(Mutex::new(lifecycle)),
        };
        let bad_handler = r#"
            export function handle(requestJson) {
                const request = JSON.parse(requestJson);
                return JSON.stringify({
                    status: request.path === "/health" ? 404 : 200,
                    headers: {},
                    body: "healthy"
                });
            }
        "#;
        let supplemental = crate::synthesizer::ContractAssertion {
            name: "agent supplemental success".into(),
            request: RequestEnvelope {
                method: "GET".into(),
                path: "/agent-check".into(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: None,
            },
            expected_status: 200,
            body_contains: Some("healthy".into()),
            header_matches: None,
        };

        let result = server
            .prepare_app(Parameters(PrepareAppParams {
                source: PrepareSource::DirectJs,
                js_code: Some(bad_handler.into()),
                draft_spec: None,
                supplemental_assertions: Some(vec![serde_json::to_value(supplemental).unwrap()]),
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        let error = result.structured_content.unwrap();
        assert_eq!(error["error"], "verification_failed");
        assert_eq!(error["failed_assertions"][0]["expected_status"], 200);
        assert_eq!(error["failed_assertions"][0]["actual_status"], 404);
        let status = server.get_app_status().await;
        assert!(status.structured_content.unwrap()["generations"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn server_registers_exactly_the_four_v02_lifecycle_tools() {
        let tools = NosrcMcpServer::tool_router().list_all();
        let mut names: Vec<String> = tools
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "activate_app".to_string(),
                "get_app_status".to_string(),
                "invoke_app".to_string(),
                "prepare_app".to_string(),
                "rollback_app".to_string(),
            ]
        );
    }
}
