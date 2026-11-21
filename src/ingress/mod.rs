use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, OriginalUri, State};
use axum::http::{header::CONTENT_LENGTH, HeaderMap, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use crate::engine::http_envelope::{ErrorEnvelope, RequestEnvelope};
use crate::engine::quickjs::{QuickJsHandler, DEFAULT_GUEST_EXECUTION_TIMEOUT};
use crate::error::EngineError;

const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const MAX_ADMITTED_REQUESTS: usize = 32;
const EXECUTION_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const EXECUTION_TASK_JOIN_GRACE: Duration = Duration::from_secs(6);
const INGRESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// Active generation representing a compiled and contract-verified JavaScript handler.
pub struct ActiveGeneration {
    pub generation_id: u64,
    pub module_hash: String,
    pub handler: Arc<Mutex<QuickJsHandler>>,
}

/// Shared ingress state holding the active handler generation and bounded admission gates.
pub struct IngressState {
    pub active_generation: ArcSwap<Option<ActiveGeneration>>,
    admission: Arc<Semaphore>,
    execution: Arc<Semaphore>,
    execution_timeout: Duration,
}

impl IngressState {
    pub fn new() -> Self {
        Self {
            active_generation: ArcSwap::from_pointee(None),
            admission: Arc::new(Semaphore::new(MAX_ADMITTED_REQUESTS)),
            execution: Arc::new(Semaphore::new(1)),
            execution_timeout: DEFAULT_GUEST_EXECUTION_TIMEOUT,
        }
    }

    /// Atomically activates a candidate only after compilation and verification succeed.
    pub fn hot_swap(&self, new_generation: ActiveGeneration) {
        tracing::info!(
            generation = new_generation.generation_id,
            hash = %new_generation.module_hash,
            "Activating verified JavaScript handler generation"
        );
        self.active_generation.store(Arc::new(Some(new_generation)));
    }
}

impl Default for IngressState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize)]
struct RuntimeStatus {
    status: &'static str,
    active_generation: Option<u64>,
    module_hash: Option<String>,
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn runtime_status(State(state): State<Arc<IngressState>>) -> Json<RuntimeStatus> {
    let active = state.active_generation.load();
    let (active_generation, module_hash) = match active.as_ref() {
        Some(generation) => (
            Some(generation.generation_id),
            Some(generation.module_hash.clone()),
        ),
        None => (None, None),
    };

    Json(RuntimeStatus {
        status: "running",
        active_generation,
        module_hash,
    })
}

/// Bounds requests before body extraction. Health and status probes bypass admission
/// and the application request deadline so overload remains observable.
async fn admission_control(
    State(state): State<Arc<IngressState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let is_head = request.method() == Method::HEAD;
    let path = request.uri().path();
    if path == "/healthz" || path == "/_nosrc/status" {
        return strip_head_body(is_head, next.run(request).await);
    }

    let permit = match state.admission.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::warn!("HTTP ingress admission limit reached");
            return strip_head_body(
                is_head,
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("retry-after", "1")],
                    "HTTP ingress is busy; retry shortly",
                )
                    .into_response(),
            );
        }
    };

    let response = match tokio::time::timeout(INGRESS_REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!("HTTP request exceeded the ingress deadline");
            (
                StatusCode::REQUEST_TIMEOUT,
                [("retry-after", "1")],
                "HTTP request deadline exceeded",
            )
                .into_response()
        }
    };
    drop(permit);
    strip_head_body(is_head, response)
}

fn strip_head_body(is_head: bool, mut response: Response) -> Response {
    if is_head {
        *response.body_mut() = Body::empty();
    }
    response
}

/// Logs safe request metadata without recording query strings, headers, or bodies.
async fn log_http_request(request: Request<Body>, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let request_bytes = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let started = Instant::now();
    let response = next.run(request).await;
    let response_bytes = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    tracing::debug!(
        %method,
        %path,
        request_bytes = ?request_bytes,
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis(),
        response_bytes = ?response_bytes,
        "HTTP request completed"
    );
    response
}

