![nosrc logo](images/banner.png)
# nosrc

**nosrc** is an experimental Rust service that turns a declarative `SPEC.md` into a JavaScript handler, compiles it into a QuickJS-backed WebAssembly component, verifies its contract assertions, and serves HTTP requests through the verified generation.

The current implementation is a runnable single-process prototype, not a production-ready application runtime.

## Current implementation

- Loads and parses the configured `SPEC.md`.
- Calls an OpenAI-compatible chat-completions endpoint to synthesize a self-contained JavaScript ES module and contract assertions.
- Componentizes JavaScript in-process with `componentize-qjs` and executes it with Wasmtime's component model.
- Applies Wasm fuel and linear-memory limits to the component store.
- Runs human-authored verification cases from `SPEC.md` and a separate model-generated supplemental assertion suite against isolated candidate instances before activation.
- Keeps the production handler instance separate from the verifier so test requests cannot mutate its initial state.
- Routes HTTP requests to the active generation and exposes health/status endpoints.
- Preserves JavaScript module-level state between requests handled by the same active generation.

## Deliberate limitations

- State is currently in-memory JavaScript module state. It survives requests within one active generation, but not process restarts or handler replacement.
- Trusted verification cases must be authored in `SPEC.md` and cover every declared endpoint. Model-generated assertions are supplemental and cannot replace the SPEC-authored suite. Neither suite proves correctness or security.
- The first WIT world exposes only the handler entrypoint. Guest code has no filesystem, process, network, database, or key-value host capability.
- Requests to one active handler instance are serialized to preserve its JavaScript state.
- HTTP request bodies and serialized guest response envelopes are limited to 1 MiB. Non-UTF-8 request bodies remain unsupported by the current text-based handler ABI.
- The LLM client limits serialized synthesis requests to 2 MiB, uses a 10-second connect timeout and 120-second total timeout, and caps success responses at 4 MiB and error responses at 8 KiB.
- There is no automatic rebuild, file watching, self-healing, automatic rollback, external persistence, or Kubernetes orchestration.
- Outbound HTTP, SQL, and durable KV capabilities are not exposed to generated code.

Do not use this prototype to serve production traffic or execute untrusted generated code without additional security review.

## SPEC verification format

Every declared endpoint must have at least one matching case under `## Verification`. Cases use a concrete request target (including a sample value for each `:parameter` segment) and a response status documented by that endpoint. Query parameters may be included in the heading and are percent-decoded into the request envelope:

```markdown
## Verification
### `GET /search?q=hello%20world&limit=10`
- Header: accept=application/json
- Response `200`
- Response header: content-type=application/json
- Body contains: `hello`
```

`Header:` sets a request header; `Response header:` asserts an exact response-header value. `Body:`, `Body contains:`, and `Response` are scalar fields and may appear at most once per case. Header names are case-insensitive. Repeated query keys in trusted verification headings are rejected because the current handler ABI represents each query key with one string value; it cannot express multi-valued query parameters. Verification cases are human-authored acceptance criteria; generated assertions are checked separately and are never their substitute.

## Requirements

- Rust 1.97 or later
- Cargo
- An OpenAI-compatible API key

## Configuration

`LLM_API_KEY` is required. Optional variables:

- `LLM_BASE_URL` (default: `https://api.openai.com/v1`)
- `LLM_MODEL` (default: `gpt-4o-mini`)

## Diagnostics

Normal startup shows nosrc `INFO` events and warnings/errors from dependencies. Pass `--verbose` (or `-v`) to add nosrc `DEBUG` diagnostics for the generation pipeline, provider request timing and sizes, individual contract assertion results, component compilation timing, and HTTP request metadata. Dependency logs remain at `WARN` and above, so enabling verbose mode does not flood the terminal with internal Wasmtime/componentizer details:

```sh
cargo run -- --verbose --spec ./examples/minimal/SPEC.md --port 8080
```

`RUST_LOG` overrides this scoped default. For example, `RUST_LOG=trace` intentionally enables very noisy diagnostics across dependencies as well. Verbose diagnostic events omit API keys, authorization headers, prompts, generated source, request/response bodies, and query strings. Failure messages may still include bounded provider error details or assertion-mismatch details; review stderr before sharing logs. Request paths can also contain application-specific identifiers.

## Build and test

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

The test suite includes a deterministic end-to-end startup run against a local mock OpenAI-compatible endpoint. It exercises synthesis, QuickJS componentization, both verification suites, production activation, and HTTP dispatch without an external API key.

## Run the minimal example

From the repository root, set a key for your OpenAI-compatible provider and start the service:

```sh
export LLM_API_KEY="your-api-key"
cargo run -- --spec ./examples/minimal/SPEC.md --port 8080
```

Add `--verbose` to inspect the generation and request lifecycle while testing:

```sh
cargo run -- --verbose --spec ./examples/minimal/SPEC.md --port 8080
```

For a compatible provider other than OpenAI, also set its API base URL and model before starting:

```sh
export LLM_BASE_URL="https://your-provider.example/v1"
export LLM_MODEL="your-model-name"
```

The service validates the SPEC-authored verification suite, synthesizes and componentizes a handler, then runs the trusted cases and a separate model-generated assertion suite before binding the HTTP listener. Startup fails closed if either suite fails. Keep this terminal running.

In a second terminal, check health and the active generation:

```sh
curl -i http://localhost:8080/healthz
curl -sS http://localhost:8080/_nosrc/status
```

Then call the example application route:

```sh
curl -i http://localhost:8080/hello
```

The example contract is `GET /hello`, returning HTTP 200 with a JSON greeting containing `"message": "hello"`. Stop the service with Ctrl-C in the first terminal.

## Architecture and security

Read [ARCHITECTURE.md](ARCHITECTURE.md) for the implemented lifecycle and next milestones, and [AGENTS.md](AGENTS.md) for repository-specific engineering constraints.

The security model remains incomplete. In particular, host capabilities are not exposed yet; when they are added, each must have a narrow WIT contract and explicit capability checks. Outbound HTTP must validate DNS answers and every redirect target before it can be treated as a robust SSRF boundary. Resource limits, candidate assertions, artifact provenance, and state lifecycle need additional testing before production use.

## License

Apache-2.0. See [LICENSE](LICENSE) if present.
