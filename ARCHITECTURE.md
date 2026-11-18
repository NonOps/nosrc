# nosrc architecture

## Current status

nosrc is an experimental, single-process Rust service. It now has a working SPEC-to-JavaScript-to-QuickJS component request lifecycle, but it is not production-ready.

At startup the executable:

1. Reads and parses the configured `SPEC.md`.
2. Validates the human-authored verification suite in `SPEC.md` and derives a content-addressed cache key from the raw SPEC, configured provider/model, synthesis prompt, WIT contract, resource budgets, componentizer/runtime versions, and `Cargo.lock`.
3. Loads a matching local artifact when available; otherwise calls the OpenAI-compatible endpoint to synthesize a JavaScript ES module and supplemental assertions, then componentizes it with `componentize-qjs`.
4. Compiles the component with the current Wasmtime 50 engine, including cache hits.
5. Instantiates an isolated candidate and runs the trusted SPEC-authored verification cases.
6. Instantiates a second isolated candidate and runs model-generated assertions as supplemental checks.
7. Rejects startup if either suite fails.
8. Creates a fresh production instance and activates it only after both suites pass.
9. Stores newly verified artifacts atomically in the local cache, then starts the HTTP server and routes application requests to the active generation.

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
- **Operations:** `GET /healthz` and `GET /_nosrc/status` are available. The HTTP listener binds to `127.0.0.1` by default; external exposure requires an explicit `--bind` address.

## Components

### CLI and synthesis

The CLI loads the SPEC, requires `LLM_API_KEY`, and calls the configured OpenAI-compatible endpoint. `LLM_BASE_URL` and `LLM_MODEL` are optional. The synthesizer must return `code_format: "javascript"`, non-empty handler source, and a non-empty deterministic supplemental assertion suite. `SPEC.md` must also contain human-authored verification cases covering every declared endpoint.

The generated JavaScript is self-contained. It cannot import packages or use ambient host APIs. The default `.nosrc-cache/` stores component bytes and the synthesis metadata in SHA-256-keyed directories. Cache hits avoid the provider request and componentization, but do not bypass component compilation or either verification suite. The cache key includes the dependency lockfile so dependency updates naturally invalidate prior entries. Use `--no-cache` to force fresh synthesis or `--cache-dir` to select another local cache directory. Cache contents are local operator-managed files, not a trust boundary; keep the directory writable only by the service operator.

### Componentization and execution

`src/engine/quickjs.rs` uses the Rust `componentize-qjs` API to turn source code and the fixed WIT contract into a component. WASI imports are stubbed. The component is compiled and executed with Wasmtime 50.

Each handler store has a fuel budget and a 64 MiB per-linear-memory growth limit. Component handle counts, tables, memories, and instances are also bounded. Fuel is reset before each invocation. The component instance and store remain alive across requests.

### Candidate verification

`SPEC.md` contains a `## Verification` section with explicit request examples. These cases are parsed independently from endpoint descriptions, checked for complete endpoint coverage, and restricted to response statuses documented for the matched endpoint. Dynamic route segments (for example `/:id`) are matched against the path portion of concrete verification request targets; query parameters are parsed separately and passed to the handler. Trusted cases can assert request headers, request bodies, response status, body substrings, and exact response-header values.

The runtime executes SPEC-authored cases against one disposable candidate instance, then runs model-generated assertions against a different disposable instance. The model cannot supply the only acceptance criteria, and supplemental tests cannot mutate trusted-suite state. A third, fresh instance is created for production only after both suites pass.

The verifier checks response status, configured body substrings, and expected response headers. Trusted SPEC cases can also set request headers and decoded query parameters. The current string-map request ABI cannot express multi-valued query parameters; repeated query keys in trusted verification headings are rejected rather than silently collapsed. Duplicate scalar verification fields and duplicate normalized header names are rejected during parsing. These remain authored contract checks—not proof that generated code is correct or secure.

### HTTP ingress

The Axum router handles health and status endpoints itself. Other requests are converted to `RequestEnvelope`, serialized, and passed to the active QuickJS handler. The listener defaults to loopback rather than exposing the service on every network interface. HTTP error envelopes return stable sanitized messages; detailed engine and guest diagnostics are reserved for server-side logs. A mutex serializes calls to the single handler instance to preserve module state. This is a deliberate first-milestone throughput limitation. Incoming request bodies are capped at 1 MiB, and serialized guest response envelopes are capped at 1 MiB before deserialization. The LLM client limits serialized synthesis requests to 2 MiB, has a 10-second connect timeout and a 120-second total request timeout, and caps success responses at 4 MiB and error responses at 8 KiB.

## MCP control interface (v0.2)

The `nosrc mcp --spec ./SPEC.md` subcommand exposes a local stdio MCP server as a control interface. It is a separate process mode and does not start an HTTP listener. The normal CLI HTTP mode remains available independently. MCP does not add guest capabilities or replace the QuickJS/Wasmtime runtime.

The stdio transport does not implement MCP authentication or authorization. Its trust boundary is the local process that launches nosrc and owns the stdin/stdout pipes; the launcher must be trusted, and those pipes must not be exposed or proxied to untrusted processes. There is no remote MCP transport in this milestone.

The MCP tools are:

- `prepare_app`: prepares a candidate from either `source: "spec"` or `source: "direct_js"`; it never activates implicitly.
- `activate_app`: activates a retained `ready` generation.
- `invoke_app`: invokes the active handler using the same `QuickJsHandler::invoke` execution path as HTTP ingress.
- `get_app_status`: reports active, previous, and retained generations with verification summaries and effective resource limits.
- `rollback_app`: reactivates the immediately previous retained generation and makes the former active generation the new rollback target.

The operator controls the SPEC loaded at process startup; the MCP client cannot replace its trusted assertions or relax the runtime ceilings. Both preparation modes must pass every human-authored SPEC assertion. SPEC-driven preparation must additionally pass the model-generated assertion suite, preserving the CLI's fail-closed invariant. In direct-JavaScript mode there is no model-generated suite; optional agent-supplied assertions are informational only and cannot override trusted SPEC results. Trusted and model-generated suites run on separate isolated instances. Each suite is evaluated sequentially on one instance so stateful request sequences are preserved. Agent-supplied assertions run on a separate disposable instance and remain informational.

Preparation copies the immutable engine, SPEC, cache handle, and operator policy while briefly holding the lifecycle mutex. LLM requests, componentization, and candidate verification then run without that mutex; componentization and Wasm execution use Tokio's blocking pool. The lifecycle mutex is acquired again only to publish a fully prepared candidate. A timeout discards the result from the caller's perspective; an already-running blocking task may finish in the background, but it cannot publish a generation after cancellation.

The policy's fuel, memory, and response-size values are passed into component instantiation and response validation, not merely reported. Request header names are normalized to lowercase. Repeated query keys use the same last-value-wins behavior as the HTTP request envelope.

The MCP lifecycle is process-local. Because the stdio subcommand is a separate mode, it does not share a live `IngressState` with an independently running HTTP process; activation and rollback change the active generation within the MCP process only. Persistent generation storage, MCP authentication/authorization, multiple applications, HTTP MCP transport, and automatic file watching remain deferred.

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

1. Extend artifact provenance with durable verification records and retained human-authored regression cases across handler revisions. The current cache records input identity, synthesis metadata, and component digest, and reruns verification at startup.
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
