use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// HTTP/1 header reads are bounded at the transport layer, before Axum middleware
/// can observe a request. Application admission and execution limits are enforced
/// by the ingress router after the headers have been parsed.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HTTP1_HEADERS: usize = 64;
/// Maximum number of accepted HTTP connections with live connection tasks.
const MAX_ACTIVE_CONNECTIONS: usize = 128;

/// Acquire a connection slot without waiting, so the accept loop never spawns
/// more than `MAX_ACTIVE_CONNECTIONS` connection tasks.
fn try_acquire_connection_slot(slots: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    slots.clone().try_acquire_owned().ok()
}

/// Serve an Axum router with explicit transport-level connection and header-read protection.
pub async fn serve(listener: TcpListener, app: Router) -> anyhow::Result<()> {
    let connection_slots = Arc::new(Semaphore::new(MAX_ACTIVE_CONNECTIONS));
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .max_headers(MAX_HTTP1_HEADERS);

    loop {
        let (stream, peer) = listener.accept().await?;
        let Some(connection_permit) = try_acquire_connection_slot(&connection_slots) else {
            tracing::warn!(%peer, "HTTP connection limit reached; closing connection");
            drop(stream);
            continue;
        };
        let io = TokioIo::new(stream);
        let service = TowerToHyperService::new(app.clone());
        let connection_builder = builder.clone();

        tokio::spawn(async move {
            let _connection_permit = connection_permit;
            if let Err(error) = connection_builder.serve_connection(io, service).await {
                tracing::debug!(%peer, %error, "HTTP connection closed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
