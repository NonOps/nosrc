<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./assets/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="./assets/logo-light.svg">
    <img alt="nosrc logo" src="./assets/logo-light.svg">
  </picture>
</p>

# nosrc

[![CI](https://github.com/NonOps/nosrc/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/NonOps/nosrc/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust 1.97+](https://img.shields.io/badge/rust-1.97%2B-orange.svg)](Cargo.toml)

**nosrc** is an experimental Rust runtime that turns a declarative `SPEC.md` into a JavaScript request handler, compiles it into a QuickJS-backed WebAssembly component, verifies its contract assertions, and serves requests through the verified handler.

The current implementation is a single-process prototype, not a production-ready application runtime.

![nosrc spec to live endpoint demo](./assets/nosrc-demo.gif)

## What is implemented

nosrc currently provides four operating modes, including an offline path for previously exported artifacts:

- **HTTP runtime:** prepare and verify a handler at startup, activate a fresh generation, and serve HTTP requests.
- **MCP control plane:** expose local stdio tools to prepare, inspect, invoke, activate, and roll back in-process generations.
- **Artifact export:** run the verification pipeline and export the JavaScript handler, Wasm component, assertion suites, and a provenance manifest for review and version control.
- **Offline bundle serving:** load an exported bundle, validate its component digest and manifest metadata, rerun both assertion suites, and serve it without LLM credentials or provider access.

These are operating modes of one executable, not separate guest runtimes. MCP can run as standalone stdio control plane or with an optional HTTP listener in the same process. In shared mode, MCP activation and rollback immediately change the generation served by HTTP.

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

The default MCP mode uses stdio and does not open an HTTP listener. It exposes six tools: `prepare_app`, `activate_app`, `invoke_app`, `get_app_status`, `rollback_app`, and `export_app`. Rollback is limited to the immediately previous retained generation. Generation state and rollback history are process-local and disappear when the MCP process exits.

MCP can also start without an operator SPEC:

```sh
cargo run -- mcp
```

This starts with empty application state and baseline runtime resource limits. The agent can prepare direct JavaScript with `source: "direct_js"`, or provide a draft contract with `source: "draft_spec"` and a `draft_spec` Markdown string. Draft SPEC data is synthesis input, not trusted operator policy. Its verification cases are included with model-generated assertions as candidate checks, and the baseline sandbox limits remain in force. Use `export_app` with the prepared generation ID to write the same offline bundle format as the CLI exporter. The bundle includes `SPEC.md`, handler source, component, assertions, and manifest, and can be served without an API key. To establish an operator-controlled acceptance policy, review the exported draft and restart with `--spec PATH`.

To expose HTTP from the same process and share the active generation with MCP, add `--http-port` (and optionally `--http-bind`):

```sh
LLM_API_KEY="your-api-key" cargo run -- mcp --spec ./examples/minimal/SPEC.md --http-port 8080
```

MCP activation and rollback update the handler used by HTTP ingress. This is one application lifecycle and one generation history, not two runtimes synchronized over a network. The HTTP listener defaults to loopback; use `--http-bind 0.0.0.0` only when external exposure is intended. The stdio transport itself remains local and has no MCP-level authentication.

### Use with an MCP client

MCP clients that read an `mcpServers` configuration (Claude Desktop and Cursor, for example) launch nosrc as a local stdio server. Install the binary so `nosrc` is on your `PATH` (`cargo install --path .`), then add this entry to your client's MCP configuration:

```json
{
  "mcpServers": {
    "nosrc": {
      "command": "nosrc",
      "args": [
        "mcp",
        "--spec",
        "/absolute/path/to/SPEC.md",
        "--cache-dir",
        "/absolute/path/to/.nosrc-cache"
      ],
      "env": {
        "LLM_API_KEY": "your-api-key"
      }
    }
  }
}
```

Replace the placeholder paths and key, and use absolute paths because the client chooses the working directory. The same entry is kept in [`examples/mcp/mcp.json`](examples/mcp/mcp.json); a unit test checks that it matches this README and that the CLI accepts its arguments. Where the file goes depends on the client, so see its MCP documentation.

The `--spec` file is the operator-controlled trusted policy: a connected agent can prepare, activate, invoke, export, and roll back generations, but cannot replace that spec or weaken its verification cases. To start without one, drop `--spec` as described above, and note that agent-supplied draft contracts are then synthesis input only, not trusted policy. `LLM_API_KEY` is needed for SPEC-driven synthesis; direct-JavaScript preparation does not call the model. Rollback reaches only the immediately previous retained generation, and the MCP process has no authentication of its own, so anything that can launch it or write to its stdin has full access to these tools.

## Export verified artifacts

```sh
export LLM_API_KEY="your-api-key"
cargo run -- export --spec ./examples/minimal/SPEC.md --out-dir ./dist
```

The export command runs synthesis (or loads a cache entry), componentization/compilation, trusted and generated verification, and fresh production-instance creation before writing:

- `handler.js` — synthesized JavaScript handler source.
- `component.wasm` — compiled WebAssembly component.
- `assertions.json` — trusted SPEC assertions and generated assertions.
- `SPEC.md` — exact SPEC text used for synthesis.
- `manifest.json` — service name, SPEC SHA-256, module/component hashes, assertion counts, synthesis rationale, and policy mode.

The export directory is for inspection, audit support, and version control. The manifest is provenance metadata, not a signed attestation or independent proof of correctness. Export is staged in a temporary sibling directory and published with a single directory rename. The destination must not already exist; remove or move an earlier export before rerunning. See [Artifact format](docs/ARTIFACTS.md).

## Serve an exported bundle offline

After exporting a bundle, start the HTTP runtime without an API key or model-provider request:

```sh
cargo run -- serve --bundle ./dist --bind 127.0.0.1 --port 8080
```

Startup validates the bundle files and SHA-256 metadata, compiles the prebuilt component with the current Wasmtime engine, reruns the assertion suites on separate candidate instances according to the manifest policy mode, and creates a fresh serving instance only after verification passes. The HTTP listener opens only after successful validation. This mode does not read a SPEC file, synthesize code, or access an LLM provider.

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

The test suite includes deterministic end-to-end runs against a local mock OpenAI-compatible endpoint. In addition to the HTTP startup path, the MCP-first lifecycle test starts empty, prepares an agent-authored draft SPEC, activates it through the shared HTTP ingress, exports the complete bundle through the MCP lifecycle, and starts a separate offline HTTP runtime from the exported artifacts. It verifies the same response before and after export, reruns bundle assertions, confirms offline serving makes no provider request, and checks that a fresh MCP process starts empty. No external model provider or credentials are used.

## Release versions

Git tags and GitHub Releases use the short milestone form: `v0.1`, `v0.2`, `v0.3`, and so on. The Cargo package keeps a three-component SemVer version (currently `0.1.0`) for Rust tooling; its major/minor components map to the release tag. Patch-level Git tags and GitHub Releases are not used. See [Release process](docs/RELEASING.md).

## Project documentation

- [Architecture and implemented design](ARCHITECTURE.md)
- [CLI reference](docs/CLI.md)
- [Exported artifact format](docs/ARTIFACTS.md)
- [Security model and open risks](docs/SECURITY.md)
- [AI coding-agent instructions](AGENTS.md)
- [Release process](docs/RELEASING.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
