use anyhow::{Context, Result};
use componentize_qjs::{componentize, ComponentizeOpts, Runtime};
use std::path::PathBuf;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

#[tokio::test]
async fn componentize_qjs_can_call_an_imported_host_interface() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wit_path = root.join("spikes/abi-v2/host-import.wit");
    let js_path = root.join("spikes/abi-v2/host-import.js");
    let js_source = std::fs::read_to_string(&js_path)
        .with_context(|| format!("reading {}", js_path.display()))?;

    let component_bytes = componentize(&ComponentizeOpts {
        wit_path: &wit_path,
        js_source: &js_source,
        js_path: Some(&js_path),
        module_root: Some(&root),
        world_name: Some("host-import"),
        stub_wasi: true,
        disable_gc: false,
        runtime: Runtime::DefaultSync,
    })
    .await
    .context("componentizing the host-import spike")?;

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, component_bytes)?;

    let mut linker = Linker::<()>::new(&engine);
    linker
        .instance("nosrc:abi-v2-spike/host")?
        .func_wrap("greet", |_store, (name,): (String,)| {
            Ok((format!("host:{name}"),))
        })?;

    let mut store = Store::new(&engine, ());
    store.set_fuel(50_000_000)?;
    let instance = linker.instantiate(&mut store, &component)?;
    let roundtrip = instance.get_typed_func::<(String,), (String,)>(&mut store, "roundtrip")?;

    let (actual,) = roundtrip.call(&mut store, ("componentize-qjs".to_owned(),))?;

    anyhow::ensure!(
        actual == "host:componentize-qjs",
        "host import returned {actual:?}, expected {:?}",
        "host:componentize-qjs"
    );
    Ok(())
}
