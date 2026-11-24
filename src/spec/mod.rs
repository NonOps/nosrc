use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
    /// Request query parameters decoded from the query string in the case heading.
    pub query: HashMap<String, String>,
    /// Headers sent with the request.
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub expected_status: Option<u16>,
    pub body_contains: Option<String>,
    /// Expected response headers, independent of request headers.
    pub response_headers: HashMap<String, String>,
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
        let mut covered_endpoints = vec![false; self.endpoints.len()];
        for case in &self.verification {
            let matching: Vec<_> = self
                .endpoints
                .iter()
                .enumerate()
                .filter(|(_, endpoint)| {
                    endpoint.method == case.method && path_matches(&endpoint.path, &case.path)
                })
                .map(|(index, endpoint)| (index, path_specificity(&endpoint.path)))
                .collect();
            let best_specificity = matching
                .iter()
                .map(|(_, specificity)| *specificity)
                .max()
                .ok_or_else(|| {
                    format!(
                        "verification case {} {} does not match a declared endpoint",
                        case.method, case.path
                    )
                })?;
            let best_matches: Vec<_> = matching
                .iter()
                .filter(|(_, specificity)| *specificity == best_specificity)
                .collect();
            if best_matches.len() != 1 {
                return Err(format!(
                    "verification case {} {} ambiguously matches multiple endpoints",
                    case.method, case.path
                ));
            }
            let endpoint_index = best_matches[0].0;
            let endpoint = &self.endpoints[endpoint_index];
            covered_endpoints[endpoint_index] = true;

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
        for (index, endpoint) in self.endpoints.iter().enumerate() {
            if !covered_endpoints[index] {
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

        for (line_idx, line) in markdown.lines().enumerate() {
            let line_num = line_idx + 1;
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

            // Major section header (## Capabilities, ## Schema, ## Endpoints, ## Verification)
            if let Some(section) = trimmed.strip_prefix("## ") {
                if let Some(ep) = current_endpoint.take() {
                    endpoints.push(ep);
                }
                if let Some(case) = current_verification.take() {
                    verification.push(case);
                }
                let sec_lower = section.trim().to_lowercase();
                current_section = match sec_lower.as_str() {
                    "capabilities" => "capabilities",
                    "schema" => "schema",
                    "endpoints" => "endpoints",
                    "verification" => "verification",
                    _ => "other",
                };
                continue;
            }

            // Endpoint and verification case headers are scoped to their own sections.
            if let Some(title) = trimmed.strip_prefix("### ") {
                let cleaned = title.trim().trim_matches('`').trim();
                match current_section {
                    "endpoints" | "endpoint_detail" => {
                        let (method, target) =
                            parse_heading_method_target(cleaned, line_num, "endpoint")?;
                        if !target.starts_with('/') {
                            return Err(HostAbiError::MalformedJson(format!(
                                "endpoint path must start with '/' on line {line_num}, got '{target}'"
                            )));
                        }
                        if let Some(ep) = current_endpoint.take() {
                            endpoints.push(ep);
                        }
                        current_endpoint = Some(SpecEndpoint {
                            method,
                            path: target.to_string(),
                            description: None,
                            body_schema: None,
                            rules: Vec::new(),
                            responses: Vec::new(),
                        });
                        current_section = "endpoint_detail";
                    }
                    "verification" | "verification_detail" => {
                        let (method, target) =
                            parse_heading_method_target(cleaned, line_num, "verification")?;
                        if let Some(case) = current_verification.take() {
                            verification.push(case);
                        }
                        let (path, query) = parse_verification_target(target)?;
                        current_verification = Some(SpecVerification {
                            method,
                            path,
                            query,
                            headers: HashMap::new(),
                            body: None,
                            expected_status: None,
                            body_contains: None,
                            response_headers: HashMap::new(),
                        });
                        current_section = "verification_detail";
                    }
                    _ => {}
                }
                continue;
            }

            if matches!(
                current_section,
                "capabilities"
                    | "schema"
                    | "endpoints"
                    | "endpoint_detail"
                    | "verification"
                    | "verification_detail"
            ) && is_unsupported_bullet(trimmed)
            {
                return Err(HostAbiError::MalformedJson(format!(
                    "unsupported bullet syntax on line {line_num}; use '- ' bullets"
                )));
            }

            // Parse content within current section
            match current_section {
                "capabilities" => {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        let item = item.trim();
                        if let Some(state_val) = item.strip_prefix("state:") {
                            let state_val = state_val.trim();
                            if state_val.is_empty() {
                                return Err(HostAbiError::MalformedJson(format!(
                                    "capability 'state:' cannot be empty on line {line_num}"
                                )));
                            }
                            capabilities.state = state_val.to_string();
                        } else if let Some(egress_val) = item.strip_prefix("egress:") {
                            let egress_str =
                                egress_val.trim().trim_matches(|c| c == '[' || c == ']');
                            for domain in egress_str.split(',') {
                                let d = domain.trim().trim_matches(|c| c == '\'' || c == '"');
                                if !d.is_empty() {
                                    capabilities.egress.insert(d.to_string());
                                }
                            }
                        } else {
                            return Err(HostAbiError::MalformedJson(format!(
                                "unsupported capability field '{item}' on line {line_num}; expected 'state:' or 'egress:'"
                            )));
                        }
                    }
                }
                "schema" => {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        let clean_schema = item.trim().trim_matches('`').trim();
                        if clean_schema.is_empty() {
                            return Err(HostAbiError::MalformedJson(format!(
                                "empty schema entry on line {line_num}"
                            )));
                        }
                        schema.push(clean_schema.to_string());
                    }
                }
                "endpoints" | "verification" => {
                    if trimmed.strip_prefix("- ").is_some() {
                        return Err(HostAbiError::MalformedJson(format!(
                            "bullet item outside of a '### <METHOD> <PATH>' heading on line {line_num}"
                        )));
                    }
                }
                "verification_detail" => {
                    if let Some(case) = current_verification.as_mut() {
                        if let Some(item) = trimmed.strip_prefix("- ") {
                            let item = item.trim();
                            if let Some(body) = item.strip_prefix("Request body:") {
                                if case.body.is_some() {
                                    return Err(duplicate_verification_field("Request body"));
                                }
                                case.body = Some(trim_verification_code_span(body));
                            } else if let Some(body) = item.strip_prefix("Body:") {
                                if case.body.is_some() {
                                    return Err(duplicate_verification_field("Request body"));
                                }
                                // Backward-compatible alias; prefer the explicit Request body label.
                                case.body = Some(body.trim().to_string());
                            } else if let Some(header) = item.strip_prefix("Header:") {
                                let (name, value) = parse_verification_header(header)?;
                                if case.headers.insert(name.clone(), value).is_some() {
                                    return Err(duplicate_verification_field(&format!(
                                        "request header '{name}'"
                                    )));
                                }
                            } else if let Some(header) = item.strip_prefix("Response header:") {
                                let (name, value) = parse_verification_header(header)?;
                                if case.response_headers.insert(name.clone(), value).is_some() {
                                    return Err(duplicate_verification_field(&format!(
                                        "response header '{name}'"
                                    )));
                                }
                            } else if let Some(status) = item.strip_prefix("Response ") {
                                if case.expected_status.is_some() {
                                    return Err(duplicate_verification_field("Response status"));
                                }
                                let status = status.trim().trim_matches('`');
                                case.expected_status = Some(parse_http_status(status)?);
                            } else if let Some(expected) = item.strip_prefix("Body contains:") {
                                if case.body_contains.is_some() {
                                    return Err(duplicate_verification_field("Body contains"));
                                }
                                case.body_contains =
                                    Some(expected.trim().trim_matches('`').to_string());
                            } else {
                                return Err(HostAbiError::MalformedJson(format!(
                                    "unsupported verification field '{item}' on line {line_num}"
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
                                let status = parse_http_status(status_part)?;
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

fn duplicate_verification_field(field: &str) -> HostAbiError {
    HostAbiError::MalformedJson(format!("duplicate verification field '{field}'"))
}

/// Remove one optional Markdown inline-code wrapper without altering the payload.
fn trim_verification_code_span(value: &str) -> String {
    let value = value.trim();
    match value
        .strip_prefix('`')
        .and_then(|value| value.strip_suffix('`'))
    {
        Some(inner) => inner.to_string(),
        None => value.to_string(),
    }
}

fn is_unsupported_bullet(trimmed: &str) -> bool {
    if trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return true;
    }
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &trimmed[digits..];
        if rest.starts_with(". ") || rest.starts_with(") ") {
            return true;
        }
    }
    false
}

fn parse_heading_method_target<'a>(
    cleaned: &'a str,
    line_num: usize,
    kind: &str,
) -> Result<(String, &'a str), HostAbiError> {
    let mut parts = cleaned.split_whitespace();
    let Some(method_raw) = parts.next() else {
        return Err(HostAbiError::MalformedJson(format!(
            "malformed {kind} heading on line {line_num}; expected '### <METHOD> <PATH>'"
        )));
    };
    let Some(target) = parts.next() else {
        return Err(HostAbiError::MalformedJson(format!(
            "malformed {kind} heading '{cleaned}' on line {line_num}; expected '### <METHOD> <PATH>'"
        )));
    };
    if parts.next().is_some() {
        return Err(HostAbiError::MalformedJson(format!(
            "malformed {kind} heading '{cleaned}' on line {line_num}; expected '### <METHOD> <PATH>'"
        )));
    }
    let method = method_raw.to_uppercase();
    if !matches!(
        method.as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    ) {
        return Err(HostAbiError::MalformedJson(format!(
            "unsupported HTTP method '{method_raw}' in {kind} heading on line {line_num}"
        )));
    }
    Ok((method, target))
}

fn parse_verification_header(header: &str) -> Result<(String, String), HostAbiError> {
    let (name, value) = header.trim().split_once('=').ok_or_else(|| {
        HostAbiError::MalformedJson(format!(
            "invalid verification header '{header}'; expected name=value"
        ))
    })?;
    let name = name.trim();
    if name.is_empty() {
        return Err(HostAbiError::MalformedJson(
            "verification header name cannot be empty".into(),
        ));
    }
    Ok((name.to_ascii_lowercase(), value.trim().to_string()))
}

/// Parse a concrete request target from a trusted verification heading.
/// The current request ABI stores one value per query key, so duplicate keys are rejected.
fn parse_verification_target(
    target: &str,
) -> Result<(String, HashMap<String, String>), HostAbiError> {
    if target.contains('#') {
        return Err(HostAbiError::MalformedJson(
            "verification request targets must not contain URL fragments".into(),
        ));
    }
    let (path, query_string) = target.split_once('?').unwrap_or((target, ""));
    if !path.starts_with('/') {
        return Err(HostAbiError::MalformedJson(format!(
            "verification request path must start with '/', got '{path}'"
        )));
    }

    let mut query = HashMap::new();
    for (key, value) in form_urlencoded::parse(query_string.as_bytes()) {
        let key = key.into_owned();
        if query.insert(key.clone(), value.into_owned()).is_some() {
            return Err(HostAbiError::MalformedJson(format!(
                "duplicate verification query parameter '{key}' is unsupported"
            )));
        }
    }
    Ok((path.to_string(), query))
}

fn parse_http_status(value: &str) -> Result<u16, HostAbiError> {
    let status: u16 = value.parse().map_err(|_| {
        HostAbiError::MalformedJson(format!("invalid HTTP response status '{value}'"))
    })?;
    if !(100..=599).contains(&status) {
        return Err(HostAbiError::MalformedJson(format!(
            "HTTP response status must be between 100 and 599, got '{value}'"
        )));
    }
    Ok(status)
}

fn path_specificity(template: &str) -> usize {
    template
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.starts_with(':'))
        .count()
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
    fn verification_query_parameters_are_decoded_and_separated_from_path() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /search`
- Response `200`: results
## Verification
### `GET /search?q=hello%20world&tag=rust%2Bwasm`
- Response `200`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        spec.validate_verification().unwrap();
        let case = &spec.verification[0];
        assert_eq!(case.path, "/search");
        assert_eq!(case.query.get("q").map(String::as_str), Some("hello world"));
        assert_eq!(case.query.get("tag").map(String::as_str), Some("rust+wasm"));
    }

    #[test]
    fn duplicate_query_parameters_are_rejected() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /search`
- Response `200`: results
## Verification
### `GET /search?q=one&q=two`
- Response `200`
"#;
        let error = ServiceSpec::parse(markdown).unwrap_err().to_string();
        assert!(error.contains("duplicate verification query parameter 'q'"));
    }

    #[test]
    fn verification_request_body_is_parsed_without_markdown_delimiters() {
        let markdown = r#"
# Service: Example
## Endpoints
### `POST /items`
- Response `200`: ok
## Verification
### `POST /items`
- Header: content-type=application/json
- Request body: `{"name":"hello","count":2}`
- Response `200`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        assert_eq!(
            spec.verification[0].body.as_deref(),
            Some(r#"{"name":"hello","count":2}"#)
        );
    }

    #[test]
    fn empty_verification_request_body_is_preserved() {
        let markdown = "# Service: Example\n## Endpoints\n### `POST /items`\n- Response `200`: ok\n## Verification\n### `POST /items`\n- Request body: ``\n- Response `200`\n";
        let spec = ServiceSpec::parse(markdown).unwrap();
        assert_eq!(spec.verification[0].body.as_deref(), Some(""));
    }

    #[test]
    fn duplicate_request_body_fields_are_rejected() {
        for fields in [
            "- Request body: first\n- Request body: second",
            "- Request body: first\n- Body: second",
        ] {
            let markdown = format!(
                "# Service: Example\n## Endpoints\n### `POST /items`\n- Response `200`: ok\n## Verification\n### `POST /items`\n{fields}\n- Response `200`\n"
            );
            let error = ServiceSpec::parse(&markdown).unwrap_err().to_string();
            assert!(
                error.contains("duplicate verification field"),
                "got: {error}"
            );
        }
    }

    #[test]
    fn duplicate_verification_scalar_fields_are_rejected() {
        for fields in [
            "- Body: first\n- Body: second",
            "- Body contains: first\n- Body contains: second",
            "- Response `200`\n- Response `200`",
        ] {
            let markdown = format!(
                "# Service: Example\n## Endpoints\n### `GET /one`\n- Response `200`: ok\n## Verification\n### `GET /one`\n{fields}\n"
            );
            assert!(ServiceSpec::parse(&markdown).is_err(), "accepted: {fields}");
        }
    }

    #[test]
    fn request_and_response_headers_are_parsed_separately_and_duplicates_rejected() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /one`
- Response `200`: ok
## Verification
### `GET /one`
- Header: accept=application/json
- Response header: content-type=application/json
- Response `200`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        let case = &spec.verification[0];
        assert_eq!(
            case.headers.get("accept").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            case.response_headers
                .get("content-type")
                .map(String::as_str),
            Some("application/json")
        );

        let duplicate = markdown.replace(
            "- Response header: content-type=application/json",
            "- Response header: content-type=application/json\n- Response header: content-type=text/plain",
        );
        assert!(ServiceSpec::parse(&duplicate).is_err());

        let duplicate_request = markdown.replace(
            "- Header: accept=application/json",
            "- Header: accept=application/json\n- Header: Accept=text/plain",
        );
        assert!(ServiceSpec::parse(&duplicate_request).is_err());
    }

    #[test]
    fn dynamic_route_verification_rejects_unexpanded_placeholders() {
        assert!(!path_matches("/items/:id", "/items/:id"));
        assert!(!path_matches("/items/:id", "/items/"));
        assert!(path_matches("/items/:id", "/items/42"));
    }

    #[test]
    fn static_route_verification_does_not_cover_dynamic_route() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /items/:id`
- Response `200`: item
### `GET /items/fixed`
- Response `200`: fixed
## Verification
### `GET /items/fixed`
- Response `200`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        assert!(spec
            .validate_verification()
            .unwrap_err()
            .contains("/items/:id"));
    }

    #[test]
    fn static_and_dynamic_routes_can_be_verified_independently() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /items/:id`
- Response `200`: item
### `GET /items/fixed`
- Response `200`: fixed
## Verification
### `GET /items/42`
- Response `200`
### `GET /items/fixed`
- Response `200`
"#;
        let spec = ServiceSpec::parse(markdown).unwrap();
        spec.validate_verification().unwrap();
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
    fn rejects_invalid_endpoint_response_status_instead_of_defaulting() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /one`
- Response `not-a-status`: ok
"#;
        assert!(ServiceSpec::parse(markdown).is_err());
    }

    #[test]
    fn rejects_out_of_range_verification_status() {
        let markdown = r#"
# Service: Example
## Endpoints
### `GET /one`
- Response `200`: ok
## Verification
### `GET /one`
- Response `999`
"#;
        assert!(ServiceSpec::parse(markdown).is_err());
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

    /// Small deterministic xorshift generator so the fuzz test needs no extra dependency
    /// and any failure reproduces from the printed seed.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound.max(1) as u64) as usize
        }
    }

    const FUZZ_SNIPPETS: &[&str] = &[
        "# Service: X\n",
        "## Capabilities\n",
        "- state: volatile\n",
        "- state: durable\n",
        "- egress: [\"example.com\"]\n",
        "## Endpoints\n",
        "### `GET /a/{id}`\n",
        "### `POST /`\n",
        "### `WAT /x`\n",
        "- Response `200`: ok\n",
        "- Response `99999`: bad\n",
        "- Rules: anything at all\n",
        "## Verification\n",
        "### `GET /a/1?x=%zz&x=2`\n",
        "- Response `200`\n",
        "- Body contains: `hi`\n",
        "- Request headers: a=b\n",
        "`\n",
        "###\n",
        "- \n",
        "\u{0}\u{feff}\u{202e}\n",
        "\r\n\t\n",
    ];

    #[test]
    fn parser_never_panics_on_mutated_specs() {
        let reference = include_str!("../../examples/minimal/SPEC.md");
        for seed in 1..=300_u64 {
            let mut rng = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut lines: Vec<String> = reference.lines().map(str::to_string).collect();
            for _ in 0..rng.below(8) + 1 {
                match rng.below(4) {
                    0 if !lines.is_empty() => {
                        let at = rng.below(lines.len());
                        lines.remove(at);
                    }
                    1 => {
                        let at = rng.below(lines.len() + 1);
                        lines.insert(at, FUZZ_SNIPPETS[rng.below(FUZZ_SNIPPETS.len())].into());
                    }
                    2 if !lines.is_empty() => {
                        let at = rng.below(lines.len());
                        let line = &mut lines[at];
                        let cut = line.char_indices().nth(rng.below(line.chars().count() + 1));
                        if let Some((index, _)) = cut {
                            line.truncate(index);
                        }
                    }
                    _ if !lines.is_empty() => {
                        let at = rng.below(lines.len());
                        let other = rng.below(lines.len());
                        lines.swap(at, other);
                    }
                    _ => {}
                }
            }
            let mutated = lines.join("\n");
            // Both outcomes are acceptable; panicking is not. Validation helpers must
            // also stay total on whatever the parser accepted.
            let outcome = std::panic::catch_unwind(|| {
                if let Ok(spec) = ServiceSpec::parse(&mutated) {
                    let _ = spec.validate_verification();
                }
            });
            assert!(
                outcome.is_ok(),
                "parser panicked for seed {seed} on input:\n{mutated}"
            );
        }
    }

    #[test]
    fn parser_handles_pathological_inputs_without_panicking() {
        let inputs = [
            String::new(),
            "\n\n\n".to_string(),
            "#".to_string(),
            "### `".to_string(),
            "### ``".to_string(),
            "### `GET`".to_string(),
            "- Response `".to_string(),
            "## Verification\n### `GET /`\n- Response `abc`".to_string(),
            "\u{0}".repeat(10_000),
            "### `GET /a`\n".repeat(5_000),
            format!("# Service: {}\n", "x".repeat(1_000_000)),
            "## Endpoints\n* GET /a\n+ POST /b\n1. DELETE /c\n".to_string(),
            "## Capabilities\n- state: \n".to_string(),
        ];
        for input in inputs {
            let _ = ServiceSpec::parse(&input);
        }
    }

    #[test]
    fn section_headings_match_exactly_not_by_substring() {
        let md = r#"
# Service: Exact Sections

## Not Capabilities
- state: persistent
- egress: [evil.com]

## My Schema Notes
- `should_not_be_parsed`

## Example Endpoints
### `GET /fake`
- Response `200`: `ok`

## Endpoints
### `GET /real`
- Response `200`: `ok`

## Verification
### `GET /real`
- Response `200`
"#;
        let spec = ServiceSpec::parse(md).expect("valid spec with unknown sections");
        assert_eq!(spec.capabilities.state, "volatile");
        assert!(spec.capabilities.egress.is_empty());
        assert!(spec.schema.is_empty());
        assert_eq!(spec.endpoints.len(), 1);
        assert_eq!(spec.endpoints[0].path, "/real");
        assert!(spec.validate_verification().is_ok());
    }

    #[test]
    fn unknown_sections_allow_arbitrary_markdown_including_star_bullets() {
        let md = r#"
# Service: Notes

## Examples
* Illustrative bullet
+ Another bullet
1. Numbered item

## Endpoints
### `GET /ping`
- Response `200`: `pong`

## Verification
### `GET /ping`
- Response `200`
"#;
        let spec = ServiceSpec::parse(md).expect("unknown sections should be ignored");
        assert_eq!(spec.endpoints.len(), 1);
    }

    #[test]
    fn rejects_unsupported_bullet_syntax_in_structured_sections() {
        for bad_bullet in [
            "* state: volatile",
            "+ state: volatile",
            "1. state: volatile",
            "2) state: volatile",
        ] {
            let md = format!("## Capabilities\n{bad_bullet}\n");
            let err = ServiceSpec::parse(&md).unwrap_err().to_string();
            assert!(err.contains("unsupported bullet syntax"), "got: {err}");
        }
    }

    #[test]
    fn rejects_malformed_headings_and_capability_fields() {
        let cases = [
            (
                "## Capabilities\n- unknown: value\n",
                "unsupported capability field",
            ),
            ("## Capabilities\n- state:\n", "cannot be empty"),
            ("## Schema\n- ``\n", "empty schema entry"),
            (
                "## Endpoints\n- stray bullet\n",
                "outside of a '### <METHOD> <PATH>'",
            ),
            ("## Endpoints\n### `GET`\n", "malformed endpoint heading"),
            (
                "## Endpoints\n### `GET /a extra`\n",
                "malformed endpoint heading",
            ),
            ("## Endpoints\n### `BOGUS /a`\n", "unsupported HTTP method"),
            ("## Endpoints\n### `GET relative`\n", "must start with '/'"),
            (
                "## Verification\n### `POST`\n",
                "malformed verification heading",
            ),
        ];
        for (md, expected_substr) in cases {
            let err = ServiceSpec::parse(md)
                .expect_err(&format!("expected error for:\n{md}"))
                .to_string();
            assert!(
                err.contains(expected_substr),
                "expected '{expected_substr}' in '{err}' for:\n{md}"
            );
        }
    }
}
