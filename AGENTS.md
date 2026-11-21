# AI coding-agent instructions for nosrc

## 1. Project identity

nosrc is an experimental Rust runtime that transforms an operator-authored `SPEC.md` into a JavaScript request handler, compiles it into a QuickJS-backed WebAssembly component, verifies it, and exposes it through HTTP, MCP stdio (optionally with shared HTTP ingress), verified artifact export, or offline bundle serving.

Read `ARCHITECTURE.md` before changing module boundaries, execution contracts, security assumptions, or lifecycle behavior. Read the relevant documents under `docs/` when changing CLI behavior, export formats, or security boundaries.

Current toolchain: Rust 1.97+, Wasmtime 50 component model, and `componentize-qjs` 0.6.

## 2. Hard invariants

1. **No unverified activation.** HTTP startup and MCP SPEC-driven preparation must pass every operator-authored SPEC verification case and the separate model-generated assertion suite. MCP direct-JavaScript preparation must pass every trusted SPEC case. MCP-first draft-SPEC candidates may be prepared and exported for review. Direct-JavaScript candidates without a startup SPEC are preparation-only and cannot be invoked, activated, or exported. Activation is forbidden until an operator-controlled SPEC with at least one trusted verification case is loaded. Supplemental client assertions are informational only. Never activate a candidate without trusted policy or after a failed check.
2. **Trusted policy belongs to the operator.** An MCP client cannot replace the startup SPEC, weaken trusted assertions, or raise runtime ceilings.
3. **Strict guest isolation.** The current QuickJS component world exports only the handler entrypoint. Do not grant filesystem, process, network, SQL, KV, or WASI capabilities without an explicit WIT contract, host implementation, policy gate, bounds, and component-level tests.
4. **Enforce resource limits.** Fuel, linear memory, component handles/tables/instances/memories, request size, and response size must be enforced by the actual runtime path, not merely reported.
5. **Honest state semantics.** JavaScript module-level state is in-memory and generation-scoped. It is lost on handler replacement or process restart. Do not claim that SQLite/redb prototypes or dependencies provide persistence to the active QuickJS guest.
6. **Keep verification isolated.** Trusted and model-generated suites use separate disposable instances. Agent-supplied informational assertions use their own disposable instance. Production instances are created only after mandatory checks pass.
7. **Preserve process-mode boundaries.** The default HTTP mode and `serve --bundle` each run HTTP ingress in their own process. `nosrc mcp` defaults to a local stdio-only control plane. With `--http-port`, MCP also starts HTTP ingress in the same process and shares one lifecycle and active generation; do not describe that configuration as separate processes.
8. **Do not invent implemented capabilities.** A crate dependency, legacy module, test fixture, or backlog item is not proof that the active QuickJS guest can use that capability.

## 3. Current state and persistence roadmap

The active guest currently supports volatile JavaScript module state only. Durable application-state persistence is deferred backlog work, not permanently out of scope. Durable generation metadata and rollback recovery are also not implemented.

When implementing persistence, treat these as distinct designs:
- guest application state, likely introduced through a narrowly scoped WIT-backed storage interface with bounded operations and candidate isolation;
- control-plane state, including prepared/active generation metadata and restart/rollback recovery.

Do not add a persistence backend or claim persistence is wired into the request path until the contract, isolation model, failure semantics, resource bounds, and tests are implemented and documented.

## 4. Current operating modes and CLI

- No subcommand: HTTP runtime. Default SPEC `./SPEC.md`, port `8080`, bind `127.0.0.1`, cache directory `./.nosrc-cache`.
- `nosrc export`: verifies and exports `handler.js`, `component.wasm`, `assertions.json`, and `manifest.json`; does not start HTTP ingress. It stages the complete bundle in a temporary sibling directory and publishes it by rename; the destination must not already exist.
- `nosrc serve --bundle PATH`: validates an exported bundle, recompiles its component, reruns both assertion suites, and starts HTTP ingress without LLM credentials or provider access. It does not load a SPEC; the bundle and its unsigned assertion/manifest files are operator-controlled inputs.
- `nosrc mcp`: local stdio MCP process by default. With `--http-port`, it also starts HTTP ingress in the same process; MCP activation and rollback update the generation served by that listener. `--export-root` (default `./.nosrc-exports`) confines `export_app` to single directory names beneath the configured root. MCP-first candidates cannot be activated until an operator SPEC is loaded.

See `docs/CLI.md` for exact flags and environment variables, and `docs/ARTIFACTS.md` before changing exported files or manifest fields.

## 5. Security and execution rules

