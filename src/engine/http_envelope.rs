use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::error::EngineError;

/// Maximum serialized response envelope accepted from a guest handler.
pub const MAX_HANDLER_RESPONSE_BYTES: usize = 1024 * 1024;

/// Incoming HTTP request serialized for guest handler consumption
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestEnvelope {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

impl RequestEnvelope {
    /// Constructs a UTF-8 request envelope from raw HTTP request components.
    ///
    /// The current guest ABI is text-based. Binary bodies are rejected instead
    /// of being silently corrupted through lossy UTF-8 conversion.
    pub fn try_from_parts(
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        body_bytes: Bytes,
    ) -> Result<Self, std::string::FromUtf8Error> {
        let query_map = uri
            .query()
            .map(|query| {
                form_urlencoded::parse(query.as_bytes())
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect()
            })
            .unwrap_or_default();

        let mut header_map = HashMap::new();
        for (name, value) in headers {
            if let Ok(value) = value.to_str() {
                header_map.insert(name.as_str().to_lowercase(), value.to_string());
            }
        }

        let body = if body_bytes.is_empty() {
            None
        } else {
            Some(String::from_utf8(body_bytes.to_vec())?)
        };

        Ok(Self {
            method: method.as_str().to_string(),
            path: uri.path().to_string(),
            query: query_map,
            headers: header_map,
            body,
        })
    }
}

/// Outgoing HTTP response produced by guest handler
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseEnvelope {
    pub status: u16,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

impl Default for ResponseEnvelope {
    fn default() -> Self {
        Self {
            status: 200,
            headers: HashMap::new(),
            body: None,
        }
    }
}

impl ResponseEnvelope {
    pub fn new(status: u16, body: Option<String>) -> Self {
        Self {
            status,
            headers: HashMap::new(),
            body,
        }
    }

    pub fn with_header(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.headers.insert(key.into(), val.into());
        self
    }

    /// Converts an untrusted guest response into a safe host-owned HTTP response.
    ///
    /// Guests may choose an ordinary final status and end-to-end headers, but may not
    /// control message framing or hop-by-hop connection behavior. Invalid status values
    /// become 500; malformed header names and values are discarded.
    pub fn into_axum_response(self) -> Response {
        let status = if (200..=599).contains(&self.status) {
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };

        let mut builder = Response::builder().status(status);
        let mut connection_tokens = HashSet::new();
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("connection") {
                connection_tokens.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|token| !token.is_empty())
                        .map(str::to_ascii_lowercase),
                );
            }
        }

        for (key, value) in self.headers {
            let normalized = key.to_ascii_lowercase();
            if is_forbidden_guest_header(&normalized) || connection_tokens.contains(&normalized) {
                continue;
            }

            if let (Ok(header_name), Ok(header_value)) = (
                HeaderName::try_from(key.as_str()),
                HeaderValue::try_from(value.as_str()),
            ) {
                builder = builder.header(header_name, header_value);
            }
        }

        // 204 and 304 responses never carry a message body. In particular, do not
        // let guest-supplied content sneak through on a bodyless status.
        let body_content = if status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED {
            String::new()
        } else {
            self.body.unwrap_or_default()
        };

        builder
            .body(axum::body::Body::from(body_content))
            .unwrap_or_else(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to construct response",
                )
                    .into_response()
            })
    }
}

/// Headers that can alter HTTP message framing or connection management are owned
/// by the host/server and must never be copied from an untrusted guest envelope.
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

/// Error response structure when Wasm guest traps, runs out of fuel, or panics
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
            EngineError::Trap(crate::error::SandboxTrap::MemoryExhausted(_)) => {
                (500, "MemoryExhausted")
            }
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

        // Engine errors can contain guest-controlled strings, filesystem paths, provider
        // details, or other internal diagnostics. Keep those in server logs only.
        let error = match err_type {
            "FuelExhausted" => "Handler execution exceeded its resource budget",
            "MemoryExhausted" => "Handler exceeded its memory limit",
            "SandboxTrap" => "Handler execution failed",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parameters_are_percent_decoded() {
        let request = RequestEnvelope::try_from_parts(
            &Method::GET,
            &"/search?q=hello%20world&plus=a%2Bb".parse().unwrap(),
            &HeaderMap::new(),
            Bytes::new(),
        )
        .unwrap();

        assert_eq!(
            request.query.get("q").map(String::as_str),
            Some("hello world")
        );
        assert_eq!(request.query.get("plus").map(String::as_str), Some("a+b"));
    }

    #[test]
    fn internal_error_details_are_not_exposed_to_http_clients() {
        let error = EngineError::Internal("secret filesystem path: /srv/private/token".into());
        let envelope = ErrorEnvelope::from_engine_error(&error);

        assert_eq!(envelope.error, "Internal runtime error");
        assert_eq!(envelope.error_type, "InternalEngineError");
        let serialized = serde_json::to_string(&envelope).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("/srv/private/token"));
    }

    #[test]
    fn guest_panic_details_are_not_exposed_to_http_clients() {
        let error = EngineError::Trap(crate::error::SandboxTrap::GuestPanic(
            "sensitive guest detail".into(),
        ));
        let envelope = ErrorEnvelope::from_engine_error(&error);

        assert_eq!(envelope.error, "Handler execution failed");
        assert!(!serde_json::to_string(&envelope)
            .unwrap()
            .contains("sensitive guest detail"));
    }

    #[test]
    fn invalid_utf8_request_body_is_rejected() {
        let error = RequestEnvelope::try_from_parts(
            &Method::POST,
            &"/submit".parse().unwrap(),
            &HeaderMap::new(),
            Bytes::from_static(&[0xff, 0xfe]),
        )
        .unwrap_err();

        assert!(error.utf8_error().error_len().is_some());
    }

    #[test]
    fn guest_cannot_set_framing_or_hop_by_hop_headers() {
        let response = ResponseEnvelope::new(200, Some("hello".into()))
            .with_header("content-type", "text/plain")
            .with_header("content-length", "999")
            .with_header("transfer-encoding", "chunked")
            .with_header("connection", "x-internal, keep-alive")
            .with_header("x-internal", "attacker-controlled")
            .with_header("keep-alive", "timeout=5")
            .into_axum_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("content-type").unwrap(), "text/plain");
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
    fn guest_status_must_be_a_final_response_status() {
        for status in [0, 100, 101, 199, 600, 999] {
            let response = ResponseEnvelope::new(status, Some("body".into())).into_axum_response();
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "status {status} should be rejected"
            );
        }
        for status in [200, 204, 304, 400, 599] {
            let response = ResponseEnvelope::new(status, None).into_axum_response();
            assert_eq!(response.status().as_u16(), status);
        }
    }

    #[tokio::test]
    async fn no_content_and_not_modified_responses_have_no_body() {
        for status in [204, 304] {
            let response =
                ResponseEnvelope::new(status, Some("must not be sent".into())).into_axum_response();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert!(body.is_empty(), "status {status} carried a body");
        }
    }
}
