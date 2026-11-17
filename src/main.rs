pub mod engine;
pub mod error;
pub mod ingress;
pub mod spec;
pub mod synthesizer;
pub mod verifier;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "nosrc",
    about = "Zero-Codebase, Spec-to-Wasm Autonomous Cluster Runtime"
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

    tracing::info!(
        spec = %cli.spec.display(),
        state = %cli.state,
        port = %cli.port,
        "Starting nosrc runtime"
    );

    let _engine = engine::WasmEngine::new()?;
    tracing::info!("Wasm execution engine initialized with CPU fuel metering");

    Ok(())
}
