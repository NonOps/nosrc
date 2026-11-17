use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::error::HostAbiError;

/// Declarative service specification parsed from `SPEC.md`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceSpec {
    pub name: String,
    pub description: Option<String>,
    pub capabilities: SpecCapabilities,
    pub schema: Vec<String>,
    pub endpoints: Vec<SpecEndpoint>,
    pub verification: Vec<SpecVerification>,
    pub raw_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecCapabilities {
    pub state: String,
    pub egress: HashSet<String>,
}

impl Default for SpecCapabilities {
    fn default() -> Self {
        Self {
            state: "volatile".to_string(),
            egress: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecEndpoint {
    pub method: String,
    pub path: String,
    pub description: Option<String>,
    pub body_schema: Option<String>,
    pub rules: Vec<String>,
    pub responses: Vec<SpecResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecResponse {
    pub status: u16,
    pub description: Option<String>,
    pub schema: Option<String>,
}

/// Human-authored verification case parsed from the trusted SPEC.md.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecVerification {
    pub method: String,
    pub path: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Option<String>,
    pub expected_status: Option<u16>,
    pub body_contains: Option<String>,
}

impl ServiceSpec {
    /// Ensure trusted cases cover every declared endpoint and assert only documented statuses.
    pub fn validate_verification(&self) -> Result<(), String> {
        if self.endpoints.is_empty() {
            return Err("SPEC must declare at least one endpoint".into());
        }
        if self.verification.is_empty() {
            return Err("SPEC must declare trusted cases under '## Verification'".into());
        }
        for case in &self.verification {
            let endpoint = self
                .endpoints
                .iter()
                .find(|endpoint| {
                    endpoint.method == case.method && path_matches(&endpoint.path, &case.path)
                })
                .ok_or_else(|| {
                    format!(
                        "verification case {} {} does not match a declared endpoint",
                        case.method, case.path
                    )
                })?;
            let status = case.expected_status.ok_or_else(|| {
                format!(
                    "verification case {} {} is missing a Response status",
                    case.method, case.path
                )
            })?;
            if !endpoint
                .responses
                .iter()
                .any(|response| response.status == status)
            {
                return Err(format!(
                    "verification case {} {} expects undocumented status {}",
                    case.method, case.path, status
                ));
            }
        }
        for endpoint in &self.endpoints {
            if !self.verification.iter().any(|case| {
                case.method == endpoint.method && path_matches(&endpoint.path, &case.path)
            }) {
                return Err(format!(
                    "endpoint {} {} has no trusted verification case",
                    endpoint.method, endpoint.path
                ));
            }
        }
        Ok(())
    }

    /// Parses a markdown string conforming to the `SPEC.md` format
    pub fn parse(markdown: &str) -> Result<Self, HostAbiError> {
        let mut name = "service".to_string();
        let mut description = None;
        let mut capabilities = SpecCapabilities::default();
        let mut schema = Vec::new();
        let mut endpoints = Vec::new();
        let mut verification = Vec::new();

        let mut current_section = "";
        let mut current_endpoint: Option<SpecEndpoint> = None;
        let mut current_verification: Option<SpecVerification> = None;

        for line in markdown.lines() {
            let trimmed = line.trim();

            if trimmed.is_empty() {
                continue;
            }

            // Top-level service title (# Service: ...)
            if let Some(title) = trimmed.strip_prefix("# ") {
                let title_clean = title.trim();
                if let Some(service_name) = title_clean.strip_prefix("Service:") {
                    name = service_name.trim().to_string();
                } else {
                    name = title_clean.to_string();
                }
                current_section = "title";
                continue;
            }

            // Major section header (## Capabilities, ## Schema, ## Endpoints)
            if let Some(section) = trimmed.strip_prefix("## ") {
                if let Some(ep) = current_endpoint.take() {
                    endpoints.push(ep);
                }
                if let Some(case) = current_verification.take() {
                    verification.push(case);
                }
                let sec_lower = section.trim().to_lowercase();
                if sec_lower.contains("capability") || sec_lower.contains("capabilities") {
                    current_section = "capabilities";
                } else if sec_lower.contains("schema") {
                    current_section = "schema";
                } else if sec_lower.contains("verification") || sec_lower.contains("verifications")
                {
                    current_section = "verification";
                } else if sec_lower.contains("endpoint") || sec_lower.contains("endpoints") {
                    current_section = "endpoints";
                } else {
                    current_section = "other";
                }
                continue;
            }

            // Endpoint and verification case headers are scoped to their own sections.
            if let Some(title) = trimmed.strip_prefix("### ") {
                let cleaned = title.trim().trim_matches('`').trim();
                let mut parts = cleaned.split_whitespace();
                let method = parts.next().unwrap_or("GET").to_uppercase();
                let path = parts.next().unwrap_or("/").to_string();
                match current_section {
                    "endpoints" | "endpoint_detail" => {
                        if let Some(ep) = current_endpoint.take() {
                            endpoints.push(ep);
                        }
                        current_endpoint = Some(SpecEndpoint {
                            method,
                            path,
                            description: None,
                            body_schema: None,
                            rules: Vec::new(),
                            responses: Vec::new(),
                        });
                        current_section = "endpoint_detail";
                    }
                    "verification" | "verification_detail" => {
                        if let Some(case) = current_verification.take() {
                            verification.push(case);
                        }
                        current_verification = Some(SpecVerification {
                            method,
                            path,
                            headers: std::collections::HashMap::new(),
                            body: None,
                            expected_status: None,
                            body_contains: None,
                        });
                        current_section = "verification_detail";
                    }
                    _ => {}
                }
                continue;
            }

            // Parse content within current section
            match current_section {
                "capabilities" => {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        let item = item.trim();
                        if let Some(state_val) = item.strip_prefix("state:") {
                            capabilities.state = state_val.trim().to_string();
                        } else if let Some(egress_val) = item.strip_prefix("egress:") {
                            let egress_str =
                                egress_val.trim().trim_matches(|c| c == '[' || c == ']');
                            for domain in egress_str.split(',') {
                                let d = domain.trim().trim_matches(|c| c == '\'' || c == '"');
                                if !d.is_empty() {
                                    capabilities.egress.insert(d.to_string());
                                }
                            }
                        }
                    }
                }
                "schema" => {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        let clean_schema = item.trim().trim_matches('`').trim();
                        if !clean_schema.is_empty() {
                            schema.push(clean_schema.to_string());
                        }
                    }
                }
                "verification_detail" => {
                    if let Some(case) = current_verification.as_mut() {
                        if let Some(item) = trimmed.strip_prefix("- ") {
                            let item = item.trim();
                            if let Some(body) = item.strip_prefix("Body:") {
                                case.body = Some(body.trim().to_string());
                            } else if let Some(header) = item.strip_prefix("Header:") {
                                let (name, value) = header.trim().split_once('=').ok_or_else(|| {
                                    HostAbiError::MalformedJson(format!(
                                        "invalid verification header '{header}'; expected name=value"
                                    ))
                                })?;
                                if name.trim().is_empty() {
                                    return Err(HostAbiError::MalformedJson(
                                        "verification header name cannot be empty".into(),
                                    ));
                                }
                                case.headers
                                    .insert(name.trim().to_lowercase(), value.trim().to_string());
                            } else if let Some(status) = item.strip_prefix("Response ") {
                                let status = status.trim().trim_matches('`');
                                case.expected_status = Some(status.parse().map_err(|_| {
                                    HostAbiError::MalformedJson(format!(
                                        "invalid verification response status '{status}'"
                                    ))
                                })?);
                            } else if let Some(expected) = item.strip_prefix("Body contains:") {
                                case.body_contains =
                                    Some(expected.trim().trim_matches('`').to_string());
                            } else {
                                return Err(HostAbiError::MalformedJson(format!(
                                    "unsupported verification field '{item}'"
                                )));
                            }
                        }
                    }
                }
                "endpoint_detail" => {
                    if let Some(ep) = current_endpoint.as_mut() {
                        if let Some(item) = trimmed.strip_prefix("- ") {
                            let item = item.trim();
                            if let Some(body_spec) = item.strip_prefix("Body:") {
                                ep.body_schema = Some(body_spec.trim().to_string());
                            } else if let Some(rules_spec) = item.strip_prefix("Rules:") {
                                ep.rules.push(rules_spec.trim().to_string());
                            } else if let Some(resp_spec) = item.strip_prefix("Response ") {
                                // e.g. `201`: { ... } or 200: ...
                                let mut parts = resp_spec.splitn(2, ':');
                                let status_part =
                                    parts.next().unwrap_or("200").trim().trim_matches('`');
                                let status: u16 = status_part.parse().unwrap_or(200);
                                let schema_part = parts.next().map(|s| s.trim().to_string());

                                ep.responses.push(SpecResponse {
                                    status,
                                    description: None,
                                    schema: schema_part,
                                });
                            } else {
                                ep.rules.push(item.to_string());
                            }
                        }
                    }
                }
                _ => {
                    if description.is_none() && !trimmed.starts_with('#') {
                        description = Some(trimmed.to_string());
                    }
                }
            }
        }

        if let Some(ep) = current_endpoint.take() {
            endpoints.push(ep);
        }
        if let Some(case) = current_verification.take() {
            verification.push(case);
        }

        Ok(Self {
            name,
            description,
            capabilities,
            schema,
            endpoints,
            verification,
            raw_markdown: markdown.to_string(),
        })
    }
}

fn path_matches(template: &str, concrete: &str) -> bool {
    let expected: Vec<_> = template.trim_matches('/').split('/').collect();
    let actual: Vec<_> = concrete.trim_matches('/').split('/').collect();
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual.iter())
            .all(|(expected, actual)| {
                if expected.starts_with(':') {
                    !actual.is_empty() && !actual.starts_with(':')
                } else {
                    expected == actual
                }
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_mode_is_volatile() {
        let markdown = r#"
# Service: Minimal

## Endpoints
### `GET /`
- Response `200`: ok
        "#;
        let spec = ServiceSpec::parse(markdown).unwrap();

        assert_eq!(spec.capabilities.state, "volatile");
        assert!(spec.capabilities.egress.is_empty());
        assert!(spec.verification.is_empty());
    }

    #[test]
    fn test_parse_reference_spec() {
        let markdown = r#"
# Service: URL Shortener & Analytics
A resilient spec-driven URL shortener.

## Capabilities
- state: embedded (sqlite)
- egress: [api.github.com, "hooks.slack.com"]

## Schema
- `links(code TEXT PRIMARY KEY, target_url TEXT NOT NULL, clicks INTEGER DEFAULT 0, created_at TEXT)`

## Endpoints
### `POST /shorten`
- Body: `{ "url": string }`
- Rules: Validate valid http/https URL. Generate 6-char alphanumeric code. Store in `links`.
- Response `201`: `{ "code": string, "short_url": string }`

### `GET /:code`
- Rules: Lookup `code`. If missing return `404`. Increment `clicks` atomically and return `302` redirect to `target_url`.
        "#;

        let spec = ServiceSpec::parse(markdown).expect("Failed to parse SPEC.md");

        assert_eq!(spec.name, "URL Shortener & Analytics");
        assert_eq!(
            spec.description,
            Some("A resilient spec-driven URL shortener.".to_string())
        );
        assert_eq!(spec.capabilities.state, "embedded (sqlite)");
        assert!(spec.capabilities.egress.contains("api.github.com"));
        assert!(spec.capabilities.egress.contains("hooks.slack.com"));

        assert_eq!(spec.schema.len(), 1);
        assert_eq!(
            spec.schema[0],
            "links(code TEXT PRIMARY KEY, target_url TEXT NOT NULL, clicks INTEGER DEFAULT 0, created_at TEXT)"
        );

        assert_eq!(spec.endpoints.len(), 2);

        // Endpoint 1: POST /shorten
        let ep1 = &spec.endpoints[0];
        assert_eq!(ep1.method, "POST");
        assert_eq!(ep1.path, "/shorten");
        assert_eq!(ep1.body_schema, Some(r#"`{ "url": string }`"#.to_string()));
        assert_eq!(ep1.responses.len(), 1);
        assert_eq!(ep1.responses[0].status, 201);

        // Endpoint 2: GET /:code
        let ep2 = &spec.endpoints[1];
        assert_eq!(ep2.method, "GET");
        assert_eq!(ep2.path, "/:code");
        assert!(!ep2.rules.is_empty());
    }

    #[test]
    fn parses_and_validates_human_authored_verification_cases() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /items/:id`
- Response `200`: item
## Verification
### `GET /items/42`
- Response `200`
- Body contains: `item`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        spec.validate_verification().unwrap();
        assert_eq!(spec.verification.len(), 1);
        assert_eq!(spec.verification[0].path, "/items/42");
        assert_eq!(spec.verification[0].expected_status, Some(200));
        assert_eq!(spec.verification[0].body_contains.as_deref(), Some("item"));
    }

    #[test]
    fn dynamic_route_verification_rejects_unexpanded_placeholders() {
        assert!(!path_matches("/items/:id", "/items/:id"));
        assert!(!path_matches("/items/:id", "/items/"));
        assert!(path_matches("/items/:id", "/items/42"));
    }

    #[test]
    fn trusted_verification_must_cover_every_endpoint() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /one`
- Response `200`: ok
### `GET /two`
- Response `200`: ok
## Verification
### `GET /one`
- Response `200`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        assert!(spec.validate_verification().unwrap_err().contains("/two"));
    }

    #[test]
    fn trusted_verification_requires_an_explicit_status() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /one`
- Response `200`: ok
## Verification
### `GET /one`
- Body contains: `ok`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        assert!(spec
            .validate_verification()
            .unwrap_err()
            .contains("missing a Response status"));
    }
}
