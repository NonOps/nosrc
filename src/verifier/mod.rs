use crate::engine::http_envelope::ResponseEnvelope;
use crate::engine::quickjs::QuickJsHandler;
use crate::error::EngineError;
use crate::synthesizer::ContractAssertion;

/// Result of evaluating a contract assertion against an isolated candidate handler.
#[derive(Debug, Clone)]
pub struct AssertionResult {
    pub name: String,
    pub passed: bool,
    pub actual_status: Option<u16>,
    pub expected_status: u16,
    pub error_message: Option<String>,
}

/// Runs the complete assertion suite against one isolated candidate instance.
///
/// All assertions share the candidate instance so module-scoped JavaScript state
/// behaves exactly as it will across requests after activation. The caller must
/// instantiate a fresh production handler after verification succeeds.
pub struct ShadowVerifier;

impl ShadowVerifier {
    pub fn new() -> Self {
        Self
    }

    pub fn verify_handler(
        &self,
        candidate: &mut QuickJsHandler,
        assertions: &[ContractAssertion],
    ) -> Result<Vec<AssertionResult>, EngineError> {
        if assertions.is_empty() {
            return Err(EngineError::Internal(
                "Shadow verification requires at least one contract assertion".into(),
            ));
        }

        let mut results = Vec::with_capacity(assertions.len());
        for assertion in assertions {
            match candidate.invoke(&assertion.request) {
                Ok(response) => {
                    let (passed, error_message) = evaluate_response(&response, assertion);
                    results.push(AssertionResult {
                        name: assertion.name.clone(),
                        passed,
                        actual_status: Some(response.status),
                        expected_status: assertion.expected_status,
                        error_message,
                    });
                }
                Err(error) => results.push(AssertionResult {
                    name: assertion.name.clone(),
                    passed: false,
                    actual_status: None,
                    expected_status: assertion.expected_status,
                    error_message: Some(format!("Candidate execution failed: {error}")),
                }),
            }
        }

        for result in &results {
            tracing::debug!(
                assertion = %result.name,
                passed = result.passed,
                expected_status = result.expected_status,
                actual_status = ?result.actual_status,
                "Contract assertion evaluated"
            );
        }

        let failed: Vec<String> = results
            .iter()
            .filter(|result| !result.passed)
            .map(|result| {
                format!(
                    "{}: {}",
                    result.name,
                    result
                        .error_message
                        .as_deref()
                        .unwrap_or("assertion failed")
                )
            })
            .collect();

        if !failed.is_empty() {
            return Err(EngineError::Internal(format!(
                "Candidate verification failed: {}",
                failed.join("; ")
            )));
        }

        Ok(results)
    }
}

impl Default for ShadowVerifier {
    fn default() -> Self {
        Self::new()
    }
}

fn evaluate_response(
    response: &ResponseEnvelope,
    assertion: &ContractAssertion,
) -> (bool, Option<String>) {
    if response.status != assertion.expected_status {
        return (
            false,
            Some(format!(
                "Expected status {}, got {}",
                assertion.expected_status, response.status
            )),
        );
    }

    if let Some(expected_substring) = &assertion.body_contains {
        let body = response.body.as_deref().unwrap_or("");
        if !body.contains(expected_substring) {
            return (
                false,
                Some(format!(
                    "Body does not contain expected substring '{expected_substring}'. Actual: '{body}'"
                )),
            );
        }
    }

    if let Some(headers) = &assertion.header_matches {
        for (name, expected_value) in headers {
            match response.headers.get(name) {
                Some(actual_value) if actual_value == expected_value => {}
                Some(actual_value) => {
                    return (
                        false,
                        Some(format!(
                            "Header '{name}' value mismatch: expected '{expected_value}', got '{actual_value}'"
                        )),
                    );
                }
                None => {
                    return (
                        false,
                        Some(format!("Missing expected header '{name}' in response")),
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
    use crate::engine::quickjs::{test_support, DEFAULT_COMPONENT_FUEL};
    use std::collections::HashMap;

    fn assertion(expected_status: u16, body_contains: Option<&str>) -> ContractAssertion {
        ContractAssertion {
            name: "test assertion".to_string(),
            request: RequestEnvelope {
                method: "GET".to_string(),
                path: "/health".to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: None,
            },
            expected_status,
            body_contains: body_contains.map(str::to_string),
            header_matches: None,
        }
    }

    #[tokio::test]
    async fn verifier_approves_a_valid_javascript_candidate() {
        let (engine, compiled) = test_support::compiled().await;
        let mut candidate = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let results = ShadowVerifier::new()
            .verify_handler(&mut candidate, &[assertion(200, Some("healthy"))])
            .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
    }

    #[tokio::test]
    async fn verifier_enforces_expected_response_headers() {
        let (engine, compiled) = test_support::compiled().await;
        let mut candidate = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();
        let mut expected = assertion(200, Some("healthy"));
        expected.header_matches = Some(HashMap::from([(
            "content-type".to_string(),
            "text/plain".to_string(),
        )]));

        let results = ShadowVerifier::new()
            .verify_handler(&mut candidate, &[expected])
            .unwrap();
        assert!(results[0].passed);

        let mut wrong_header = assertion(200, Some("healthy"));
        wrong_header.header_matches = Some(HashMap::from([(
            "content-type".to_string(),
            "application/json".to_string(),
        )]));
        assert!(ShadowVerifier::new()
            .verify_handler(&mut candidate, &[wrong_header])
            .is_err());
    }

    #[tokio::test]
    async fn verifier_passes_query_parameters_to_the_guest_handler() {
        let (engine, compiled) = test_support::compiled().await;
        let mut candidate = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();
        let mut query_assertion = assertion(200, Some("rust wasm"));
        query_assertion.request.path = "/query".to_string();
        query_assertion.request.query =
            HashMap::from([("q".to_string(), "rust wasm".to_string())]);

        let results = ShadowVerifier::new()
            .verify_handler(&mut candidate, &[query_assertion])
            .unwrap();
        assert!(results[0].passed);
    }

    #[tokio::test]
    async fn verifier_rejects_an_empty_assertion_suite() {
        let (engine, compiled) = test_support::compiled().await;
        let mut candidate = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let error = ShadowVerifier::new()
            .verify_handler(&mut candidate, &[])
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("at least one contract assertion"));
    }

    #[tokio::test]
    async fn verifier_rejects_a_candidate_that_fails_an_assertion() {
        let (engine, compiled) = test_support::compiled().await;
        let mut candidate = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();
        let mut failing_assertion = assertion(200, None);
        failing_assertion.request.path = "/error".to_string();

        let result = ShadowVerifier::new().verify_handler(&mut candidate, &[failing_assertion]);

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn assertions_share_candidate_module_state_in_order() {
        let (engine, compiled) = test_support::compiled().await;
        let mut candidate = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let mut first_request = assertion(200, None).request;
        first_request.path = "/counter".to_string();
        let second_request = first_request.clone();
        let first = ContractAssertion {
            name: "first call".to_string(),
            request: first_request,
            expected_status: 200,
            body_contains: Some("1".to_string()),
            header_matches: None,
        };
        let second = ContractAssertion {
            name: "second call".to_string(),
            request: second_request,
            expected_status: 200,
            body_contains: Some("2".to_string()),
            header_matches: None,
        };

        let results = ShadowVerifier::new()
            .verify_handler(&mut candidate, &[first, second])
            .unwrap();
        assert!(results.iter().all(|result| result.passed));
    }
}
