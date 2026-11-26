use std::collections::HashMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
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
use crate::error::{EngineError, SandboxTrap};
use crate::mcp::lifecycle::{
    AppLifecycle, GenerationId, PreparationError, PreparedGeneration, VerificationSuite,
};

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
    /// ES module source exporting the ABI v2 `handle(request)` function.
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
    /// Single directory name for the bundle, created under the operator-configured export root.
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
    pub export_root: PathBuf,
}

fn resolve_export_destination(root: &Path, name: &Path) -> anyhow::Result<PathBuf> {
    let mut components = name.components();
    let valid_name =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !valid_name {
        anyhow::bail!("out_dir must be a single directory name beneath the configured export root");
    }
    std::fs::create_dir_all(root)?;
    let canonical_root = std::fs::canonicalize(root)?;
    let destination = canonical_root.join(name);
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => anyhow::bail!("export destination already exists"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(destination)
}

const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1024;
const MAX_BODY_PREVIEW_BYTES: usize = 768;

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn redact_text(value: &str) -> String {
    let mut output = value.to_string();
    for marker in [
        "Bearer ",
        "api_key=",
        "api-key=",
        "password=",
        "secret=",
        "token=",
    ] {
        let mut search_from = 0;
        while let Some(relative) = output[search_from..]
            .to_ascii_lowercase()
            .find(&marker.to_ascii_lowercase())
        {
            let start = search_from + relative + marker.len();
            let end = output[start..]
                .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | '"' | '\''))
                .map(|offset| start + offset)
                .unwrap_or(output.len());
            if end > start {
                output.replace_range(start..end, "[REDACTED]");
                search_from = start + "[REDACTED]".len();
            } else {
                search_from = start;
            }
            if search_from >= output.len() {
                break;
            }
        }
    }
    output
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                let key = key.to_ascii_lowercase().replace('-', "_");
                let sensitive_keys = [
                    "authorization",
                    "cookie",
                    "set_cookie",
                    "password",
                    "passwd",
                    "secret",
                    "private_key",
                    "signing_key",
                    "token",
                    "api_key",
                    "apikey",
                    "credential",
                    "credentials",
                ];
                if sensitive_keys
                    .iter()
                    .any(|sensitive| key.contains(sensitive))
                {
                    *value = serde_json::Value::String("[REDACTED]".into());
                } else {
                    redact_json_value(value);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_value(item);
            }
        }
        serde_json::Value::String(text) => *text = redact_text(text),
        _ => {}
    }
}

fn safe_body_preview(body: Option<&str>) -> Option<String> {
    let body = body?;
    let mut value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    redact_json_value(&mut value);
    let serialized = serde_json::to_string(&value).ok()?;
    Some(truncate_utf8(&serialized, MAX_BODY_PREVIEW_BYTES))
}

fn safe_response_headers(
    headers: Option<&std::collections::HashMap<String, String>>,
) -> serde_json::Value {
    let Some(headers) = headers else {
        return serde_json::json!({});
    };

    // Sort before normalizing so mixed-case duplicate names resolve deterministically.
    let mut candidates: Vec<_> = headers
        .iter()
        .filter_map(|(name, value)| {
            let normalized = name.to_ascii_lowercase();
            matches!(
                normalized.as_str(),
                "content-type"
                    | "content-length"
                    | "cache-control"
                    | "etag"
                    | "vary"
                    | "x-content-type-options"
                    | "x-frame-options"
            )
            .then(|| (normalized, name, value))
        })
        .collect();
    candidates.sort_by(|left, right| (&left.0, left.1, left.2).cmp(&(&right.0, right.1, right.2)));

    let mut safe = serde_json::Map::new();
    let mut total_bytes = 0usize;
    for (normalized, _, value) in candidates {
        if safe.contains_key(&normalized) {
            continue;
        }
        if safe.len() >= 16 || total_bytes.saturating_add(value.len()) > 2048 {
            break;
        }
        total_bytes = total_bytes.saturating_add(value.len());
        safe.insert(
            normalized,
            serde_json::Value::String(truncate_utf8(&redact_text(value), 512)),
        );
    }
    serde_json::Value::Object(safe)
}

