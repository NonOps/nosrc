use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Write};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::engine::http_envelope::RequestEnvelope;
use crate::error::EngineError;
use crate::spec::ServiceSpec;

const LLM_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const LLM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LLM_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_LLM_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_LLM_ERROR_BYTES: usize = 8 * 1024;
/// Total attempts per synthesis request, including the first one.
const MAX_LLM_ATTEMPTS: u32 = 3;
const LLM_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

/// Configuration for the OpenAI-compatible LLM endpoint
#[derive(Clone)]
pub struct LlmConfig {
    /// Empty when no key is configured; synthesis then fails before any request is sent.
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub temperature: f32,
}

impl fmt::Debug for LlmConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmConfig")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("temperature", &self.temperature)
            .finish()
    }
}

impl LlmConfig {
    /// Read the configuration from the environment and require `LLM_API_KEY`.
    pub fn from_env() -> Result<Self, EngineError> {
        Self::from_lookup(|name| std::env::var(name).ok(), true)
    }

    /// Like `from_env`, but a missing `LLM_API_KEY` is reported only when synthesis is
    /// actually attempted, so a verified cache hit does not need provider credentials.
    pub fn from_env_deferred_key() -> Result<Self, EngineError> {
        Self::from_lookup(|name| std::env::var(name).ok(), false)
    }

    fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        require_key: bool,
    ) -> Result<Self, EngineError> {
        let api_key = match lookup("LLM_API_KEY") {
            Some(key) if !key.trim().is_empty() => key,
            _ if require_key => return Err(missing_api_key_error()),
            _ => String::new(),
        };

        let base_url =
            lookup("LLM_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        validate_base_url(&base_url)?;

        let model = lookup("LLM_MODEL").unwrap_or_else(|| "gpt-4o-mini".to_string());

        Ok(Self {
            api_key,
            base_url,
            model,
            temperature: 0.1,
        })
    }
}

fn missing_api_key_error() -> EngineError {
    EngineError::Internal("Missing required environment variable LLM_API_KEY".into())
}

/// Reject provider URLs that would send the API key over an unencrypted, non-loopback connection.
fn validate_base_url(base_url: &str) -> Result<(), EngineError> {
    let url = reqwest::Url::parse(base_url).map_err(|error| {
        EngineError::Internal(format!("LLM_BASE_URL is not a valid URL: {error}"))
    })?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(url.host_str().unwrap_or("")) => Ok(()),
        "http" => Err(EngineError::Internal(
            "LLM_BASE_URL must use https unless it points at a loopback address".into(),
        )),
        other => Err(EngineError::Internal(format!(
            "LLM_BASE_URL must use https, got scheme '{other}'"
        ))),
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Remove credential material from a provider error body before it is logged or returned.
///
/// Providers sometimes echo the rejected key, in full or partially masked (for example
/// `sk-proj-****abcd`), so both the configured key and `sk-` style tokens are replaced.
fn scrub_provider_error(body: &str, api_key: &str) -> String {
    let without_key = if api_key.is_empty() {
        body.to_string()
    } else {
        body.replace(api_key, "[redacted]")
    };

    let is_token_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '*' | '.');
    let mut scrubbed = String::with_capacity(without_key.len());
    let mut token = String::new();
    for c in without_key.chars() {
        if is_token_char(c) {
            token.push(c);
        } else {
            push_scrubbed_token(&mut scrubbed, &mut token);
            scrubbed.push(c);
        }
    }
    push_scrubbed_token(&mut scrubbed, &mut token);
    scrubbed
}

fn push_scrubbed_token(output: &mut String, token: &mut String) {
    if token.starts_with("sk-") {
        output.push_str("[redacted]");
    } else {
        output.push_str(token);
    }
    token.clear();
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Exponential backoff between attempts: 500 ms, then 1 s.
fn retry_delay(attempt: u32) -> Duration {
    LLM_RETRY_BASE_DELAY * 2u32.saturating_pow(attempt.saturating_sub(1))
}

/// An executable contract assertion synthesized alongside handler code
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContractAssertion {
    pub name: String,
    pub request: RequestEnvelope,
    pub expected_status: u16,
    #[serde(default)]
    pub body_contains: Option<String>,
    #[serde(default)]
    pub header_matches: Option<std::collections::HashMap<String, String>>,
}

/// Structured synthesis output expected from LLM completion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthesisOutput {
    /// Pure ES2023 JS module or Wasm handler source implementing the spec
    pub handler_code: String,
    /// Format identifier: "javascript" or "wat"
    pub code_format: String,
    /// Self-contained executable contract test suite
    pub assertions: Vec<ContractAssertion>,
    /// Summary of changes or implementation notes
    pub rationale: Option<String>,
}

