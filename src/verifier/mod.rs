use std::sync::Arc;
use wasmtime::Module;

use crate::engine::host_abi::HostCapabilities;
use crate::engine::http_envelope::ResponseEnvelope;
use crate::engine::{WasmEngine, DEFAULT_FUEL_BUDGET};
use crate::error::EngineError;
use crate::synthesizer::ContractAssertion;

/// Result of evaluating a contract test assertion inside the shadow verifier
#[derive(Debug, Clone)]
pub struct AssertionResult {
    pub name: String,
    pub passed: bool,
    pub actual_status: Option<u16>,
    pub expected_status: u16,
    pub error_message: Option<String>,
}

/// Shadow Verifier that runs candidate Wasm modules against assertions in an isolated sandbox
pub struct ShadowVerifier<'a> {
    engine: &'a WasmEngine,
    base_capabilities: HostCapabilities,
}

impl<'a> ShadowVerifier<'a> {
    pub fn new(engine: &'a WasmEngine, base_capabilities: HostCapabilities) -> Self {
        Self {
            engine,
            base_capabilities,
        }
    }

    /// Creates an isolated ephemeral capability set for shadow testing
    /// (e.g. In-memory SQLite `:memory:` and temporary KV so production state is untouched)
    fn create_shadow_capabilities(&self) -> Result<HostCapabilities, EngineError> {
        if self.base_capabilities.state_mode == crate::engine::host_abi::StateMode::External {
            Ok(HostCapabilities::new_external(
                self.base_capabilities.allowlisted_domains.clone(),
            ))
        } else {
            // Ephemeral in-memory SQLite clone
            let shadow_db = rusqlite::Connection::open_in_memory()
                .map_err(|e| EngineError::Internal(format!("Failed to open shadow db: {e}")))?;

            // Temporary isolated redb database
            let tmp_path =
                std::env::temp_dir().join(format!("nosrc_shadow_kv_{}.redb", uuid::Uuid::now_v7()));
            let shadow_kv = redb::Database::create(&tmp_path)
                .map_err(|e| EngineError::Internal(format!("Failed to open shadow kv: {e}")))?;

            Ok(HostCapabilities::new_embedded(
                shadow_db,
                Some(Arc::new(shadow_kv)),
                self.base_capabilities.allowlisted_domains.clone(),
            ))
        }
    }

    /// Verifies candidate bytecode against 100% of assertions.
    /// Rejects if ANY assertion fails, traps, or exceeds fuel budget.
    pub fn verify_module(
        &self,
        module: &Module,
        assertions: &[ContractAssertion],
    ) -> Result<Vec<AssertionResult>, EngineError> {
        if assertions.is_empty() {
            return Err(EngineError::Internal(
                "Shadow verification requires at least one contract assertion".into(),
            ));
        }

        let mut results = Vec::with_capacity(assertions.len());

        for assertion in assertions {
            let shadow_caps = self.create_shadow_capabilities()?;

            let invocation_res = self.engine.invoke_handler(
                module,
                shadow_caps,
                &assertion.request,
                DEFAULT_FUEL_BUDGET,
            );

            match invocation_res {
                Ok(resp) => {
                    let (passed, err_msg) = evaluate_response(&resp, assertion);
                    results.push(AssertionResult {
                        name: assertion.name.clone(),
                        passed,
                        actual_status: Some(resp.status),
                        expected_status: assertion.expected_status,
                        error_message: err_msg,
                    });
                }
                Err(err) => {
                    results.push(AssertionResult {
                        name: assertion.name.clone(),
                        passed: false,
                        actual_status: None,
                        expected_status: assertion.expected_status,
                        error_message: Some(format!("Handler trapped or failed: {err}")),
                    });
                }
            }
        }

        // Hard Invariant Check: 100% of assertions must pass
        let all_passed = results.iter().all(|r| r.passed);
        if !all_passed {
            let failed_names: Vec<String> = results
                .iter()
                .filter(|r| !r.passed)
                .map(|r| format!("{}: {:?}", r.name, r.error_message))
                .collect();
            return Err(EngineError::Internal(format!(
                "Shadow sandbox verification failed: {}",
                failed_names.join("; ")
            )));
        }

        Ok(results)
    }
}

