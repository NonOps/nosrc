# nosrc architecture

## Status and scope

nosrc is an experimental, single-process Rust service. The repository is not production-ready and the architecture below distinguishes the working bootstrap from the intended runtime.

The current executable:

1. Reads and parses the configured `SPEC.md`.
2. Initializes the existing Wasmtime core-module engine.
3. Starts an HTTP server.
4. Exposes `GET /healthz` and `GET /_nosrc/status`.
5. Returns `503 Service Unavailable` for application requests until a verified handler generation is activated.

The end-to-end SPEC-to-handler pipeline is not yet wired together. The existing synthesizer, verifier, ingress dispatch, and host ABI are prototypes, not a production deployment path.

## Decisions

- **Handler language:** JavaScript (ES modules). Do not switch the public handler contract to WAT.
- **JavaScript runtime:** embedded QuickJS compiled into a WebAssembly component.
- **Componentization:** integrate `componentize-qjs` with a matching Wasmtime component-model host. Keep one Wasmtime major version in the executable.
- **Deployment model:** single-node, single-process runtime first. No Kubernetes operator or cluster-level automation in the initial implementation.
- **State:** embedded state is the first supported target. External database/cache integrations are deferred until the embedded request path is proven.
- **Activation:** a candidate must compile, pass contract verification, and only then become active. A failed candidate must leave the previous generation untouched.
- **Operations:** health and status endpoints are part of the initial service contract.

## Current components

### CLI and process

The CLI accepts a SPEC path, state-mode selection, and HTTP port. It loads the specification, initializes the engine, and starts the Axum server. The state-mode flag is currently configuration metadata; it does not imply that all persistence modes are implemented.

### HTTP ingress

The ingress layer accepts HTTP requests and converts them to the internal request envelope before dispatch. Health and status endpoints are handled by the host. Application requests return 503 while no generation is active.

### Existing Wasmtime engine

The current engine executes core Wasm modules through a custom host ABI. It has fuel metering and a linear-memory limiter. This is legacy scaffolding and is not the selected JavaScript execution architecture.

### Synthesis and verification prototypes

The repository contains an OpenAI-compatible synthesis client, contract assertion types, and a shadow verifier. They are not yet connected to process startup or handler activation. The verifier's state isolation and lifecycle need correction before it can gate stateful candidates.

## Target request lifecycle

1. Load and validate the declarative service specification.
2. Synthesize a JavaScript ES module implementing the handler contract and its assertions.
3. Componentize the JavaScript with the pinned QuickJS toolchain and the repository's WIT contract.
4. Instantiate the component in a Wasmtime component-model store with bounded fuel, memory, and explicitly granted host capabilities.
5. Run contract assertions against isolated candidate state.
6. Reject the candidate on any compilation, instantiation, trap, fuel exhaustion, or assertion failure.
7. Atomically activate only a fully verified candidate.
8. Route HTTP requests to the active generation; retain the previous generation until in-flight requests finish.

No candidate is active merely because synthesis or componentization succeeded.

## First implementation milestone

The first complete runnable milestone must demonstrate all of the following in integration tests:

- A JavaScript handler can be compiled/componentized and loaded.
- An HTTP request reaches the JavaScript handler and receives the expected response.
- State written by one request is visible to a later request.
- Fuel exhaustion and memory-limit violations are contained and reported.
- An invalid candidate is rejected without replacing the active generation.
- The service remains running and exposes health/status endpoints.

Keep this milestone single-process and embedded-state only. Do not add file watching, automatic rebuilds, self-healing, automatic rollback, external persistence, or Kubernetes orchestration as part of it.

## Security requirements before production use

- Guest code must not receive ambient filesystem, process, or network access.
- Host capabilities must be explicitly declared, narrowly scoped, and tested for disabled-by-default behavior.
- Any outbound HTTP capability must validate DNS answers and every redirect target, including IPv4 and IPv6 private, loopback, link-local, and metadata ranges.
- Database operations need query, row-count, result-size, and execution-time bounds.
- Guest request and response sizes must be bounded; request bodies must not be silently converted with lossy UTF-8 decoding.
- Candidate verification must use isolated state and must not create persistent temporary artifacts.
- Resource limits must be tested against the actual QuickJS component runtime, not inferred from core-module tests.

## Deferred work

- Automatic self-healing or production-error-driven regeneration.
- File watching and automatic handler rebuilds.
- External PostgreSQL/Redis integrations.
- Multi-node coordination, Kubernetes operators, and cluster automation.
- Automated rollback and long-running control-plane workflows.

These are intentionally deferred until the first runnable handler lifecycle is complete and tested.
