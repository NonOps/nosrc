use componentize_qjs::{componentize, ComponentizeOpts, Runtime};
use sha2::{Digest, Sha256};
use std::thread;
use std::time::Duration;
use wasmtime::component::{Component, Func, Linker, Val};
use wasmtime::{Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder};

use crate::engine::http_envelope::{
    Header, RequestEnvelope, ResponseBody, ResponseEnvelope, MAX_HANDLER_RESPONSE_BYTES,
};
use crate::error::{EngineError, SandboxTrap};

pub const DEFAULT_COMPONENT_FUEL: u64 = 250_000_000;
pub const DEFAULT_COMPONENT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_GUEST_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
// Keep the epoch cadence above Windows' default timer granularity. A 1 ms
// thread sleep can take 10-16 ms there, stretching tick-count deadlines by an
// order of magnitude and making epoch-timeout tests fail nondeterministically.
const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);
const COMPONENT_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// ABI version tag embedded in the compiled-component cache identity.
pub const ABI_VERSION_TAG: &str = "component-runtime-abi-v2";

/// The WIT world definition for the ABI v2 typed HTTP handler.
///
/// Package versioned as `nosrc:runtime@2.0.0`; the world exports
/// `handle: func(request: request) -> response` using typed records.
/// Header values are `list<u8>` (raw bytes), not assumed UTF-8.
/// Request body carries authoritative raw bytes plus an optional host-validated
/// UTF-8 view; response body is an explicit variant.
pub const HANDLER_WIT: &str = r#"
package nosrc:runtime@2.0.0;

interface http {
    record header {
        name: string,
        value: list<u8>,
    }

    record request {
        method: string,
        scheme: option<string>,
        authority: option<string>,
        path: string,
        query: option<string>,
        headers: list<header>,
        body: list<u8>,
        body-text: option<string>,
    }

    variant response-body {
        text(string),
        bytes(list<u8>),
    }

    record response {
        status: u16,
        headers: list<header>,
        body: response-body,
    }
}

world handler {
    use http.{request, response};
    export handle: func(request: request) -> response;
}
"#;

struct ComponentStoreState {
    limits: StoreLimits,
    memory_high_water_bytes: Option<usize>,
    pending_memory_growth: Option<(usize, usize)>,
    memory_growth_failed: bool,
}

impl ComponentStoreState {
    fn commit_pending_memory_growth(&mut self) {
        // Wasmtime reports failed growth synchronously during the guest call. Once that
        // call returns, any remaining permitted growth is committed telemetry.
        self.pending_memory_growth = None;
    }
}