fn evaluate_response(
    resp: &ResponseEnvelope,
    assertion: &ContractAssertion,
) -> (bool, Option<String>) {
    if resp.status != assertion.expected_status {
        return (
            false,
            Some(format!(
                "Expected status {}, got {}",
                assertion.expected_status, resp.status
            )),
        );
    }

    if let Some(expected_substr) = &assertion.body_contains {
        let body = resp.body.as_deref().unwrap_or("");
        if !body.contains(expected_substr) {
            return (
                false,
                Some(format!(
                    "Body does not contain expected substring '{expected_substr}'. Actual: '{body}'"
                )),
            );
        }
    }

    if let Some(headers) = &assertion.header_matches {
        for (k, expected_v) in headers {
            match resp.headers.get(k) {
                Some(actual_v) if actual_v == expected_v => {}
                Some(actual_v) => {
                    return (
                        false,
                        Some(format!(
                            "Header '{k}' value mismatch: expected '{expected_v}', got '{actual_v}'"
                        )),
                    );
                }
                None => {
                    return (
                        false,
                        Some(format!("Missing expected header '{k}' in response")),
                    );
                }
            }
        }
    }

    (true, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::http_envelope::RequestEnvelope;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_shadow_verifier_approves_valid_module() {
        let engine = WasmEngine::new().unwrap();

        let resp_json = r#"{"status":200,"headers":{},"body":"healthy"}"#;
        let resp_len = resp_json.len();
        let wat = format!(
            r#"
            (module
                (import "nosrc_host" "host_set_response" (func $set_resp (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 2048) "{}")

                (func (export "handle_request") (param i32 i32) (result i32)
                    (call $set_resp (i32.const 2048) (i32.const {}))
                )
            )
            "#,
            resp_json.replace('"', "\\\""),
            resp_len
        );

        let module = engine.compile(wat.as_bytes()).unwrap();

        let assertion = ContractAssertion {
            name: "Health Check".to_string(),
            request: RequestEnvelope {
                method: "GET".to_string(),
                path: "/healthz".to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: None,
            },
            expected_status: 200,
            body_contains: Some("healthy".to_string()),
            header_matches: None,
        };

        let verifier = ShadowVerifier::new(&engine, HostCapabilities::new_external(HashSet::new()));

        let results = verifier
            .verify_module(&module, &[assertion])
            .expect("Verification must pass");

        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
    }

    #[test]
    fn test_shadow_verifier_rejects_empty_assertion_suite() {
        let engine = WasmEngine::new().unwrap();
        let module = engine.compile(b"(module)").unwrap();
        let verifier = ShadowVerifier::new(&engine, HostCapabilities::new_external(HashSet::new()));

        let result = verifier.verify_module(&module, &[]);

        let error = result.expect_err("An empty assertion suite must not pass verification");
        assert!(
            error
                .to_string()
                .contains("at least one contract assertion"),
            "Unexpected verification error: {error}"
        );
    }

    #[test]
    fn test_shadow_verifier_rejects_failing_assertion() {
        let engine = WasmEngine::new().unwrap();

        // Guest returns status 500
        let resp_json = r#"{"status":500,"headers":{},"body":"database error"}"#;
        let wat = format!(
            r#"
            (module
                (import "nosrc_host" "host_set_response" (func $set_resp (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "{}")

                (func (export "handle_request") (param i32 i32) (result i32)
                    (call $set_resp (i32.const 1024) (i32.const {}))
                )
            )
            "#,
            resp_json.replace('"', "\\\""),
            resp_json.len()
        );

        let module = engine.compile(wat.as_bytes()).unwrap();

        // Assertion requires status 200
        let assertion = ContractAssertion {
            name: "Strict Contract".to_string(),
            request: RequestEnvelope {
                method: "POST".to_string(),
                path: "/action".to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: None,
            },
            expected_status: 200,
            body_contains: None,
            header_matches: None,
        };

        let verifier = ShadowVerifier::new(&engine, HostCapabilities::new_external(HashSet::new()));

        let res = verifier.verify_module(&module, &[assertion]);
        assert!(res.is_err(), "Verification must reject status mismatch");
    }
}
