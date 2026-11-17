# `nosrc` — Architecture Document
**Zero-Codebase, Spec-to-Wasm Autonomous Cluster Runtime**

## 1. Executive Summary & Core Philosophy
`nosrc` eliminates source code repositories, build pipelines, container registries, and deployment manifests. Developers deploy a **single static Rust binary** into a cluster or host, provide an OpenAI-compatible LLM endpoint (`LLM_API_KEY`, `LLM_BASE_URL`), and mount or push a declarative specification (`SPEC.md`).

`nosrc` continuously synthesizes, compiles, verifies, and hot-swaps isolated WebAssembly (Wasm) modules entirely in memory. When live traffic triggers unhandled edge cases, `nosrc` captures the failing interaction, updates its internal regression suite, regenerates the Wasm module, verifies behavioral compliance in a shadow sandbox, and atomically swaps traffic with zero dropped connections.

### Design Principles
1. **Single-Binary Autonomy:** Zero external compilers, CLIs, Node/Python runtimes, or orchestration frameworks required inside the container.
2. **Capability-Based Security:** AI-synthesized logic runs strictly inside a `wasmtime` sandbox with deterministic CPU fuel limits, memory caps, and explicit host-function allowlists.
3. **Atomic Zero-Downtime Evolution:** Traffic never routes to unverified Wasm bytecode. Every mutation passes contract assertions in a shadow pool before pointer swap.
4. **Pluggable Persistence:** Runs stateful out of the box with embedded SQLite/KV (`--state embedded`), or completely stateless when wired to external databases (`--state external`).

---

## 2. High-Level System Architecture

    External Inputs:
      • Live HTTP/Webhook Traffic (:8080)
      • SPEC.md (File Watcher / Control API)
      • OpenAI-Compatible LLM Endpoint
                        │
                        ▼
    ┌──────────────────────────────────────────────────────────────────────┐
    │                    `nosrc` Single Static Binary                      │
    │                                                                      │
    │  ┌──────────────────────┐  ArcSwap   ┌────────────────────────────┐  │
    │  │   Axum Ingress &     │ ─────────► │ Active Wasm Execution Pool │  │
    │  │   Control Plane      │            │  Wasmtime + QuickJS Wasm   │  │
    │  └──────────┬───────────┘            │  (Fuel-Metered Sandbox)    │  │
    │             │                        └─────────────┬──────────────┘  │
    │             ▼                                      │ Host ABI        │
    │  ┌──────────────────────┐            ┌─────────────▼──────────────┐  │
    │  │  Autonomous Evolution│            │ Pluggable Host Capabilities│  │
    │  │  & Healing Loop      │            │ • Embedded: SQLite + redb  │  │
    │  │  1. Spec/Error Diff  │            │ • External: Postgres/HTTP  │  │
    │  │  2. LLM Synthesis    │            │ • Secrets, Clock, Crypto   │  │
    │  │  3. Wizer Snapshot   │            └────────────────────────────┘  │
    │  │  4. Shadow Verifier  │                                            │
    │  └──────────────────────┘                                            │
    └──────────────────────────────────────────────────────────────────────┘

---

## 3. Core Subsystems

### 3.1 Ingress & Control Plane (`axum` + `tokio`)
* **Data Plane (`:8080`):** Accepts all incoming HTTP/JSON/Webhook traffic, serializes request envelopes (method, path, headers, query, body) into a compact binary/JSON payload, and dispatches them to the active Wasm module pool.
* **Control Plane (`:9090` or Unix Socket / File Watcher):**
  * Watches `SPEC.md` via `notify` (inotify/kqueue) for instant local updates.
  * Exposes `POST /_nosrc/spec` for remote CI-less spec pushes.
  * Exposes `GET /_nosrc/status` and `GET /_nosrc/generations` for inspecting active Wasm hashes, fuel usage, and self-healing history.

### 3.2 In-Memory Synthesis & Compilation Pipeline
To achieve ~20–30 ms cold-start and hot-swap latency without bundling heavy LLVM toolchains:
1. **Base Runtime Embedding:** At build time, `nosrc` embeds a pre-compiled QuickJS Wasm binary (`quickjs_base.wasm`, ~1.5 MB) via `include_bytes!`.
2. **Code & Contract Synthesis:** When `SPEC.md` updates (or a runtime anomaly occurs), the **Synthesizer** sends the spec, active database schema, and accumulated regression cases to the configured LLM endpoint. The LLM returns a structured JSON payload containing:
   * `handler_js`: Pure ES2023 handler module exporting `handle(req)`.
   * `assertions`: Executable contract tests (`input` -> `expected_status` / `schema_match`).
3. **In-Memory Heap Snapshotting (`wizer`):** Instead of parsing JS on every HTTP request, `nosrc` instantiates `quickjs_base.wasm` in memory, evaluates `handler_js` once during initialization, and snapshots the initialized linear memory into a standalone, ready-to-execute `.wasm` module (`wasmtime::Module`).

### 3.3 Shadow Verifier & Atomic Hot-Swap (`ArcSwap`)
* **Shadow Sandbox:** Every newly compiled `wasmtime::Module` is instantiated against an isolated ephemeral state clone (in-memory SQLite `:memory:` or transactional rollback) and executed against all synthesized and historical regression assertions.
* **Gatekeeping:** If any assertion fails or exceeds the fuel budget, the candidate module is rejected and the error trace is fed back to the Synthesizer (up to `MAX_RETRIES = 3`).
* **Zero-Downtime Swap:** Once verified, `nosrc` swaps the global `ArcSwap<ActiveGeneration>` pointer. In-flight requests finish cleanly on the previous generation; new requests immediately hit the new generation.