impl ResourceLimiter for ComponentStoreState {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = self.limits.memory_growing(current, desired, maximum)?;
        if allowed {
            let previous_peak = self.memory_high_water_bytes.unwrap_or(current).max(current);
            self.pending_memory_growth = Some((previous_peak, desired));
            self.memory_high_water_bytes = Some(previous_peak.max(desired));
        } else {
            // A denied growth request is a memory-limit failure even when Wasmtime
            // does not invoke memory_grow_failed for a limiter rejection.
            self.memory_growth_failed = true;
        }
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.limits.table_growing(current, desired, maximum)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.memory_growth_failed = true;
        if let Some((previous_peak, desired)) = self.pending_memory_growth.take() {
            if self.memory_high_water_bytes == Some(desired) {
                self.memory_high_water_bytes = Some(previous_peak);
            }
        }
        self.limits.memory_grow_failed(error)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.limits.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.limits.instances()
    }

    fn tables(&self) -> usize {
        self.limits.tables()
    }

    fn memories(&self) -> usize {
        self.limits.memories()
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RuntimeTelemetry {
    pub schema_version: u8,
    /// Fuel consumed by the most recent invocation; absent when Wasmtime could not report it.
    pub last_invocation_fuel_consumed: Option<u64>,
    /// Largest fuel consumption observed by this handler generation.
    pub generation_fuel_high_water: Option<u64>,
    /// Largest linear-memory size observed by the current Wasmtime instance.
    pub instance_memory_high_water_bytes: Option<usize>,
    /// Largest linear-memory size observed across this handler generation, including replaced instances.
    pub generation_memory_high_water_bytes: Option<usize>,
    /// Number of successfully recreated component instances after guest traps in this generation.
    pub instance_recreations: u64,
}

impl Default for RuntimeTelemetry {
    fn default() -> Self {
        Self {
            schema_version: 1,
            last_invocation_fuel_consumed: None,
            generation_fuel_high_water: None,
            instance_memory_high_water_bytes: None,
            generation_memory_high_water_bytes: None,
            instance_recreations: 0,
        }
    }
}

type InstantiatedComponent = (Store<ComponentStoreState>, Func);

fn epoch_deadline_ticks(timeout: Duration) -> u64 {
    timeout
        .as_nanos()
        .div_ceil(EPOCH_TICK_INTERVAL.as_nanos())
        .clamp(1, u64::MAX as u128) as u64
}

/// Drive Wasmtime's global epoch using a weak reference so the ticker does not
/// keep an otherwise-unused engine alive.
fn start_epoch_ticker(engine: &Engine) -> Result<(), EngineError> {
    let engine = engine.weak();
    thread::Builder::new()
        .name("nosrc-wasmtime-epoch".into())
        .spawn(move || loop {
            thread::sleep(EPOCH_TICK_INTERVAL);
            let Some(engine) = engine.upgrade() else {
                break;
            };
            engine.increment_epoch();
        })
        .map(|_| ())
        .map_err(|error| {
            EngineError::Internal(format!("Failed to start Wasmtime epoch ticker: {error}"))
        })
}

/// Wasmtime component-model engine for JavaScript handlers compiled with QuickJS.
#[derive(Clone)]
pub struct QuickJsEngine {
    engine: Engine,
}

#[derive(Clone)]
pub struct CompiledHandler {
    component: Component,
    pub module_hash: String,
}

pub struct QuickJsHandler {
    engine: Engine,
    component: Component,
    store: Store<ComponentStoreState>,
    handle: Func,
    fuel_budget: u64,
    response_limit_bytes: usize,
    memory_limit_bytes: usize,
    poisoned: bool,
    telemetry: RuntimeTelemetry,
}

impl QuickJsEngine {
    pub fn new() -> Result<Self, EngineError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        config.epoch_interruption(true);

        let engine = Engine::new(&config).map_err(|error| {
            EngineError::Internal(format!("Failed to create component engine: {error}"))
        })?;
        start_epoch_ticker(&engine)?;

        Ok(Self { engine })
    }

    /// Compile an ES module into a standalone QuickJS WebAssembly component.
    pub async fn compile_handler(&self, javascript: &str) -> Result<CompiledHandler, EngineError> {
        let (compiled, _) = self.compile_handler_with_artifact(javascript).await?;
        Ok(compiled)
    }

    /// Componentize source and return the exact bytes that can be cached.
    pub async fn compile_handler_with_artifact(
        &self,
        javascript: &str,
    ) -> Result<(CompiledHandler, Vec<u8>), EngineError> {
        let wit_dir = tempfile::tempdir().map_err(|error| {
            EngineError::Internal(format!("Failed to create temporary WIT directory: {error}"))
        })?;
        let wit_path = wit_dir.path().join("handler.wit");
        std::fs::write(&wit_path, HANDLER_WIT).map_err(|error| {
            EngineError::Internal(format!("Failed to write handler WIT contract: {error}"))
        })?;

        let component_bytes = componentize(&ComponentizeOpts {
            wit_path: &wit_path,
            js_source: javascript,
            js_path: None,
            module_root: None,
            world_name: Some("handler"),
            stub_wasi: true,
            disable_gc: false,
            runtime: Runtime::DefaultSync,
        })
        .await
        .map_err(|error| {
            EngineError::Internal(format!("QuickJS componentization failed: {error:#}"))
        })?;

        let compiled = self.compile_component(&component_bytes)?;
        Ok((compiled, component_bytes))
    }

    /// Compile a previously componentized artifact with this engine configuration.
    pub fn compile_component(
        &self,
        component_bytes: &[u8],
    ) -> Result<CompiledHandler, EngineError> {
        let module_hash = crate::hex_lower(Sha256::digest(component_bytes));
        let component = Component::new(&self.engine, component_bytes).map_err(|error| {
            EngineError::Internal(format!("Invalid handler component: {error}"))
        })?;

        Ok(CompiledHandler {
            component,
            module_hash,
        })
    }

    /// Instantiate a compiled candidate with per-generation resource limits.
    pub fn instantiate(
        &self,
        compiled: &CompiledHandler,
        fuel_budget: u64,
    ) -> Result<QuickJsHandler, EngineError> {
        self.instantiate_with_limits(
            compiled,
            fuel_budget,
            DEFAULT_COMPONENT_MEMORY_BYTES,
            MAX_HANDLER_RESPONSE_BYTES,
        )
    }

    /// Instantiate with operator-selected limits that cannot exceed runtime ceilings.
    pub fn instantiate_with_limits(
        &self,
        compiled: &CompiledHandler,
        fuel_budget: u64,
        memory_limit_bytes: usize,
        response_limit_bytes: usize,
    ) -> Result<QuickJsHandler, EngineError> {
        if fuel_budget == 0 || (!cfg!(test) && fuel_budget > DEFAULT_COMPONENT_FUEL) {
            return Err(EngineError::Internal(
                "Fuel limit exceeds the runtime ceiling".into(),
            ));
        }
        if memory_limit_bytes == 0 || memory_limit_bytes > DEFAULT_COMPONENT_MEMORY_BYTES {
            return Err(EngineError::Internal(
                "Memory limit exceeds the runtime ceiling".into(),
            ));
        }
        if response_limit_bytes == 0 || response_limit_bytes > MAX_HANDLER_RESPONSE_BYTES {
            return Err(EngineError::Internal(
                "Response limit exceeds the runtime ceiling".into(),
            ));
        }
        let (store, handle) = instantiate_component(
            &self.engine,
            &compiled.component,
            fuel_budget,
            memory_limit_bytes,
        )?;

        Ok(QuickJsHandler {
            engine: self.engine.clone(),
            component: compiled.component.clone(),
            store,
            handle,
            fuel_budget,
            response_limit_bytes,
            memory_limit_bytes,
            poisoned: false,
            telemetry: RuntimeTelemetry::default(),
        })
    }
}

fn instantiate_component(
    engine: &Engine,
    component: &Component,
    fuel_budget: u64,
    memory_limit_bytes: usize,
) -> Result<InstantiatedComponent, EngineError> {
    let limits = StoreLimitsBuilder::new()
        .memory_size(memory_limit_bytes)
        .table_elements(10_000)
        .instances(16)
        .tables(4)
        .memories(4)
        .build();

    let mut store = Store::new(
        engine,
        ComponentStoreState {
            limits,
            memory_high_water_bytes: None,
            pending_memory_growth: None,
            memory_growth_failed: false,
        },
    );
    store
        .set_fuel(fuel_budget)
        .map_err(|error| EngineError::Internal(format!("Failed to set component fuel: {error}")))?;
    store.limiter(|state| state);
    store.set_max_component_handles(1_024);
    store.set_epoch_deadline(epoch_deadline_ticks(COMPONENT_STARTUP_TIMEOUT));

    let linker = Linker::<ComponentStoreState>::new(engine);
    let instance = match linker.instantiate(&mut store, component) {
        Ok(instance) => instance,
        Err(error) => {
            let memory_growth_failed = store.data().memory_growth_failed;
            return Err(classify_component_error(
                error,
                memory_limit_bytes,
                memory_growth_failed,
            ));
        }
    };
    let handle = instance.get_func(&mut store, "handle").ok_or_else(|| {
        EngineError::Instantiation(
            "Handler component must export handle(request: request) -> response".into(),
        )
    })?;

    store
        .set_fuel(fuel_budget)
        .map_err(|error| EngineError::Internal(format!("Failed to set component fuel: {error}")))?;
    // Component startup allocations are part of the instance high-water baseline.
    store.data_mut().pending_memory_growth = None;

    Ok((store, handle))
}

// ---------------------------------------------------------------------------
// WIT value helpers: RequestEnvelope -> Val and Val -> ResponseEnvelope
// ---------------------------------------------------------------------------

