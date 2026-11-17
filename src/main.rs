pub mod engine;
pub mod error;
pub mod ingress;
pub mod spec;
pub mod synthesizer;
pub mod verifier;

use clap::Parser;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "nosrc",
    about = "Specification-driven WebAssembly runtime prototype"
)]
pub struct Cli {
    #[arg(short, long, default_value = "./SPEC.md")]
    pub spec: std::path::PathBuf,

    #[arg(short, long, default_value = "embedded", value_parser = ["embedded", "external"])]
    pub state: String,

    #[arg(short, long, default_value = "8080")]
    pub port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let spec_text = tokio::fs::read_to_string(&cli.spec)
        .await
        .map_err(|error| anyhow::anyhow!("Failed to read SPEC at {}: {error}", cli.spec.display()))?;
    let spec = spec::ServiceSpec::parse(&spec_text)
        .map_err(|error| anyhow::anyhow!("Failed to parse SPEC at {}: {error}", cli.spec.display()))?;

    tracing::info!(
        service = %spec.name,
        endpoints = spec.endpoints.len(),
        spec = %cli.spec.display(),
        state_mode = %cli.state,
        "Loaded service specification"
    );

    let engine = Arc::new(engine::WasmEngine::new()?);
    let state = Arc::new(ingress::IngressState::new(engine));
    let app = ingress::build_ingress_router(state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cli.port)).await?;

    tracing::info!(
        address = %listener.local_addr()?,
        "nosrc is listening; no handler generation is active until a candidate is compiled and verified"
    );

    axum::serve(listener, app).await?;
    Ok(())
}
