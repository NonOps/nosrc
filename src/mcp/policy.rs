use crate::engine::http_envelope::RequestEnvelope;
use crate::engine::quickjs::{DEFAULT_COMPONENT_FUEL, DEFAULT_COMPONENT_MEMORY_BYTES};
use crate::spec::ServiceSpec;
use crate::synthesizer::ContractAssertion;

/// Operator-owned acceptance policy. MCP clients cannot modify this value.
#[derive(Debug, Clone)]
pub struct AcceptancePolicy {
    pub spec_assertions: Vec<ContractAssertion>,
    pub invariants: PolicyInvariants,
}

/// Hard ceilings enforced by the runtime, independent of agent input.
#[derive(Debug, Clone)]
pub struct PolicyInvariants {
    pub max_response_bytes: usize,
    pub max_fuel: u64,
    pub max_memory_bytes: usize,
}

impl AcceptancePolicy {
    /// Baseline sandbox ceilings for MCP-first development without an operator SPEC.
    pub fn baseline() -> Self {
        Self {
            spec_assertions: Vec::new(),
            invariants: PolicyInvariants {
                max_response_bytes: crate::engine::http_envelope::MAX_HANDLER_RESPONSE_BYTES,
                max_fuel: DEFAULT_COMPONENT_FUEL,
                max_memory_bytes: DEFAULT_COMPONENT_MEMORY_BYTES,
            },
        }
    }

    pub fn from_spec(spec: &ServiceSpec) -> Result<Self, String> {
        spec.validate_verification()?;
        if spec.capabilities.state != "volatile" {
            return Err(format!(
                "unsupported SPEC state mode '{}'",
                spec.capabilities.state
            ));
        }
        if !spec.capabilities.egress.is_empty() {
            return Err("SPEC requests outbound egress, which is unavailable".into());
        }
        if !spec.schema.is_empty() {
            return Err("SPEC declares database schema, but SQL is unavailable".into());
        }

        let spec_assertions = spec
            .verification
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
                        format!(
                            "verification case {} {} has no response status",
                            case.method, case.path
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
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self {
            spec_assertions,
            invariants: PolicyInvariants {
                require_endpoint_coverage: true,
                max_response_bytes: crate::engine::http_envelope::MAX_HANDLER_RESPONSE_BYTES,
                max_fuel: DEFAULT_COMPONENT_FUEL,
                max_memory_bytes: DEFAULT_COMPONENT_MEMORY_BYTES,
            },
        })
    }

    /// Validate policy ceilings before any candidate is compiled.
    pub fn validate(&self) -> Result<(), String> {
        if self.invariants.max_response_bytes == 0
            || self.invariants.max_response_bytes
                > crate::engine::http_envelope::MAX_HANDLER_RESPONSE_BYTES
        {
            return Err("response-size policy exceeds the runtime ceiling".into());
        }
        if self.invariants.max_fuel == 0 || self.invariants.max_fuel > DEFAULT_COMPONENT_FUEL {
            return Err("fuel policy exceeds the runtime ceiling".into());
        }
        if self.invariants.max_memory_bytes == 0
            || self.invariants.max_memory_bytes > DEFAULT_COMPONENT_MEMORY_BYTES
        {
            return Err("memory policy exceeds the runtime ceiling".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn policy_rejects_relaxed_runtime_ceilings() {
        let policy = AcceptancePolicy {
            spec_assertions: vec![ContractAssertion {
                name: "trusted".into(),
                request: RequestEnvelope {
                    method: "GET".into(),
                    path: "/".into(),
                    query: HashMap::new(),
                    headers: HashMap::new(),
                    body: None,
                },
                expected_status: 200,
                body_contains: None,
                header_matches: None,
            }],
            invariants: PolicyInvariants {
                require_endpoint_coverage: true,
                max_response_bytes: crate::engine::http_envelope::MAX_HANDLER_RESPONSE_BYTES,
                max_fuel: DEFAULT_COMPONENT_FUEL + 1,
                max_memory_bytes: DEFAULT_COMPONENT_MEMORY_BYTES,
            },
        };
        assert!(policy.validate().unwrap_err().contains("fuel"));
    }
}
