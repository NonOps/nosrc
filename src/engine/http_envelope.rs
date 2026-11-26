use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::error::EngineError;

/// Maximum serialized Wasmtime-bound guest response in bytes.
/// Both the body bytes and WIT framing overhead must stay below this.
pub const MAX_HANDLER_RESPONSE_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// ABI v2 — typed WIT request and response records
// ---------------------------------------------------------------------------

/// A single HTTP header field (name normalized to lowercase, value as raw bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: Vec<u8>,
}

/// Incoming HTTP request passed to the guest handler.
///
/// Matches the final `nosrc:runtime@2.0.0` WIT contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// HTTP method (uppercase).
    pub method: String,
    /// Scheme from the parsed URI, if present.
    pub scheme: Option<String>,
    /// URI authority, falling back to the HTTP/1 `Host` header value.
    pub authority: Option<String>,
    /// Encoded path component as exposed by `http::Uri`; not percent-decoded.
    /// For OPTIONS `*` the value is `*`.
    pub path: String,
    /// Raw encoded query component without the leading `?`.
    /// `None` when no query delimiter is present; `Some("")` when explicitly empty.
    pub query: Option<String>,
    /// Request headers.  Names are lowercase.  Values are raw bytes.
    pub headers: Vec<Header>,
    /// Message content after transport framing is removed.
    /// Empty content is `[]`; there is no absent/present distinction at the ABI level.
    pub body: Vec<u8>,
}

impl RequestEnvelope {
    /// Build a `RequestEnvelope` from raw Axum HTTP request components.
    ///
    /// Returns an error when:
    /// - The method is CONNECT (tunnel switching is unsupported).
    /// - A header name is invalid (not an ASCII token).
    /// - A header value contains prohibited bytes (CR, LF, or NUL).
    pub fn try_from_parts(
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        body_bytes: Bytes,
    ) -> Result<Self, RequestError> {
        // CONNECT tunneling is not supported.
        if method == Method::CONNECT {
            return Err(RequestError::ConnectUnsupported);
        }

        let scheme = uri.scheme().map(|s| s.as_str().to_owned());

        // authority: URI authority first, then Host header.
        let authority = uri.authority().map(|a| a.as_str().to_owned()).or_else(|| {
            headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        });

        // path: encoded path; use `*` for OPTIONS asterisk-form.
        let path = {
            let p = uri.path();
            if p.is_empty() && method == Method::OPTIONS {
                "*".to_owned()
            } else {
                p.to_owned()
            }
        };

        // query: raw encoded query without leading `?`.
        let query = uri.query().map(str::to_owned);

        // headers: normalize names to lowercase; validate values.
        let mut header_list = Vec::with_capacity(headers.len());
        for (name, value) in headers {
            let name_str = name.as_str().to_lowercase();
            let value_bytes = value.as_bytes();
            // RFC 9110 §5.5: reject CR, LF, NUL in field values.
            if value_bytes
                .iter()
                .any(|&b| b == b'\r' || b == b'\n' || b == 0)
            {
                return Err(RequestError::InvalidHeaderValue(name_str));
            }
            header_list.push(Header {
                name: name_str,
                value: value_bytes.to_vec(),
            });
        }

        Ok(Self {
            method: method.as_str().to_owned(),
            scheme,
            authority,
            path,
            query,
            headers: header_list,
            body: body_bytes.to_vec(),
        })
    }
}

