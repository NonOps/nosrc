use componentize_qjs::{componentize, ComponentizeOpts, Runtime};
use sha2::{Digest, Sha256};
use std::thread;
use std::time::Duration;
use wasmtime::component::{Component, Linker, TypedFunc};
use wasmtime::{Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder};

use crate::engine::http_envelope::{RequestEnvelope, ResponseEnvelope, MAX_HANDLER_RESPONSE_BYTES};
use crate::error::{EngineError, SandboxTrap};

pub const DEFAULT_COMPONENT_FUEL: u64 = 50_000_000;
pub const DEFAULT_COMPONENT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_GUEST_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(test)]
const EPOCH_TICK_INTERVAL: Duration = Duration::from_micros(100);
const COMPONENT_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

pub const HANDLER_WIT: &str = r#"
package nosrc:runtime;

world handler {
    export handle: func(request: string) -> string;
}
"#;

struct ComponentStoreState {
    limits: StoreLimits,
    memory_high_water_bytes: Option<usize>,
    pending_memory_growth: Option<(usize, usize)>,
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

type ComponentHandle = TypedFunc<(String,), (String,)>;
type InstantiatedComponent = (Store<ComponentStoreState>, ComponentHandle);

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
    handle: ComponentHandle,
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
        },
    );
    store
        .set_fuel(fuel_budget)
        .map_err(|error| EngineError::Internal(format!("Failed to set component fuel: {error}")))?;
    store.limiter(|state| state);
    store.set_max_component_handles(1_024);
    store.set_epoch_deadline(epoch_deadline_ticks(COMPONENT_STARTUP_TIMEOUT));

    let linker = Linker::<ComponentStoreState>::new(engine);
    let instance = linker
        .instantiate(&mut store, component)
        .map_err(classify_component_error)?;
    let handle = instance
        .get_typed_func::<(String,), (String,)>(&mut store, "handle")
        .map_err(|error| {
            EngineError::Instantiation(format!(
                "Handler component must export handle(request: string) -> string: {error}"
            ))
        })?;

    store
        .set_fuel(fuel_budget)
        .map_err(|error| EngineError::Internal(format!("Failed to set component fuel: {error}")))?;
    // Component startup allocations are part of the instance high-water baseline.
    store.data_mut().pending_memory_growth = None;

    Ok((store, handle))
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
        let request_json = serde_json::to_string(request).map_err(|error| {
            EngineError::Internal(format!("Failed to serialize request envelope: {error}"))
        })?;

        self.store.set_fuel(self.fuel_budget).map_err(|error| {
            EngineError::Internal(format!("Failed to reset component fuel: {error}"))
        })?;
        self.store.set_epoch_deadline(epoch_deadline_ticks(timeout));

        let response_json = match self.handle.call(&mut self.store, (request_json,)) {
            Ok((response_json,)) => response_json,
            Err(error) => {
                self.record_invocation_telemetry();
                let trap = classify_component_error(error);
                let failure_kind = match &trap {
                    EngineError::Trap(SandboxTrap::FuelExhausted) => "fuel_exhausted",
                    EngineError::Trap(SandboxTrap::ExecutionTimeout) => "execution_timeout",
                    EngineError::Trap(SandboxTrap::MemoryExhausted(_)) => "memory_exhausted",
                    EngineError::Trap(SandboxTrap::GuestPanic(_)) => "guest_panic",
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
                return Err(trap);
            }
        };

        self.record_invocation_telemetry();

        if response_json.len() > self.response_limit_bytes {
            return Err(EngineError::Internal(format!(
                "JavaScript handler response exceeds the {} byte limit",
                self.response_limit_bytes
            )));
        }

        serde_json::from_str(&response_json).map_err(|error| {
            EngineError::Internal(format!(
                "JavaScript handler returned an invalid response envelope: {error}"
            ))
        })
    }
}