fn bound_verification_diagnostic(mut diagnostic: serde_json::Value) -> serde_json::Value {
    let original_count = diagnostic["failed_assertions"]
        .as_array()
        .map_or(0, Vec::len);
    let mut omitted_count = 0usize;

    loop {
        diagnostic["omitted_assertions"] = serde_json::json!(omitted_count);
        if serde_json::to_vec(&diagnostic)
            .is_ok_and(|serialized| serialized.len() <= MAX_DIAGNOSTIC_BYTES)
        {
            return diagnostic;
        }

        let Some(assertions) = diagnostic["failed_assertions"].as_array_mut() else {
            break;
        };
        if assertions.pop().is_none() {
            break;
        }
        omitted_count += 1;
    }

    // The fallback is deliberately small and still reports how many cases were omitted.
    serde_json::json!({
        "diagnostics_version": 1,
        "error": "verification_failed",
        "suite": diagnostic["suite"],
        "failed_assertions": [],
        "omitted_assertions": original_count,
        "message": "Diagnostic details omitted to satisfy the response size limit"
    })
}

fn resource_limit(error: &EngineError) -> Option<&'static str> {
    match error {
        EngineError::Trap(SandboxTrap::FuelExhausted) => Some("fuel"),
        EngineError::Trap(SandboxTrap::MemoryExhausted(_)) => Some("linear_memory"),
        EngineError::Trap(SandboxTrap::ExecutionTimeout) => Some("wall_clock"),
        EngineError::Instantiation(message) if is_memory_limit_message(message) => {
            Some("linear_memory")
        }
        _ => None,
    }
}

fn is_memory_limit_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("memory limit")
        || message.contains("failed to grow memory")
        || message.contains("memory growth")
        || message.contains("growth operation limited")
}

fn safe_compiler_diagnostics(message: &str) -> String {
    let diagnostics = message
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let lower = trimmed.to_ascii_lowercase();
            let is_source_context = trimmed.contains(" | ")
                || trimmed.starts_with('>')
                || ["export ", "function ", "const ", "let ", "return "]
                    .iter()
                    .any(|source| lower.contains(source));
            let is_diagnostic = [
                "error",
                "warning",
                "syntax",
                "unexpected",
                "expected",
                "invalid",
                "unsupported",
                "failed",
                "line ",
                "column ",
                " at ",
            ]
            .iter()
            .any(|marker| lower.contains(marker));
            (is_diagnostic && !is_source_context).then(|| redact_text(trimmed))
        })
        .take(12)
        .collect::<Vec<_>>()
        .join("\n");

    if diagnostics.is_empty() {
        "JavaScript componentization failed; source context was omitted.".into()
    } else {
        truncate_utf8(&diagnostics, MAX_DIAGNOSTIC_BYTES)
    }
}

