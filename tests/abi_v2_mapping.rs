use anyhow::{ensure, Context, Result};
use componentize_qjs::{componentize, ComponentizeOpts, Runtime};
use std::path::PathBuf;
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, Engine, ResourceLimiter, Store};

const ONE_MIB: usize = 1024 * 1024;
const PRODUCTION_FUEL_BUDGET: u64 = 50_000_000;
const MEASUREMENT_FUEL_BUDGET: u64 = 100_000_000;
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
        self.peak_requested_linear_memory_bytes = self.peak_requested_linear_memory_bytes.max(desired);
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

fn record_field<'a>(value: &'a Val, name: &str) -> Result<&'a Val> {
    let Val::Record(fields) = value else {
        anyhow::bail!("expected record for field {name}");
    };
    fields
        .iter()
        .find_map(|(field, value)| (field == name).then_some(value))
        .with_context(|| format!("missing record field {name}"))
}

fn expect_bytes(value: &Val, expected: &[u8], context: &str) -> Result<()> {
    let Val::List(values) = value else {
        anyhow::bail!("{context}: expected list<u8>, got {value:?}");
    };
    let actual = values
        .iter()
        .map(|value| match value {
            Val::U8(byte) => Ok(*byte),
            other => anyhow::bail!("{context}: expected u8 element, got {other:?}"),
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(actual == expected, "{context}: bytes differed: {actual:?}");
    Ok(())
}

fn instantiate(
    engine: &Engine,
    component: &Component,
    fuel: u64,
) -> Result<(Store<Limits>, wasmtime::component::Instance)> {
    let mut linker = Linker::<Limits>::new(engine);
    linker.instance("nosrc:abi-v2-mapping/host")?.func_new(
        "inspect",
        |_store, _func, params, results| {
            let result = (|| -> Result<()> {
                ensure!(params.len() == 1, "inspect expected one argument");
                let payload = &params[0];

                let request = record_field(payload, "request")?;
                let meta = record_field(request, "meta")?;
                ensure!(
                    matches!(record_field(meta, "request-id")?, Val::String(value) if value == "req-7"),
                    "nested record request-id mapping was incorrect: {meta:?}"
                );
                ensure!(
                    matches!(record_field(meta, "attempt")?, Val::U32(2)),
                    "nested record attempt mapping was incorrect: {meta:?}"
                );

                ensure!(
                    matches!(record_field(payload, "note")?, Val::Option(None)),
                    "option none did not map to an empty option"
                );
                ensure!(
                    matches!(
                        record_field(payload, "correlation-id")?,
                        Val::Option(Some(value))
                            if matches!(value.as_ref(), Val::String(text) if text == "trace-9")
                    ),
                    "option some did not map to the contained string"
                );

                let expected = [0, 1, 127, 255];
                let kind = record_field(payload, "kind")?;
                match kind {
                    Val::Variant(case, Some(value)) if case == "bytes" => {
                        expect_bytes(value, &expected, "variant bytes payload")?;
                    }
                    other => anyhow::bail!("variant mapping was incorrect: {other:?}"),
                }
                expect_bytes(record_field(payload, "raw")?, &expected, "record raw field")?;

                results[0] = Val::String("host-mapping-ok".to_owned());
                Ok(())
            })();
            result.map_err(|error| wasmtime::Error::msg(error.to_string()))
        },
    )?;

    let mut store = Store::new(engine, Limits::default());
    // Component instantiation executes WebAssembly too, so fuel must be
    // installed before linker.instantiate rather than after it.
    store.set_fuel(fuel)?;
    store.limiter(|limits| limits);
    let instance = linker.instantiate(&mut store, component)?;
    Ok((store, instance))
}

#[tokio::test]
async fn componentize_qjs_maps_nested_wit_values_and_measures_one_mib_binary_bodies() -> Result<()>
{
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wit_path = root.join("spikes/abi-v2/mapping.wit");
    let js_path = root.join("spikes/abi-v2/mapping.js");
    let js_source = std::fs::read_to_string(&js_path)
        .with_context(|| format!("reading {}", js_path.display()))?;

    let component_bytes = componentize(&ComponentizeOpts {
        wit_path: &wit_path,
        js_source: &js_source,
        js_path: Some(&js_path),
        module_root: Some(&root),
        world_name: Some("mapping"),
        stub_wasi: true,
        disable_gc: false,
        runtime: Runtime::DefaultSync,
    })
    .await
    .context("componentizing the ABI mapping spike")?;

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, component_bytes)?;

    // Verify records, nested records, variants, options, and list<u8> through
    // the generated JavaScript-to-WIT import mapping.
    let (mut store, instance) = instantiate(&engine, &component, MEASUREMENT_FUEL_BUDGET)?;
    // Keep the same explicit budget for instantiation and the mapping probe.
    let probe = instance.get_typed_func::<(), (String,)>(&mut store, "probe")?;
    let fuel_before_probe = store.get_fuel()?;
    let (mapping_result,) = probe.call(&mut store, ())?;
    let probe_fuel_used = fuel_before_probe - store.get_fuel()?;
    println!("ABI mapping probe: fuel={probe_fuel_used}");
    ensure!(
        mapping_result.starts_with("host-mapping-ok;uint8array=true;"),
        "unexpected JS/WIT mapping result: {mapping_result}"
    );
    println!("ABI mapping result: {mapping_result}");

    // Measure request decoding independently: the 1 MiB Vec<u8> crosses the
    // component boundary into JavaScript as a Uint8Array.
    let (mut request_store, request_instance) =
        instantiate(&engine, &component, MEASUREMENT_FUEL_BUDGET)?;
    let decode_request = request_instance
        .get_typed_func::<(Vec<u8>,), (u32,)>(&mut request_store, "decode-request")?;
    let request_body = vec![0x5a; ONE_MIB];
    let request_fuel_before_call = request_store.get_fuel()?;
    let (decoded_length,) = decode_request.call(&mut request_store, (request_body,))?;
    ensure!(
        decoded_length as usize == ONE_MIB,
        "request body length changed"
    );
    let request_fuel_used = request_fuel_before_call - request_store.get_fuel()?;
    let request_peak_memory = request_store.data().peak_requested_linear_memory_bytes;
    println!(
        "1 MiB request decode: fuel={request_fuel_used}, production_budget={PRODUCTION_FUEL_BUDGET}, peak_requested_linear_memory_bytes={request_peak_memory}"
    );

    // Measure response encoding independently: JavaScript allocates and
    // returns a 1 MiB Uint8Array, which Wasmtime lifts into Vec<u8>.
    let (mut response_store, response_instance) =
        instantiate(&engine, &component, MEASUREMENT_FUEL_BUDGET)?;
    let encode_response = response_instance
        .get_typed_func::<(u32,), (Vec<u8>,)>(&mut response_store, "encode-response")?;
    let response_fuel_before_call = response_store.get_fuel()?;
    let (response_body,) = encode_response.call(&mut response_store, (ONE_MIB as u32,))?;
    ensure!(
        response_body.len() == ONE_MIB,
        "response body length changed"
    );
    ensure!(
        response_body.first() == Some(&0x5a) && response_body.last() == Some(&0x5a),
        "response body contents changed"
    );
    let response_fuel_used = response_fuel_before_call - response_store.get_fuel()?;
    let response_peak_memory = response_store.data().peak_requested_linear_memory_bytes;
    println!(
        "1 MiB response encode: fuel={response_fuel_used}, production_budget={PRODUCTION_FUEL_BUDGET}, peak_requested_linear_memory_bytes={response_peak_memory}"
    );

    // Keep the larger diagnostic ceiling so an over-budget measurement is
    // printed above, but fail the test when either call exceeds production.
    ensure!(
        request_fuel_used <= PRODUCTION_FUEL_BUDGET,
        "request decoding used {request_fuel_used} fuel, exceeding the production budget of {PRODUCTION_FUEL_BUDGET}"
    );
    ensure!(
        response_fuel_used <= PRODUCTION_FUEL_BUDGET,
        "response encoding used {response_fuel_used} fuel, exceeding the production budget of {PRODUCTION_FUEL_BUDGET}"
    );
    Ok(())
}
