# nosrc architecture

## Current status

nosrc is an experimental, single-process Rust service. It now has a working SPEC-to-JavaScript-to-QuickJS component request lifecycle, but it is not production-ready.

At startup the executable:

1. Reads and parses the configured `SPEC.md`.
2. Calls an OpenAI-compatible chat-completions endpoint to synthesize a JavaScript ES module and contract assertions.
3. Componentizes the JavaScript in-process with `componentize-qjs` using the `nosrc:runtime/handler` WIT world.
4. Compiles the resulting component with Wasmtime 50.
5. Instantiates an isolated candidate and runs all synthesized contract assertions.
6. Rejects startup if synthesis, componentization, instantiation, or any assertion fails.
7. Creates a fresh production instance and activates it only after candidate verification succeeds.
8. Starts the HTTP server and routes application requests to the active generation.

The runtime requires Rust 1.97 or later. JavaScript module-level state is retained between requests to the same active generation. Requests to one handler instance are serialized so this state has deterministic behavior.

## Decisions

- **Handler language:** self-contained JavaScript ES modules.
- **JavaScript runtime:** QuickJS compiled into a WebAssembly component by `componentize-qjs`.
- **Component runtime:** Wasmtime 50 component model. Do not introduce a second Wasmtime major version.
- **Execution contract:** `handle(requestJson: string) -> responseJson: string`. Both values are JSON strings using the request and response envelope schemas in `src/engine/http_envelope.rs`.
- **Deployment model:** single-node, single-process runtime first. No Kubernetes operator or cluster automation.
- **Capabilities:** the initial WIT world exposes only the handler entrypoint. No WASI, filesystem, process, network, SQL, or KV capability is granted.
- **Activation:** compile and verify an isolated candidate, then instantiate a separate production handler and atomically publish it.
- **State:** module-level JavaScript state survives across requests within a generation. It is volatile and resets on restart or handler replacement.
- **Operations:** `GET /healthz` and `GET /_nosrc/status` are available.

## Components

### CLI and synthesis

The CLI loads the SPEC, requires `LLM_API_KEY`, and calls the configured OpenAI-compatible endpoint. `LLM_BASE_URL` and `LLM_MODEL` are optional. The synthesizer must return `code_format: "javascript"`, non-empty handler source, and a non-empty deterministic assertion suite.

The generated JavaScript is self-contained. It cannot import packages or use ambient host APIs.

### Componentization and execution

`src/engine/quickjs.rs` uses the Rust `componentize-qjs` API to turn source code and the fixed WIT contract into a component. WASI imports are stubbed. The component is compiled and executed with Wasmtime 50.

Each handler store has a fuel budget and a 64 MiB per-linear-memory growth limit. Component handle counts, tables, memories, and instances are also bounded. Fuel is reset before each invocation. The component instance and store remain alive across requests.

### Candidate verification

`src/verifier` runs the entire assertion suite sequentially against one isolated candidate instance. Sharing that instance is intentional: it tests the same module-level state behavior that applies to production requests. The candidate instance is discarded after verification; production gets a newly instantiated component.

The verifier checks response status, configured body substrings, and expected headers. These are contract checks, not proof that generated code is correct or secure.

### HTTP ingress

The Axum router handles health and status endpoints itself. Other requests are converted to `RequestEnvelope`, serialized, and passed to the active QuickJS handler. A mutex serializes calls to the single handler instance to preserve module state. This is a deliberate first-milestone throughput limitation.

## Implemented test coverage

- JavaScript componentization and execution.
- JavaScript module state retained across multiple calls.
- Invalid JavaScript rejected during componentization.
- Infinite JavaScript loop stopped by Wasmtime fuel metering.
- Candidate approval, rejection, empty-suite rejection, and sequential stateful assertions.
- HTTP ingress dispatch, no-active-generation behavior, health, and status endpoints.

The component memory limiter is configured in the actual QuickJS component store; a dedicated memory-exhaustion regression test remains to be added.

## Next engineering milestones

1. Add a dedicated component memory-exhaustion test and verify the configured cap against actual QuickJS allocation behavior.
2. Add explicit WIT-backed embedded KV operations with bounded key/value sizes and isolated candidate state.
3. Add bounded request/response sizes and preserve binary request bodies without lossy UTF-8 conversion.
4. Add safe generation replacement and in-flight request draining before any hot-reload feature.
5. Harden the LLM client (timeouts, response-size limits, schema validation, and secret-safe diagnostics).

Do not add autonomous self-healing, automatic regeneration from production failures, file watching, automatic rollback, external persistence, or Kubernetes orchestration in these milestones.

## Security requirements before production use

- Guest code must not receive ambient filesystem, process, or network access.
- Every future host capability must be declared in WIT, granted explicitly, and tested for disabled-by-default behavior.
- Any future outbound HTTP capability must validate DNS answers and every redirect target, including IPv4 and IPv6 private, loopback, link-local, and metadata ranges.
- Database operations need query, row-count, result-size, and execution-time bounds before they are exposed.
- Request and response sizes must be bounded; request bodies must not be silently converted with lossy UTF-8 decoding.
- Candidate verification must use isolated state and must not create persistent temporary artifacts.
- Resource limits must be tested against the actual QuickJS component runtime.
- LLM output is untrusted input. Validate its format, componentize it, and verify assertions before activation.