/// Encode a `Header` as a WIT `record header { name: string, value: list<u8> }`.
fn encode_header_val(header: &Header) -> Val {
    Val::Record(vec![
        ("name".to_owned(), Val::String(header.name.clone())),
        (
            "value".to_owned(),
            Val::List(header.value.iter().map(|&b| Val::U8(b)).collect()),
        ),
    ])
}

/// Encode a `RequestEnvelope` as the WIT `record request { ... }`.
fn encode_request_val(request: &RequestEnvelope) -> Val {
    let scheme = match &request.scheme {
        Some(s) => Val::Option(Some(Box::new(Val::String(s.clone())))),
        None => Val::Option(None),
    };
    let authority = match &request.authority {
        Some(a) => Val::Option(Some(Box::new(Val::String(a.clone())))),
        None => Val::Option(None),
    };
    let query = match &request.query {
        Some(q) => Val::Option(Some(Box::new(Val::String(q.clone())))),
        None => Val::Option(None),
    };
    let headers = Val::List(request.headers.iter().map(encode_header_val).collect());
    // Raw bytes remain authoritative. A valid UTF-8 view is decoded on the host,
    // outside guest fuel, so common JSON/text handlers do not need an expensive
    // JavaScript byte-to-string codec. Invalid UTF-8 remains available as bytes.
    let body = Val::List(request.body.iter().map(|&b| Val::U8(b)).collect());
    let body_text = match std::str::from_utf8(&request.body) {
        Ok(text) => Val::Option(Some(Box::new(Val::String(text.to_owned())))),
        Err(_) => Val::Option(None),
    };

    Val::Record(vec![
        ("method".to_owned(), Val::String(request.method.clone())),
        ("scheme".to_owned(), scheme),
        ("authority".to_owned(), authority),
        ("path".to_owned(), Val::String(request.path.clone())),
        ("query".to_owned(), query),
        ("headers".to_owned(), headers),
        ("body".to_owned(), body),
        ("body-text".to_owned(), body_text),
    ])
}