/// A JSON serializer sink that refuses to retain more than the configured limit.
struct LimitedBodyWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl LimitedBodyWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(8 * 1024)),
            max_bytes,
            exceeded: false,
        }
    }
}

impl Write for LimitedBodyWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buffer.len()) > self.max_bytes {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "serialized request exceeds configured size limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Why reading a provider response body failed.
enum BodyReadError {
    /// The body exceeded the configured byte limit.
    TooLarge(String),
    /// The transport failed while streaming the body (timeout, reset, ...).
    Transport {
        retryable: bool,
        class: &'static str,
    },
}

impl BodyReadError {
    fn into_engine_error(self) -> EngineError {
        match self {
            Self::TooLarge(message) => EngineError::Internal(message),
            Self::Transport { class, .. } => {
                EngineError::Internal(format!("Failed to read LLM response: {class}"))
            }
        }
    }
}

/// Failure class for a reqwest error. Raw reqwest errors can embed the request URL, so only
/// this class is ever reported.
fn transport_error_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "failed"
    }
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout()
}

/// Read a provider response without allowing an unbounded body allocation.
async fn read_body_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, BodyReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(BodyReadError::TooLarge(format!(
            "provider response exceeds the {max_bytes} byte limit"
        )));
    }

    let mut body = Vec::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| BodyReadError::Transport {
                retryable: is_retryable_transport_error(&error),
                class: transport_error_class(&error),
            })?;
        let Some(chunk) = chunk else { break };
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(BodyReadError::TooLarge(format!(
                "LLM response exceeds the {max_bytes} byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

#[cfg(test)]
async fn read_limited_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, EngineError> {
    read_body_limited(response, max_bytes)
        .await
        .map_err(BodyReadError::into_engine_error)
}

/// Return the model-visible SPEC, excluding operator acceptance policy.
///
/// Verification cases are deliberately host-only inputs. Examples remain visible but
/// are illustrative and must never be treated as acceptance criteria.
fn synthesis_spec_markdown(markdown: &str) -> String {
    let mut output = Vec::new();
    let mut in_verification = false;
    let mut has_examples = false;
    let mut fence_marker = None;

    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = trimmed.chars().next().filter(|ch| *ch == '`' || *ch == '~') {
            let marker_len = trimmed.chars().take_while(|ch| *ch == marker).count();
            if marker_len >= 3 {
                if fence_marker == Some(marker) {
                    fence_marker = None;
                } else if fence_marker.is_none() {
                    fence_marker = Some(marker);
                }
                if !in_verification {
                    output.push(line);
                }
                continue;
            }
        }

        if fence_marker.is_none() {
            if let Some(section) = trimmed.strip_prefix("## ") {
                let section = section.trim().to_ascii_lowercase();
                in_verification = section.contains("verification");
                has_examples |= section == "examples";
            }
        }

        if !in_verification {
            output.push(line);
        }
    }

    let mut visible = output.join("\n");
    if !has_examples {
        if !visible.is_empty() {
            visible.push_str("\n\n");
        }
        visible.push_str(
            "## Examples\nNo additional illustrative examples were provided. Follow the endpoint descriptions and response contracts above.",
        );
    }
    visible
}

fn component_cache_key_for_inputs(
    synthesis: &SynthesisOutput,
    lockfile: &[u8],
    cargo_manifest: &[u8],
) -> String {
    use sha2::{Digest, Sha256};

    let synthesis_bytes = serde_json::to_vec(synthesis)
        .expect("serializing a synthesis output for its cache identity cannot fail");
    let synthesis_digest = Sha256::digest(&synthesis_bytes);
    let fuel = crate::engine::quickjs::DEFAULT_COMPONENT_FUEL.to_be_bytes();
    let memory = (crate::engine::quickjs::DEFAULT_COMPONENT_MEMORY_BYTES as u64).to_be_bytes();
    let response = (crate::engine::http_envelope::MAX_HANDLER_RESPONSE_BYTES as u64).to_be_bytes();
    let execution_timeout = crate::engine::quickjs::DEFAULT_GUEST_EXECUTION_TIMEOUT
        .as_nanos()
        .to_be_bytes();
    let fields: [&[u8]; 14] = [
        b"nosrc-component-cache-v1",
        synthesis_digest.as_slice(),
        lockfile,
        cargo_manifest,
        crate::engine::quickjs::HANDLER_WIT.as_bytes(),
        b"componentize-qjs-0.6.0;wasmtime-50;runtime-default-sync;stub-wasi=true;disable-gc=false;component-model=true;consume-fuel=true",
        env!("CARGO_PKG_VERSION").as_bytes(),
        &fuel,
        &memory,
        &response,
        &execution_timeout,
        b"component-runtime-abi-v1",
        std::env::consts::ARCH.as_bytes(),
        std::env::consts::OS.as_bytes(),
    ];

    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    crate::hex_lower(hasher.finalize())
}

/// Synthesizer service managing LLM prompt construction and schema enforcement
pub struct Synthesizer {
    config: LlmConfig,
    client: reqwest::Client,
}

impl Synthesizer {
    pub fn new(config: LlmConfig) -> Self {
        Self::with_request_timeout(config, LLM_REQUEST_TIMEOUT)
    }

    fn with_request_timeout(config: LlmConfig, request_timeout: Duration) -> Self {
        Self {
            config,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(3))
                .connect_timeout(LLM_CONNECT_TIMEOUT)
                .timeout(request_timeout)
                .build()
                .unwrap_or_default(),
        }
    }

    /// Identity for inputs that can change model-generated output.
    ///
    /// Compilation dependencies and runtime/toolchain settings are deliberately excluded,
    /// so a Cargo.lock-only change can reuse the same synthesis result. The API key is
    /// excluded; provider URL, model, and temperature remain part of the identity.
    pub fn synthesis_cache_key(&self, spec: &ServiceSpec) -> String {
        use sha2::{Digest, Sha256};

        let prompt = self.build_system_prompt();
        let temperature = self.config.temperature.to_bits().to_be_bytes();
        let fields: [&[u8]; 7] = [
            b"nosrc-synthesis-cache-v1",
            b"synthesis-output-schema-v1",
            spec.raw_markdown.as_bytes(),
            self.config.base_url.as_bytes(),
            self.config.model.as_bytes(),
            &temperature,
            prompt.as_bytes(),
        ];

        let mut hasher = Sha256::new();
        for field in fields {
            hasher.update((field.len() as u64).to_be_bytes());
            hasher.update(field);
        }
        crate::hex_lower(hasher.finalize())
    }

    /// Identity for compiled bytes: the exact synthesis output plus the component
    /// toolchain, ABI, and resource settings that affect compilation or execution.
    pub fn component_cache_key(&self, synthesis: &SynthesisOutput) -> String {
        component_cache_key_for_inputs(
            synthesis,
            include_bytes!("../../Cargo.lock"),
            include_bytes!("../../Cargo.toml"),
        )
    }

    /// Kept as a source-compatible alias; this now identifies synthesis only.
    #[deprecated(note = "use synthesis_cache_key; compiled components have a separate identity")]
    pub fn artifact_cache_key(&self, spec: &ServiceSpec) -> String {
        self.synthesis_cache_key(spec)
    }

    /// Build the strict JavaScript handler and contract-assertion prompt.
    pub fn build_system_prompt(&self) -> String {
        r#"You are the nosrc service implementation synthesizer.
Implement the supplied declarative SPEC.md as a self-contained ES module for the nosrc QuickJS runtime.

Runtime contract:
- Export exactly one function: `export function handle(requestJson)`.
- `requestJson` is a JSON string encoding an object with method, path, query, headers, and optional body fields.
- Return a JSON string encoding a response object with numeric status, headers object, and optional string body.
- JavaScript module-level variables are the only state available and persist between requests handled by the same active generation. Durable state, SQL, KV, and outbound egress are unsupported and rejected before synthesis.
- Do not import modules or use filesystem, process, network, environment, timers, or other ambient capabilities. No WASI capabilities are available.
- Keep request and response bodies as strings. For JSON APIs, parse request.body with JSON.parse and serialize response bodies with JSON.stringify.
- Implement only behavior described by SPEC.md. Do not invent endpoints or capabilities.

Return ONLY valid JSON matching this exact structure:
{
  "handler_code": "export function handle(requestJson) { /* implementation */ }",
  "code_format": "javascript",
  "assertions": [
    {
      "name": "descriptive test name",
      "request": {
        "method": "POST",
        "path": "/shorten",
        "query": {},
        "headers": {"content-type": "application/json"},
        "body": "{\"url\":\"https://example.com\"}"
      },
      "expected_status": 201,
      "body_contains": "code"
    }
  ],
  "rationale": "Brief implementation summary"
}

Assertions must cover every endpoint's documented success response and at least one relevant validation or error case where the SPEC defines one. Keep assertions deterministic and independent of external services."#
        .to_string()
    }

    /// Synthesize a handler and assertions from a `ServiceSpec`
    pub async fn synthesize(
        &self,
        spec: &ServiceSpec,
        historical_regressions: &[ContractAssertion],
    ) -> Result<SynthesisOutput, EngineError> {
        self.synthesize_with_feedback(spec, historical_regressions, None)
            .await
    }

    /// Synthesize a candidate with bounded, host-generated repair feedback.
    ///
    /// Feedback is deliberately separate from historical regressions: callers must
    /// never pass operator assertion definitions or their request/expected values.
    pub(crate) async fn synthesize_with_feedback(
        &self,
        spec: &ServiceSpec,
        historical_regressions: &[ContractAssertion],
        repair_feedback: Option<&str>,
    ) -> Result<SynthesisOutput, EngineError> {
        if self.config.api_key.trim().is_empty() {
            return Err(missing_api_key_error());
        }
        let endpoint = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let visible_spec = synthesis_spec_markdown(&spec.raw_markdown);
        let mut user_prompt = format!(
            "Implement the following service specification. The `## Examples` section is illustrative only; it is not acceptance policy. Operator verification cases are intentionally withheld and will be enforced independently by the host.\n\n```markdown\n{}\n```\n",
            visible_spec
        );

        if !historical_regressions.is_empty() {
            let regressions_json = serde_json::to_string_pretty(historical_regressions)
                .map_err(|e| EngineError::Internal(e.to_string()))?;
            user_prompt.push_str(&format!(
                "\nCRITICAL: The updated implementation MUST ALSO satisfy these historical regression test cases without failing:\n```json\n{}\n```\n",
                regressions_json
            ));
        }

        if let Some(feedback) = repair_feedback {
            // Lifecycle code supplies only short, host-authored category/count summaries.
            user_prompt.push_str("\nThe previous candidate did not pass host verification. Repair the implementation while preserving the SPEC contract. Do not weaken or remove assertions.\n");
            user_prompt.push_str(feedback);
            user_prompt.push('\n');
        }

        let body = serde_json::json!({
            "model": self.config.model,
            "temperature": self.config.temperature,
            "response_format": { "type": "json_object" },
            "messages": [
                {
                    "role": "system",
                    "content": self.build_system_prompt()
                },
                {
                    "role": "user",
                    "content": user_prompt
                }
            ]
        });

        let mut request_writer = LimitedBodyWriter::new(MAX_LLM_REQUEST_BYTES);
        if let Err(error) = serde_json::to_writer(&mut request_writer, &body) {
            if request_writer.exceeded {
                return Err(EngineError::Internal(format!(
                    "LLM request exceeds the {MAX_LLM_REQUEST_BYTES} byte limit"
                )));
            }
            return Err(EngineError::Internal(format!(
                "Failed to serialize LLM request: {error}"
            )));
        }
        let request_bytes = request_writer.bytes;
        let request_size = request_bytes.len();
        let request_started = Instant::now();
        tracing::debug!(
            model = %self.config.model,
            request_bytes = request_size,
            "Sending synthesis request to LLM provider"
        );

        let mut attempt = 1;
        let response_bytes = loop {
            let result = self
                .client
                .post(&endpoint)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
                .header("Content-Type", "application/json")
                .body(request_bytes.clone())
                .send()
                .await;

            let resp = match result {
                Ok(resp) => resp,
                Err(error) => {
                    if is_retryable_transport_error(&error) && attempt < MAX_LLM_ATTEMPTS {
                        tracing::warn!(attempt, "LLM request failed; retrying");
                        tokio::time::sleep(retry_delay(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    // reqwest errors can embed the request URL, so report the failure class only.
                    return Err(EngineError::Internal(format!(
                        "LLM request {} after {attempt} attempt(s)",
                        transport_error_class(&error)
                    )));
                }
            };

            tracing::debug!(
                status = %resp.status(),
                elapsed_ms = request_started.elapsed().as_millis(),
                "LLM provider response headers received"
            );

            let status = resp.status();
            if status.is_success() {
                match read_body_limited(resp, MAX_LLM_RESPONSE_BYTES).await {
                    Ok(bytes) => break bytes,
                    Err(BodyReadError::Transport {
                        retryable: true, ..
                    }) if attempt < MAX_LLM_ATTEMPTS => {
                        tracing::warn!(attempt, "Reading LLM response failed; retrying");
                        tokio::time::sleep(retry_delay(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    Err(error) => return Err(error.into_engine_error()),
                }
            }

            if is_retryable_status(status) && attempt < MAX_LLM_ATTEMPTS {
                tracing::warn!(%status, attempt, "LLM provider returned a retryable status");
                tokio::time::sleep(retry_delay(attempt)).await;
                attempt += 1;
                continue;
            }

            // A failure while reading the error body must not mask the HTTP status.
            let err_text = match read_body_limited(resp, MAX_LLM_ERROR_BYTES).await {
                Ok(bytes) => {
                    scrub_provider_error(&String::from_utf8_lossy(&bytes), &self.config.api_key)
                }
                Err(_) => "<error body unavailable>".to_string(),
            };
            return Err(EngineError::Internal(format!(
                "LLM returned HTTP {status}: {err_text}"
            )));
        };

        tracing::debug!(
            response_bytes = response_bytes.len(),
            elapsed_ms = request_started.elapsed().as_millis(),
            "LLM provider response body received"
        );
        let resp_json: serde_json::Value =
            serde_json::from_slice(&response_bytes).map_err(|e| {
                EngineError::Internal(format!("Failed to parse LLM response JSON: {e}"))
            })?;

        let content_str = resp_json["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| {
                EngineError::Internal("LLM response missing content field in choices".into())
            })?;

        let output: SynthesisOutput = serde_json::from_str(content_str)
            .map_err(|e| EngineError::Internal(format!("Malformed synthesis JSON payload: {e}")))?;
        tracing::debug!(
            code_format = %output.code_format,
            handler_bytes = output.handler_code.len(),
            assertions = output.assertions.len(),
            "Structured synthesis output parsed"
        );

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use axum::Router;

    #[test]
    fn cache_identities_separate_synthesis_inputs_from_component_toolchain() {
        let base_markdown = include_str!("../../examples/minimal/SPEC.md");
        let spec = ServiceSpec::parse(base_markdown).unwrap();
        let changed_spec = ServiceSpec::parse(&format!("{base_markdown}\n")).unwrap();
        let config = LlmConfig {
            api_key: "not-part-of-cache-identity".into(),
            base_url: "https://provider.example/v1".into(),
            model: "model-a".into(),
            temperature: 0.1,
        };
        let synthesizer = Synthesizer::new(config.clone());
        let changed_model = Synthesizer::new(LlmConfig {
            model: "model-b".into(),
            ..config
        });
        let output = SynthesisOutput {
            handler_code: "export function handle() { return '{}'; }".into(),
            code_format: "javascript".into(),
            assertions: Vec::new(),
            rationale: Some("test".into()),
        };

        let original_key = synthesizer.synthesis_cache_key(&spec);
        assert_ne!(original_key, synthesizer.synthesis_cache_key(&changed_spec));
        assert_ne!(original_key, changed_model.synthesis_cache_key(&spec));

        // Lockfile-only changes leave synthesis identity stable but invalidate component identity.
        assert_eq!(original_key, synthesizer.synthesis_cache_key(&spec));
        assert_ne!(
            component_cache_key_for_inputs(&output, b"lockfile-v1", b"cargo-manifest"),
            component_cache_key_for_inputs(&output, b"lockfile-v2", b"cargo-manifest")
        );
        assert_ne!(
            component_cache_key_for_inputs(&output, b"lockfile-v1", b"cargo-manifest-v2"),
            component_cache_key_for_inputs(&output, b"lockfile-v1", b"cargo-manifest-v3")
        );
        assert_ne!(
            component_cache_key_for_inputs(&output, b"lockfile-v1", b"cargo-manifest"),
            component_cache_key_for_inputs(
                &SynthesisOutput {
                    handler_code: "export function handle() { return 'changed'; }".into(),
                    ..output
                },
                b"lockfile-v1",
                b"cargo-manifest",
            )
        );
    }

    #[test]
    fn serialized_request_writer_stops_at_configured_limit() {
        let mut writer = LimitedBodyWriter::new(4);
        writer.write_all(b"1234").unwrap();
        assert!(writer.write_all(b"5").is_err());
        assert!(writer.exceeded);
        assert_eq!(writer.bytes, b"1234");
    }

    #[tokio::test]
    async fn provider_response_content_length_is_checked_before_reading() {
        let app = Router::new().route("/large", get(|| async { Body::from("0123456789") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let response = reqwest::get(format!("http://{address}/large"))
            .await
            .unwrap();
        let error = read_limited_body(response, 5).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("provider response exceeds the 5 byte limit"));

        let _ = shutdown_tx.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn provider_response_without_content_length_is_bounded_while_streaming() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();

            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            socket
                .write_all(b"A\r\n0123456789\r\n0\r\n\r\n")
                .await
                .unwrap();
        });

        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        assert_eq!(response.content_length(), None);

        let error = read_limited_body(response, 5).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("LLM response exceeds the 5 byte limit"));

        server.await.unwrap();
    }

    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn debug_output_redacts_api_key() {
        let config = LlmConfig {
            api_key: "sk-super-secret".into(),
            base_url: "https://example.test/v1".into(),
            model: "m".into(),
            temperature: 0.0,
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-super-secret"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("https://example.test/v1"));
    }

    #[test]
    fn from_lookup_requires_key_only_when_asked() {
        let empty = env_from(&[]);
        assert!(LlmConfig::from_lookup(&empty, true).is_err());
        let blank = env_from(&[("LLM_API_KEY", "  ")]);
        assert!(LlmConfig::from_lookup(&blank, true).is_err());

        let config = LlmConfig::from_lookup(&empty, false).unwrap();
        assert!(config.api_key.is_empty());
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4o-mini");

        let keyed = env_from(&[("LLM_API_KEY", "k"), ("LLM_MODEL", "x")]);
        let config = LlmConfig::from_lookup(&keyed, true).unwrap();
        assert_eq!(config.api_key, "k");
        assert_eq!(config.model, "x");
    }

    #[test]
    fn base_url_must_be_https_unless_loopback() {
        for ok in [
            "https://api.openai.com/v1",
            "http://localhost:8080/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:1234/v1",
        ] {
            assert!(validate_base_url(ok).is_ok(), "{ok} should be accepted");
        }
        for bad in [
            "http://api.example.com/v1",
            "http://10.0.0.5/v1",
            "ftp://example.com",
            "not a url",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn provider_error_scrubbing_removes_key_and_sk_tokens() {
        let key = "abc123secret";
        let body = format!(
            "bad key {key}; also sk-proj-****wxyz and \"sk-live_ABC\" ok; normal text stays"
        );
        let scrubbed = scrub_provider_error(&body, key);
        assert!(!scrubbed.contains(key));
        assert!(!scrubbed.contains("sk-proj"));
        assert!(!scrubbed.contains("sk-live"));
        assert!(scrubbed.contains("normal text stays"));
        assert_eq!(scrub_provider_error("plain", ""), "plain");
    }

    #[test]
    fn retry_policy_covers_throttling_and_server_errors_only() {
        use reqwest::StatusCode;
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(2), Duration::from_millis(1000));
    }

    #[tokio::test]
    async fn synthesis_without_api_key_fails_before_any_request() {
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: String::new(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            temperature: 0.0,
        });
        let spec = ServiceSpec::parse(include_str!("../../examples/minimal/SPEC.md")).unwrap();
        let error = synthesizer.synthesize(&spec, &[]).await.unwrap_err();
        assert!(error.to_string().contains("LLM_API_KEY"));
    }

    #[tokio::test]
    async fn truncated_response_body_error_omits_request_url() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            // Promise 100 bytes, send 3, then close: the body read fails mid-stream.
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabc",
                )
                .await
                .unwrap();
        });

        let response = reqwest::get(format!("http://{address}/secret-path?token=abc"))
            .await
            .unwrap();
        let error = read_limited_body(response, 1024).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Failed to read LLM response"));
        assert!(!message.contains("secret-path"));
        assert!(!message.contains("token=abc"));
        assert!(!message.contains(&address.to_string()));

        server.await.unwrap();
    }

    struct Reply {
        status: u16,
        body: String,
        delay: Duration,
    }

    impl Reply {
        fn new(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                body: body.into(),
                delay: Duration::ZERO,
            }
        }
    }

    /// Start a mock chat-completions provider. `handler` receives the zero-based request index.
    async fn spawn_provider(
        handler: impl Fn(usize) -> Reply + Send + Sync + 'static,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let counter = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let counter = counter.clone();
                move || {
                    let counter = counter.clone();
                    let handler = handler.clone();
                    async move {
                        let index = counter.fetch_add(1, Ordering::SeqCst);
                        let reply = handler(index);
                        if !reply.delay.is_zero() {
                            tokio::time::sleep(reply.delay).await;
                        }
                        (StatusCode::from_u16(reply.status).unwrap(), reply.body)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}/v1"), counter, task)
    }

    const TEST_KEY: &str = "test-key-123456";

    fn test_synthesizer(base_url: String, timeout: Duration) -> Synthesizer {
        Synthesizer::with_request_timeout(
            LlmConfig {
                api_key: TEST_KEY.into(),
                base_url,
                model: "test-model".into(),
                temperature: 0.0,
            },
            timeout,
        )
    }

    fn test_spec() -> ServiceSpec {
        ServiceSpec::parse(include_str!("../../examples/minimal/SPEC.md")).unwrap()
    }

    fn completion_body(content: &str) -> String {
        serde_json::json!({ "choices": [{ "message": { "content": content } }] }).to_string()
    }

    fn valid_synthesis_content() -> String {
        serde_json::json!({
            "handler_code": "export function handle(r) { return r; }",
            "code_format": "javascript",
            "assertions": [],
            "rationale": "ok"
        })
        .to_string()
    }

    fn count(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn synthesis_prompt_excludes_operator_verification_and_keeps_examples() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /items`
- Response `200`: items
## Examples
### Request
```markdown
## Verification
This is example text, not a section heading.
```
```json
{"method":"GET","path":"/items"}
```
## Verification
### `GET /items?secret=trusted-case-marker`
- Response `200`
- Body contains: `trusted-case-marker`
## Notes
This note remains visible.
"#;

        let visible = synthesis_spec_markdown(markdown);
        assert!(visible.contains("## Examples"));
        assert!(visible.contains(r#"{"method":"GET","path":"/items"}"#));
        assert!(visible.contains("## Notes"));
        assert!(visible.contains("This note remains visible."));
        assert!(visible.contains("This is example text, not a section heading."));
        assert!(!visible.contains("trusted-case-marker"));
    }

    #[test]
    fn synthesis_prompt_adds_examples_section_when_missing() {
        let visible = synthesis_spec_markdown("# Service: Example\n## Endpoints\n### `GET /`");
        assert!(visible.contains("## Examples"));
        assert!(visible.contains("No additional illustrative examples were provided."));
    }

    #[tokio::test]
    async fn unauthorized_response_is_not_retried_and_key_is_scrubbed() {
        let (url, requests, server) = spawn_provider(|_| {
            Reply::new(
                401,
                format!(r#"{{"error":"Incorrect API key provided: {TEST_KEY}"}}"#),
            )
        })
        .await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("401"), "{error}");
        assert!(!error.contains(TEST_KEY), "{error}");
        assert_eq!(count(&requests), 1);
        server.abort();
    }

    #[tokio::test]
    async fn bad_request_is_not_retried() {
        let (url, requests, server) = spawn_provider(|_| Reply::new(400, "bad")).await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("400"), "{error}");
        assert_eq!(count(&requests), 1);
        server.abort();
    }

    #[tokio::test]
    async fn throttled_then_successful_response_is_retried() {
        let (url, requests, server) = spawn_provider(|index| {
            if index < 2 {
                Reply::new(429, "slow down")
            } else {
                Reply::new(200, completion_body(&valid_synthesis_content()))
            }
        })
        .await;
        let output = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap();
        assert_eq!(output.code_format, "javascript");
        assert_eq!(count(&requests), 3);
        server.abort();
    }

    #[tokio::test]
    async fn persistent_server_error_stops_after_the_attempt_limit() {
        let (url, requests, server) = spawn_provider(|_| Reply::new(503, "unavailable")).await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("503"), "{error}");
        assert_eq!(count(&requests), MAX_LLM_ATTEMPTS as usize);
        server.abort();
    }

    #[tokio::test]
    async fn oversized_error_body_does_not_hide_the_status() {
        let huge = "x".repeat(MAX_LLM_ERROR_BYTES * 3);
        let (url, _requests, server) = spawn_provider(move |_| Reply::new(400, huge.clone())).await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("400"), "{error}");
        assert!(error.len() < MAX_LLM_ERROR_BYTES, "{}", error.len());
        server.abort();
    }

    #[tokio::test]
    async fn malformed_provider_json_is_reported() {
        let (url, _requests, server) = spawn_provider(|_| Reply::new(200, "{not json")).await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Failed to parse LLM response JSON"),
            "{error}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn missing_choices_field_is_reported() {
        let (url, _requests, server) = spawn_provider(|_| Reply::new(200, r#"{"id":"x"}"#)).await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing content field"), "{error}");
        server.abort();
    }

    #[tokio::test]
    async fn empty_choices_array_is_reported() {
        let (url, _requests, server) =
            spawn_provider(|_| Reply::new(200, r#"{"choices":[]}"#)).await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing content field"), "{error}");
        server.abort();
    }

    #[tokio::test]
    async fn non_synthesis_content_is_reported() {
        let (url, _requests, server) =
            spawn_provider(|_| Reply::new(200, completion_body("just prose, not JSON"))).await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Malformed synthesis JSON payload"),
            "{error}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn content_missing_required_fields_is_reported() {
        let (url, _requests, server) =
            spawn_provider(|_| Reply::new(200, completion_body(r#"{"handler_code":"x"}"#))).await;
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Malformed synthesis JSON payload"),
            "{error}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn slow_provider_times_out_and_error_omits_the_url() {
        let (url, requests, server) = spawn_provider(|_| Reply {
            status: 200,
            body: String::new(),
            delay: Duration::from_secs(5),
        })
        .await;
        let error = test_synthesizer(url.clone(), Duration::from_millis(200))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(!error.contains(&url), "{error}");
        assert_eq!(count(&requests), MAX_LLM_ATTEMPTS as usize);
        server.abort();
    }

    #[tokio::test]
    async fn refused_connection_is_retried_and_reported_without_the_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{address}/v1");
        let error = test_synthesizer(url, Duration::from_secs(5))
            .synthesize(&test_spec(), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("could not connect"), "{error}");
        assert!(error.contains("3 attempt(s)"), "{error}");
        assert!(!error.contains(&address.to_string()), "{error}");
    }
}
