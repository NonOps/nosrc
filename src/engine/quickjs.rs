use componentize_qjs::{componentize, ComponentizeOpts, Runtime};
use wasmtime::component::{Component, Linker, TypedFunc};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

use crate::engine::http_envelope::{RequestEnvelope, ResponseEnvelope};
use crate::error::{EngineError, SandboxTrap};

pub const DEFAULT_COMPONENT_FUEL: u64 = 50_000_000;
pub const DEFAULT_COMPONENT_MEMORY_BYTES: usize = 64 * 1024 * 1024;

const HANDLER_WIT: &str = r#"
package nosrc:runtime;

world handler {
    export handle: func(request: string) -> string;
}
"#;

struct ComponentStoreState {
    limits: StoreLimits,
}

/// Wasmtime component-model engine for JavaScript handlers compiled with QuickJS.
pub struct QuickJsEngine {
    engine: Engine,
}

pub struct CompiledHandler {
    component: Component,
}

pub struct QuickJsHandler {
    store: Store<ComponentStoreState>,
    handle: TypedFunc<(String,), (String,)>,
    fuel_budget: u64,
}

impl QuickJsEngine {
    pub fn new() -> Result<Self, EngineError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);

        let engine = Engine::new(&config).map_err(|error| {
            EngineError::Internal(format!("Failed to create component engine: {error}"))
        })?;

        Ok(Self { engine })
    }

    /// Compile an ES module into a standalone QuickJS WebAssembly component.
    ///
    /// The source is intentionally self-contained in this first runtime slice.
    /// Filesystem module imports and WASI capabilities are not granted.
    pub async fn compile_handler(
        &self,
        javascript: &str,
    ) -> Result<CompiledHandler, EngineError> {
        let wit_dir = tempfile::tempdir().map_err(|error| {
            EngineError::Internal(format!("Failed to create temporary WIT directory: {error}"))
        })?;
        let wit_path = wit_dir.path().join("handler.wit");
        std::fs::write(&wit_path, HANDLER_WIT).map_err(|error| {
            EngineError::Internal(format!("Failed to write handler WIT contract: {error}"))
        })?;

        let component_bytes = componentize(
            &ComponentizeOpts {
                wit_path: &wit_path,
                js_source: javascript,
                js_path: None,
                module_root: None,
                world_name: Some("handler"),
                stub_wasi: true,
                disable_gc: false,
                runtime: Runtime::DefaultSync,
            },
        )
        .await
        .map_err(|error| {
            EngineError::Internal(format!("QuickJS componentization failed: {error:#}"))
        })?;

        let component = Component::new(&self.engine, component_bytes)
            .map_err(|error| EngineError::Internal(format!("Invalid handler component: {error}")))?;

        Ok(CompiledHandler { component })
    }

    /// Instantiate a compiled candidate with per-generation resource limits.
    pub fn instantiate(
        &self,
        compiled: &CompiledHandler,
        fuel_budget: u64,
    ) -> Result<QuickJsHandler, EngineError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(DEFAULT_COMPONENT_MEMORY_BYTES)
            .table_elements(10_000)
            .instances(4)
            .tables(4)
            .memories(4)
            .build();

        let mut store = Store::new(&self.engine, ComponentStoreState { limits });
        store
            .set_fuel(fuel_budget)
            .map_err(|error| EngineError::Internal(format!("Failed to set component fuel: {error}")))?;
        store.limiter(|state| &mut state.limits);
        store.set_max_component_handles(1_024);

        let linker = Linker::<ComponentStoreState>::new(&self.engine);
        let instance = linker
            .instantiate(&mut store, &compiled.component)
            .map_err(|error| {
                EngineError::Instantiation(format!("Failed to instantiate handler component: {error}"))
            })?;
        let handle = instance
            .get_typed_func::<(String,), (String,)>(&mut store, "handle")
            .map_err(|error| {
                EngineError::Instantiation(format!(
                    "Handler component must export handle(request: string) -> string: {error}"
                ))
            })?;

        Ok(QuickJsHandler {
            store,
            handle,
            fuel_budget,
        })
    }
}

impl QuickJsHandler {
    /// Invoke the handler while retaining the same component instance between calls.
    /// This preserves JavaScript module state across HTTP requests.
    pub fn invoke(
        &mut self,
        request: &RequestEnvelope,
    ) -> Result<ResponseEnvelope, EngineError> {
        let request_json = serde_json::to_string(request).map_err(|error| {
            EngineError::Internal(format!("Failed to serialize request envelope: {error}"))
        })?;

        self.store.set_fuel(self.fuel_budget).map_err(|error| {
            EngineError::Internal(format!("Failed to reset component fuel: {error}"))
        })?;

        let (response_json,) = self
            .handle
            .call(&mut self.store, (request_json,))
            .map_err(classify_component_trap)?;

        serde_json::from_str(&response_json).map_err(|error| {
            EngineError::Internal(format!(
                "JavaScript handler returned an invalid response envelope: {error}"
            ))
        })
    }
}

fn classify_component_trap(error: wasmtime::Error) -> EngineError {
    if let Some(trap) = error.downcast_ref::<wasmtime::Trap>() {
        if *trap == wasmtime::Trap::OutOfFuel {
            return EngineError::Trap(SandboxTrap::FuelExhausted);
        }
    }

    EngineError::Trap(SandboxTrap::GuestPanic(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn request() -> RequestEnvelope {
        RequestEnvelope {
            method: "GET".to_string(),
            path: "/counter".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        }
    }

    #[tokio::test]
    async fn javascript_module_state_survives_multiple_requests() {
        let engine = QuickJsEngine::new().unwrap();
        let source = r#"
            let count = 0;

            export function handle(requestJson) {
                count += 1;
                return JSON.stringify({
                    status: 200,
                    headers: { "content-type": "text/plain" },
                    body: String(count)
                });
            }
        "#;

        let compiled = engine.compile_handler(source).await.unwrap();
        let mut handler = engine
            .instantiate(&compiled, DEFAULT_COMPONENT_FUEL)
            .unwrap();

        assert_eq!(handler.invoke(&request()).unwrap().body.as_deref(), Some("1"));
        assert_eq!(handler.invoke(&request()).unwrap().body.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn invalid_javascript_is_rejected_before_instantiation() {
        let engine = QuickJsEngine::new().unwrap();
        let result = engine.compile_handler("export function handle( {").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn javascript_infinite_loop_is_stopped_by_fuel_budget() {
        let engine = QuickJsEngine::new().unwrap();
        let source = r#"
            export function handle(requestJson) {
                while (true) {}
            }
        "#;

        let compiled = engine.compile_handler(source).await.unwrap();
        let mut handler = engine.instantiate(&compiled, 100_000).unwrap();
        let error = handler.invoke(&request()).unwrap_err();
        assert!(
            matches!(error, EngineError::Trap(SandboxTrap::FuelExhausted)),
            "Expected fuel exhaustion, got: {error}"
        );
    }
}
