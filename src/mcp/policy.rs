use crate::engine::http_envelope::RequestEnvelope;
use crate::engine::quickjs::{DEFAULT_COMPONENT_FUEL, DEFAULT_COMPONENT_MEMORY_BYTES};
use crate::spec::ServiceSpec;
use crate::synthesizer::ContractAssertion;
use std::str::FromStr;

/// Operator-selected runtime budgets. Every value may be lowered from its immutable ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorRuntimeLimits {
    pub max_response_bytes: usize,
    pub max_fuel: u64,
    pub max_memory_bytes: usize,
}

impl Default for OperatorRuntimeLimits {
    fn default() -> Self {
        Self {
            max_response_bytes: crate::engine::http_envelope::MAX_HANDLER_RESPONSE_BYTES,
            max_fuel: DEFAULT_COMPONENT_FUEL,
            max_memory_bytes: DEFAULT_COMPONENT_MEMORY_BYTES,
        }
    }
}

impl OperatorRuntimeLimits {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let defaults = Self::default();
        Ok(Self {
            max_fuel: parse_limit(
                "NOSRC_MAX_FUEL",
                lookup("NOSRC_MAX_FUEL"),
                defaults.max_fuel,
                DEFAULT_COMPONENT_FUEL,
            )?,
            max_memory_bytes: parse_limit(
                "NOSRC_MAX_MEMORY_BYTES",
                lookup("NOSRC_MAX_MEMORY_BYTES"),
                defaults.max_memory_bytes,
                DEFAULT_COMPONENT_MEMORY_BYTES,
            )?,
            max_response_bytes: parse_limit(
                "NOSRC_MAX_RESPONSE_BYTES",
                lookup("NOSRC_MAX_RESPONSE_BYTES"),
                defaults.max_response_bytes,
                crate::engine::http_envelope::MAX_HANDLER_RESPONSE_BYTES,
            )?,
        })
    }
}

