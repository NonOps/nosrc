pub mod host_abi;
pub mod http_envelope;
pub mod quickjs;

use wasmtime::{
    AsContext, Caller, Config, Engine, Instance, Linker, Module, ResourceLimiter, Store,
};

use crate::error::EngineError;
use host_abi::{
    HostCapabilities, StateMode, ABI_OK, ERR_CAPABILITY_DISABLED, ERR_DB_SQL, ERR_KV_STORE,
    ERR_MALFORMED_JSON, ERR_MEMORY_ACCESS, ERR_NOT_FOUND,
};
use http_envelope::{RequestEnvelope, ResponseEnvelope};

/// Memory limit and fuel budget defaults
pub const DEFAULT_FUEL_BUDGET: u64 = 50_000_000; // ~50ms of CPU instructions
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024; // 64 MB cap

/// Per-instance sandbox state stored inside `wasmtime::Store<SandboxState>`
pub struct SandboxState {
    pub capabilities: HostCapabilities,
    pub limiter: MemoryLimiter,
    /// Scratch response buffer for passing data back into guest memory
    pub last_response: Vec<u8>,
}

/// Linear memory limiter enforcing the 64 MB cap
pub struct MemoryLimiter {
    pub max_memory_bytes: usize,
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        if desired > self.max_memory_bytes {
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        if desired > 10_000 {
            Ok(false)
        } else {
            Ok(true)
        }
    }
}

/// Core execution engine wrapping `wasmtime::Engine` with fuel and strict security
pub struct WasmEngine {
    engine: Engine,
}

impl WasmEngine {
    pub fn new() -> Result<Self, EngineError> {
        let mut config = Config::new();
        // 1. Enforce strict CPU fuel metering
        config.consume_fuel(true);
        // 2. Epoch-based interruption (disabled by default)
        config.epoch_interruption(false);

        let engine = Engine::new(&config)
            .map_err(|e| EngineError::Internal(format!("Failed to create wasmtime engine: {e}")))?;

        Ok(Self { engine })
    }

    /// Compile raw WebAssembly bytecode or WAT text into a validated `wasmtime::Module`
    pub fn compile(&self, wasm_or_wat: &[u8]) -> Result<Module, EngineError> {
        Module::new(&self.engine, wasm_or_wat).map_err(EngineError::Compilation)
    }

    /// Instantiate a module within a sandboxed store
    pub fn instantiate(
        &self,
        module: &Module,
        capabilities: HostCapabilities,
        fuel_budget: u64,
    ) -> Result<(Store<SandboxState>, Instance), EngineError> {
        let state = SandboxState {
            capabilities,
            limiter: MemoryLimiter {
                max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            },
            last_response: Vec::new(),
        };

        let mut store = Store::new(&self.engine, state);
        store
            .set_fuel(fuel_budget)
            .map_err(|e| EngineError::Internal(format!("Failed to set fuel: {e}")))?;

        store.limiter(|s| &mut s.limiter);

        let mut linker = Linker::new(&self.engine);
        self.register_host_abi(&mut linker)?;

        let instance = linker
            .instantiate(&mut store, module)
            .map_err(|e| EngineError::Instantiation(e.to_string()))?;

        Ok((store, instance))
    }