fn runtime_diagnostic(error: EngineError) -> serde_json::Value {
    if let Some(resource) = resource_limit(&error) {
        return serde_json::json!({
            "diagnostics_version": 1,
            "error": "resource_limit_exceeded",
            "resource": resource,
            "message": truncate_utf8(&redact_text(&error.to_string()), 1024)
        });
    }

    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    if lower.contains("componentization")
        || lower.contains("invalid handler component")
        || lower.contains("wasm compilation")
    {
        return serde_json::json!({
            "diagnostics_version": 1,
            "error": "compilation_failed",
            "diagnostics": safe_compiler_diagnostics(&message)
        });
    }

    serde_json::json!({
        "diagnostics_version": 1,
        "error": "preparation_failed",
        "message": truncate_utf8(&redact_text(&message), 1024)
    })
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
            "activation_eligible": generation.map(|item| item.trusted_assertions_passed > 0).unwrap_or(false),
            "model_assertions_passed": generation.map(|item| item.model_assertions_passed).unwrap_or(0),
            "model_assertions_failed": generation.map(|item| item.model_assertions_failed).unwrap_or(0),
            "supplemental_assertions_passed": generation.map(|item| item.supplemental_assertions_passed).unwrap_or(0),
            "supplemental_assertions_failed": generation.map(|item| item.supplemental_assertions_failed).unwrap_or(0),
            "telemetry": generation.and_then(|item| item.telemetry.as_ref())
        })))
    };

    match tokio::time::timeout(timeout_duration, operation).await {
        Ok(Ok(result)) => result,
        Ok(Err(PreparationError::Verification { suite, results })) => {
            let suite_name = match suite {
                VerificationSuite::Operator => "operator",
                VerificationSuite::Generated => "generated",
            };
            let failed_assertions: Vec<_> = results
                .iter()
                .map(|result| {
                    let mut diagnostic = json!({
                        "id": format!("{suite_name}-{:04}", result.case_index + 1),
                        "failure": result.failure_kind.unwrap_or("assertion_failed"),
                        "expected_status": result.expected_status,
                        "expected_status_max": result.expected_status_max,
                        "actual_status": result.actual_status,
                        "response_headers": safe_response_headers(result.actual_headers.as_ref())
                    });
                    if let Some(preview) = safe_body_preview(result.actual_body.as_deref()) {
                        diagnostic["body_preview"] = json!(preview);
                    } else if result.actual_body.is_some() {
                        diagnostic["body_preview_omitted"] =
                            json!("Non-JSON response bodies are omitted from MCP diagnostics");
                    }
                    if let Some(resource) = result.failure_kind.and_then(|kind| match kind {
                        "fuel_exhausted" => Some("fuel"),
                        "memory_exhausted" => Some("linear_memory"),
                        "execution_timeout" => Some("wall_clock"),
                        _ => None,
                    }) {
                        diagnostic["resource"] = json!(resource);
                    }
                    diagnostic
                })
                .collect();
            CallToolResult::structured_error(bound_verification_diagnostic(json!({
                "diagnostics_version": 1,
                "error": "verification_failed",
                "suite": suite_name,
                "failed_assertions": failed_assertions,
                "policy_note": "Verification policy is operator-controlled and is not included in diagnostics."
            })))
        }
        Ok(Err(PreparationError::Runtime(error))) => {
            CallToolResult::structured_error(runtime_diagnostic(error))
        }
        Err(_) => CallToolResult::structured_error(json!({
            "diagnostics_version": 1,
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
        description = "Compile and verify a JavaScript handler against the operator-controlled SPEC without activating it. Preparation never activates a generation. Failures return bounded diagnostics without exposing trusted assertion contents."
    )]
    async fn prepare_app(
        &self,
        Parameters(params): Parameters<PrepareAppParams>,
    ) -> CallToolResult {
        self.prepare_app_with_synthesizer(params, None).await
    }

    async fn prepare_app_with_synthesizer(
        &self,
        params: PrepareAppParams,
        draft_synthesizer: Option<crate::synthesizer::Synthesizer>,
    ) -> CallToolResult {
        self.prepare_app_with_synthesizer_timeout(
            params,
            draft_synthesizer,
            Duration::from_secs(120),
        )
        .await
    }

    async fn prepare_app_with_synthesizer_timeout(
        &self,
        params: PrepareAppParams,
        draft_synthesizer: Option<crate::synthesizer::Synthesizer>,
        timeout_duration: Duration,
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
                    Some(spec) => match draft_synthesizer {
                        Some(synthesizer) => {
                            context
                                .prepare_from_draft_spec_with_synthesizer(
                                    spec,
                                    assertions,
                                    synthesizer,
                                )
                                .await
                        }
                        None => context.prepare_from_draft_spec(spec, assertions).await,
                    },
                    None => Err(PreparationError::Runtime(
                        crate::error::EngineError::Internal("draft_spec is required".into()),
                    )),
                },
            }
        };
        prepare_and_publish(self.lifecycle.clone(), timeout_duration, preparation).await
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
        description = "Export a prepared generation beneath the operator-configured export root. out_dir must be one directory name, not an absolute path or path containing parent or subdirectory components. The resulting bundle can be served offline with 'nosrc serve --bundle' without provider credentials."
    )]
    async fn export_app(&self, Parameters(params): Parameters<ExportAppParams>) -> CallToolResult {
        let export = {
            let lifecycle = self.lifecycle.lock().await;
            match lifecycle
                .export_generation(&GenerationId::from_string(params.generation_id.clone()))
            {
                Ok(export) => export,
                Err(error) => {
                    return CallToolResult::structured_error(json!({
                        "error": "export_unavailable",
                        "message": error.to_string()
                    }));
                }
            }
        };
        let out_dir = match resolve_export_destination(&self.export_root, &params.out_dir) {
            Ok(path) => path,
            Err(error) => {
                return CallToolResult::structured_error(json!({
                    "error": "invalid_export_destination",
                    "message": error.to_string()
                }))
            }
        };
        let policy_mode = export.policy_mode.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::export::export_artifacts_with_policy(
                &out_dir,
                &export.spec,
                &export.spec_text,
                &export.output,
                &export.component_bytes,
                &export.module_hash,
                crate::export::ExportPolicy {
                    trusted_assertions: &export.trusted_assertions,
                    mode: &export.policy_mode,
                    repair_summary: &export.repair_summary,
                },
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
        let request = RequestEnvelope {
            method,
            scheme: None,
            authority: None,
            path: parsed.path().to_string(),
            query: parsed.query().map(str::to_string),
            headers: headers
                .into_iter()
                .map(|(name, value)| crate::engine::http_envelope::Header {
                    name,
                    value: value.into_bytes(),
                })
                .collect(),
            body: params.body.map(String::into_bytes).unwrap_or_default(),
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
            export_root: std::env::temp_dir().join("nosrc-mcp-test-exports"),
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
        assert_eq!(
            prepared.is_error,
            Some(false),
            "prepare_app failed: {:?}",
            prepared.structured_content
        );
        let prepared_content = prepared.structured_content.unwrap();
        assert_eq!(prepared_content["telemetry"]["schema_version"], 1);
        assert_eq!(prepared_content["telemetry"]["instance_recreations"], 0);
        assert!(prepared_content["telemetry"]["last_invocation_fuel_consumed"].is_null());
        assert!(prepared_content["telemetry"]["generation_fuel_high_water"].is_null());
        for field in [
            "instance_memory_high_water_bytes",
            "generation_memory_high_water_bytes",
        ] {
            let value = &prepared_content["telemetry"][field];
            assert!(
                value.is_null()
                    || value
                        .as_u64()
                        .is_some_and(|bytes| bytes <= 64 * 1024 * 1024),
                "memory telemetry must be null or a bounded byte count: {value}"
            );
        }
        let generation_id = prepared_content["generation_id"]
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
        assert_eq!(
            invoked.structured_content.unwrap()["body"],
            serde_json::json!({ "tag": "text", "val": "healthy" })
        );

        let final_status = server.get_app_status().await;
        let final_status = final_status.structured_content.unwrap();
        assert_eq!(
            final_status["active_generation"].as_str(),
            Some(generation_id.as_str())
        );
        let telemetry = &final_status["generations"][0]["telemetry"];
        assert_eq!(telemetry["schema_version"], 1);
        assert!(telemetry["last_invocation_fuel_consumed"]
            .as_u64()
            .is_some());
        assert!(telemetry["generation_fuel_high_water"].as_u64().is_some());
        assert!(telemetry["instance_memory_high_water_bytes"]
            .as_u64()
            .is_some());
        assert!(telemetry["generation_memory_high_water_bytes"]
            .as_u64()
            .is_some());

        let echoed = server
            .invoke_app(Parameters(InvokeAppParams {
                path: "/echo?tag=first&tag=last".into(),
                method: None,
                headers: Some(HashMap::from([("X-Test".to_string(), "yes".to_string())])),
                body: None,
            }))
            .await;
        let echoed_content = echoed.structured_content.unwrap();
        assert_eq!(echoed_content["body"]["tag"], "text");
        let echoed_body = echoed_content["body"]["val"].as_str().unwrap();
        let envelope: serde_json::Value = serde_json::from_str(echoed_body).unwrap();
        assert_eq!(envelope["headerNames"], "x-test");
        // ABI v2 preserves the raw encoded query instead of parsing or collapsing duplicates.
        assert_eq!(envelope["query"], "tag=first&tag=last");
    }

    #[tokio::test]
    async fn export_app_reuses_existing_bundle_pipeline_and_serves_offline() {
        use crate::engine::quickjs::test_support::HANDLER_SOURCE;
        use crate::spec::ServiceSpec;

        let spec_text = "# Service: mcp-export-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n- Body contains: `healthy`\n";
        let spec = ServiceSpec::parse(spec_text).unwrap();
        let lifecycle =
            AppLifecycle::new_with_spec_text(&spec, spec_text.to_string(), None).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let server = NosrcMcpServer {
            export_root: temp.path().join("exports"),
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

        let bundle_name = PathBuf::from("offline-bundle");
        let bundle_path = server.export_root.join(&bundle_name);
        let exported = server
            .export_app(Parameters(ExportAppParams {
                generation_id,
                out_dir: bundle_name,
            }))
            .await;
        assert_eq!(exported.is_error, Some(false));
        assert_eq!(
            std::fs::read_to_string(bundle_path.join("SPEC.md")).unwrap(),
            spec_text
        );

        let (_, summary) = crate::bundle::load_and_verify(&bundle_path).unwrap();
        assert_eq!(summary.service_name, "mcp-export-test");
        assert_eq!(summary.trusted_assertions, 1);
        assert_eq!(summary.generated_assertions, 0);
    }

    #[tokio::test]
    async fn mcp_first_mode_prepares_direct_javascript_without_operator_policy() {
        use crate::engine::quickjs::test_support::HANDLER_SOURCE;

        let lifecycle = AppLifecycle::new_mcp_first(None).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let server = NosrcMcpServer {
            export_root: temp.path().join("exports"),
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
        let activation = server
            .activate_app(Parameters(ActivateAppParams { generation_id }))
            .await;
        assert_eq!(activation.is_error, Some(true));
        let activation_error = activation.structured_content.unwrap()["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            activation_error.contains(
                "Activation requires an operator-controlled SPEC with at least one trusted verification case; review the draft and restart nosrc with --spec"
            ),
            "unexpected activation error: {activation_error}"
        );
        assert!(server
            .lifecycle
            .lock()
            .await
            .get_status()
            .active_generation
            .is_none());
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
            export_root: std::env::temp_dir().join("nosrc-mcp-test-exports"),
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
    async fn stdio_tools_list_exposes_export_app() {
        use crate::spec::ServiceSpec;
        use rmcp::ServiceExt;

        let spec = ServiceSpec::parse(
            "# Service: mcp-tools-list-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n",
        )
        .unwrap();
        let server = NosrcMcpServer {
            export_root: std::env::temp_dir().join("nosrc-mcp-test-exports"),
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
                "export_app",
                "get_app_status",
                "invoke_app",
                "prepare_app",
                "rollback_app",
            ]
        );

        client.cancel().await.unwrap();
        server_task.await.unwrap();
    }

    #[test]
    fn verification_diagnostics_are_bounded_after_serialization() {
        let diagnostic = json!({
            "diagnostics_version": 1,
            "error": "verification_failed",
            "suite": "operator",
            "failed_assertions": (0..100).map(|index| json!({
                "id": format!("operator-{index:04}"),
                "failure": "body_mismatch",
                "expected_status": 200,
                "actual_status": 200,
                "response_headers": {"content-type": "application/json"},
                "body_preview": "x".repeat(MAX_BODY_PREVIEW_BYTES)
            })).collect::<Vec<_>>(),
            "policy_note": "Verification policy is operator-controlled and is not included in diagnostics."
        });

        let bounded = bound_verification_diagnostic(diagnostic);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(bounded["omitted_assertions"].as_u64().unwrap() > 0);
        assert_eq!(bounded["error"], "verification_failed");
    }

    #[test]
    fn body_preview_is_omitted_when_body_is_absent_or_non_json() {
        assert!(safe_body_preview(None).is_none());
        assert!(safe_body_preview(Some("plain text response")).is_none());
        assert!(safe_body_preview(Some("<html>not json</html>")).is_none());
    }

    #[test]
    fn response_header_allowlist_normalizes_case_and_resolves_duplicates_deterministically() {
        let headers = HashMap::from([
            ("Content-Type".to_string(), "text/plain".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            ("Set-Cookie".to_string(), "secret=value".to_string()),
        ]);
        let safe = safe_response_headers(Some(&headers));
        assert_eq!(safe["content-type"], "text/plain");
        assert!(safe.get("set-cookie").is_none());
    }

    #[test]
    fn compiler_diagnostics_omit_source_context() {
        let message = "QuickJS componentization failed:\nerror: Unexpected token\n  1 | const password = \"must-not-leak\";";
        let diagnostics = safe_compiler_diagnostics(message);
        assert!(diagnostics.contains("Unexpected token"));
        assert!(!diagnostics.contains("must-not-leak"));
        assert!(!diagnostics.contains("const password"));
    }

    #[tokio::test]
    async fn invalid_javascript_returns_an_mcp_error() {
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-invalid-js\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n",
        ).unwrap();
        let lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let server = NosrcMcpServer {
            export_root: std::env::temp_dir().join("nosrc-mcp-test-exports"),
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
            "compilation_failed"
        );
    }

    #[tokio::test]
    async fn prepare_app_reports_resource_exhaustion_category() {
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-resource-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /loop`\n- Response `200`\n\n## Verification\n### `GET /loop`\n- Response `200`\n",
        ).unwrap();
        let server = NosrcMcpServer {
            export_root: std::env::temp_dir().join("nosrc-mcp-test-exports"),
            lifecycle: Arc::new(Mutex::new(AppLifecycle::new(&spec, None).unwrap())),
        };
        let result = server
            .prepare_app(Parameters(PrepareAppParams {
                source: PrepareSource::DirectJs,
                js_code: Some("export function handle(_request) { while (true) {} }".into()),
                draft_spec: None,
                supplemental_assertions: None,
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        let error = result.structured_content.unwrap();
        assert_eq!(error["error"], "verification_failed");
        assert_eq!(error["suite"], "operator");
        assert_eq!(error["failed_assertions"][0]["id"], "operator-0001");
        assert_eq!(error["failed_assertions"][0]["resource"], "fuel");
        assert_eq!(error["failed_assertions"][0]["failure"], "fuel_exhausted");
    }

    #[tokio::test]
    async fn supplemental_assertions_cannot_bypass_operator_spec() {
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-trust-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n- Body contains: `healthy`\n",
        ).unwrap();
        let lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let server = NosrcMcpServer {
            export_root: std::env::temp_dir().join("nosrc-mcp-test-exports"),
            lifecycle: Arc::new(Mutex::new(lifecycle)),
        };
        let bad_handler = r#"
            export function handle(request) {
                return {
                    status: request.path === "/health" ? 404 : 200,
                    headers: [],
                    body: { tag: "text", val: "healthy" }
                };
            }
        "#;
        let supplemental = crate::synthesizer::ContractAssertion {
            name: "agent supplemental success".into(),
            request: RequestEnvelope {
                method: "GET".into(),
                scheme: None,
                authority: None,
                path: "/agent-check".into(),
                query: None,
                headers: vec![],
                body: vec![],
            },
            expected_status: 200,
            expected_status_max: None,
            body_contains: Some("healthy".into()),
            header_matches: None,
            response_json_schema: None,
            json_path_assertions: Vec::new(),
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
        assert_eq!(error["suite"], "operator");
        assert_eq!(error["failed_assertions"][0]["id"], "operator-0001");
        assert_eq!(error["failed_assertions"][0]["failure"], "status_mismatch");
        assert_eq!(error["failed_assertions"][0]["expected_status"], 200);
        assert_eq!(error["failed_assertions"][0]["actual_status"], 404);
        assert!(error["failed_assertions"][0].get("name").is_none());
        let status = server.get_app_status().await;
        assert!(status.structured_content.unwrap()["generations"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn verification_diagnostics_include_redacted_json_body_preview_and_safe_headers() {
        use crate::spec::ServiceSpec;

        let spec = ServiceSpec::parse(
            "# Service: mcp-body-diagnostic\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`\n\n## Verification\n### `GET /health`\n- Response `200`\n- Body contains: `expected-value`\n",
        ).unwrap();
        let server = NosrcMcpServer {
            export_root: std::env::temp_dir().join("nosrc-mcp-test-exports"),
            lifecycle: Arc::new(Mutex::new(AppLifecycle::new(&spec, None).unwrap())),
        };
        let handler = r#"
            export function handle(_request) {
                return {
                    status: 200,
                    headers: [
                        { name: "content-type", value: [97,112,112,108,105,99,97,116,105,111,110,47,106,115,111,110] },
                        { name: "set-cookie", value: [115,101,115,115,105,111,110,61,112,114,105,118,97,116,101] }
                    ],
                    body: { tag: "text", val: JSON.stringify({
                        message: "actual-value",
                        api_key: "must-not-leak"
                    }) }
                };
            }
        "#;
        let result = server
            .prepare_app(Parameters(PrepareAppParams {
                source: PrepareSource::DirectJs,
                js_code: Some(handler.into()),
                draft_spec: None,
                supplemental_assertions: None,
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        let error = result.structured_content.unwrap();
        let diagnostic = &error["failed_assertions"][0];
        assert_eq!(diagnostic["failure"], "body_mismatch");
        assert_eq!(
            diagnostic["response_headers"]["content-type"],
            "application/json"
        );
        assert!(diagnostic["response_headers"].get("set-cookie").is_none());
        let preview = diagnostic["body_preview"].as_str().unwrap();
        assert!(preview.contains("[REDACTED]"));
        assert!(!preview.contains("must-not-leak"));
    }

    #[test]
    fn server_registers_all_six_lifecycle_tools() {
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
                "export_app".to_string(),
                "get_app_status".to_string(),
                "invoke_app".to_string(),
                "prepare_app".to_string(),
                "rollback_app".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn mcp_first_draft_lifecycle_exports_and_serves_offline_end_to_end() {
        use axum::extract::State;
        use axum::routing::post;
        use axum::{Json, Router};
        use serde_json::{json, Value};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        async fn mock_draft_synthesis(
            State((request_count, repair_feedback_seen)): State<(
                Arc<AtomicUsize>,
                Arc<AtomicBool>,
            )>,
            Json(request): Json<Value>,
        ) -> Json<Value> {
            let request_index = request_count.fetch_add(1, Ordering::SeqCst);
            let user_prompt = request["messages"][1]["content"]
                .as_str()
                .unwrap_or_default();
            if request_index > 0
                && user_prompt.contains("candidate-generated verification")
                && user_prompt.contains("assertion definitions are intentionally withheld")
            {
                repair_feedback_seen.store(true, Ordering::SeqCst);
            }
            let handler_code = if request_index == 0 {
                r#"
                    export function handle(_request) {
                        return {
                            status: 500,
                            headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                            body: { tag: "text", val: "first candidate fails" }
                        };
                    }
                "#
            } else {
                r#"
                    export function handle(request) {
                        if (request.method === "GET" && request.path === "/hello") {
                            return {
                                status: 200,
                                headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                                body: { tag: "text", val: "hello from nosrc" }
                            };
                        }
                        return {
                            status: 404,
                            headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                            body: { tag: "text", val: "not found" }
                        };
                    }
                "#
            };
            let synthesis = json!({
                "handler_code": handler_code,
                "code_format": "javascript",
                "assertions": [{
                    "name": "generated hello response",
                    "request": {
                        "method": "GET",
                        "scheme": null,
                        "authority": null,
                        "path": "/hello",
                        "query": null,
                        "headers": [],
                        "body": []
                    },
                    "expected_status": 200,
                    "body_contains": "hello from nosrc"
                }],
                "rationale": "Deterministic MCP-first end-to-end fixture"
            });
            Json(json!({
                "choices": [{
                    "message": {
                        "content": serde_json::to_string(&synthesis).unwrap()
                    }
                }]
            }))
        }

        async fn serve_http(
            listener: tokio::net::TcpListener,
            app: axum::Router,
            shutdown: tokio::sync::oneshot::Receiver<()>,
        ) {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown.await;
                })
                .await
                .unwrap();
        }

        let request_count = Arc::new(AtomicUsize::new(0));
        let repair_feedback_seen = Arc::new(AtomicBool::new(false));
        let mock_provider = Router::new()
            .route("/v1/chat/completions", post(mock_draft_synthesis))
            .with_state((request_count.clone(), repair_feedback_seen.clone()));
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_address = provider_listener.local_addr().unwrap();
        let (provider_shutdown_tx, provider_shutdown_rx) = tokio::sync::oneshot::channel();
        let provider_task = tokio::spawn(serve_http(
            provider_listener,
            mock_provider,
            provider_shutdown_rx,
        ));

        // MCP-first startup has no active generation and needs no operator SPEC.
        let lifecycle = AppLifecycle::new_mcp_first(None).unwrap();
        assert_eq!(lifecycle.get_status().status, "idle");
        assert!(lifecycle.get_status().active_generation.is_none());
        let server = NosrcMcpServer {
            export_root: std::env::temp_dir().join("nosrc-mcp-test-exports"),
            lifecycle: Arc::new(Mutex::new(lifecycle)),
        };

        let draft_spec = "# Service: mcp-first-e2e\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /hello`\n- Response `200`: hello from nosrc\n\n## Verification\n### `GET /hello`\n- Response `200`\n- Body contains: `hello from nosrc`\n";
        let synthesizer = crate::synthesizer::Synthesizer::new(crate::synthesizer::LlmConfig {
            api_key: "test-only-key".into(),
            base_url: format!("http://{provider_address}/v1"),
            model: "nosrc-mcp-first-e2e".into(),
            temperature: 0.0,
        });
        // Exercise the same validation, preparation, and publication path as the MCP tool,
        // injecting only the local mock provider instead of reading process credentials.
        let prepared = server
            .prepare_app_with_synthesizer_timeout(
                PrepareAppParams {
                    source: PrepareSource::DraftSpec,
                    js_code: None,
                    draft_spec: Some(draft_spec.to_string()),
                    supplemental_assertions: None,
                },
                Some(synthesizer),
                Duration::from_secs(300),
            )
            .await;
        assert_eq!(
            prepared.is_error,
            Some(false),
            "draft preparation failed: {:?}",
            prepared.structured_content
        );
        let generation_id = prepared.structured_content.unwrap()["generation_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert!(repair_feedback_seen.load(Ordering::SeqCst));

        // Agent-authored draft policy is exportable for review, but cannot activate.
        let activation = server
            .activate_app(Parameters(ActivateAppParams {
                generation_id: generation_id.clone(),
            }))
            .await;
        assert_eq!(activation.is_error, Some(true));
        assert!(server
            .lifecycle
            .lock()
            .await
            .get_status()
            .active_generation
            .is_none());

        // Export through the MCP tool and confirm the intent and implementation coexist.
        let temp = tempfile::tempdir().unwrap();
        let export_root = temp.path().join("exports");
        let bundle_name = PathBuf::from("mcp-first-bundle");
        let bundle_path = export_root.join(&bundle_name);
        let export_server = NosrcMcpServer {
            export_root: export_root.clone(),
            lifecycle: server.lifecycle.clone(),
        };
        let exported = export_server
            .export_app(Parameters(ExportAppParams {
                generation_id,
                out_dir: bundle_name,
            }))
            .await;
        assert_eq!(
            exported.is_error,
            Some(false),
            "export failed: {:?}",
            exported.structured_content
        );
        assert_eq!(
            std::fs::read_to_string(bundle_path.join("SPEC.md")).unwrap(),
            draft_spec
        );
        assert!(bundle_path.join("handler.js").is_file());
        assert!(bundle_path.join("component.wasm").is_file());
        assert!(bundle_path.join("assertions.json").is_file());
        assert!(bundle_path.join("manifest.json").is_file());
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(bundle_path.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["policy_mode"], "draft");

        // A fresh offline runtime validates the exported component and both suites.
        let (offline_generation, summary) = crate::bundle::load_and_verify(&bundle_path).unwrap();
        assert_eq!(summary.service_name, "mcp-first-e2e");
        assert_eq!(summary.trusted_assertions, 0);
        assert_eq!(summary.generated_assertions, 2);
        let offline_state = Arc::new(crate::ingress::IngressState::new());
        offline_state.hot_swap(offline_generation);
        let offline_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let offline_address = offline_listener.local_addr().unwrap();
        let offline_app = crate::ingress::build_ingress_router(offline_state);
        let (offline_shutdown_tx, offline_shutdown_rx) = tokio::sync::oneshot::channel();
        let offline_task = tokio::spawn(serve_http(
            offline_listener,
            offline_app,
            offline_shutdown_rx,
        ));
        let offline_response = reqwest::get(format!("http://{offline_address}/hello"))
            .await
            .unwrap();
        assert_eq!(offline_response.status(), reqwest::StatusCode::OK);
        assert_eq!(offline_response.text().await.unwrap(), "hello from nosrc");

        // Offline serving must not synthesize again or contact the mock provider.
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        // Process-local MCP state is intentionally empty after a restart.
        let restarted = AppLifecycle::new_mcp_first(None).unwrap();
        assert_eq!(restarted.get_status().status, "idle");
        assert!(restarted.get_status().active_generation.is_none());

        let _ = offline_shutdown_tx.send(());
        let _ = provider_shutdown_tx.send(());
        offline_task.await.unwrap();
        provider_task.await.unwrap();
    }
    #[test]
    fn export_destination_rejects_paths_outside_the_configured_root() {
        let temp = tempfile::tempdir().unwrap();
        for invalid in [
            Path::new("../outside"),
            Path::new("nested/bundle"),
            Path::new("/tmp/outside"),
            Path::new("."),
            Path::new(""),
        ] {
            assert!(
                resolve_export_destination(temp.path(), invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        let resolved = resolve_export_destination(temp.path(), Path::new("bundle")).unwrap();
        assert_eq!(resolved, temp.path().canonicalize().unwrap().join("bundle"));
    }

    #[cfg(unix)]
    #[test]
    fn export_destination_rejects_symlink_destinations() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = temp.path().join("escape");
        symlink(outside.path(), &link).unwrap();

        assert!(resolve_export_destination(temp.path(), Path::new("escape")).is_err());
    }
}
