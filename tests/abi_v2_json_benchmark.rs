use anyhow::{ensure, Context, Result};
use componentize_qjs::{componentize, ComponentizeOpts, Runtime};
use std::path::PathBuf;
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, Engine, ResourceLimiter, Store};

const ONE_MIB: usize = 1024 * 1024;
const PRODUCTION_FUEL_BUDGET: u64 = 250_000_000;
const MEASUREMENT_FUEL_BUDGET: u64 = 1_000_000_000;
const PRODUCTION_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct Limits {
    peak_requested_linear_memory_bytes: usize,
}

impl ResourceLimiter for Limits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        self.peak_requested_linear_memory_bytes =
            self.peak_requested_linear_memory_bytes.max(desired);
        Ok(desired <= PRODUCTION_MEMORY_LIMIT)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        Ok(desired <= 10_000)
    }
}

async fn componentize_fixture() -> Result<(Engine, Component)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wit_path = root.join("spikes/abi-v2/json-benchmark.wit");
    let js_path = root.join("spikes/abi-v2/json-benchmark.js");
    let js_source = std::fs::read_to_string(&js_path)
        .with_context(|| format!("reading {}", js_path.display()))?;

    let component_bytes = componentize(&ComponentizeOpts {
        wit_path: &wit_path,
        js_source: &js_source,
        js_path: Some(&js_path),
        module_root: Some(&root),
        world_name: Some("json-benchmark"),
        stub_wasi: true,
        disable_gc: false,
        runtime: Runtime::DefaultSync,
    })
    .await
    .context("componentizing the JSON ABI benchmark fixture")?;

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, component_bytes)?;
    Ok((engine, component))
}

fn instantiate(
    engine: &Engine,
    component: &Component,
) -> Result<(Store<Limits>, wasmtime::component::Instance)> {
    let linker = Linker::<Limits>::new(engine);
    let mut store = Store::new(engine, Limits::default());
    // Fuel is installed before instantiation because component startup executes Wasm.
    store.set_fuel(MEASUREMENT_FUEL_BUDGET)?;
    store.limiter(|limits| limits);
    let instance = linker.instantiate(&mut store, component)?;
    Ok((store, instance))
}

fn one_mib_json() -> String {
    const PREFIX: &str = r#"{"payload":""#;
    const SUFFIX: &str = r#""}"#;
    format!(
        "{PREFIX}{}{SUFFIX}",
        "x".repeat(ONE_MIB - PREFIX.len() - SUFFIX.len())
    )
}

fn report_measurement(
    label: &str,
    fuel_used: u64,
    peak_requested_linear_memory_bytes: usize,
) -> bool {
    let fuel_ok = fuel_used <= PRODUCTION_FUEL_BUDGET;
    let memory_ok = peak_requested_linear_memory_bytes <= PRODUCTION_MEMORY_LIMIT;
    println!(
        "{label}: invocation_fuel={fuel_used}, production_fuel_budget={PRODUCTION_FUEL_BUDGET}, fuel_within_budget={fuel_ok}, peak_requested_linear_memory_bytes={peak_requested_linear_memory_bytes}, production_memory_limit_bytes={PRODUCTION_MEMORY_LIMIT}, memory_within_limit={memory_ok}"
    );
    fuel_ok && memory_ok
}

fn invocation_failure_category(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<wasmtime::Trap>() {
        Some(wasmtime::Trap::OutOfFuel) => "fuel-exhaustion",
        Some(_) => "wasm-trap",
        None => "host-or-component-call-error",
    }
}

fn report_invocation_failure(label: &str, error: &anyhow::Error, store: &Store<Limits>) {
    let category = invocation_failure_category(error);
    let remaining_fuel = store
        .get_fuel()
        .map(|fuel| fuel.to_string())
        .unwrap_or_else(|_| "unavailable".to_owned());
    eprintln!(
        "{label} failed: category={category}, remaining_fuel={remaining_fuel}, error={error:#}"
    );
}

fn call_string_export(
    store: &mut Store<Limits>,
    instance: &wasmtime::component::Instance,
    export_name: &str,
    argument: Val,
) -> Result<(String, u64, usize)> {
    let function = instance
        .get_func(&mut *store, export_name)
        .with_context(|| format!("missing component export {export_name}"))?;
    let fuel_before = store.get_fuel()?;
    let mut results = [Val::String(String::new())];
    function.call(&mut *store, &[argument], &mut results)?;
    let fuel_used = fuel_before - store.get_fuel()?;
    let Val::String(response) = results.into_iter().next().unwrap() else {
        anyhow::bail!("{export_name} returned a non-string result");
    };
    let peak_memory = store.data().peak_requested_linear_memory_bytes;
    Ok((response, fuel_used, peak_memory))
}

