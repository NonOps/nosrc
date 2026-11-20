pub mod lifecycle;
pub mod policy;
mod tools;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

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
    if let Some((bind, port)) = http_listener {
        let listener = tokio::net::TcpListener::bind((bind, port)).await?;
        let address = listener.local_addr()?;
        let app = crate::ingress::build_ingress_router(ingress_state);
        tracing::info!(%address, "nosrc MCP and HTTP interfaces share one application lifecycle");
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, "Shared HTTP ingress stopped");
            }
        });
    }
    let server = NosrcMcpServer {
        lifecycle: Arc::new(Mutex::new(lifecycle)),
        export_root,
    };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
