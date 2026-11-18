pub mod lifecycle;
pub mod policy;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use crate::artifact_cache::ArtifactCache;

use rmcp::{transport::io::stdio, ServiceExt};
use tokio::sync::Mutex;

use crate::spec::ServiceSpec;
use lifecycle::AppLifecycle;
use tools::NosrcMcpServer;

/// Run the MCP server over stdio. stdout is owned exclusively by the protocol transport.
pub async fn serve(spec: ServiceSpec, cache_dir: PathBuf, no_cache: bool) -> anyhow::Result<()> {
    let cache = if no_cache {
        None
    } else {
        Some(ArtifactCache::new(cache_dir))
    };
    let lifecycle = AppLifecycle::new(&spec, cache)?;
    let server = NosrcMcpServer {
        lifecycle: Arc::new(Mutex::new(lifecycle)),
    };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