### 3.4 Pluggable Host Capabilities (ABI Boundary)
Wasm modules have zero direct OS, filesystem, or network access. All side effects cross a strict, versioned Host ABI (`nosrc:runtime/host@0.1.0`):

| Capability | `--state embedded` (Default) | `--state external` |
| :--- | :--- | :--- |
| **Relational DB** | Enabled (`rusqlite` bundled with WAL mode) | **Disabled** (Host returns `ERR_CAPABILITY_DISABLED`) |
| **Key-Value Store** | Enabled (`redb` embedded ACID store) | **Disabled** (Host returns `ERR_CAPABILITY_DISABLED`) |
| **External DB / Cache** | Optional via env vars | Enabled (`DATABASE_URL` via `sqlx`, `REDIS_URL`) |
| **Outbound HTTP** | Allowlisted domains from `SPEC.md` | Allowlisted domains from `SPEC.md` |
| **Determinism & Clock** | Host-provided UTC timestamp & UUIDv7 | Host-provided UTC timestamp & UUIDv7 |

### 3.5 Autonomous Self-Healing Loop
1. **Anomaly Capture:** If the active Wasm module panics, runs out of fuel, or returns an unhandled `5xx` error not defined in `SPEC.md`, the ingress layer captures the sanitized request envelope and stack trace.
2. **Deduplication & Rate Limiting:** Errors are hashed by route + error signature (`DashMap`) with a circuit breaker to prevent LLM token storms during traffic spikes.
3. **Regression Promotion:** The failing request is converted into a permanent regression test case stored in `.nosrc/regressions.json`.
4. **Autonomous Patch & Swap:** The Synthesizer generates an updated module that satisfies both `SPEC.md` and the new regression test, verifies it in the shadow sandbox, and hot-swaps production traffic automatically.

---

## 4. Security & Sandboxing Model
Running AI-generated logic in production requires defense-in-depth:
* **CPU Fuel Metering:** `wasmtime::Config::consume_fuel(true)` assigns a strict instruction budget per request (default: `50_000_000` fuel units, ~50ms CPU). Infinite loops trap deterministically without affecting other worker threads.
* **Memory Caps:** `ResourceLimiter` restricts each Wasm instance linear memory to `64 MB` max.
* **Egress Allowlisting:** `host_http_fetch` blocks all private RFC1918/link-local IP ranges (`169.254.169.254` cloud metadata protection) and permits only domains explicitly declared in `SPEC.md`.
* **SQL Parameterization:** `host_db_query(sql, params_json)` enforces bound parameters only, preventing SQL injection even if synthesized handler logic is flawed.

---

## 5. Configuration & Deployment Contract

### Environment Variables & CLI Flags

    nosrc run \
      --spec ./SPEC.md \
      --listen 0.0.0.0:8080 \
      --state embedded \              # Options: embedded | external
      --data-dir /var/lib/nosrc \     # Used only when --state embedded
      --max-fuel 50000000 \
      --self-heal true

Required environment variables:
* `LLM_API_KEY`: API key for synthesis/healing.
* `LLM_BASE_URL`: OpenAI-compatible endpoint (e.g., `https://openai.com`, OpenRouter, or local `vLLM`/`Ollama`).
* `LLM_MODEL`: Target model identifier (e.g., `gpt-4o`, `claude-3-7-sonnet`, `qwen-2.5-coder-32b`).

### Reference `SPEC.md` Format

    # Service: URL Shortener & Analytics
    ## Capabilities
    - state: embedded (sqlite)
    - egress: []

    ## Schema
    - `links(code TEXT PRIMARY KEY, target_url TEXT NOT NULL, clicks INTEGER DEFAULT 0, created_at TEXT)`

    ## Endpoints
    ### `POST /shorten`
    - Body: `{ "url": string }`
    - Rules: Validate valid http/https URL. Generate 6-char alphanumeric code. Store in `links`.
    - Response `201`: `{ "code": string, "short_url": string }`

    ### `GET /:code`
    - Rules: Lookup `code`. If missing return `404`. Increment `clicks` atomically and return `302` redirect to `target_url`.

---

## 6. Implementation Roadmap (Milestones)

1. **Milestone 1 — The Wasm Heart (`engine/`):** Boot `wasmtime` with `quickjs_base.wasm`, implement `host_abi.rs` with `--state embedded` (SQLite + `redb`) and `--state external` capability toggles, and serve a hardcoded JS handler via `axum`.
2. **Milestone 2 — Spec-to-Module Compiler (`synthesizer/`):** Watch `SPEC.md`, call the LLM endpoint to generate `handler_js` + JSON contract tests, verify in shadow sandbox, and swap via `ArcSwap`.
3. **Milestone 3 — Autonomous Self-Healer (`healer/`):** Wire 5xx/trap telemetry into the regression store and enable automatic background regeneration with circuit breakers.
4. **Milestone 4 — Release Packaging:** Cross-compile static Linux/macOS binaries (x86_64-unknown-linux-musl, aarch64-apple-darwin), publish a 10-line Kubernetes manifest, and ship a 60-second terminal demo recording.
