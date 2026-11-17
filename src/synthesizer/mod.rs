use serde::{Deserialize, Serialize};

use crate::engine::http_envelope::RequestEnvelope;
use crate::error::EngineError;
use crate::spec::ServiceSpec;

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
                .build()
                .unwrap_or_default(),
        }
    }

    /// Build the strict system prompt specifying the Host ABI and output schema
    pub fn build_system_prompt(&self) -> String {
        r#"You are the nosrc autonomous runtime synthesizer.
Your task is to synthesize an executable handler and test contract from a declarative SPEC.md.

Host ABI functions available to guest:
- `host_kv_get(key_ptr, key_len) -> len | ERR_NOT_FOUND`
- `host_kv_set(key_ptr, key_len, val_ptr, val_len) -> 0 | error`
- `host_kv_delete(key_ptr, key_len) -> 1 | 0 | error`
- `host_db_query(sql_ptr, sql_len, params_ptr, params_len) -> json_len`
- `host_db_execute(sql_ptr, sql_len, params_ptr, params_len) -> affected_rows`
- `host_set_response(ptr, len) -> len`

Return ONLY valid JSON matching this exact structure:
{
  "handler_code": "... source code ...",
  "code_format": "wat",
  "assertions": [
    {
      "name": "test name",
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
}"#
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

        let resp = self
            .client
            .post(&endpoint)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| EngineError::Internal(format!("LLM request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_text = resp.text().await.unwrap_or_default();
            return Err(EngineError::Internal(format!(
                "LLM returned HTTP {status}: {err_text}"
            )));
        }

        let resp_json: serde_json::Value = resp.json().await.map_err(|e| {
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