/// Errors that can occur while building a `RequestEnvelope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// CONNECT method is not supported.
    ConnectUnsupported,
    /// A header value contains a prohibited byte.
    InvalidHeaderValue(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::ConnectUnsupported => {
                write!(f, "CONNECT method is not supported by this handler ABI")
            }
            RequestError::InvalidHeaderValue(name) => {
                write!(
                    f,
                    "Header '{name}' contains a prohibited byte (CR, LF, or NUL)"
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Response body variant
// ---------------------------------------------------------------------------

/// Guest-chosen response body: either a UTF-8 string or raw bytes.
///
/// Matches the WIT `variant response-body { text(string), bytes(list<u8>) }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tag", content = "val")]
#[serde(rename_all = "lowercase")]
pub enum ResponseBody {
    /// Guest returns a UTF-8 string; the host encodes it as UTF-8 on the wire.
    Text(String),
    /// Guest returns raw bytes; the host emits them byte-for-byte.
    Bytes(Vec<u8>),
}

/// Outgoing HTTP response produced by the guest handler.
///
/// Matches the final `nosrc:runtime@2.0.0` WIT response record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    /// Final HTTP response status (200–599).
    pub status: u16,
    /// Response headers.  Names should be lowercase.  Values are raw bytes.
    pub headers: Vec<Header>,
    /// Response body chosen by the guest.
    /// An empty text `""` or empty bytes `[]` is a deliberate zero-length body.
    pub body: ResponseBody,
}

impl ResponseEnvelope {
    pub fn new(status: u16, body: ResponseBody) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        self.headers.push(Header {
            name: name.into(),
            value: value.into(),
        });
        self
    }

    /// Converts an untrusted guest response into a safe host-owned HTTP response.
    ///
    /// Validation rules (fail closed):
    /// - Status must be 200–599; anything else becomes 500.
    /// - Response header names and values are validated; invalid entries make the
    ///   whole response an internal error rather than being silently discarded.
    /// - Hop-by-hop and framing headers are stripped.
    /// - Guest body is suppressed for HEAD, 204, 205, and 304.
    pub fn into_axum_response(self) -> Response {
        // Validate status.
        let status_code = if (200..=599).contains(&self.status) {
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        } else {
            return invalid_guest_response("Guest returned an invalid HTTP status code");
        };

        let mut builder = Response::builder().status(status_code);

        // Collect Connection-header nominated tokens case-insensitively.
        // A malformed or non-ASCII value is a malformed guest response; silently
        // skipping it could forward a field that the guest explicitly nominated.
        let mut connection_tokens: HashSet<String> = HashSet::new();
        for header in &self.headers {
            if header.name.eq_ignore_ascii_case("connection") {
                let value_str = match std::str::from_utf8(&header.value) {
                    Ok(value) if value.is_ascii() => value,
                    _ => {
                        return invalid_guest_response(
                            "Guest returned a malformed Connection header",
                        )
                    }
                };
                for token in value_str.split(',') {
                    let token = token.trim();
                    if token.is_empty() {
                        return invalid_guest_response(
                            "Guest returned a malformed Connection header",
                        );
                    }
                    let parsed = match HeaderName::from_bytes(token.as_bytes()) {
                        Ok(name) => name,
                        Err(_) => {
                            return invalid_guest_response(
                                "Guest returned a malformed Connection header",
                            )
                        }
                    };
                    connection_tokens.insert(parsed.as_str().to_owned());
                }
            }
        }

        // Validate and apply response headers.
        for header in &self.headers {
            let name_lc = header.name.to_ascii_lowercase();
            if is_forbidden_guest_header(&name_lc) || connection_tokens.contains(&name_lc) {
                continue;
            }
            // Validate header name (must be a valid HTTP token).
            let header_name = match HeaderName::try_from(header.name.as_str()) {
                Ok(n) => n,
                Err(_) => {
                    return invalid_guest_response(&format!(
                        "Guest returned an invalid response header name: {}",
                        header.name
                    ));
                }
            };
            // Validate header value (must be valid HTTP header bytes).
            let header_value = match HeaderValue::try_from(header.value.as_slice()) {
                Ok(v) => v,
                Err(_) => {
                    return invalid_guest_response(&format!(
                        "Guest returned an invalid response header value for: {}",
                        header.name
                    ));
                }
            };
            builder = builder.header(header_name, header_value);
        }

        // Determine body bytes; suppress for no-body statuses.
        let body_bytes: Vec<u8> = if matches!(
            status_code,
            StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT | StatusCode::NOT_MODIFIED
        ) {
            Vec::new()
        } else {
            match &self.body {
                ResponseBody::Text(s) => s.as_bytes().to_vec(),
                ResponseBody::Bytes(b) => b.clone(),
            }
        };

        builder
            .body(axum::body::Body::from(body_bytes))
            .unwrap_or_else(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to construct response",
                )
                    .into_response()
            })
    }
}

