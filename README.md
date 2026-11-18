![nosrc logo](images/banner.png)
# nosrc

**nosrc** is an experimental Rust runtime that turns a declarative `SPEC.md` into a JavaScript request handler, compiles it into a QuickJS-backed WebAssembly component, verifies its contract assertions, and serves requests through the verified handler.

The current implementation is a single-process prototype, not a production-ready application runtime.

## What is implemented

nosrc currently provides four operating modes, including an offline path for previously exported artifacts:

- **HTTP runtime:** prepare and verify a handler at startup, activate a fresh generation, and serve HTTP requests.
- **MCP control plane:** expose local stdio tools to prepare, inspect, invoke, activate, and roll back in-process generations.
- **Artifact export:** run the verification pipeline and export the JavaScript handler, Wasm component, assertion suites, and a provenance manifest for review and version control.
- **Offline bundle serving:** load an exported bundle, validate its component digest and manifest metadata, rerun both assertion suites, and serve it without LLM credentials or provider access.

These are operating modes of one executable, not separate guest runtimes. MCP is a separate process mode and does not share live state with an independently running HTTP process.

## Runtime and verification

- Loads and parses the operator-provided `SPEC.md`.
- Calls an OpenAI-compatible chat-completions endpoint to synthesize a self-contained JavaScript ES module and supplemental assertions when no matching artifact is cached.
- Uses a content-addressed local cache keyed by the raw SPEC, provider/model configuration, synthesis prompt, WIT contract, resource budgets, componentizer/runtime versions, and `Cargo.lock`.
- Componentizes JavaScript in-process with `componentize-qjs` and executes it with Wasmtime's component model.
- Applies Wasm fuel and linear-memory limits to the component store.
- Runs human-authored SPEC verification cases and a separate model-generated assertion suite against isolated candidate instances before activation in HTTP and SPEC-driven MCP workflows.
- Keeps the production handler instance separate from verifier instances so verification requests cannot mutate its initial state.
- Preserves JavaScript module-level state between requests handled by the same active generation.

The model-generated assertion suite is supplemental. It does not replace the operator-authored SPEC verification suite, and passing tests do not prove correctness or security.

## Quick start: HTTP

Requirements: Rust 1.97 or later, Cargo, and an OpenAI-compatible API key.

```sh
export LLM_API_KEY="your-api-key"
cargo run -- --spec ./examples/minimal/SPEC.md --port 8080
```

The listener binds to `127.0.0.1` by default. Use `--bind 0.0.0.0` only when external access is intended. Check the runtime and example endpoint:

```sh
curl -i http://localhost:8080/healthz
curl -sS http://localhost:8080/_nosrc/status
curl -i http://localhost:8080/hello
```

## Run as an MCP server

```sh
export LLM_API_KEY="your-api-key" # required for source: "spec"
cargo run -- mcp --spec ./examples/minimal/SPEC.md
```

The MCP server uses stdio and does not open an HTTP listener. It exposes five tools: `prepare_app`, `activate_app`, `invoke_app`, `get_app_status`, and `rollback_app`. Rollback is limited to the immediately previous retained generation. Generation state and rollback history are process-local and disappear when the MCP process exits.

## Export verified artifacts

```sh
export LLM_API_KEY="your-api-key"
cargo run -- export --spec ./examples/minimal/SPEC.md --out-dir ./dist
```

The export command runs synthesis (or loads a cache entry), componentization/compilation, trusted and generated verification, and fresh production-instance creation before writing:

- `handler.js` — synthesized JavaScript handler source.
- `component.wasm` — compiled WebAssembly component.
- `assertions.json` — trusted SPEC assertions and generated assertions.
- `manifest.json` — service name, SPEC SHA-256, module/component hashes, assertion counts, and synthesis rationale.

The export directory is for inspection, audit support, and version control. The manifest is provenance metadata, not a signed attestation or independent proof of correctness. Export is staged in a temporary sibling directory and published with a single directory rename. The destination must not already exist; remove or move an earlier export before rerunning. See [Artifact format](docs/ARTIFACTS.md).

