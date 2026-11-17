use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::time::Duration;

use crate::engine::http_envelope::RequestEnvelope;
use crate::error::EngineError;
use crate::spec::ServiceSpec;

const LLM_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const LLM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LLM_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_LLM_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_LLM_ERROR_BYTES: usize = 8 * 1024;

/// Configuration for the OpenAI-compatible LLM endpoint
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub temperature: f32,
}

impl LlmConfig {
    pub fn from_env() -> Result<Self, EngineError> {
        let api_key = std::env::var("LLM_API_KEY").map_err(|_| {
            EngineError::Internal("Missing required environment variable LLM_API_KEY".into())
        })?;

        let base_url = std::env::var("LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

        Ok(Self {
            api_key,
            base_url,
            model,
            temperature: 0.1,
        })
    }
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

/// Read a provider response without allowing an unbounded body allocation.
async fn read_limited_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, EngineError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(EngineError::Internal(format!(
            "provider response exceeds the {max_bytes} byte limit"
        )));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| EngineError::Internal(format!("Failed to read LLM response: {error}")))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(EngineError::Internal(format!(
                "LLM response exceeds the {max_bytes} byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// Synthesizer service managing LLM prompt construction and schema enforcement
pub struct Synthesizer {
    config: LlmConfig,
    client: reqwest::Client,
}

impl Synthesizer {
    pub fn new(config: LlmConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(3))
                .connect_timeout(LLM_CONNECT_TIMEOUT)
                .timeout(LLM_REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
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
        let endpoint = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let mut user_prompt = format!(
            "Implement the following service specification:\n\n```markdown\n{}\n```\n",
            spec.raw_markdown
        );

        if !historical_regressions.is_empty() {
            let regressions_json = serde_json::to_string_pretty(historical_regressions)
                .map_err(|e| EngineError::Internal(e.to_string()))?;
            user_prompt.push_str(&format!(
                "\nCRITICAL: The updated implementation MUST ALSO satisfy these historical regression test cases without failing:\n```json\n{}\n```\n",
                regressions_json
            ));
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

        let resp = self
            .client
            .post(&endpoint)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .body(request_bytes)
            .send()
            .await
            .map_err(|e| EngineError::Internal(format!("LLM request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_bytes = read_limited_body(resp, MAX_LLM_ERROR_BYTES).await?;
            let err_text = String::from_utf8_lossy(&error_bytes);
            return Err(EngineError::Internal(format!(
                "LLM returned HTTP {status}: {err_text}"
            )));
        }

        let response_bytes = read_limited_body(resp, MAX_LLM_RESPONSE_BYTES).await?;
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

        let response = reqwest::get(format!("http://{address}/"))
            .await
            .unwrap();
        assert_eq!(response.content_length(), None);

        let error = read_limited_body(response, 5).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("LLM response exceeds the 5 byte limit"));

        server.await.unwrap();
    }
}
