pub mod engine;
pub mod error;
pub mod ingress;
pub mod spec;
pub mod synthesizer;
pub mod verifier;

use clap::Parser;
use std::sync::{Arc, Mutex};

use engine::quickjs::{QuickJsEngine, DEFAULT_COMPONENT_FUEL};
use ingress::{ActiveGeneration, IngressState};
use synthesizer::{LlmConfig, Synthesizer};
use verifier::ShadowVerifier;

#[derive(Parser, Debug)]
#[command(
    name = "nosrc",
    about = "Specification-driven JavaScript service runtime"
)]
pub struct Cli {
    #[arg(short, long, default_value = "./SPEC.md")]
    pub spec: std::path::PathBuf,

    #[arg(short, long, default_value = "8080")]
    pub port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let spec_text = tokio::fs::read_to_string(&cli.spec)
        .await
        .map_err(|error| {
            anyhow::anyhow!("Failed to read SPEC at {}: {error}", cli.spec.display())
        })?;
    let spec = spec::ServiceSpec::parse(&spec_text).map_err(|error| {
        anyhow::anyhow!("Failed to parse SPEC at {}: {error}", cli.spec.display())
    })?;

    if spec.capabilities.state != "volatile" {
        anyhow::bail!(
            "SPEC state mode '{}' is not supported yet; this runtime supports only volatile, generation-scoped JavaScript state",
            spec.capabilities.state
        );
    }
    if !spec.capabilities.egress.is_empty() {
        anyhow::bail!(
            "SPEC requests outbound egress ({}) but network capabilities are not available in this runtime",
            spec.capabilities.egress.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if !spec.schema.is_empty() {
        anyhow::bail!("SPEC declares database schema, but SQL capabilities are not available in this runtime");
    }

    tracing::info!(
        service = %spec.name,
        endpoints = spec.endpoints.len(),
        spec = %cli.spec.display(),
        "Loaded service specification"
    );

    let synthesizer = Synthesizer::new(LlmConfig::from_env()?);
    let output = synthesizer.synthesize(&spec, &[]).await?;
    if output.code_format != "javascript" {
        anyhow::bail!(
            "Synthesizer returned unsupported code format '{}'; expected 'javascript'",
            output.code_format
        );
    }
    if output.handler_code.trim().is_empty() {
        anyhow::bail!("Synthesizer returned an empty JavaScript handler");
    }

    let engine = QuickJsEngine::new()?;
    let compiled = engine.compile_handler(&output.handler_code).await?;

    // Verify in a disposable candidate instance. The active instance is created
    // separately so verification requests cannot mutate production module state.
    let mut candidate = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let verification = ShadowVerifier::new().verify_handler(&mut candidate, &output.assertions)?;
    drop(candidate);

    tracing::info!(
        assertions = verification.len(),
        module_hash = %compiled.module_hash,
        rationale = output.rationale.as_deref().unwrap_or(""),
        "Candidate compiled and passed contract verification"
    );

    let active_handler = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let state = Arc::new(IngressState::new());
    state.hot_swap(ActiveGeneration {
        generation_id: 1,
        module_hash: compiled.module_hash,
        handler: Arc::new(Mutex::new(active_handler)),
    });

    let app = ingress::build_ingress_router(state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cli.port)).await?;

    tracing::info!(
        address = %listener.local_addr()?,
        "nosrc is listening with a verified JavaScript handler"
    );

    axum::serve(listener, app).await?;
    Ok(())
}
