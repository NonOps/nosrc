# `AGENTS.md` — AI Coding Agent Instructions for `nosrc`

## 1. Project Identity & Core Objective
You are building **`nosrc`**, a **single-binary, zero-codebase, Spec-to-Wasm autonomous runtime** written in Rust.
* **Core Premise:** Users drop one static Rust binary into a cluster or host with an OpenAI-compatible LLM endpoint (`LLM_API_KEY`, `LLM_BASE_URL`) and a declarative `SPEC.md`.
* **Execution Model:** `nosrc` synthesizes ES2023 JavaScript handlers from `SPEC.md`, snapshots them into isolated WebAssembly (Wasm) modules in memory via `wasmtime` + embedded QuickJS, verifies them against contract assertions in a shadow sandbox, and atomically swaps live traffic using `ArcSwap`.
* **Self-Healing:** Runtime `5xx` errors or Wasm traps are captured, deduplicated, promoted to regression tests, patched via LLM synthesis, verified in shadow mode, and hot-swapped without dropping requests.

---

## 2. Hard Architectural Invariants (Never Violate)
1. **Zero External Runtime Dependencies:** Never shell out to `node`, `npm`, `cargo`, `rustc`, `gcc`, `python`, or external CLI tools at runtime. Everything must compile, snapshot, and execute in-process inside one static Rust binary (`musl`-compatible).
2. **Strict Capability Isolation:** Wasm guest code must have zero direct OS, filesystem, or socket access. Every side effect must cross the explicit Host ABI boundary (`src/engine/host_abi.rs`).
3. **Pluggable State Toggles:**
   * `--state embedded` (default): Enables bundled SQLite (`rusqlite`) and embedded KV (`redb`).
   * `--state external`: Explicitly disables embedded SQLite and `redb` at the host ABI level (returning `ERR_CAPABILITY_DISABLED`) and routes persistence to external endpoints (`DATABASE_URL`, `REDIS_URL`).
4. **Never Route Unverified Bytecode:** Never swap an active `wasmtime::Module` pointer until 100% of synthesized contract assertions and historical regression tests pass inside an isolated shadow sandbox.
5. **Deterministic Sandboxing:** Every Wasm execution `Store` must enforce CPU fuel limits (`consume_fuel(true)`) and linear memory caps (`ResourceLimiter`, default 64 MB).

---

## 3. Technology Stack & Approved Crates
Keep the dependency tree lean, fast to compile, and static-link friendly:
* **Async Runtime & HTTP:** `tokio` (full features), `axum`, `tower-http`, `reqwest` (with `rustls-tls`, never OpenSSL).
* **Wasm Engine & Snapshotting:** `wasmtime`, `wizer` (or pre-initialized QuickJS Wasm heap snapshot).
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
  * Use `ArcSwap<ActiveGeneration>` for reading the active Wasm module on the hot HTTP request path so reads remain wait-free.
* **Security Rules in `host_abi.rs`:**
  * Block private/loopback/link-local IP ranges (`127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`) inside `host_http_fetch`.
  * Enforce parameterized queries only in `host_db_query`; reject raw multi-statement strings from guest handlers.

---

## 5. Standard Build & Verification Commands
Before concluding any task, run and verify:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

When testing embedded vs. external state modes:

    cargo run -- run --spec ./examples/url-shortener/SPEC.md --state embedded
    cargo run -- run --spec ./examples/url-shortener/SPEC.md --state external

---

## 6. Agent Workflow When Implementing Features
1. **Check `ARCHITECTURE.md` First:** Align module placement, CLI flags, and ABI signatures with `ARCHITECTURE.md`.
2. **Design the Host ABI Contract Before Guest JS:** Whenever adding a capability (KV, SQL, HTTP, Crypto), define the Rust host function, error code, and capability gate first, then update the LLM system prompt schema in `src/synthesizer/`.
3. **Write Shadow Sandbox Tests:** Every new capability or self-healing rule must include an integration test proving that invalid/malicious guest code traps cleanly without crashing the host process.
4. **Keep Milestones Incremental:** Do not build UI dashboards or Kubernetes operators until Milestone 1 (Wasm execution + Host ABI) and Milestone 2 (LLM synthesis + shadow swap) pass end-to-end tests.