fn classify_component_error(error: wasmtime::Error) -> EngineError {
    if let Some(trap) = error.downcast_ref::<wasmtime::Trap>() {
        match trap {
            wasmtime::Trap::OutOfFuel => EngineError::Trap(SandboxTrap::FuelExhausted),
            wasmtime::Trap::Interrupt => EngineError::Trap(SandboxTrap::ExecutionTimeout),
            _ => EngineError::Trap(SandboxTrap::GuestPanic(error.to_string())),
        }
    } else {
        EngineError::Instantiation(error.to_string())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use tokio::sync::OnceCell;

    // Give epoch tests a larger test-only fuel budget so fuel exhaustion cannot win
    // the race against the accelerated test ticker. Production limits remain unchanged.
    pub const EPOCH_TEST_FUEL: u64 = 5_000_000_000;
    pub const EPOCH_TEST_TIMEOUT: Duration = Duration::from_millis(1);

    pub const HANDLER_SOURCE: &str = r#"
        let count = 0;

        export function handle(requestJson) {
            const request = JSON.parse(requestJson);

            if (request.path === "/loop") {
                while (true) {}
            }

            if (request.path === "/memory") {
                // Force QuickJS to grow its Wasm linear memory beyond the 64 MiB store limit.
                new Uint8Array(128 * 1024 * 1024);
                return JSON.stringify({ status: 200, headers: {}, body: "unexpected allocation success" });
            }

            if (request.path === "/error") {
                return JSON.stringify({ status: 500, headers: {}, body: "failed" });
            }

            if (request.path === "/small-oversized") {
                return JSON.stringify({ status: 200, headers: {}, body: "x".repeat(1024) });
            }

            if (request.path === "/oversized") {
                return JSON.stringify({ status: 200, headers: {}, body: "x".repeat(1024 * 1024 + 1) });
            }

            if (request.path === "/health") {
                return JSON.stringify({
                    status: 200,
                    headers: { "Content-Type": "text/plain" },
                    body: "healthy"
                });
            }

            if (request.path === "/echo") {
                return JSON.stringify({
                    status: 200,
                    headers: {},
                    body: JSON.stringify({ headers: request.headers, query: request.query })
                });
            }

            if (request.path === "/query") {
                return JSON.stringify({
                    status: 200,
                    headers: { "content-type": "text/plain" },
                    body: request.query.q || "missing"
                });
            }

            if (request.path === "/counter") {
                count += 1;
                return JSON.stringify({ status: 200, headers: {}, body: String(count) });
            }

            return JSON.stringify({
                status: 200,
                headers: { "content-type": "text/plain" },
                body: request.method + " " + request.path
            });
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
    use std::collections::HashMap;

    fn request(path: &str) -> RequestEnvelope {
        RequestEnvelope {
            method: "GET".to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        }
    }

    #[test]
    fn committed_memory_growth_cannot_be_rolled_back_by_a_later_failure_callback() {
        let mut state = ComponentStoreState {
            limits: StoreLimitsBuilder::new()
                .memory_size(DEFAULT_COMPONENT_MEMORY_BYTES)
                .build(),
            memory_high_water_bytes: Some(8),
            pending_memory_growth: None,
        };

        assert!(state.memory_growing(8, 16, None).unwrap());
        assert_eq!(state.memory_high_water_bytes, Some(16));
        assert_eq!(state.pending_memory_growth, Some((8, 16)));

        state.commit_pending_memory_growth();
        assert_eq!(state.pending_memory_growth, None);

        state
            .memory_grow_failed(wasmtime::Error::msg("late failure callback"))
            .unwrap();
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
            handler
                .invoke(&request("/counter"))
                .unwrap()
                .body
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            handler
                .invoke(&request("/counter"))
                .unwrap()
                .body
                .as_deref(),
            Some("2")
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
            matches!(result, Err(EngineError::Trap(SandboxTrap::GuestPanic(_)))),
            "Expected the guest allocation to fail under the 64 MiB memory cap, got: {result:?}"
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
        assert_eq!(response.body.as_deref(), Some("healthy"));
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
            handler
                .invoke(&request("/counter"))
                .unwrap()
                .body
                .as_deref(),
            Some("1")
        );

        let error = handler.invoke(&request("/loop")).unwrap_err();
        assert!(
            matches!(error, EngineError::Trap(SandboxTrap::FuelExhausted)),
            "Expected fuel exhaustion, got: {error}"
        );
        assert_eq!(handler.telemetry_snapshot().instance_recreations, 1);

        // The replacement is a fresh component instance, so module-level state starts over.
        assert_eq!(
            handler
                .invoke(&request("/counter"))
                .unwrap()
                .body
                .as_deref(),
            Some("1")
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
        assert_eq!(response.body.as_deref(), Some("healthy"));
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
}