## Serve an exported bundle offline

After exporting a bundle, start the HTTP runtime without an API key or model-provider request:

```sh
cargo run -- serve --bundle ./dist --bind 127.0.0.1 --port 8080
```

Startup validates the bundle files and SHA-256 metadata, compiles the prebuilt component with the current Wasmtime engine, reruns the trusted and generated assertion suites on separate candidate instances, and creates a fresh serving instance only after both suites pass. The HTTP listener opens only after successful validation. This mode does not read a SPEC file, synthesize code, or access an LLM provider.

The manifest is unsigned, and the exported assertion suites are files in the same bundle. Hash validation detects accidental or inconsistent component changes, not malicious replacement of the component, assertions, and manifest together. Treat the bundle and its source directory as operator-controlled inputs. See [Artifact format](docs/ARTIFACTS.md).

## SPEC verification format

Every declared endpoint must have at least one matching case under `## Verification`. Cases use a concrete request target (including a sample value for each `:parameter` segment):

```markdown
## Verification
### `GET /search?q=hello%20world&limit=10`
- Header: accept=application/json
- Response `200`
- Response header: content-type=application/json
- Body contains: `hello`
```

`Header:` sets a request header; `Response header:` asserts an exact response-header value. `Body:`, `Body contains:`, and `Response` are scalar fields and may appear at most once per case. Header names are case-insensitive. Repeated query keys in trusted verification headings are rejected because the current handler ABI represents each query key with one string value.

## Configuration and diagnostics

`LLM_API_KEY` is required for synthesis. Optional variables:

- `LLM_BASE_URL` (default: `https://api.openai.com/v1`)
- `LLM_MODEL` (default: `gpt-4o-mini`)

Pass `--verbose` (or `-v`) for nosrc DEBUG diagnostics covering generation stages, provider request timing and sizes, assertion results, component compilation, and HTTP request metadata. Diagnostics are written to stderr and omit API keys, authorization headers, prompts, generated source, request/response bodies, and query strings. `RUST_LOG` overrides the scoped default and can enable noisy dependency logs. Review logs before sharing them; bounded provider errors and assertion mismatch details may still contain sensitive context.

See [CLI reference](docs/CLI.md) for all commands, options, and environment variables.

## Current limitations and roadmap

- JavaScript module state is volatile and scoped to one active generation. It does not survive handler replacement or process restart.
- The current WIT world exposes only the handler entrypoint. Guest code has no filesystem, process, network, database, or key-value host capability.
- Requests to one active handler instance are serialized.
- HTTP request bodies and serialized guest response envelopes are limited to 1 MiB. Non-UTF-8 request bodies are unsupported by the current text-based handler ABI.
- LLM synthesis requests are limited to 2 MiB; connect and total timeouts are 10 and 120 seconds; successful and error response bodies are capped at 4 MiB and 8 KiB.
- The local artifact cache is an optimization, not a verification bypass. Cached components are recompiled and both verification suites run again.
- There is no automatic rebuild, file watching, automatic rollback, remote MCP transport, authentication/authorization for MCP, or Kubernetes orchestration.
- Durable application-state persistence and durable generation/rollback metadata are not implemented yet. They remain planned backlog work; current state is volatile and generation-scoped.

The security model remains incomplete. Do not use this prototype to serve production traffic or execute untrusted generated code without additional security review. See [Architecture](ARCHITECTURE.md) and [Security model](docs/SECURITY.md).

## Build and test

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

The test suite includes a deterministic end-to-end startup run against a local mock OpenAI-compatible endpoint. It exercises synthesis, QuickJS componentization, verification, production activation, and HTTP dispatch without an external API key.

## Project documentation

- [Architecture and implemented design](ARCHITECTURE.md)
- [CLI reference](docs/CLI.md)
- [Exported artifact format](docs/ARTIFACTS.md)
- [Security model and open risks](docs/SECURITY.md)
- [AI coding-agent instructions](AGENTS.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