fn parse_limit<T>(name: &str, raw: Option<String>, default: T, ceiling: T) -> Result<T, String>
where
    T: FromStr + PartialOrd + PartialEq + Default + Copy + std::fmt::Display,
{
    let Some(raw) = raw else { return Ok(default) };
    let parsed = raw
        .trim()
        .parse::<T>()
        .map_err(|_| format!("{name} must be an integer between 1 and {ceiling}; got '{raw}'"))?;
    if parsed == T::default() || parsed > ceiling {
        return Err(format!(
            "{name} must be between 1 and {ceiling}; got '{raw}'"
        ));
    }
    Ok(parsed)
}

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
        Self::baseline_with_limits(OperatorRuntimeLimits::default())
    }

    pub fn baseline_from_env() -> Result<Self, String> {
        Ok(Self::baseline_with_limits(
            OperatorRuntimeLimits::from_env()?
        ))
    }

    pub fn baseline_with_limits(limits: OperatorRuntimeLimits) -> Self {
        Self {
            spec_assertions: Vec::new(),
            invariants: PolicyInvariants {
                max_response_bytes: limits.max_response_bytes,
                max_fuel: limits.max_fuel,
                max_memory_bytes: limits.max_memory_bytes,
            },
        }
    }

    pub fn from_spec(spec: &ServiceSpec) -> Result<Self, String> {
        Self::from_spec_with_limits(spec, OperatorRuntimeLimits::from_env()?)
    }

    pub fn from_spec_with_limits(
        spec: &ServiceSpec,
        limits: OperatorRuntimeLimits,
    ) -> Result<Self, String> {
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
                        scheme: None,
                        authority: None,
                        path: case.path.clone(),
                        query: (!case.query.is_empty()).then(|| {
                            let mut serializer = form_urlencoded::Serializer::new(String::new());
                            for (k, v) in &case.query {
                                serializer.append_pair(k, v);
                            }
                            serializer.finish()
                        }),
                        headers: case
                            .headers
                            .iter()
                            .map(|(name, value)| crate::engine::http_envelope::Header {
                                name: name.clone(),
                                value: value.as_bytes().to_vec(),
                            })
                            .collect(),
                        body: case.body.as_deref().unwrap_or("").as_bytes().to_vec(),
                    },
                    expected_status: case.expected_status.ok_or_else(|| {
                        format!(
                            "verification case {} {} has no response status",
                            case.method, case.path
                        )
                    })?,
                    expected_status_max: case.expected_status_max,
                    body_contains: case.body_contains.clone(),
                    header_matches: if case.response_headers.is_empty() {
                        None
                    } else {
                        Some(case.response_headers.clone())
                    },
                    response_json_schema: case.response_json_schema.clone(),
                    json_path_assertions: case.json_path_assertions.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self {
            spec_assertions,
            invariants: PolicyInvariants {
                max_response_bytes: limits.max_response_bytes,
                max_fuel: limits.max_fuel,
                max_memory_bytes: limits.max_memory_bytes,
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

    #[test]
    fn operator_limits_default_to_hard_ceilings_and_accept_lower_values() {
        let defaults = OperatorRuntimeLimits::from_lookup(|_| None).unwrap();
        assert_eq!(defaults, OperatorRuntimeLimits::default());
        let lower = OperatorRuntimeLimits::from_lookup(|name| match name {
            "NOSRC_MAX_FUEL" => Some("200000000".into()),
            "NOSRC_MAX_MEMORY_BYTES" => Some("33554432".into()),
            "NOSRC_MAX_RESPONSE_BYTES" => Some("524288".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(lower.max_fuel, 200_000_000);
        assert_eq!(lower.max_memory_bytes, 32 * 1024 * 1024);
        assert_eq!(lower.max_response_bytes, 512 * 1024);
    }

    #[test]
    fn configured_limits_are_copied_into_the_operator_policy() {
        let spec = ServiceSpec::parse(
            r#"# Service: Example
## Endpoints
### `GET /`
- Response `200`: ok
## Verification
### `GET /`
- Response `200`
"#,
        )
        .unwrap();
        let limits = OperatorRuntimeLimits {
            max_fuel: 200_000_000,
            max_memory_bytes: 32 * 1024 * 1024,
            max_response_bytes: 512 * 1024,
        };
        let policy = AcceptancePolicy::from_spec_with_limits(&spec, limits).unwrap();
        assert_eq!(policy.invariants.max_fuel, limits.max_fuel);
        assert_eq!(policy.invariants.max_memory_bytes, limits.max_memory_bytes);
        assert_eq!(
            policy.invariants.max_response_bytes,
            limits.max_response_bytes
        );
    }

    #[test]
    fn operator_limits_reject_zero_invalid_and_over_ceiling_values() {
        for (name, value) in [
            ("NOSRC_MAX_FUEL", "0"),
            ("NOSRC_MAX_FUEL", "not-a-number"),
            ("NOSRC_MAX_FUEL", ""),
            ("NOSRC_MAX_FUEL", "   "),
            ("NOSRC_MAX_FUEL", "\t\n"),
            ("NOSRC_MAX_MEMORY_BYTES", ""),
            ("NOSRC_MAX_MEMORY_BYTES", "  "),
            ("NOSRC_MAX_RESPONSE_BYTES", ""),
            ("NOSRC_MAX_RESPONSE_BYTES", "\t "),
            ("NOSRC_MAX_FUEL", "250000001"),
            ("NOSRC_MAX_MEMORY_BYTES", "67108865"),
            ("NOSRC_MAX_RESPONSE_BYTES", "1048577"),
        ] {
            let error =
                OperatorRuntimeLimits::from_lookup(|key| (key == name).then(|| value.to_string()))
                    .unwrap_err();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn trusted_policy_passes_request_body_to_the_guest_envelope() {
        let spec = ServiceSpec::parse(
            r#"# Service: Example
## Endpoints
### `POST /items`
- Response `200`: ok
## Verification
### `POST /items`
- Request body: `{"name":"hello"}`
- Response `200`
"#,
        )
        .unwrap();
        let policy = AcceptancePolicy::from_spec(&spec).unwrap();
        assert_eq!(
            policy.spec_assertions[0].request.body,
            br#"{"name":"hello"}"#.to_vec()
        );
    }

    #[test]
    fn policy_rejects_relaxed_runtime_ceilings() {
        let policy = AcceptancePolicy {
            spec_assertions: vec![ContractAssertion {
                name: "trusted".into(),
                request: RequestEnvelope {
                    method: "GET".into(),
                    scheme: None,
                    authority: None,
                    path: "/".into(),
                    query: None,
                    headers: vec![],
                    body: vec![],
                },
                expected_status: 200,
                expected_status_max: None,
                body_contains: None,
                header_matches: None,
                response_json_schema: None,
                json_path_assertions: Vec::new(),
            }],
            invariants: PolicyInvariants {
                max_response_bytes: crate::engine::http_envelope::MAX_HANDLER_RESPONSE_BYTES,
                max_fuel: DEFAULT_COMPONENT_FUEL + 1,
                max_memory_bytes: DEFAULT_COMPONENT_MEMORY_BYTES,
            },
        };
        assert!(policy.validate().unwrap_err().contains("fuel"));
    }
}