/// Creates the Axum router for health, status, and application ingress.
pub fn build_ingress_router(state: Arc<IngressState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/_nosrc/status", get(runtime_status))
        .fallback(any(handle_http_request))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn(log_http_request))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admission_control,
        ))
        .with_state(state)
}

async fn handle_http_request(
    State(state): State<Arc<IngressState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let generation = state.active_generation.load();
    let Some(active) = generation.as_ref() else {
        return ErrorEnvelope {
            error: "No active verified JavaScript handler generation".to_string(),
            error_type: "NoActiveGeneration".to_string(),
            status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        }
        .into_axum_response();
    };

    let envelope = match RequestEnvelope::try_from_parts(&method, &uri, &headers, body) {
        Ok(envelope) => envelope,
        Err(_) => {
            return ErrorEnvelope {
                error: "Request body must be valid UTF-8 for this handler ABI".to_string(),
                error_type: "InvalidRequestBody".to_string(),
                status: StatusCode::BAD_REQUEST.as_u16(),
            }
            .into_axum_response();
        }
    };

    let execution_permit = match tokio::time::timeout(
        EXECUTION_QUEUE_TIMEOUT,
        state.execution.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            return ErrorEnvelope {
                error: "Handler execution is unavailable".to_string(),
                error_type: "ExecutionUnavailable".to_string(),
                status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
            }
            .into_axum_response();
        }
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "1")],
                "Handler is busy; execution queue wait timed out",
            )
                .into_response();
        }
    };

    let handler = active.handler.clone();
    let execution_timeout = state.execution_timeout;
    let mut execution = tokio::task::spawn_blocking(move || {
        // Keep the permit inside the blocking task until the epoch trap is handled and
        // bounded instance recovery completes, even if the HTTP future is cancelled.
        let _execution_permit = execution_permit;
        let mut handler = handler
            .lock()
            .map_err(|_| EngineError::Internal("Handler execution lock was poisoned".into()))?;
        handler.invoke_with_timeout(&envelope, execution_timeout)
    });

    // Wasmtime epoch checks run on a periodic ticker. Allow time beyond the guest
    // deadline for the trap to unwind and bounded instance recovery to complete.
    match tokio::time::timeout(
        execution_timeout + EXECUTION_TASK_JOIN_GRACE,
        &mut execution,
    )
    .await
    {
        Err(_) => {
            tracing::error!("Handler execution exceeded the HTTP response deadline");
            ErrorEnvelope {
                error: "Handler execution exceeded the response deadline".to_string(),
                error_type: "ExecutionTimeout".to_string(),
                status: StatusCode::GATEWAY_TIMEOUT.as_u16(),
            }
            .into_axum_response()
        }
        Ok(Ok(Ok(response))) => response.into_axum_response(),
        Ok(Ok(Err(error))) => {
            tracing::warn!(error = %error, "JavaScript handler execution failed");
            ErrorEnvelope::from_engine_error(&error).into_axum_response()
        }
        Ok(Err(error)) => {
            tracing::error!(error = %error, "Handler execution task failed");
            ErrorEnvelope::from_engine_error(&EngineError::Internal(error.to_string()))
                .into_axum_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::quickjs::{test_support, DEFAULT_COMPONENT_FUEL};
    use axum::body::Body;
    use axum::http::Request;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_endpoint_reports_process_health() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_endpoint_reports_no_active_generation_at_startup() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .uri("/_nosrc/status")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["status"], "running");
        assert!(status["active_generation"].is_null());
        assert!(status["module_hash"].is_null());
    }

    #[tokio::test]
    async fn oversized_request_body_is_rejected_before_handler_dispatch() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(Body::from(vec![120; MAX_REQUEST_BODY_BYTES + 1]))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn saturated_ingress_rejects_application_requests_but_keeps_health_available() {
        let state = Arc::new(IngressState::new());
        let permits: Vec<_> = (0..MAX_ADMITTED_REQUESTS)
            .map(|_| state.admission.clone().try_acquire_owned().unwrap())
            .collect();
        let app = build_ingress_router(state);

        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..1_000 {
            let app = app.clone();
            requests.spawn(async move {
                let request = Request::builder().uri("/app").body(Body::empty()).unwrap();
                app.oneshot(request).await.unwrap().status()
            });
        }
        // Probe health while the overload burst is still outstanding.
        let health = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), app.clone().oneshot(health))
                .await
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::OK
        );

        while let Some(result) = requests.join_next().await {
            assert_eq!(result.unwrap(), StatusCode::SERVICE_UNAVAILABLE);
        }
        drop(permits);
    }

    #[tokio::test]
    async fn head_responses_without_an_active_generation_have_no_body() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .method(Method::HEAD)
            .uri("/hello")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn application_routes_return_503_until_a_generation_is_active() {
        let app = build_ingress_router(Arc::new(IngressState::new()));
        let request = Request::builder()
            .uri("/hello")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn epoch_timeout_releases_execution_permit_for_a_queued_request() {
        let (engine, compiled) = test_support::compiled().await;
        let handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let mut ingress_state = IngressState::new();
        ingress_state.execution_timeout = Duration::from_millis(10);
        let state = Arc::new(ingress_state);
        state.hot_swap(ActiveGeneration {
            generation_id: 1,
            module_hash: "epoch-timeout-test".to_string(),
            handler: Arc::new(Mutex::new(handler)),
        });
        let app = build_ingress_router(state.clone());

        let loop_request = Request::builder().uri("/loop").body(Body::empty()).unwrap();
        let loop_app = app.clone();
        let loop_task = tokio::spawn(async move { loop_app.oneshot(loop_request).await.unwrap() });

        // Wait until the first request owns the serialized execution permit before
        // queuing a second request behind it.
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.execution.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first request should acquire the execution permit");
        let health_request = Request::builder().uri("/health").body(Body::empty()).unwrap();
        let health_app = app.clone();
        let health_task =
            tokio::spawn(async move { health_app.oneshot(health_request).await.unwrap() });

        let loop_response = tokio::time::timeout(Duration::from_secs(2), loop_task)
            .await
            .expect("epoch deadline should stop the infinite guest loop")
            .unwrap();
        assert_eq!(loop_response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            loop_response.headers().get("x-nosrc-error").unwrap(),
            "ExecutionTimeout"
        );

        let health_response = tokio::time::timeout(Duration::from_secs(2), health_task)
            .await
            .expect("queued request should acquire the released execution permit")
            .unwrap();
        assert_eq!(health_response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(health_response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"healthy");
    }

    #[tokio::test]
    async fn cancelled_http_request_does_not_leave_guest_execution_running() {
        let (engine, compiled) = test_support::compiled().await;
        let handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let mut ingress_state = IngressState::new();
        ingress_state.execution_timeout = Duration::from_millis(10);
        let state = Arc::new(ingress_state);
        state.hot_swap(ActiveGeneration {
            generation_id: 1,
            module_hash: "cancelled-epoch-test".to_string(),
            handler: Arc::new(Mutex::new(handler)),
        });
        let app = build_ingress_router(state.clone());

        let request = Request::builder().uri("/loop").body(Body::empty()).unwrap();
        let request_task = tokio::spawn(app.oneshot(request));
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.execution.available_permits() != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("request should acquire the execution permit");

        // Dropping the HTTP future cannot cancel spawn_blocking. The Wasmtime
        // epoch deadline must still stop the guest and release its owned permit.
        request_task.abort();
        let released = tokio::time::timeout(Duration::from_secs(2), async {
            while state.execution.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;

        assert!(
            released.is_ok(),
            "cancelled request left its blocking guest execution holding the permit"
        );
    }

    #[tokio::test]
    async fn graceful_shutdown_drains_epoch_interrupted_guest_execution() {
        let (engine, compiled) = test_support::compiled().await;
        let handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let mut ingress_state = IngressState::new();
        ingress_state.execution_timeout = Duration::from_millis(10);
        let state = Arc::new(ingress_state);
        state.hot_swap(ActiveGeneration {
            generation_id: 1,
            module_hash: "shutdown-epoch-test".to_string(),
            handler: Arc::new(Mutex::new(handler)),
        });
        let app = build_ingress_router(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(crate::server::serve_with_shutdown(
            listener,
            app,
            async move {
                let _ = shutdown_rx.await;
            },
            Duration::from_secs(2),
        ));

        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /loop HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while state.execution.available_permits() != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("guest request should enter execution before shutdown");

        shutdown_tx.send(()).unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .expect("epoch interruption should let the in-flight response drain")
            .unwrap();
        let response = String::from_utf8(response).unwrap().to_ascii_lowercase();
        assert!(response.starts_with("http/1.1 504 gateway timeout"));
        assert!(response.contains("x-nosrc-error: executiontimeout"));

        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server should finish draining the interrupted request")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn ingress_dispatches_requests_to_the_active_quickjs_handler_and_strips_head_body() {
        let (engine, compiled) = test_support::compiled().await;
        let handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let state = Arc::new(IngressState::new());
        state.hot_swap(ActiveGeneration {
            generation_id: 1,
            module_hash: "test-component".to_string(),
            handler: Arc::new(Mutex::new(handler)),
        });

        let app = build_ingress_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/hello")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"GET /hello");

        let head = Request::builder()
            .method("HEAD")
            .uri("/hello")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(head).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    async fn app_with_active_handler() -> Router {
        let (engine, compiled) = test_support::compiled().await;
        let handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();
        let state = Arc::new(IngressState::new());
        state.hot_swap(ActiveGeneration {
            generation_id: 7,
            module_hash: "test-component".to_string(),
            handler: Arc::new(Mutex::new(handler)),
        });
        build_ingress_router(state)
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn reserved_routes_are_not_shadowed_by_the_application_handler() {
        let app = app_with_active_handler().await;

        // The handler would answer every path, but the runtime owns these two routes.
        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz?probe=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert!(body_text(health).await.is_empty());

        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/_nosrc/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let status: serde_json::Value = serde_json::from_str(&body_text(status).await).unwrap();
        assert_eq!(status["status"], "running");
        assert_eq!(status["active_generation"], 7);
        assert_eq!(status["module_hash"], "test-component");

        // Near-miss paths belong to the application, not the runtime.
        for path in ["/healthz/", "/healthz2", "/_nosrc/status/", "/_nosrc"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(body_text(response).await, format!("GET {path}"), "{path}");
        }
    }

    #[tokio::test]
    async fn concurrent_requests_are_serialized_without_lost_updates() {
        let app = app_with_active_handler().await;
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let app = app.clone();
            requests.spawn(async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .uri("/counter")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                body_text(response).await.parse::<u32>().unwrap()
            });
        }

        let mut seen = Vec::new();
        while let Some(result) = requests.join_next().await {
            seen.push(result.unwrap());
        }
        seen.sort_unstable();
        // The module-level counter starts at zero in this fresh instance, so serialized
        // execution must hand out each value exactly once.
        let first = seen[0];
        let expected: Vec<u32> = (first..first + 16).collect();
        assert_eq!(seen, expected);
    }

    #[tokio::test]
    async fn non_utf8_header_values_are_dropped_instead_of_failing_the_request() {
        let app = app_with_active_handler().await;
        let request = Request::builder()
            .uri("/echo?q=1")
            .header("x-good", "ok")
            .header(
                "x-binary",
                axum::http::HeaderValue::from_bytes(&[0xff, 0xfe, 0xfd]).unwrap(),
            )
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let echoed: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(echoed["headers"]["x-good"], "ok");
        assert!(echoed["headers"].get("x-binary").is_none());
        assert_eq!(echoed["query"]["q"], "1");
    }

    #[tokio::test]
    async fn invalid_utf8_request_bodies_are_rejected_with_a_client_error() {
        let app = app_with_active_handler().await;
        let request = Request::builder()
            .method("POST")
            .uri("/echo")
            .body(Body::from(vec![0xff, 0xfe, 0x00]))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-nosrc-error"], "InvalidRequestBody");
    }
}