/// Decode a WIT `list<header>` Val into `Vec<Header>`.
fn decode_headers_val(val: &Val) -> Result<Vec<Header>, String> {
    let Val::List(items) = val else {
        return Err(format!("expected list<header>, got {val:?}"));
    };
    let mut headers = Vec::with_capacity(items.len());
    for item in items {
        let Val::Record(fields) = item else {
            return Err(format!("expected header record, got {item:?}"));
        };
        let name = fields
            .iter()
            .find_map(|(k, v)| {
                if k == "name" {
                    if let Val::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .ok_or_else(|| "header record missing 'name' field".to_owned())?;
        let value_val = fields
            .iter()
            .find_map(|(k, v)| if k == "value" { Some(v) } else { None })
            .ok_or_else(|| "header record missing 'value' field".to_owned())?;
        let Val::List(byte_vals) = value_val else {
            return Err(format!("header value must be list<u8>, got {value_val:?}"));
        };
        let value: Result<Vec<u8>, _> = byte_vals
            .iter()
            .map(|bv| {
                if let Val::U8(b) = bv {
                    Ok(*b)
                } else {
                    Err(format!("header value byte must be u8, got {bv:?}"))
                }
            })
            .collect();
        headers.push(Header {
            name,
            value: value?,
        });
    }
    Ok(headers)
}

/// Decode a WIT `variant response-body { text(string), bytes(list<u8>) }` Val.
fn decode_response_body_val(val: &Val) -> Result<ResponseBody, String> {
    match val {
        Val::Variant(case, payload) => match case.as_str() {
            "text" => {
                let inner = payload
                    .as_ref()
                    .ok_or_else(|| "text variant must have a payload".to_owned())?;
                if let Val::String(s) = inner.as_ref() {
                    Ok(ResponseBody::Text(s.clone()))
                } else {
                    Err(format!(
                        "text variant payload must be string, got {inner:?}"
                    ))
                }
            }
            "bytes" => {
                let inner = payload
                    .as_ref()
                    .ok_or_else(|| "bytes variant must have a payload".to_owned())?;
                let Val::List(byte_vals) = inner.as_ref() else {
                    return Err(format!(
                        "bytes variant payload must be list<u8>, got {inner:?}"
                    ));
                };
                let bytes: Result<Vec<u8>, _> = byte_vals
                    .iter()
                    .map(|bv| {
                        if let Val::U8(b) = bv {
                            Ok(*b)
                        } else {
                            Err(format!("bytes variant byte must be u8, got {bv:?}"))
                        }
                    })
                    .collect();
                Ok(ResponseBody::Bytes(bytes?))
            }
            other => Err(format!("unknown response-body variant case: {other}")),
        },
        other => Err(format!("expected response-body variant, got {other:?}")),
    }
}

/// Decode a WIT `record response { status, headers, body }` Val into a `ResponseEnvelope`.
fn decode_response_val(val: &Val) -> Result<ResponseEnvelope, String> {
    let Val::Record(fields) = val else {
        return Err(format!("expected response record, got {val:?}"));
    };

    let status = fields
        .iter()
        .find_map(|(k, v)| {
            if k == "status" {
                if let Val::U16(s) = v {
                    Some(*s)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .ok_or_else(|| "response record missing 'status' u16 field".to_owned())?;

    let headers_val = fields
        .iter()
        .find_map(|(k, v)| if k == "headers" { Some(v) } else { None })
        .ok_or_else(|| "response record missing 'headers' field".to_owned())?;
    let headers = decode_headers_val(headers_val)?;

    let body_val = fields
        .iter()
        .find_map(|(k, v)| if k == "body" { Some(v) } else { None })
        .ok_or_else(|| "response record missing 'body' field".to_owned())?;
    let body = decode_response_body_val(body_val)?;

    Ok(ResponseEnvelope {
        status,
        headers,
        body,
    })
}

impl QuickJsHandler {
    /// Return bounded resource measurements without exposing guest data.
    pub fn telemetry_snapshot(&self) -> RuntimeTelemetry {
        let mut telemetry = self.telemetry.clone();
        telemetry.instance_memory_high_water_bytes = self.store.data().memory_high_water_bytes;
        telemetry.generation_memory_high_water_bytes = match (
            telemetry.generation_memory_high_water_bytes,
            telemetry.instance_memory_high_water_bytes,
        ) {
            (Some(generation), Some(instance)) => Some(generation.max(instance)),
            (generation, instance) => generation.or(instance),
        };
        telemetry
    }

    fn record_invocation_telemetry(&mut self) {
        // A permitted growth that did not report a failure is treated as successful
        // once the guest call returns. Failed growth callbacks roll the estimate back.
        self.store.data_mut().commit_pending_memory_growth();
        self.telemetry.last_invocation_fuel_consumed = self
            .store
            .get_fuel()
            .ok()
            .map(|remaining| self.fuel_budget.saturating_sub(remaining));
        if let Some(consumed) = self.telemetry.last_invocation_fuel_consumed {
            self.telemetry.generation_fuel_high_water = Some(
                self.telemetry
                    .generation_fuel_high_water
                    .unwrap_or_default()
                    .max(consumed),
            );
        }
        if let Some(memory) = self.store.data().memory_high_water_bytes {
            self.telemetry.generation_memory_high_water_bytes = Some(
                self.telemetry
                    .generation_memory_high_water_bytes
                    .unwrap_or_default()
                    .max(memory),
            );
        }
    }

    /// Invoke the handler with the default wall-clock deadline.
    pub fn invoke(&mut self, request: &RequestEnvelope) -> Result<ResponseEnvelope, EngineError> {
        self.invoke_with_timeout(request, DEFAULT_GUEST_EXECUTION_TIMEOUT)
    }

    /// Invoke the handler while retaining its instance after successful requests.
    /// A Wasmtime epoch deadline interrupts guest execution independently of fuel.
    pub fn invoke_with_timeout(
        &mut self,
        request: &RequestEnvelope,
        timeout: Duration,
    ) -> Result<ResponseEnvelope, EngineError> {
        if self.poisoned {
            return Err(EngineError::Internal(
                "Handler instance is unavailable after a failed recovery attempt".into(),
            ));
        }

        self.telemetry.last_invocation_fuel_consumed = None;
        self.store.data_mut().memory_growth_failed = false;

        self.store.set_fuel(self.fuel_budget).map_err(|error| {
            EngineError::Internal(format!("Failed to reset component fuel: {error}"))
        })?;
        self.store.set_epoch_deadline(epoch_deadline_ticks(timeout));

        // Encode the request as a WIT Val.
        let request_val = encode_request_val(request);
        let mut results = vec![Val::Bool(false)]; // placeholder; will be replaced

        let call_result = self
            .handle
            .call(&mut self.store, &[request_val], &mut results);

        match call_result {
            Ok(()) => {
                self.record_invocation_telemetry();
                // Decode the response Val.
                let response_val = results.into_iter().next().ok_or_else(|| {
                    EngineError::Internal(
                        "Handler returned no values; expected one response record".into(),
                    )
                })?;

                // Bound the complete guest response before allocating decoded host
                // structures. Include body bytes, header names/values, per-header
                // framing overhead, and reject excessive header counts.
                let response_size =
                    estimate_response_size(&response_val, 128).map_err(EngineError::Internal)?;
                if response_size > self.response_limit_bytes {
                    return Err(EngineError::Internal(format!(
                        "JavaScript handler response exceeds the {} byte limit",
                        self.response_limit_bytes
                    )));
                }

                decode_response_val(&response_val).map_err(|error| {
                    EngineError::Internal(format!(
                        "JavaScript handler returned an invalid response: {error}"
                    ))
                })
            }
            Err(error) => {
                let memory_growth_failed = self.store.data().memory_growth_failed;
                self.record_invocation_telemetry();
                let trap =
                    classify_component_error(error, self.memory_limit_bytes, memory_growth_failed);
                let failure_kind = match &trap {
                    EngineError::Trap(SandboxTrap::FuelExhausted) => "fuel_exhausted",
                    EngineError::Trap(SandboxTrap::ExecutionTimeout) => "execution_timeout",
                    EngineError::Trap(SandboxTrap::MemoryExhausted(_)) => "memory_exhausted",
                    EngineError::Trap(SandboxTrap::GuestTrap) => "guest_trap",
                    EngineError::Trap(SandboxTrap::Unreachable) => "unreachable",
                    _ => "execution_error",
                };
                tracing::warn!(
                    failure_kind,
                    "Guest handler trapped; discarding and recreating its component instance"
                );
                // A trapped component instance is not reusable. Recreate it from the
                // compiled component; if recovery fails, permanently reject later calls
                // rather than attempting to reuse the trapped store.
                match instantiate_component(
                    &self.engine,
                    &self.component,
                    self.fuel_budget,
                    self.memory_limit_bytes,
                ) {
                    Ok((store, handle)) => {
                        self.store = store;
                        self.handle = handle;
                        self.telemetry.instance_recreations =
                            self.telemetry.instance_recreations.saturating_add(1);
                    }
                    Err(recovery_error) => {
                        self.poisoned = true;
                        return Err(EngineError::Internal(format!(
                            "Guest execution failed ({trap}); handler recovery failed: {recovery_error}"
                        )));
                    }
                }
                Err(trap)
            }
        }
    }
}

/// Estimate the complete response size without materializing Rust strings or
/// byte vectors. Header count is independently bounded to avoid pathological
/// lists of tiny/empty fields.
fn estimate_response_size(response_val: &Val, max_headers: usize) -> Result<usize, String> {
    let Val::Record(fields) = response_val else {
        return Err("response must be a record".into());
    };
    let headers = fields
        .iter()
        .find_map(|(name, value)| (name == "headers").then_some(value))
        .ok_or_else(|| "response record missing 'headers' field".to_owned())?;
    let Val::List(headers) = headers else {
        return Err("response headers must be a list".into());
    };
    if headers.len() > max_headers {
        return Err(format!("response contains more than {max_headers} headers"));
    }

    let mut total = 16usize;
    for header in headers {
        let Val::Record(parts) = header else {
            return Err("response header must be a record".into());
        };
        let name = parts
            .iter()
            .find_map(|(key, value)| {
                if key == "name" {
                    if let Val::String(name) = value {
                        Some(name)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .ok_or_else(|| "response header is missing a string name".to_owned())?;
        let value = parts
            .iter()
            .find_map(|(key, value)| (key == "value").then_some(value))
            .ok_or_else(|| "response header is missing a value".to_owned())?;
        let Val::List(value) = value else {
            return Err("response header value must be list<u8>".into());
        };
        let header_size = name
            .len()
            .checked_add(value.len())
            .and_then(|size| size.checked_add(32))
            .ok_or_else(|| "response size overflow".to_owned())?;
        total = total
            .checked_add(header_size)
            .ok_or_else(|| "response size overflow".to_owned())?;
    }

    let body = fields
        .iter()
        .find_map(|(name, value)| (name == "body").then_some(value))
        .ok_or_else(|| "response record missing 'body' field".to_owned())?;
    let body_size = estimate_response_body_size(body)
        .ok_or_else(|| "response body has an invalid shape".to_owned())?;
    total
        .checked_add(body_size)
        .ok_or_else(|| "response size overflow".to_owned())
}

/// Estimate response body byte size from a Val without full decoding.
fn estimate_response_body_size(body_val: &Val) -> Option<usize> {
    match body_val {
        Val::Variant(case, payload) => {
            let inner = payload.as_ref()?;
            match case.as_str() {
                "text" => {
                    if let Val::String(s) = inner.as_ref() {
                        Some(s.len())
                    } else {
                        None
                    }
                }
                "bytes" => {
                    if let Val::List(items) = inner.as_ref() {
                        Some(items.len())
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn classify_component_error(
    error: wasmtime::Error,
    memory_limit_bytes: usize,
    memory_growth_failed: bool,
) -> EngineError {
    if error.downcast_ref::<wasmtime::Trap>().is_some() {
        EngineError::Trap(classify_guest_trap(
            &error,
            memory_limit_bytes,
            memory_growth_failed,
        ))
    } else {
        EngineError::Instantiation(error.to_string())
    }
}

fn classify_guest_trap(
    error: &wasmtime::Error,
    memory_limit_bytes: usize,
    memory_growth_failed: bool,
) -> SandboxTrap {
    // Preserve the typed execution-limit traps even if an earlier, caught allocation
    // failure set the limiter signal during the same guest invocation.
    if let Some(trap) = error.downcast_ref::<wasmtime::Trap>() {
        match trap {
            wasmtime::Trap::OutOfFuel => return SandboxTrap::FuelExhausted,
            wasmtime::Trap::Interrupt => return SandboxTrap::ExecutionTimeout,
            _ => {}
        }
    }

    if memory_growth_failed {
        return SandboxTrap::MemoryExhausted(
            memory_limit_bytes
                .div_ceil(1024 * 1024)
                .min(u32::MAX as usize) as u32,
        );
    }

    if let Some(trap) = error.downcast_ref::<wasmtime::Trap>() {
        // QuickJS can lower an uncaught JavaScript exception to Wasm unreachable.
        // Keep this stable across targets instead of exposing that implementation detail.
        if matches!(trap, wasmtime::Trap::UnreachableCodeReached) {
            return SandboxTrap::GuestTrap;
        }
    }

    SandboxTrap::GuestTrap
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use tokio::sync::OnceCell;

    // Epoch tests exercise epoch interruption, not fuel exhaustion. Use the largest
    // representable fuel budget so host scheduling cannot make the busy-loop guest hit
    // the fuel limit before the background epoch ticker runs. Production limits remain
    // unchanged.
    pub const EPOCH_TEST_FUEL: u64 = u64::MAX;
    // Keep a realistic window for recovery requests in ingress tests, which reuse the
    // same timeout after the looping guest is interrupted. The effectively unbounded
    // test fuel budget keeps ticker scheduling from changing the trap classification.
    pub const EPOCH_TEST_TIMEOUT: Duration = Duration::from_millis(250);

    /// ABI v2 handler source. Uses typed WIT records: request is a typed record,
    /// response is `{ status, headers, body }` where body is `text(string) | bytes(list<u8>)`.
    pub const HANDLER_SOURCE: &str = r#"
        let count = 0;

        export function handle(request) {
            if (request.path === "/loop") {
                while (true) {}
            }

            if (request.path === "/memory") {
                // Force QuickJS to grow its Wasm linear memory beyond the 64 MiB store limit.
                new Uint8Array(128 * 1024 * 1024);
                return { status: 200, headers: [], body: { tag: "text", val: "unexpected allocation success" } };
            }

            if (request.path === "/catch-memory") {
                try {
                    new Uint8Array(128 * 1024 * 1024);
                } catch (error) {
                    return { status: 200, headers: [], body: { tag: "text", val: "allocation caught" } };
                }
                return { status: 200, headers: [], body: { tag: "text", val: "unexpected allocation success" } };
            }

            if (request.path === "/caught-memory-then-throw") {
                try {
                    new Uint8Array(128 * 1024 * 1024);
                } catch (error) {
                    throw new Error("unrelated guest failure after caught allocation");
                }
                throw new Error("allocation unexpectedly succeeded");
            }

            if (request.path === "/error") {
                return { status: 500, headers: [], body: { tag: "text", val: "failed" } };
            }

            if (request.path === "/throw") {
                throw new Error("guest-controlled secret detail");
            }

            if (request.path === "/small-oversized") {
                return { status: 200, headers: [], body: { tag: "text", val: "x".repeat(1024) } };
            }

            if (request.path === "/oversized") {
                return { status: 200, headers: [], body: { tag: "text", val: "x".repeat(1024 * 1024 + 1) } };
            }

            if (request.path === "/health") {
                return {
                    status: 200,
                    headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                    body: { tag: "text", val: "healthy" }
                };
            }

            if (request.path === "/health-bytes") {
                // Return health using the bytes variant.
                const bytes = [104, 101, 97, 108, 116, 104, 121]; // "healthy"
                return { status: 200, headers: [], body: { tag: "bytes", val: new Uint8Array(bytes) } };
            }

            if (request.path === "/echo") {
                // Echo query and first header names back as text.
                const headerNames = request.headers.map(h => h.name).join(",");
                const queryStr = request.query !== null ? request.query : "";
                return {
                    status: 200,
                    headers: [],
                    body: { tag: "text", val: JSON.stringify({ query: queryStr, headerNames }) }
                };
            }

            if (request.path === "/query") {
                // Parse query string for ?q= and return its value.
                const q = request.query || "";
                const match = q.match(/(?:^|&)q=([^&]*)/);
                const val = match ? decodeURIComponent(match[1]) : "missing";
                return {
                    status: 200,
                    headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                    body: { tag: "text", val }
                };
            }

            if (request.path === "/counter") {
                count += 1;
                return { status: 200, headers: [], body: { tag: "text", val: String(count) } };
            }

            if (request.path === "/binary-echo") {
                // Echo the request body bytes back unchanged.
                return { status: 200, headers: [], body: { tag: "bytes", val: request.body } };
            }

            if (request.path === "/body-length") {
                // Return request body byte length as text.
                return { status: 200, headers: [], body: { tag: "text", val: String(request.body.length) } };
            }

            return {
                status: 200,
                headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                body: { tag: "text", val: request.method + " " + request.path }
            };
        }
    "#;

    static FIXTURE: OnceCell<(QuickJsEngine, CompiledHandler)> = OnceCell::const_new();

    pub async fn compiled() -> &'static (QuickJsEngine, CompiledHandler) {
        FIXTURE
            .get_or_init(|| async {
                let engine = QuickJsEngine::new().unwrap();
                let component = engine.compile_handler(HANDLER_SOURCE).await.unwrap();
                (engine, component)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(path: &str) -> RequestEnvelope {
        RequestEnvelope {
            method: "GET".to_string(),
            scheme: None,
            authority: None,
            path: path.to_string(),
            query: None,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn request_body_exposes_raw_bytes_and_host_validated_utf8_view() {
        let mut req = request("/body");
        req.body = "snowman ☃ and rocket 🚀".as_bytes().to_vec();

        let Val::Record(fields) = encode_request_val(&req) else {
            panic!("request must encode as a WIT record");
        };
        let body = fields.iter().find(|(name, _)| name == "body").unwrap();
        let Val::List(bytes) = &body.1 else {
            panic!("request.body must remain the authoritative byte list");
        };
        assert_eq!(
            bytes.len(),
            req.body.len(),
            "raw request bytes must be preserved exactly"
        );

        let body_text = fields.iter().find(|(name, _)| name == "body-text").unwrap();
        assert!(matches!(
            &body_text.1,
            Val::Option(Some(value))
                if matches!(value.as_ref(), Val::String(text) if text == "snowman ☃ and rocket 🚀")
        ));
    }

    #[test]
    fn invalid_utf8_request_body_has_no_text_view_but_preserves_bytes() {
        let mut req = request("/body");
        req.body = vec![0x00, 0xff, 0xc3, 0x28];

        let Val::Record(fields) = encode_request_val(&req) else {
            panic!("request must encode as a WIT record");
        };
        let body = fields.iter().find(|(name, _)| name == "body").unwrap();
        let Val::List(bytes) = &body.1 else {
            panic!("request.body must remain the authoritative byte list");
        };
        assert_eq!(
            bytes
                .iter()
                .map(|value| match value {
                    Val::U8(byte) => *byte,
                    other => panic!("expected u8, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            req.body
        );

        let body_text = fields.iter().find(|(name, _)| name == "body-text").unwrap();
        assert!(matches!(&body_text.1, Val::Option(None)));
    }

    #[test]
    fn guest_trap_classification_uses_typed_traps_and_limiter_state_only() {
        let memory_limit_bytes = 64 * 1024 * 1024;

        assert!(matches!(
            classify_guest_trap(&wasmtime::Trap::OutOfFuel.into(), memory_limit_bytes, false),
            SandboxTrap::FuelExhausted
        ));
        assert!(matches!(
            classify_guest_trap(&wasmtime::Trap::Interrupt.into(), memory_limit_bytes, false),
            SandboxTrap::ExecutionTimeout
        ));
        assert!(matches!(
            classify_guest_trap(&wasmtime::Trap::OutOfFuel.into(), memory_limit_bytes, true),
            SandboxTrap::FuelExhausted
        ));
        assert!(matches!(
            classify_guest_trap(&wasmtime::Trap::Interrupt.into(), memory_limit_bytes, true),
            SandboxTrap::ExecutionTimeout
        ));
        assert!(matches!(
            classify_guest_trap(
                &wasmtime::Trap::UnreachableCodeReached.into(),
                memory_limit_bytes,
                false
            ),
            SandboxTrap::GuestTrap
        ));
        assert!(
            matches!(
                classify_guest_trap(
                    &wasmtime::Trap::MemoryOutOfBounds.into(),
                    memory_limit_bytes,
                    false
                ),
                SandboxTrap::GuestTrap
            ),
            "unclassified Wasmtime traps must not be inferred from engine message text"
        );
        assert!(
            matches!(
                classify_guest_trap(
                    &wasmtime::Trap::MemoryOutOfBounds.into(),
                    memory_limit_bytes,
                    true
                ),
                SandboxTrap::MemoryExhausted(64)
            ),
            "the resource limiter's explicit signal identifies memory-limit failures"
        );
    }

    #[test]
    fn committed_memory_growth_cannot_be_rolled_back_by_a_later_failure_callback() {
        let mut state = ComponentStoreState {
            limits: StoreLimitsBuilder::new()
                .memory_size(DEFAULT_COMPONENT_MEMORY_BYTES)
                .build(),
            memory_high_water_bytes: Some(8),
            pending_memory_growth: None,
            memory_growth_failed: false,
        };

        assert!(state.memory_growing(8, 16, None).unwrap());
        assert_eq!(state.memory_high_water_bytes, Some(16));
        assert_eq!(state.pending_memory_growth, Some((8, 16)));

        state.commit_pending_memory_growth();
        assert_eq!(state.pending_memory_growth, None);

        state
            .memory_grow_failed(wasmtime::Error::msg("late failure callback"))
            .unwrap();
        assert!(state.memory_growth_failed);
        assert!(matches!(
            classify_guest_trap(
                &wasmtime::Trap::MemoryOutOfBounds.into(),
                DEFAULT_COMPONENT_MEMORY_BYTES,
                state.memory_growth_failed
            ),
            SandboxTrap::MemoryExhausted(64)
        ));
        assert_eq!(
            state.memory_high_water_bytes,
            Some(16),
            "a callback without a matching pending growth must not roll back committed telemetry"
        );
    }

    #[tokio::test]
    async fn javascript_module_state_survives_multiple_requests() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        assert_eq!(
            handler.invoke(&request("/counter")).unwrap().body,
            ResponseBody::Text("1".into())
        );
        assert_eq!(
            handler.invoke(&request("/counter")).unwrap().body,
            ResponseBody::Text("2".into())
        );
        let telemetry = handler.telemetry_snapshot();
        assert!(telemetry.last_invocation_fuel_consumed.is_some());
        assert!(telemetry.generation_fuel_high_water.is_some());
        assert!(telemetry.instance_memory_high_water_bytes.is_some());
        assert_eq!(telemetry.schema_version, 1);
    }

    #[tokio::test]
    async fn configured_response_limit_is_enforced_before_deserialization() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate_with_limits(
                compiled,
                DEFAULT_COMPONENT_FUEL,
                DEFAULT_COMPONENT_MEMORY_BYTES,
                128,
            )
            .unwrap();
        let error = handler.invoke(&request("/small-oversized")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("JavaScript handler response exceeds the 128 byte limit"),
            "Unexpected configured response-limit error: {error}"
        );
    }

    #[tokio::test]
    async fn invalid_javascript_is_rejected_before_instantiation() {
        let engine = QuickJsEngine::new().unwrap();
        let result = engine.compile_handler("export function handle( {").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn javascript_memory_growth_is_limited_by_store_budget() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let result = handler.invoke(&request("/memory"));
        assert!(
            matches!(
                result,
                Err(EngineError::Trap(SandboxTrap::MemoryExhausted(64)))
            ),
            "Expected a stable memory-limit category for allocation failure, got: {result:?}"
        );
        let telemetry = handler.telemetry_snapshot();
        assert!(telemetry.last_invocation_fuel_consumed.is_some());
        assert!(
            telemetry
                .instance_memory_high_water_bytes
                .is_some_and(|bytes| bytes <= DEFAULT_COMPONENT_MEMORY_BYTES),
            "memory high-water must reflect an observed allocation within the hard limit: {telemetry:?}"
        );
        assert!(
            telemetry
                .generation_memory_high_water_bytes
                .is_some_and(|bytes| bytes <= DEFAULT_COMPONENT_MEMORY_BYTES),
            "generation memory high-water must retain the trapped instance measurement: {telemetry:?}"
        );
    }

    #[tokio::test]
    async fn caught_memory_failure_then_unrelated_guest_exception_keeps_limiter_category() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let caught = handler.invoke(&request("/catch-memory")).unwrap();
        assert_eq!(caught.body, ResponseBody::Text("allocation caught".into()));

        let error = handler
            .invoke(&request("/caught-memory-then-throw"))
            .unwrap_err();
        assert!(
            matches!(error, EngineError::Trap(SandboxTrap::MemoryExhausted(64))),
            "the limiter signal is invocation-scoped and currently takes precedence over a later guest exception: {error:?}"
        );
    }

    #[tokio::test]
    async fn poisoned_handler_fails_closed_without_reusing_the_guest_instance() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        // Model the terminal state after instance recreation has failed.
        handler.poisoned = true;
        let error = handler.invoke(&request("/health")).unwrap_err();

        assert!(
            matches!(error, EngineError::Internal(_)),
            "A poisoned handler must reject later invocations, got: {error:?}"
        );
        assert!(error
            .to_string()
            .contains("Handler instance is unavailable after a failed recovery attempt"));
        assert_eq!(handler.telemetry_snapshot().instance_recreations, 0);
    }

    #[tokio::test]
    async fn javascript_exception_is_a_stable_guest_trap_and_handler_recovers() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let error = handler.invoke(&request("/throw")).unwrap_err();
        assert!(
            matches!(&error, EngineError::Trap(SandboxTrap::GuestTrap)),
            "Expected a stable guest-trap category, got: {error:?}"
        );
        assert!(
            !error.to_string().contains("guest-controlled secret detail"),
            "Guest-controlled exception text must not enter the public error value"
        );
        assert_eq!(handler.telemetry_snapshot().instance_recreations, 1);

        let response = handler
            .invoke(&request("/health"))
            .expect("handler should recover after an arbitrary guest trap");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, ResponseBody::Text("healthy".into()));
    }

    /// Small enough that the infinite loop stops within milliseconds, but comfortably above
    /// the fuel that component instantiation itself consumes. A 100_000 budget was too tight
    /// on macOS, where instantiation alone exhausted it.
    const LOOP_TEST_FUEL: u64 = 5_000_000;

    #[tokio::test]
    async fn javascript_infinite_loop_is_stopped_by_fuel_budget() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine.instantiate(compiled, LOOP_TEST_FUEL).unwrap();
        let error = handler.invoke(&request("/loop")).unwrap_err();
        assert!(
            matches!(error, EngineError::Trap(SandboxTrap::FuelExhausted)),
            "Expected fuel exhaustion, got: {error}"
        );
        let telemetry = handler.telemetry_snapshot();
        assert!(telemetry.last_invocation_fuel_consumed.is_some());
        assert_eq!(
            telemetry.last_invocation_fuel_consumed,
            Some(LOOP_TEST_FUEL)
        );
        assert_eq!(telemetry.generation_fuel_high_water, Some(LOOP_TEST_FUEL));
    }

    #[tokio::test]
    async fn handler_remains_usable_after_fuel_exhaustion() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine.instantiate(compiled, LOOP_TEST_FUEL).unwrap();

        let error = handler.invoke(&request("/loop")).unwrap_err();
        assert!(
            matches!(error, EngineError::Trap(SandboxTrap::FuelExhausted)),
            "Expected fuel exhaustion, got: {error}"
        );

        let generation_memory_before = handler
            .telemetry_snapshot()
            .generation_memory_high_water_bytes;
        let response = handler
            .invoke(&request("/health"))
            .expect("same handler should accept a healthy request after a guest trap");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, ResponseBody::Text("healthy".into()));
        let telemetry = handler.telemetry_snapshot();
        assert!(telemetry.generation_fuel_high_water.is_some());
        assert!(telemetry.instance_memory_high_water_bytes.is_some());
        if let Some(before) = generation_memory_before {
            assert!(
                telemetry
                    .generation_memory_high_water_bytes
                    .unwrap_or_default()
                    >= before
            );
        }
    }

    #[tokio::test]
    async fn guest_state_resets_and_recreation_counter_increments_after_trap() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine.instantiate(compiled, LOOP_TEST_FUEL).unwrap();

        assert_eq!(
            handler.invoke(&request("/counter")).unwrap().body,
            ResponseBody::Text("1".into())
        );

        let error = handler.invoke(&request("/loop")).unwrap_err();
        assert!(
            matches!(error, EngineError::Trap(SandboxTrap::FuelExhausted)),
            "Expected fuel exhaustion, got: {error}"
        );
        assert_eq!(handler.telemetry_snapshot().instance_recreations, 1);

        // The replacement is a fresh component instance, so module-level state starts over.
        assert_eq!(
            handler.invoke(&request("/counter")).unwrap().body,
            ResponseBody::Text("1".into())
        );
        assert_eq!(handler.telemetry_snapshot().instance_recreations, 1);
    }

    #[tokio::test]
    async fn epoch_deadline_interrupts_guest_independently_of_fuel_and_recovers() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, test_support::EPOCH_TEST_FUEL)
            .unwrap();
        let started = std::time::Instant::now();

        let error = handler
            .invoke_with_timeout(&request("/loop"), test_support::EPOCH_TEST_TIMEOUT)
            .unwrap_err();

        assert!(
            matches!(error, EngineError::Trap(SandboxTrap::ExecutionTimeout)),
            "Expected epoch execution timeout, got: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "Epoch interruption took too long: {:?}",
            started.elapsed()
        );

        let response = handler
            .invoke(&request("/health"))
            .expect("handler should recover after an epoch interruption");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, ResponseBody::Text("healthy".into()));
    }

    #[test]
    fn epoch_deadline_rounds_up_and_never_allows_an_immediate_expiration() {
        assert_eq!(epoch_deadline_ticks(Duration::ZERO), 1);
        assert_eq!(epoch_deadline_ticks(EPOCH_TICK_INTERVAL), 1);
        assert_eq!(
            epoch_deadline_ticks(EPOCH_TICK_INTERVAL + Duration::from_nanos(1)),
            2
        );
    }

    #[tokio::test]
    async fn binary_echo_preserves_non_utf8_bytes() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let body_bytes = vec![0x00, 0x01, 0x7f, 0xff, 0xfe];
        let req = RequestEnvelope {
            method: "POST".to_string(),
            scheme: None,
            authority: None,
            path: "/binary-echo".to_string(),
            query: None,
            headers: Vec::new(),
            body: body_bytes.clone(),
        };
        let response = handler.invoke(&req).unwrap();
        assert_eq!(response.body, ResponseBody::Bytes(body_bytes));
    }

    #[tokio::test]
    async fn body_length_reflects_binary_byte_count() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let body_bytes = vec![1u8; 42];
        let req = RequestEnvelope {
            method: "POST".to_string(),
            scheme: None,
            authority: None,
            path: "/body-length".to_string(),
            query: None,
            headers: Vec::new(),
            body: body_bytes,
        };
        let response = handler.invoke(&req).unwrap();
        assert_eq!(response.body, ResponseBody::Text("42".into()));
    }

    #[tokio::test]
    async fn query_field_carries_raw_encoded_string() {
        let (engine, compiled) = test_support::compiled().await;
        let mut handler = engine
            .instantiate(compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        let req = RequestEnvelope {
            method: "GET".to_string(),
            scheme: None,
            authority: None,
            path: "/query".to_string(),
            query: Some("q=rust%20wasm".to_string()),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let response = handler.invoke(&req).unwrap();
        // The JS handler decodes %20 → space, so it returns "rust wasm"
        assert_eq!(response.body, ResponseBody::Text("rust wasm".into()));
    }
    #[tokio::test]
    async fn utf8_body_round_trip_preserves_bmp_and_supplementary_characters() {
        let source = r#"
            function decodeUtf8(bytes) {
                let encoded = "";
                for (const byte of bytes) encoded += "%" + byte.toString(16).padStart(2, "0");
                return decodeURIComponent(encoded);
            }
            function encodeUtf8(value) {
                const encoded = encodeURIComponent(value);
                const bytes = [];
                for (let i = 0; i < encoded.length; i++) {
                    if (encoded[i] === "%") {
                        bytes.push(parseInt(encoded.slice(i + 1, i + 3), 16));
                        i += 2;
                    } else {
                        bytes.push(encoded.charCodeAt(i));
                    }
                }
                return new Uint8Array(bytes);
            }
            export function handle(request) {
                return {
                    status: 200,
                    headers: [],
                    body: { tag: "bytes", val: encodeUtf8(decodeUtf8(request.body)) }
                };
            }
        "#;
        let engine = QuickJsEngine::new().unwrap();
        let compiled = engine.compile_handler(source).await.unwrap();
        let mut handler = engine
            .instantiate(&compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();
        let original = "snow 雪 and globe 🌍";
        let request = RequestEnvelope {
            method: "POST".into(),
            scheme: None,
            authority: None,
            path: "/utf8".into(),
            query: None,
            headers: vec![],
            body: original.as_bytes().to_vec(),
        };
        let response = handler.invoke(&request).unwrap();
        assert_eq!(
            response.body,
            ResponseBody::Bytes(original.as_bytes().to_vec())
        );
    }

    #[test]
    fn response_size_estimate_includes_headers_and_rejects_excessive_count() {
        let header = || {
            Val::Record(vec![
                ("name".into(), Val::String("x-test".into())),
                ("value".into(), Val::List(vec![Val::U8(b'a')])),
            ])
        };
        let response = Val::Record(vec![
            ("headers".into(), Val::List(vec![header()])),
            (
                "body".into(),
                Val::Variant("text".into(), Some(Box::new(Val::String(String::new())))),
            ),
        ]);
        assert!(estimate_response_size(&response, 128).unwrap() >= 16 + 6 + 1 + 32);

        let too_many = Val::Record(vec![
            (
                "headers".into(),
                Val::List((0..129).map(|_| header()).collect()),
            ),
            (
                "body".into(),
                Val::Variant("text".into(), Some(Box::new(Val::String(String::new())))),
            ),
        ]);
        assert!(estimate_response_size(&too_many, 128)
            .unwrap_err()
            .contains("more than 128 headers"));
    }
}
