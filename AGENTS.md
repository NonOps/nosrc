# `AGENTS.md` — AI Coding Agent Instructions for `nosrc`

## 1. Project Identity & Core Objective
You are building **`nosrc`**, an experimental Rust service intended to turn a declarative `SPEC.md` into verified JavaScript request handlers executed in a WebAssembly sandbox.

**Current implementation:** the CLI reads and parses `SPEC.md`, initializes the existing Wasmtime core-module engine, and serves HTTP health/status endpoints. The application request path returns `503` until a verified handler generation is explicitly activated. The complete synthesis-to-activation path is not implemented yet.

**Chosen direction:** JavaScript remains the handler language. The planned runtime is embedded QuickJS componentized to WebAssembly and hosted by Wasmtime's component model. Implement and validate this as a coherent execution path; do not mix two Wasmtime major versions in the binary.

**Explicitly out of scope:** autonomous self-healing, automatic regeneration from production failures, and automatic rollback.

---

## 2. Hard Architectural Invariants (Never Violate)
1. **Zero External Runtime Dependencies:** Never shell out to `node`, `npm`, `cargo`, `rustc`, `gcc`, `python`, or external CLI tools at runtime. Everything must compile, snapshot, and execute in-process inside one static Rust binary (`musl`-compatible).
2. **Strict Capability Isolation:** Wasm guest code must have zero direct OS, filesystem, or socket access. Every side effect must cross the explicit Host ABI boundary (`src/engine/host_abi.rs`).
3. **Honest State-Mode Semantics:** The current `--state` option is parsed and logged only. Do not claim embedded or external persistence is selected by the running service until the request lifecycle actually wires the corresponding capability implementations.
4. **Never Route Unverified Bytecode:** Never swap an active `wasmtime::Module` pointer until 100% of synthesized contract assertions and historical regression tests pass inside an isolated shadow sandbox.
5. **Deterministic Sandboxing:** Every guest execution must enforce CPU fuel limits and a bounded memory budget. Prove that these limits apply to the final QuickJS component runtime before exposing it to requests.

---

## 3. Technology Stack & Approved Crates
Keep the dependency tree lean, fast to compile, and static-link friendly:
* **Async Runtime & HTTP:** `tokio` (full features), `axum`, `tower-http`, `reqwest` (with `rustls-tls`, never OpenSSL).
* **Wasm Engine & Componentization:** `wasmtime` with the selected QuickJS componentization toolchain. Pin compatible versions and avoid duplicate Wasmtime major versions.
* **Lock-Free Hot-Swapping & Concurrency:** `arc-swap`, `dashmap`.
* **Embedded State:** `rusqlite` (with `bundled` feature), `redb`.
* **External State (Optional):** `sqlx` (runtime-tokio-rustls, postgres), `redis`.
* **CLI, File Watching & Serialization:** `clap` (derive), `notify`, `serde`, `serde_json`, `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`.

---

## 4. Coding Standards & Rust Conventions
* **Error Handling:**
  * Use `thiserror` for structured domain and Host ABI errors (`EngineError`, `SandboxTrap`, `CapabilityDisabled`).
  * Use `anyhow::Result` in `main.rs`, CLI setup, and top-level background workers.
  * Never call `.unwrap()` or `.expect()` on network inputs, LLM responses, or Wasm guest outputs in production paths.
* **Concurrency & Locking:**
  * Never hold a `std::sync::Mutex` or `rusqlite::Connection` across an `.await` point. Offload blocking SQLite/Wasm execution to `tokio::task::spawn_blocking` or dedicated worker channels.
  * Use the existing `ArcSwap<Option<ActiveGeneration>>` pattern for reading the active generation on the HTTP request path so reads remain wait-free.
* **Security Rules in `host_abi.rs`:**
  * Block private/loopback/link-local IP ranges (`127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`) inside `host_http_fetch`.
  * Enforce parameterized queries only in `host_db_query`; reject raw multi-statement strings from guest handlers.

---

## 5. Standard Build & Verification Commands
Before concluding any task, run and verify:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

Run the service with an explicit SPEC path and port:

    cargo run -- --spec ./SPEC.md --port 8080

The `--state` option is currently a placeholder and does not select a working persistence backend.

---

## 6. Agent Workflow When Implementing Features
1. **Check `ARCHITECTURE.md` First:** Align module placement, CLI flags, and ABI signatures with `ARCHITECTURE.md`.
2. **Design the Host ABI Contract Before Guest JS:** Whenever adding a capability (KV, SQL, HTTP, Crypto), define the Rust host function, error code, and capability gate first, then update the LLM system prompt schema in `src/synthesizer/`.
3. **Write Shadow Sandbox Tests:** Every new capability or runtime behavior must include an integration test proving that invalid/malicious guest code traps cleanly without crashing the host process.
4. **Keep Milestones Incremental:** Complete the single-process JavaScript-to-Wasm request lifecycle, including persistence and candidate verification, before adding file watching, autonomous behavior, external persistence, dashboards, or Kubernetes operators.
