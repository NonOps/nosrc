# nosrc architecture

## Current status

nosrc is an experimental, single-process Rust service. It now has a working SPEC-to-JavaScript-to-QuickJS component request lifecycle, but it is not production-ready.

At startup the executable:

1. Reads and parses the configured `SPEC.md`.
2. Validates the human-authored verification suite in `SPEC.md` and calls an OpenAI-compatible chat-completions endpoint to synthesize a JavaScript ES module and supplemental contract assertions.
3. Componentizes the JavaScript in-process with `componentize-qjs` using the `nosrc:runtime/handler` WIT world.
4. Compiles the resulting component with Wasmtime 50.
5. Instantiates an isolated candidate and runs the trusted SPEC-authored verification cases.
6. Instantiates a second isolated candidate and runs model-generated assertions as supplemental checks.
7. Rejects startup if either suite fails.
8. Creates a fresh production instance and activates it only after both suites pass.
9. Starts the HTTP server and routes application requests to the active generation.

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

The CLI loads the SPEC, requires `LLM_API_KEY`, and calls the configured OpenAI-compatible endpoint. `LLM_BASE_URL` and `LLM_MODEL` are optional. The synthesizer must return `code_format: "javascript"`, non-empty handler source, and a non-empty deterministic supplemental assertion suite. `SPEC.md` must also contain human-authored verification cases covering every declared endpoint.

The generated JavaScript is self-contained. It cannot import packages or use ambient host APIs.

### Componentization and execution

`src/engine/quickjs.rs` uses the Rust `componentize-qjs` API to turn source code and the fixed WIT contract into a component. WASI imports are stubbed. The component is compiled and executed with Wasmtime 50.

Each handler store has a fuel budget and a 64 MiB per-linear-memory growth limit. Component handle counts, tables, memories, and instances are also bounded. Fuel is reset before each invocation. The component instance and store remain alive across requests.

### Candidate verification

`SPEC.md` contains a `## Verification` section with explicit request examples. These cases are parsed independently from endpoint descriptions, checked for complete endpoint coverage, and restricted to response statuses documented for the matched endpoint. Dynamic route segments (for example `/:id`) are matched against the path portion of concrete verification request targets; query parameters are parsed separately and passed to the handler. Trusted cases can assert request headers, request bodies, response status, body substrings, and exact response-header values.

The runtime executes SPEC-authored cases against one disposable candidate instance, then runs model-generated assertions against a different disposable instance. The model cannot supply the only acceptance criteria, and supplemental tests cannot mutate trusted-suite state. A third, fresh instance is created for production only after both suites pass.

The verifier checks response status, configured body substrings, and expected response headers. Trusted SPEC cases can also set request headers and decoded query parameters. In the current string-map request ABI, repeated query keys are rejected rather than collapsed. Duplicate scalar verification fields and duplicate normalized header names are rejected during parsing. These remain authored contract checks—not proof that generated code is correct or secure.

### HTTP ingress

The Axum router handles health and status endpoints itself. Other requests are converted to `RequestEnvelope`, serialized, and passed to the active QuickJS handler. A mutex serializes calls to the single handler instance to preserve module state. This is a deliberate first-milestone throughput limitation. Incoming request bodies are capped at 1 MiB, and serialized guest response envelopes are capped at 1 MiB before deserialization. The LLM client limits serialized synthesis requests to 2 MiB, has a 10-second connect timeout and a 120-second total request timeout, and caps success responses at 4 MiB and error responses at 8 KiB.

## Implemented test coverage

- JavaScript componentization and execution.
- JavaScript module state retained across multiple calls.
- Invalid JavaScript rejected during componentization.
- Infinite JavaScript loop stopped by Wasmtime fuel metering.
- Candidate approval, rejection, empty-suite rejection, and sequential stateful assertions.
- SPEC verification parsing, documented-status validation, and complete endpoint coverage.
- Separate trusted and model-generated candidate verification instances.
- HTTP ingress dispatch, no-active-generation behavior, health, and status endpoints.
- QuickJS component memory exhaustion under the configured store budget.

## Next engineering milestones

1. Retain human-authored regression cases across handler revisions and add provenance records for SPEC hashes, model identifiers, component hashes, and verification outcomes.
2. Add explicit WIT-backed embedded KV operations with bounded key/value sizes and isolated candidate state.
3. Preserve binary request bodies through a versioned or otherwise explicit handler ABI change; the current text ABI rejects non-UTF-8 bodies.
4. Add safe generation replacement and in-flight request draining before any hot-reload feature.
5. Harden LLM schema validation and secret-safe diagnostics; client timeouts and response-size bounds are now implemented.

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