    /// Invokes the guest HTTP entry point (`handle_request`) with a `RequestEnvelope`
    /// and retrieves the resulting `ResponseEnvelope`.
    pub fn invoke_handler(
        &self,
        module: &Module,
        capabilities: HostCapabilities,
        request: &RequestEnvelope,
        fuel_budget: u64,
    ) -> Result<ResponseEnvelope, EngineError> {
        let (mut store, instance) = self.instantiate(module, capabilities, fuel_budget)?;

        let req_json = serde_json::to_vec(request)
            .map_err(|e| EngineError::Internal(format!("Failed to serialize request: {e}")))?;

        // Find exported memory
        let Some(wasmtime::Extern::Memory(mem)) = instance.get_export(&mut store, "memory") else {
            return Err(EngineError::Instantiation(
                "Module missing 'memory' export".into(),
            ));
        };

        // Determine where to write the request in guest memory:
        // Use guest exported `alloc` function if present, otherwise default to offset 1024
        let req_ptr = if let Ok(alloc) = instance.get_typed_func::<i32, i32>(&mut store, "alloc") {
            alloc
                .call(&mut store, req_json.len() as i32)
                .map_err(|e| EngineError::Internal(format!("Guest alloc failed: {e}")))?
        } else {
            1024
        };

        if req_ptr < 0 {
            return Err(EngineError::Internal(
                "Invalid pointer from guest alloc".into(),
            ));
        }

        // Write request envelope into guest memory
        mem.write(&mut store, req_ptr as usize, &req_json)
            .map_err(|e| {
                EngineError::Internal(format!("Failed to write request into memory: {e}"))
            })?;

        // Call exported `handle_request(req_ptr, req_len) -> resp_len | resp_ptr`
        let handle_fn = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "handle_request")
            .map_err(|e| {
                EngineError::Instantiation(format!("Missing 'handle_request' export: {e}"))
            })?;

