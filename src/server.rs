use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;

/// HTTP/1 header reads are bounded at the transport layer, before Axum middleware
/// can observe a request. Application admission and execution limits are enforced
/// by the ingress router after the headers have been parsed.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HTTP1_HEADERS: usize = 64;

/// Serve an Axum router with explicit transport-level header-read protection.
pub async fn serve(listener: TcpListener, app: Router) -> anyhow::Result<()> {
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .max_headers(MAX_HTTP1_HEADERS);

    loop {
        let (stream, peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let service = TowerToHyperService::new(app.clone());
        let connection_builder = builder.clone();

        tokio::spawn(async move {
            if let Err(error) = connection_builder.serve_connection(io, service).await {
                tracing::debug!(%peer, %error, "HTTP connection closed");
            }
        });
    }
}