#[tokio::test]
async fn compares_full_one_mib_json_body_representations_and_response_serialization() -> Result<()>
{
    let (engine, component) = componentize_fixture().await?;
    let json = one_mib_json();
    ensure!(
        json.len() == ONE_MIB,
        "request fixture must be exactly 1 MiB"
    );

    // Candidate A: the actual WIT record with authoritative bytes and optional
    // decoded UTF-8 text. Both representations are present at the guest boundary.
    let record_body = Val::Record(vec![
        (
            "bytes".to_owned(),
            Val::List(json.as_bytes().iter().copied().map(Val::U8).collect()),
        ),
        (
            "utf8-text".to_owned(),
            Val::Option(Some(Box::new(Val::String(json.clone())))),
        ),
    ]);
    let (mut record_store, record_instance) = instantiate(&engine, &component)?;
    let mut all_within_production_limits = true;
    match call_string_export(
        &mut record_store,
        &record_instance,
        "handle-record",
        record_body,
    ) {
        Ok((record_response, record_fuel, record_peak)) => {
            ensure!(
                record_response.len() == ONE_MIB && record_response == json,
                "bytes-plus-text request path changed the JSON response"
            );
            all_within_production_limits &= report_measurement(
                "bytes-plus-optional-text request/handler path",
                record_fuel,
                record_peak,
            );
        }
        Err(error) => {
            report_invocation_failure(
                "bytes-plus-optional-text request/handler path",
                &error,
                &record_store,
            );
            all_within_production_limits = false;
        }
    }

    // Candidate B: the actual WIT text variant. The host explicitly selects the
    // text case; the guest never guesses based on whether bytes happen to be UTF-8.
    let text_variant = Val::Variant("text".to_owned(), Some(Box::new(Val::String(json.clone()))));
    let (mut variant_store, variant_instance) = instantiate(&engine, &component)?;
    match call_string_export(
        &mut variant_store,
        &variant_instance,
        "handle-variant",
        text_variant,
    ) {
        Ok((variant_response, variant_fuel, variant_peak)) => {
            ensure!(
                variant_response.len() == ONE_MIB && variant_response == json,
                "text-variant request path changed the JSON response"
            );
            all_within_production_limits &= report_measurement(
                "text-variant request/handler path",
                variant_fuel,
                variant_peak,
            );
        }
        Err(error) => {
            report_invocation_failure("text-variant request/handler path", &error, &variant_store);
            all_within_production_limits = false;
        }
    }

    // Measure guest response serialization independently. The output is exactly
    // 1 MiB of JSON, produced by JSON.stringify rather than returned as input text.
    let response_payload = "x".repeat(ONE_MIB - r#"{"payload":""#.len() - r#""}"#.len());
    let (mut response_store, response_instance) = instantiate(&engine, &component)?;
    match call_string_export(
        &mut response_store,
        &response_instance,
        "serialize-response",
        Val::String(response_payload),
    ) {
        Ok((serialized_response, response_fuel, response_peak)) => {
            ensure!(
                serialized_response.len() == ONE_MIB,
                "serialized response must be exactly 1 MiB"
            );
            let parsed: serde_json::Value = serde_json::from_str(&serialized_response)?;
            ensure!(
                parsed["payload"].as_str().map(str::len)
                    == Some(ONE_MIB - r#"{"payload":""#.len() - r#""}"#.len()),
                "serialized response payload did not round-trip"
            );
            all_within_production_limits &= report_measurement(
                "1 MiB JSON response serialization",
                response_fuel,
                response_peak,
            );
        }
        Err(error) => {
            report_invocation_failure("1 MiB JSON response serialization", &error, &response_store);
            all_within_production_limits = false;
        }
    }

    // Arbitrary binary is never classified as text merely because some byte
    // sequences happen to be valid UTF-8. Verify exact bytes, including NUL and
    // invalid UTF-8, through the binary variant.
    let binary = vec![0x00, 0xff, 0xc3, 0x28, 0x80, 0x00, 0x7f, 0xfe];
    let (mut binary_store, binary_instance) = instantiate(&engine, &component)?;
    let binary_roundtrip = binary_instance
        .get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut binary_store, "roundtrip-binary")?;
    let (returned_binary,) = binary_roundtrip.call(&mut binary_store, (binary.clone(),))?;
    ensure!(returned_binary == binary, "binary round-trip changed bytes");

    ensure!(
        all_within_production_limits,
        "one or more JSON benchmark paths exceeded the production limits or could not complete with the measurement fuel budget"
    );
    Ok(())
}
