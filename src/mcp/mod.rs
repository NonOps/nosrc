pub mod lifecycle;
pub mod policy;
mod tools;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::artifact_cache::ArtifactCache;

use rmcp::{transport::io::stdio, ServiceExt};
use tokio::sync::Mutex;

use crate::spec::ServiceSpec;
use lifecycle::AppLifecycle;
use tools::NosrcMcpServer;

/// Run the MCP server over stdio. stdout is owned exclusively by the protocol transport.
pub async fn serve(
    spec: Option<(ServiceSpec, String)>,
    cache_dir: PathBuf,
    no_cache: bool,
    export_root: PathBuf,
    http_listener: Option<(IpAddr, u16)>,
    shutdown_timeout: Duration,
) -> anyhow::Result<()> {
    let cache = if no_cache {
        None
    } else {
        Some(ArtifactCache::new(cache_dir))
    };
    let lifecycle = match spec {
        Some((spec, spec_text)) => AppLifecycle::new_with_spec_text(&spec, spec_text, cache)?,
        None => AppLifecycle::new_mcp_first(cache)?,
    };
    let ingress_state = lifecycle.ingress_state();
    let http_task = if let Some((bind, port)) = http_listener {
        let listener = tokio::net::TcpListener::bind((bind, port)).await?;
        let address = listener.local_addr()?;
        let app = crate::ingress::build_ingress_router(ingress_state);
        tracing::info!(%address, "nosrc MCP and HTTP interfaces share one application lifecycle");
        Some(tokio::spawn(async move {
            crate::server::serve(listener, app, shutdown_timeout).await
        }))
    } else {
        None
    };

    let server = NosrcMcpServer {
        lifecycle: Arc::new(Mutex::new(lifecycle)),
        export_root,
    };
    let service = server.serve(stdio()).await?;

    if let Some(mut http_task) = http_task {
        // A shutdown signal received by shared HTTP ingress ends the whole MCP
        // process as well. If the stdio client disconnects first, stop HTTP.
        tokio::select! {
            result = service.waiting() => {
                result?;
                http_task.abort();
                let _ = http_task.await;
            }
            result = &mut http_task => {
                result.map_err(|error| anyhow::anyhow!("Shared HTTP task failed: {error}"))??;
            }
        }
    } else {
        service.waiting().await?;
    }

    Ok(())
}