- The handler ABI is `handle(requestJson: string) -> responseJson: string`.
- The current WIT world exposes no guest imports or host capabilities beyond the handler export.
- Do not add ambient host access or rely on WASI stubs as a security boundary.
- Any future outbound HTTP feature must defend against DNS rebinding and redirect-based SSRF, including IPv4/IPv6 private, loopback, link-local, and metadata ranges.
- Any future database feature must use parameterized operations and enforce query, result, row-count, and execution-time bounds.
- Never log API keys, authorization headers, prompts, generated source, request/response bodies, or query strings.
- Treat LLM responses, cache contents, exported artifacts, MCP inputs, and guest outputs as untrusted data.
- MCP stdio has no MCP-level authentication/authorization; trust derives from the local launcher and pipe ownership.

See `docs/SECURITY.md` for the current threat model and known gaps.

## 6. Concurrency and lifecycle

- Do not hold the MCP lifecycle mutex during provider requests, componentization, candidate verification, or guest execution. Snapshot immutable inputs, perform expensive work outside the lock, and lock only to publish a completed candidate.
- Componentization and Wasm verification belong on Tokio's blocking pool.
- A preparation timeout must prevent a late worker from publishing a generation. It does not necessarily terminate the already-running blocking task.
- Rollback is limited to the immediately previous retained generation.
- The HTTP accept loop must enforce a finite connection limit before spawning per-connection tasks; retain each connection permit for the full task lifetime.
- Requests to one QuickJS handler instance are serialized to preserve module state.
- Do not add hot reload or generation draining without an explicit concurrency/lifecycle design and regression tests.

## 7. Rust and dependency conventions

- Keep the dependency tree lean and compatible with static/musl-oriented builds.
- Use `thiserror` for structured domain errors and `anyhow::Result` at CLI/top-level orchestration boundaries.
- Never unwrap network input, provider responses, or guest-controlled output in production paths.
- Never hold a blocking mutex or SQLite connection across an `.await`.
- Keep one Wasmtime major version in the executable.
- Do not add dependencies or modules to the approved stack by copying stale examples from old docs; inspect current `Cargo.toml` and implementation first.

## 8. Required validation

Before concluding implementation work, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Add component-runtime tests for new guest-visible behavior, including disabled-capability, malformed-input, resource-exhaustion, and failure-recovery cases as applicable. Update docs whenever CLI flags, public interfaces, artifact formats, security guarantees, limits, or lifecycle semantics change.

### Test placement

Follow the existing layout instead of creating new test locations:

- **Unit and module tests live inline in `src/`**, in a `#[cfg(test)] mod tests` block at the bottom of the file they cover. A module whose tests are too large for that may use a sibling `tests.rs` declared as `#[cfg(test)] mod tests;` (as `src/engine/tests.rs` does). Prefer extending the nearest existing `tests` module over starting a new file.
- **`tests/` is only for black-box tests that spawn the built executable** through `env!("CARGO_BIN_EXE_nosrc")` (for example, `tests/mcp_stdio.rs` driving the real stdio protocol). nosrc is a binary-only crate with no `lib.rs`, so files in `tests/` cannot import internal modules. If a test does not need to launch the binary, it belongs in `src/`.
- **Consistency checks for docs, example files, and metadata** (the MCP client config in `examples/mcp/mcp.json`, README badges) are unit tests in the `tests` module of `src/main.rs`. Read repository files with `include_str!`, and keep them free of network access, API keys, and running servers.
- **Shell tooling under `scripts/`** carries its own `--self-test`, which CI runs (see section 10).
- **Test-only helpers stay with their tests.** Do not add test utilities or fixtures to non-test code paths.

## 9. Documentation synchronization rule

For every new externally observable capability or changed contract, update all relevant documentation in the same PR:

- `README.md`: user-visible capability and quick-start usage.
- `ARCHITECTURE.md`: implementation, data flow, guarantees, and limitations.
- `docs/CLI.md`, `docs/ARTIFACTS.md`, or `docs/SECURITY.md`: detailed contract where relevant.
- `AGENTS.md`: contributor/reviewer invariants that future agents must preserve.

Do not document planned work as implemented. Mark backlog items as planned/deferred and describe current behavior separately.


## 10. Release versioning

- Actual Git tags and GitHub Release names use exactly `vMAJOR.MINOR` (for example, `v0.1` and `v0.2`).
- Do not create three-component release tags such as `v0.1.0`, or patch-level release tags such as `v0.1.1`.
- Keep `Cargo.toml`'s package version in valid three-component SemVer form (for example, `0.1.0`). Its major/minor components must match the release tag.
- Run `bash scripts/check-release-tag.sh --self-test` when changing the release validation script. CI runs this test automatically.
- See `docs/RELEASING.md` before tagging or publishing a release.
