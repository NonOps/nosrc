use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::EngineError;

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
    /// Constructs a `RequestEnvelope` from raw incoming HTTP request components
    pub fn from_parts(method: &Method, uri: &Uri, headers: &HeaderMap, body_bytes: Bytes) -> Self {
        let mut query_map = HashMap::new();
        if let Some(query_str) = uri.query() {
            for pair in query_str.split('&') {
                let mut parts = pair.splitn(2, '=');
                if let Some(key) = parts.next() {
                    let val = parts.next().unwrap_or("");
                    query_map.insert(key.to_string(), val.to_string());
                }
            }
        }

        let mut header_map = HashMap::new();
        for (name, val) in headers.iter() {
            if let Ok(v_str) = val.to_str() {
                header_map.insert(name.as_str().to_lowercase(), v_str.to_string());
            }
        }

        let body = if body_bytes.is_empty() {
            None
        } else {
            match String::from_utf8(body_bytes.to_vec()) {
                Ok(s) => Some(s),
                Err(_) => {
                    // Base64 or fallback representation if not valid UTF-8
                    Some(String::from_utf8_lossy(&body_bytes).to_string())
                }
            }
        };

        Self {
            method: method.as_str().to_string(),
            path: uri.path().to_string(),
            query: query_map,
            headers: header_map,
            body,
        }
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

    /// Converts the `ResponseEnvelope` into an Axum HTTP `Response`
    pub fn into_axum_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        let mut builder = Response::builder().status(status);

        for (k, v) in self.headers {
            if let (Ok(h_name), Ok(h_val)) = (
                HeaderName::try_from(k.as_str()),
                HeaderValue::try_from(v.as_str()),
            ) {
                builder = builder.header(h_name, h_val);
            }
        }

        let body_content = self.body.unwrap_or_default();
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

        Self {
            error: err.to_string(),
            error_type: err_type.to_string(),
            status,
        }
    }

    pub fn into_axum_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
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
