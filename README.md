# nosrc

**nosrc** is an experimental Rust service that turns a declarative `SPEC.md` into a JavaScript handler, compiles it into a QuickJS-backed WebAssembly component, verifies its contract assertions, and serves HTTP requests through the verified generation.

The current implementation is a runnable single-process prototype, not a production-ready application runtime.

## Current implementation

- Loads and parses the configured `SPEC.md`.
- Calls an OpenAI-compatible chat-completions endpoint to synthesize a self-contained JavaScript ES module and contract assertions.
- Componentizes JavaScript in-process with `componentize-qjs` and executes it with Wasmtime's component model.
- Applies Wasm fuel and linear-memory limits to the component store.
- Runs the complete assertion suite against an isolated candidate instance before activation.
- Keeps the production handler instance separate from the verifier so test requests cannot mutate its initial state.
- Routes HTTP requests to the active generation and exposes health/status endpoints.
- Preserves JavaScript module-level state between requests handled by the same active generation.

## Deliberate limitations

- State is currently in-memory JavaScript module state. It survives requests within one active generation, but not process restarts or handler replacement.
- Candidate verification uses the generated assertions, which are useful contract checks but are not independent proof of correctness.
- The first WIT world exposes only the handler entrypoint. Guest code has no filesystem, process, network, database, or key-value host capability.
- Requests to one active handler instance are serialized to preserve its JavaScript state.
- There is no automatic rebuild, file watching, self-healing, automatic rollback, external persistence, or Kubernetes orchestration.
- Outbound HTTP, SQL, and durable KV capabilities are not exposed to generated code.

Do not use this prototype to serve production traffic or execute untrusted generated code without additional security review.

## Requirements

- Rust 1.97 or later
- Cargo
- An OpenAI-compatible API key

## Configuration

`LLM_API_KEY` is required. Optional variables:

- `LLM_BASE_URL` (default: `https://api.openai.com/v1`)
- `LLM_MODEL` (default: `gpt-4o-mini`)

## Build and test

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## Run

```sh
export LLM_API_KEY="..."
cargo run -- --spec ./examples/minimal/SPEC.md --port 8080
```

The service synthesizes, componentizes, and verifies a handler before binding the HTTP listener. Startup fails closed if synthesis, compilation, instantiation, or any contract assertion fails.

Check process health and the active generation:

```sh
curl http://localhost:8080/healthz
curl http://localhost:8080/_nosrc/status
```

Application routes are dispatched to the active verified JavaScript handler.

## Architecture and security

Read [ARCHITECTURE.md](ARCHITECTURE.md) for the implemented lifecycle and next milestones, and [AGENTS.md](AGENTS.md) for repository-specific engineering constraints.

The security model remains incomplete. In particular, host capabilities are not exposed yet; when they are added, each must have a narrow WIT contract and explicit capability checks. Outbound HTTP must validate DNS answers and every redirect target before it can be treated as a robust SSRF boundary. Resource limits, candidate assertions, artifact provenance, and state lifecycle need additional testing before production use.

## License

Apache-2.0. See [LICENSE](LICENSE) if present.
