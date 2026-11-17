# `AGENTS.md` — AI Coding Agent Instructions for `nosrc`

## 1. Project Identity & Core Objective
You are building **`nosrc`**, an experimental Rust service intended to turn a declarative `SPEC.md` into verified JavaScript request handlers executed in a WebAssembly sandbox.

**Current implementation:** the CLI reads and parses `SPEC.md`, synthesizes JavaScript and contract assertions through the configured OpenAI-compatible endpoint, componentizes the handler with `componentize-qjs`, verifies it in an isolated candidate instance, and activates a fresh production instance before starting HTTP ingress. The runtime uses Wasmtime 50 and requires Rust 1.97 or later.

**Runtime limitations:** module-level JavaScript state persists across requests within one active generation only. Requests to a generation are serialized. No guest filesystem, process, network, SQL, KV, or WASI capabilities are exposed. Durable state, host capabilities, and hot-reload are not implemented.

**Explicitly out of scope:** autonomous self-healing, automatic regeneration from production failures, and automatic rollback.

---

## 2. Hard Architectural Invariants (Never Violate)
1. **Zero External Runtime Dependencies:** Never shell out to `node`, `npm`, `cargo`, `rustc`, `gcc`, `python`, or external CLI tools at runtime. Everything must compile, snapshot, and execute in-process inside one static Rust binary (`musl`-compatible).
2. **Strict Capability Isolation:** QuickJS guest code must have zero direct OS, filesystem, process, or socket access. The current component world exposes only the handler entrypoint; do not add capabilities without a narrow WIT interface and tests.
3. **Honest State Semantics:** JavaScript module-level state is in-memory and generation-scoped. It is not durable across restarts or handler replacement. Do not claim SQLite, redb, PostgreSQL, or Redis persistence is wired into the QuickJS request path.
4. **Never Route Unverified Code:** Never activate a QuickJS component until it compiles, instantiates, and passes 100% of the human-authored SPEC verification cases and the separate model-generated supplemental assertions. Run the suites in separate isolated candidate instances, then instantiate a separate production instance.
5. **Deterministic Sandboxing:** Every QuickJS component store must enforce CPU fuel limits and a bounded linear-memory budget. Keep tests against the component runtime, not only the legacy core-module engine.

---

## 3. Technology Stack & Approved Crates
Keep the dependency tree lean, fast to compile, and static-link friendly:
* **Async Runtime & HTTP:** `tokio` (full features), `axum`, `tower-http`, `reqwest` (with `rustls-tls`, never OpenSSL).
* **Wasm Engine & Componentization:** `wasmtime = 50` and `componentize-qjs = 0.6`. Keep one Wasmtime major version in the executable; the dependency requires Rust 1.97 or later.
* **Lock-Free Hot-Swapping & Concurrency:** `arc-swap`, `dashmap`.
* **State:** JavaScript module state is currently in-memory only. SQLite and redb are legacy prototypes, not exposed to the QuickJS component.
* **External State:** deferred; do not add PostgreSQL/Redis until the embedded state contract and isolation model are designed.
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

Run the service with an explicit SPEC path and port (requires `LLM_API_KEY`):

    cargo run -- --spec ./SPEC.md --port 8080

---

## 6. Agent Workflow When Implementing Features
1. **Check `ARCHITECTURE.md` First:** Align module placement, CLI flags, and ABI signatures with `ARCHITECTURE.md`.
2. **Design the WIT Contract Before Guest JS:** Whenever adding a capability (KV, SQL, HTTP, Crypto), define its WIT interface, Rust host implementation, capability gate, resource bounds, and tests first, then update the LLM system prompt in `src/synthesizer/`.
3. **Write Component Sandbox Tests:** Every new capability or runtime behavior must include an integration test proving that invalid or resource-exhausting guest code is contained without crashing the host process.
4. **Keep Milestones Incremental:** Complete the single-process JavaScript-to-Wasm request lifecycle, including persistence and candidate verification, before adding file watching, autonomous behavior, external persistence, dashboards, or Kubernetes operators.