        let resp_code = handle_fn
            .call(&mut store, (req_ptr, req_json.len() as i32))
            .map_err(|err| {
                if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
                    if *trap == wasmtime::Trap::OutOfFuel {
                        return EngineError::Trap(crate::error::SandboxTrap::FuelExhausted);
                    }
                }
                EngineError::Trap(crate::error::SandboxTrap::GuestPanic(err.to_string()))
            })?;

        // If resp_code < 0, it represents an error code
        if resp_code < 0 {
            return Err(EngineError::Internal(format!(
                "Guest returned error code: {resp_code}"
            )));
        }

        // Read response bytes from store.data().last_response if set by guest calling host_set_response,
        // or directly from guest memory if resp_code represents a length or pointer
        let resp_bytes = if !store.data().last_response.is_empty() {
            store.data().last_response.clone()
        } else {
            let data = mem.data(&store);
            let len = resp_code as usize;
            if req_ptr as usize + len > data.len() {
                return Err(EngineError::Internal(
                    "Response exceeds memory bounds".into(),
                ));
            }
            data[req_ptr as usize..req_ptr as usize + len].to_vec()
        };

        let response: ResponseEnvelope = serde_json::from_slice(&resp_bytes)
            .map_err(|e| EngineError::Internal(format!("Failed to deserialize response: {e}")))?;

        Ok(response)
    }

    /// Register the `nosrc:runtime/host` functions into the Wasm Linker
    fn register_host_abi(&self, linker: &mut Linker<SandboxState>) -> Result<(), EngineError> {
        // 1. Host ABI Version Query
        linker
            .func_wrap("nosrc_host", "host_abi_version", || -> i32 { 1 })
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 2. State Capability Check
        linker
            .func_wrap(
                "nosrc_host",
                "has_embedded_state",
                |caller: Caller<'_, SandboxState>| -> i32 {
                    if caller.data().capabilities.state_mode == StateMode::Embedded {
                        1
                    } else {
                        0
                    }
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 3. Response Buffer Retrieval: returns byte length of last response
        linker
            .func_wrap(
                "nosrc_host",
                "response_len",
                |caller: Caller<'_, SandboxState>| -> i32 {
                    caller.data().last_response.len() as i32
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 3b. Host Set Response: allows guest to set the outgoing response payload
        linker
            .func_wrap(
                "nosrc_host",
                "host_set_response",
                |mut caller: Caller<'_, SandboxState>, ptr: i32, len: i32| -> i32 {
                    let bytes = match read_guest_memory(&mut caller, ptr, len) {
                        Ok(b) => b,
                        Err(code) => return code,
                    };
                    let len = bytes.len() as i32;
                    caller.data_mut().last_response = bytes;
                    len
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 4. Response Buffer Copy: copies last response into guest memory buffer
        linker
            .func_wrap(
                "nosrc_host",
                "response_read",
                |mut caller: Caller<'_, SandboxState>, out_ptr: i32, max_len: i32| -> i32 {
                    if out_ptr < 0 || max_len < 0 {
                        return ERR_MEMORY_ACCESS;
                    }

                    let resp = caller.data().last_response.clone();
                    let copy_len = resp.len().min(max_len as usize);

                    let Some(wasmtime::Extern::Memory(mem)) = caller.get_export("memory") else {
                        return ERR_MEMORY_ACCESS;
                    };

                    if mem
                        .write(&mut caller, out_ptr as usize, &resp[..copy_len])
                        .is_err()
                    {
                        return ERR_MEMORY_ACCESS;
                    }

                    copy_len as i32
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 5. KV Get: host_kv_get(key_ptr, key_len) -> status / length
        linker
            .func_wrap(
                "nosrc_host",
                "host_kv_get",
                |mut caller: Caller<'_, SandboxState>, key_ptr: i32, key_len: i32| -> i32 {
                    let key_bytes = match read_guest_memory(&mut caller, key_ptr, key_len) {
                        Ok(b) => b,
                        Err(code) => return code,
                    };
                    let key = match std::str::from_utf8(&key_bytes) {
                        Ok(s) => s,
                        Err(_) => return ERR_MALFORMED_JSON,
                    };

                    let caps = caller.data().capabilities.clone();
                    match caps.kv_get(key) {
                        Ok(Some(val)) => {
                            let len = val.len() as i32;
                            caller.data_mut().last_response = val;
                            len
                        }
                        Ok(None) => ERR_NOT_FOUND,
                        Err(crate::error::HostAbiError::CapabilityDisabled(_)) => {
                            ERR_CAPABILITY_DISABLED
                        }
                        Err(_) => ERR_KV_STORE,
                    }
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 6. KV Set: host_kv_set(key_ptr, key_len, val_ptr, val_len) -> ABI_OK or error
        linker
            .func_wrap(
                "nosrc_host",
                "host_kv_set",
                |mut caller: Caller<'_, SandboxState>,
                 key_ptr: i32,
                 key_len: i32,
                 val_ptr: i32,
                 val_len: i32|
                 -> i32 {
                    let key_bytes = match read_guest_memory(&mut caller, key_ptr, key_len) {
                        Ok(b) => b,
                        Err(code) => return code,
                    };
                    let key = match std::str::from_utf8(&key_bytes) {
                        Ok(s) => s,
                        Err(_) => return ERR_MALFORMED_JSON,
                    };

                    let val_bytes = match read_guest_memory(&mut caller, val_ptr, val_len) {
                        Ok(b) => b,
                        Err(code) => return code,
                    };

                    let caps = caller.data().capabilities.clone();
                    match caps.kv_set(key, &val_bytes) {
                        Ok(()) => ABI_OK,
                        Err(crate::error::HostAbiError::CapabilityDisabled(_)) => {
                            ERR_CAPABILITY_DISABLED
                        }
                        Err(_) => ERR_KV_STORE,
                    }
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 7. KV Delete: host_kv_delete(key_ptr, key_len) -> 1 if deleted, 0 if not found, negative if error
        linker
            .func_wrap(
                "nosrc_host",
                "host_kv_delete",
                |mut caller: Caller<'_, SandboxState>, key_ptr: i32, key_len: i32| -> i32 {
                    let key_bytes = match read_guest_memory(&mut caller, key_ptr, key_len) {
                        Ok(b) => b,
                        Err(code) => return code,
                    };
                    let key = match std::str::from_utf8(&key_bytes) {
                        Ok(s) => s,
                        Err(_) => return ERR_MALFORMED_JSON,
                    };

                    let caps = caller.data().capabilities.clone();
                    match caps.kv_delete(key) {
                        Ok(true) => 1,
                        Ok(false) => 0,
                        Err(crate::error::HostAbiError::CapabilityDisabled(_)) => {
                            ERR_CAPABILITY_DISABLED
                        }
                        Err(_) => ERR_KV_STORE,
                    }
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 8. DB Execute: host_db_execute(sql_ptr, sql_len, params_ptr, params_len) -> affected rows
        linker
            .func_wrap(
                "nosrc_host",
                "host_db_execute",
                |mut caller: Caller<'_, SandboxState>,
                 sql_ptr: i32,
                 sql_len: i32,
                 params_ptr: i32,
                 params_len: i32|
                 -> i32 {
                    let sql_bytes = match read_guest_memory(&mut caller, sql_ptr, sql_len) {
                        Ok(b) => b,
                        Err(code) => return code,
                    };
                    let sql = match std::str::from_utf8(&sql_bytes) {
                        Ok(s) => s,
                        Err(_) => return ERR_MALFORMED_JSON,
                    };

                    let params_json = if params_len > 0 {
                        let params_bytes =
                            match read_guest_memory(&mut caller, params_ptr, params_len) {
                                Ok(b) => b,
                                Err(code) => return code,
                            };
                        match serde_json::from_slice(&params_bytes) {
                            Ok(v) => v,
                            Err(_) => return ERR_MALFORMED_JSON,
                        }
                    } else {
                        serde_json::Value::Array(Vec::new())
                    };

                    let caps = caller.data().capabilities.clone();
                    let result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(async { caps.db_execute(sql, &params_json).await })
                    });

                    match result {
                        Ok(affected) => affected as i32,
                        Err(crate::error::HostAbiError::CapabilityDisabled(_)) => {
                            ERR_CAPABILITY_DISABLED
                        }
                        Err(_) => ERR_DB_SQL,
                    }
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // 9. DB Query: host_db_query(sql_ptr, sql_len, params_ptr, params_len) -> JSON response len
        linker
            .func_wrap(
                "nosrc_host",
                "host_db_query",
                |mut caller: Caller<'_, SandboxState>,
                 sql_ptr: i32,
                 sql_len: i32,
                 params_ptr: i32,
                 params_len: i32|
                 -> i32 {
                    let sql_bytes = match read_guest_memory(&mut caller, sql_ptr, sql_len) {
                        Ok(b) => b,
                        Err(code) => return code,
                    };
                    let sql = match std::str::from_utf8(&sql_bytes) {
                        Ok(s) => s,
                        Err(_) => return ERR_MALFORMED_JSON,
                    };

                    let params_json = if params_len > 0 {
                        let params_bytes =
                            match read_guest_memory(&mut caller, params_ptr, params_len) {
                                Ok(b) => b,
                                Err(code) => return code,
                            };
                        match serde_json::from_slice(&params_bytes) {
                            Ok(v) => v,
                            Err(_) => return ERR_MALFORMED_JSON,
                        }
                    } else {
                        serde_json::Value::Array(Vec::new())
                    };

                    let caps = caller.data().capabilities.clone();
                    let result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(async { caps.db_query(sql, &params_json).await })
                    });

                    match result {
                        Ok(rows) => {
                            let json_bytes = match serde_json::to_vec(&rows) {
                                Ok(b) => b,
                                Err(_) => return ERR_MALFORMED_JSON,
                            };
                            let len = json_bytes.len() as i32;
                            caller.data_mut().last_response = json_bytes;
                            len
                        }
                        Err(crate::error::HostAbiError::CapabilityDisabled(_)) => {
                            ERR_CAPABILITY_DISABLED
                        }
                        Err(_) => ERR_DB_SQL,
                    }
                },
            )
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        Ok(())
    }
}

/// Helper to read guest linear memory
fn read_guest_memory(
    caller: &mut Caller<'_, SandboxState>,
    ptr: i32,
    len: i32,
) -> Result<Vec<u8>, i32> {
    if ptr < 0 || len < 0 {
        return Err(ERR_MEMORY_ACCESS);
    }
    let Some(wasmtime::Extern::Memory(mem)) = caller.get_export("memory") else {
        return Err(ERR_MEMORY_ACCESS);
    };

    let start = ptr as usize;
    let end = start.checked_add(len as usize).ok_or(ERR_MEMORY_ACCESS)?;

    let data = mem.data(caller.as_context());
    if end > data.len() {
        return Err(ERR_MEMORY_ACCESS);
    }

    Ok(data[start..end].to_vec())
}

#[cfg(test)]
mod tests;
