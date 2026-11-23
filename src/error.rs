use thiserror::Error;

#[derive(Error, Debug)]
pub enum EngineError {
    #[error("Wasm compilation error: {0:?}")]
    Compilation(wasmtime::Error),

    #[error("Wasm instantiation error: {0}")]
    Instantiation(String),

    #[error("Wasm execution trapped: {0}")]
    Trap(#[from] SandboxTrap),

    #[error("Host ABI error: {0}")]
    HostAbi(#[from] HostAbiError),

    #[error("Internal engine failure: {0}")]
    Internal(String),
}

#[derive(Error, Debug)]
pub enum SandboxTrap {
    #[error("CPU fuel exhausted (instruction budget exceeded)")]
    FuelExhausted,

    #[error("Guest execution exceeded its wall-clock deadline")]
    ExecutionTimeout,

    #[error("Memory limit exceeded ({0} MB cap)")]
    MemoryExhausted(u32),

    #[error("Unreachable code reached")]
    Unreachable,

    #[error("Guest trapped during execution")]
    GuestTrap,
}

#[derive(Error, Debug)]
pub enum HostAbiError {
    #[error("Capability disabled: {0}")]
    CapabilityDisabled(String),

    #[error("Invalid memory pointer/length: ptr={0}, len={1}")]
    MemoryAccessOutOfBounds(u32, u32),

    #[error("Malformed JSON payload: {0}")]
    MalformedJson(String),

    #[error("Invalid UTF-8 sequence")]
    Utf8Error(#[from] std::string::FromUtf8Error),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Key-value store error: {0}")]
    KvError(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Egress forbidden: domain {domain} is not allowlisted in SPEC.md")]
    EgressForbidden { domain: String },

    #[error("SSRF violation: blocked access to private/link-local address {0}")]
    SsrfBlocked(String),
}