fn invalid_guest_response(detail: &str) -> Response {
    tracing::warn!("Invalid guest response: {detail}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Guest returned an invalid HTTP response",
    )
        .into_response()
}

/// Headers that alter HTTP message framing or connection management are owned
/// by the host and must never be forwarded from an untrusted guest.
fn is_forbidden_guest_header(name: &str) -> bool {
    matches!(
        name,
        "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

// ---------------------------------------------------------------------------
// Error response
// ---------------------------------------------------------------------------

/// Error response structure when Wasm guest traps, runs out of fuel, or panics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: String,
    pub error_type: String,
    pub status: u16,
}

impl ErrorEnvelope {
    pub fn from_engine_error(err: &EngineError) -> Self {
        let (status, err_type) = match err {
            EngineError::Trap(crate::error::SandboxTrap::FuelExhausted) => (504, "FuelExhausted"),
            EngineError::Trap(crate::error::SandboxTrap::ExecutionTimeout) => {
                (504, "ExecutionTimeout")
            }
            EngineError::Trap(crate::error::SandboxTrap::MemoryExhausted(_)) => {
                (500, "MemoryExhausted")
            }
            EngineError::Trap(crate::error::SandboxTrap::GuestTrap) => (500, "GuestTrap"),
            EngineError::Trap(_) => (500, "SandboxTrap"),
            EngineError::HostAbi(crate::error::HostAbiError::CapabilityDisabled(_)) => {
                (501, "CapabilityDisabled")
            }
            EngineError::HostAbi(crate::error::HostAbiError::EgressForbidden { .. }) => {
                (403, "EgressForbidden")
            }
            EngineError::HostAbi(crate::error::HostAbiError::SsrfBlocked(_)) => {
                (403, "SsrfBlocked")
            }
            _ => (500, "InternalEngineError"),
        };

        let error = match err_type {
            "FuelExhausted" => "Handler execution exceeded its resource budget",
            "MemoryExhausted" => "Handler exceeded its memory limit",
            "GuestTrap" | "SandboxTrap" => "Handler execution failed",
            "CapabilityDisabled" => "Requested capability is unavailable",
            "EgressForbidden" | "SsrfBlocked" => "Requested network access is forbidden",
            _ => "Internal runtime error",
        };

        Self {
            error: error.to_string(),
            error_type: err_type.to_string(),
            status,
        }
    }

    pub fn into_axum_response(self) -> Response {
        let status = if (200..=599).contains(&self.status) {
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        let body = serde_json::to_string(&self).unwrap_or_default();

        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .header("X-Nosrc-Error", self.error_type)
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to construct error response",
                )
                    .into_response()
            })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn get_request(path: &str) -> RequestEnvelope {
        let uri: Uri = path.parse().unwrap();
        RequestEnvelope::try_from_parts(&Method::GET, &uri, &HeaderMap::new(), Bytes::new())
            .unwrap()
    }

    // --- RequestEnvelope construction ---

    #[test]
    fn connect_method_is_rejected() {
        let uri: Uri = "http://example.com:443".parse().unwrap();
        let err = RequestEnvelope::try_from_parts(
            &Method::CONNECT,
            &uri,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .unwrap_err();
        assert!(matches!(err, RequestError::ConnectUnsupported));
    }

    #[test]
    fn header_names_are_lowercased() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Mixed-Case", "value".parse().unwrap());
        let req = RequestEnvelope::try_from_parts(
            &Method::GET,
            &"/".parse().unwrap(),
            &headers,
            Bytes::new(),
        )
        .unwrap();
        assert!(req.headers.iter().any(|h| h.name == "x-mixed-case"));
    }

    #[test]
    fn prohibited_header_value_bytes_are_rejected_by_http_before_abi_mapping() {
        // http::HeaderValue prevents prohibited CR/LF/NUL bytes from reaching Axum.
        // The ABI repeats the check as defence in depth for alternate ingress adapters.
        for value in [b"good\r\nbad".as_slice(), b"good\0bad".as_slice()] {
            assert!(axum::http::HeaderValue::from_bytes(value).is_err());
        }
    }

    #[test]
    fn binary_body_bytes_are_preserved_exactly() {
        let body = Bytes::from(vec![0x00, 0x01, 0x7f, 0xff, 0xfe]);
        let req = RequestEnvelope::try_from_parts(
            &Method::POST,
            &"/upload".parse().unwrap(),
            &HeaderMap::new(),
            body.clone(),
        )
        .unwrap();
        assert_eq!(req.body, body.as_ref());
    }

    #[test]
    fn query_is_raw_encoded_without_leading_question_mark() {
        let req = get_request("/search?q=hello%20world&plus=a%2Bb&empty=");
        assert_eq!(
            req.query.as_deref(),
            Some("q=hello%20world&plus=a%2Bb&empty=")
        );
    }

    #[test]
    fn absent_query_is_none() {
        let req = get_request("/path");
        assert!(req.query.is_none());
    }

    #[test]
    fn path_retains_percent_encoding() {
        let req = get_request("/data/%2F/item");
        assert_eq!(req.path, "/data/%2F/item");
    }

    #[test]
    fn scheme_and_authority_from_absolute_uri() {
        let uri: Uri = "https://example.com/path".parse().unwrap();
        let req =
            RequestEnvelope::try_from_parts(&Method::GET, &uri, &HeaderMap::new(), Bytes::new())
                .unwrap();
        assert_eq!(req.scheme.as_deref(), Some("https"));
        assert_eq!(req.authority.as_deref(), Some("example.com"));
    }

    #[test]
    fn authority_falls_back_to_host_header() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "api.example.com".parse().unwrap());
        let req = RequestEnvelope::try_from_parts(
            &Method::GET,
            &"/path".parse().unwrap(),
            &headers,
            Bytes::new(),
        )
        .unwrap();
        assert_eq!(req.authority.as_deref(), Some("api.example.com"));
    }

    #[test]
    fn repeated_headers_are_all_preserved() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", "a=1; Path=/".parse().unwrap());
        headers.append("set-cookie", "b=2; Path=/".parse().unwrap());
        let req = RequestEnvelope::try_from_parts(
            &Method::GET,
            &"/".parse().unwrap(),
            &headers,
            Bytes::new(),
        )
        .unwrap();
        let cookies: Vec<_> = req
            .headers
            .iter()
            .filter(|h| h.name == "set-cookie")
            .collect();
        assert_eq!(cookies.len(), 2);
    }

    #[test]
    fn empty_body_is_empty_bytes_not_absent() {
        let req = RequestEnvelope::try_from_parts(
            &Method::POST,
            &"/api".parse().unwrap(),
            &HeaderMap::new(),
            Bytes::new(),
        )
        .unwrap();
        assert_eq!(req.body, b"");
    }

    // --- ResponseEnvelope ---

    #[test]
    fn invalid_status_becomes_500() {
        for status in [0u16, 100, 101, 199, 600, 999] {
            let response = ResponseEnvelope::new(status, ResponseBody::Text(String::new()))
                .into_axum_response();
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "status {status} should be rejected"
            );
        }
    }

    #[test]
    fn valid_status_codes_are_forwarded() {
        for status in [200u16, 204, 205, 304, 400, 599] {
            let response = ResponseEnvelope::new(status, ResponseBody::Text(String::new()))
                .into_axum_response();
            assert_eq!(response.status().as_u16(), status, "status {status}");
        }
    }

    #[test]
    fn invalid_response_header_name_makes_whole_response_a_500() {
        // A header name with a space is invalid.
        let mut envelope = ResponseEnvelope::new(200, ResponseBody::Text("ok".into()));
        envelope.headers.push(Header {
            name: "bad name".to_owned(),
            value: b"value".to_vec(),
        });
        let response = envelope.into_axum_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn invalid_response_header_value_makes_whole_response_a_500() {
        let mut envelope = ResponseEnvelope::new(200, ResponseBody::Text("ok".into()));
        // CR in a header value is invalid.
        envelope.headers.push(Header {
            name: "x-test".to_owned(),
            value: b"bad\rvalue".to_vec(),
        });
        let response = envelope.into_axum_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn guest_cannot_set_framing_or_hop_by_hop_headers() {
        let envelope = ResponseEnvelope::new(200, ResponseBody::Text("hello".into()))
            .with_header("content-type", b"text/plain".to_vec())
            .with_header("content-length", b"999".to_vec())
            .with_header("transfer-encoding", b"chunked".to_vec())
            .with_header("connection", b"x-internal, keep-alive".to_vec())
            .with_header("x-internal", b"attacker-controlled".to_vec())
            .with_header("keep-alive", b"timeout=5".to_vec());
        let response = envelope.into_axum_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain"
        );
        for name in [
            "content-length",
            "transfer-encoding",
            "connection",
            "keep-alive",
            "x-internal",
        ] {
            assert!(!response.headers().contains_key(name), "{name} leaked");
        }
    }

    #[test]
    fn connection_header_matching_is_case_insensitive_and_strips_nominated_fields() {
        let envelope = ResponseEnvelope::new(200, ResponseBody::Text("ok".into()))
            .with_header("Connection", b"x-internal".to_vec())
            .with_header("X-Internal", b"secret".to_vec());
        let response = envelope.into_axum_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("x-internal"));
    }

    #[test]
    fn malformed_connection_header_rejects_the_response() {
        for value in [&b"x-internal,,keep-alive"[..], &[0xff][..]] {
            let envelope = ResponseEnvelope::new(200, ResponseBody::Text("ok".into()))
                .with_header("Connection", value.to_vec());
            let response = envelope.into_axum_response();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    #[test]
    fn multiple_set_cookie_headers_are_preserved() {
        let envelope = ResponseEnvelope::new(200, ResponseBody::Text(String::new()))
            .with_header("set-cookie", b"a=1; Path=/".to_vec())
            .with_header("set-cookie", b"b=2; Path=/".to_vec());
        let response = envelope.into_axum_response();
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
    }

    #[tokio::test]
    async fn no_content_reset_content_and_not_modified_responses_have_no_body() {
        for status in [204u16, 205, 304] {
            let response =
                ResponseEnvelope::new(status, ResponseBody::Text("must not be sent".into()))
                    .into_axum_response();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert!(body.is_empty(), "status {status} carried a body");
        }
    }

    #[tokio::test]
    async fn binary_body_is_emitted_byte_for_byte() {
        let raw = vec![0x00, 0x01, 0x7f, 0xff, 0xfe];
        let response =
            ResponseEnvelope::new(200, ResponseBody::Bytes(raw.clone())).into_axum_response();
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), raw.as_slice());
    }

    #[tokio::test]
    async fn text_body_is_utf8_encoded() {
        let response = ResponseEnvelope::new(200, ResponseBody::Text("こんにちは".into()))
            .into_axum_response();
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), "こんにちは".as_bytes());
    }

    #[test]
    fn error_envelope_does_not_expose_internal_details() {
        let error = EngineError::Internal("secret filesystem path: /srv/private/token".into());
        let envelope = ErrorEnvelope::from_engine_error(&error);
        assert_eq!(envelope.error, "Internal runtime error");
        assert_eq!(envelope.error_type, "InternalEngineError");
        let serialized = serde_json::to_string(&envelope).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("/srv/private/token"));
    }
}
