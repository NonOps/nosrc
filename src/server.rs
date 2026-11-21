use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

/// HTTP/1 header reads are bounded at the transport layer, before Axum middleware
/// can observe a request. Application admission and execution limits are enforced
/// by the ingress router after the headers have been parsed.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HTTP1_HEADERS: usize = 64;
/// Maximum number of accepted HTTP connections with live connection tasks.
const MAX_ACTIVE_CONNECTIONS: usize = 128;
/// Brief delay before retrying a transient accept error.
const ACCEPT_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Acquire a connection slot without waiting, so the accept loop never spawns
/// more than `MAX_ACTIVE_CONNECTIONS` connection tasks.
fn try_acquire_connection_slot(slots: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    slots.clone().try_acquire_owned().ok()
}

fn is_retryable_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::ConnectionAborted
    ) || matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ECONNABORTED)
    )
}

/// Retry transient accept failures without taking down the listener. Kept generic
/// so the retry policy can be tested with deterministic injected accept results.
async fn accept_with_retry<T, F, Fut>(mut accept: F) -> io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    loop {
        match accept().await {
            Ok(value) => return Ok(value),
            Err(error) if is_retryable_accept_error(&error) => {
                tracing::warn!(%error, "Transient HTTP accept failure; retrying");
                tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Wait for an OS shutdown signal. On Unix both SIGINT and SIGTERM are handled;
/// on other platforms Ctrl-C is the supported signal.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(error) = result {
                            tracing::warn!(%error, "Failed to listen for Ctrl-C");
                        }
                    }
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "Failed to register SIGTERM handler; waiting for Ctrl-C");
                if let Err(error) = tokio::signal::ctrl_c().await {
                    tracing::warn!(%error, "Failed to listen for Ctrl-C");
                }
            }
        }
    }

    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "Failed to listen for Ctrl-C");
    }

    tracing::info!("Shutdown signal received; stopping HTTP ingress");
}

/// Serve an Axum router, stop accepting on SIGINT/SIGTERM, and drain active
/// connections for at most `drain_timeout`.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    drain_timeout: Duration,
) -> anyhow::Result<()> {
    serve_with_shutdown(listener, app, shutdown_signal(), drain_timeout).await
}

/// Testable server loop with an injected shutdown future.
pub async fn serve_with_shutdown<F>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
    drain_timeout: Duration,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send,
{
    let connection_slots = Arc::new(Semaphore::new(MAX_ACTIVE_CONNECTIONS));
    let mut connections = JoinSet::new();
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .max_headers(MAX_HTTP1_HEADERS);
    tokio::pin!(shutdown);

    loop {
        let accepted = tokio::select! {
            _ = &mut shutdown => break,
            result = accept_with_retry(|| listener.accept()) => result?,
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "HTTP connection task failed");
                }
                continue;
            }
        };

        let (stream, peer) = accepted;
        let Some(connection_permit) = try_acquire_connection_slot(&connection_slots) else {
            tracing::warn!(%peer, "HTTP connection limit reached; closing connection");
            drop(stream);
            continue;
        };
        let io = TokioIo::new(stream);
        let service = TowerToHyperService::new(app.clone());
        let connection_builder = builder.clone();

        connections.spawn(async move {
            let _connection_permit = connection_permit;
            if let Err(error) = connection_builder.serve_connection(io, service).await {
                tracing::debug!(%peer, %error, "HTTP connection closed");
            }
        });
    }

    // Drop the listener before draining so no new connections can be accepted.
    drop(listener);
    tracing::info!(
        active_connections = connections.len(),
        drain_timeout_secs = drain_timeout.as_secs(),
        "HTTP listener stopped; draining active connections"
    );

    let drain = async {
        while let Some(result) = connections.join_next().await {
            if let Err(error) = result {
                tracing::warn!(%error, "HTTP connection task failed during shutdown");
            }
        }
    };

    if tokio::time::timeout(drain_timeout, drain).await.is_err() {
        tracing::warn!(
            active_connections = connections.len(),
            "HTTP connection drain deadline reached; aborting remaining connections"
        );
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }

    tracing::info!("HTTP ingress shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::get;
    use axum::Router;
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{oneshot, Notify};

    #[test]
    fn connection_slots_enforce_the_configured_limit() {
        let slots = Arc::new(Semaphore::new(MAX_ACTIVE_CONNECTIONS));
        let permits: Vec<_> = (0..MAX_ACTIVE_CONNECTIONS)
            .map(|_| try_acquire_connection_slot(&slots).expect("slot should be available"))
            .collect();

        assert!(try_acquire_connection_slot(&slots).is_none());
        drop(permits);
        assert_eq!(slots.available_permits(), MAX_ACTIVE_CONNECTIONS);
    }

    #[tokio::test]
    async fn injected_transient_accept_failures_are_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut results = VecDeque::from([
            Err(io::Error::from_raw_os_error(libc::EMFILE)),
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, "aborted")),
            Ok(42_u8),
        ]);
        let observed_attempts = attempts.clone();

        let accepted = accept_with_retry(|| {
            observed_attempts.fetch_add(1, Ordering::SeqCst);
            std::future::ready(results.pop_front().expect("injected result"))
        })
        .await
        .unwrap();

        assert_eq!(accepted, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_transient_accept_failure_is_returned() {
        let accepted = accept_with_retry(|| {
            std::future::ready(Err::<(), _>(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "listener is unusable",
            )))
        })
        .await;

        assert_eq!(
            accepted.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    async fn slow_response(State(started): State<Arc<Notify>>) -> &'static str {
        started.notify_one();
        tokio::time::sleep(Duration::from_millis(100)).await;
        "finished"
    }

    #[tokio::test]
    async fn graceful_shutdown_drains_an_in_flight_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let started = Arc::new(Notify::new());
        let app = Router::new()
            .route("/", get(slow_response))
            .with_state(started.clone());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let server = tokio::spawn(serve_with_shutdown(
            listener,
            app,
            async move {
                let _ = shutdown_rx.await;
            },
            Duration::from_secs(1),
        ));

        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        started.notified().await;
        shutdown_tx.send(()).unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).contains("finished"));
        server.await.unwrap().unwrap();
    }
}
