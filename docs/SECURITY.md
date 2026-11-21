# Security model and known limitations

## Security posture

nosrc is an experimental prototype, not a production security boundary. Generated JavaScript is untrusted input. The current design reduces guest authority by executing a self-contained QuickJS handler inside a WebAssembly component with a deliberately small WIT world and bounded runtime resources. This does not eliminate vulnerabilities in the host, componentizer, runtime, dependencies, or application logic.

Do not expose this prototype to untrusted users or production traffic without a separate security review and deployment-specific controls.

## Trust boundaries

### Operator-controlled SPEC

The optional startup SPEC and its verification cases are supplied by the operator. Every activation requires a non-empty operator-controlled verification suite. In MCP-first mode there is no application-specific trusted policy; baseline runtime resource ceilings still apply. An MCP client's `draft_spec` is synthesis input and candidate-test material only. Draft-SPEC generations may be inspected and exported for review; direct-JavaScript candidates without a SPEC can only be prepared as compile checks. `activate_app` rejects activation until the operator reviews the SPEC and restarts with it as `--spec`.

### LLM output

The model is not trusted. Its response is parsed into the expected structure, handler source is componentized, and required assertion suites run before activation. The generated suite is supplemental; it is not an independent security review and cannot replace the trusted SPEC suite.

### Guest component

The current WIT world exposes only the `handle` function. WASI imports are stubbed and the guest has no host-provided filesystem, process, network, SQL, or KV capability. Do not infer guest capability from Rust dependencies, legacy host ABI code, or unused prototypes elsewhere in the repository.

### Cache and exported artifacts

The local artifact cache is operator-managed and is not a trust boundary. Cache entries have integrity metadata, but a party able to modify the cache directory may be able to replace its contents; current engine compilation and verification still run on cache hits.

Exported manifest hashes are unsigned metadata. They do not provide authenticity or non-repudiation. Protect cache and export directories with filesystem permissions and use a trusted external mechanism if artifact signing or attestations are required.

### MCP stdio and shared HTTP

The MCP control plane listens on stdio and does not implement MCP-level authentication or authorization. Trust is inherited from the local process launcher and ownership of stdin/stdout pipes. Do not expose stdio through an untrusted bridge without adding and reviewing an appropriate security layer.

The optional `mcp --http-port` mode also opens an HTTP application listener in the same process. MCP and HTTP share one lifecycle and active handler; activating or rolling back through MCP changes HTTP traffic. The HTTP listener defaults to loopback, but an explicit external bind exposes the application to network clients. The MCP transport remains unauthenticated, so access to its stdio pipes is a privileged control-plane capability. Protect the launcher and any bridge that owns those pipes.

## Enforced limits and behavior

- Component fuel budget: 50,000,000 units by default, with operator policy unable to exceed the runtime ceiling.
- Guest wall-clock execution deadline: 30 seconds by default, enforced by Wasmtime epoch interruption independently of fuel. The epoch ticker advances every 10 ms; interruption is coarse-grained and may occur after the nominal deadline at the next instrumented guest check.
- Per-linear-memory growth: 64 MiB default ceiling.
- Component handles, table elements, instances, tables, and memories are bounded.
- Active HTTP connection limit: 128; the accept loop closes connections above the cap before spawning per-connection tasks.
- HTTP request body limit: 1 MiB.
- Serialized guest response envelope limit: 1 MiB.
- LLM serialized request limit: 2 MiB.
- LLM connect timeout: 10 seconds; total request timeout: 120 seconds.
- LLM success response limit: 4 MiB; error response limit: 8 KiB.
- One active handler instance serializes invocations to preserve module state. A guest trap or epoch timeout discards the instance and triggers recreation; if recreation fails, subsequent calls fail closed rather than reusing the trapped instance.
- MCP preparation timeout prevents late publication, but an already-running blocking task may continue in the background.

These limits mitigate resource exhaustion but require ongoing adversarial testing against the actual component runtime and host process.

## Logging

Normal diagnostics are scoped to nosrc events. Verbose mode enables nosrc DEBUG events. Diagnostics omit API keys, authorization headers, prompts, generated source, request/response bodies, and query strings. `LlmConfig` prints the API key as `<redacted>` in `Debug` output, and bounded provider error details are scrubbed of the configured key and `sk-` style tokens before being reported (a best-effort filter; other provider-specific secret formats are not recognized). `LLM_BASE_URL` must use `https` unless it points at a loopback address, so the bearer token is not sent in cleartext over a network. Provider error details and assertion mismatch details may still contain other sensitive context. Request paths can contain application-specific identifiers. Review logs before sharing them.

## Known gaps and future capability requirements

- No guest-visible durable state, SQL, KV, or outbound network capability is implemented.
- Durable application-state persistence and durable generation/control-plane metadata are backlog items. Any storage capability must have a narrow WIT contract, explicit policy gates, bounded operations, candidate isolation, defined consistency/recovery behavior, and component-level tests.
- The current string-based request ABI does not support arbitrary non-UTF-8 request bodies.
- Assertions are examples, not proof of correctness or security.
- MCP-first mode intentionally starts without application-specific trusted assertions. Candidates may be prepared and exported for review under baseline sandbox limits, but cannot be activated until an operator-reviewed SPEC is supplied at restart.
- The export manifest is not signed and does not contain complete build provenance or assertion execution records.
- Offline bundle serving validates component hashes and reruns exported assertions, but the manifest and assertions are unsigned and are stored alongside the component. It does not authenticate a bundle against an independently trusted SPEC or signer.
- Offline bundle artifact opens reject symbolic links and other reparse points on Unix and Windows using no-follow open flags, then inspect the opened handle and enforce bounded reads. Other operating-system targets fail closed because an equivalent no-follow primitive is not implemented there.
- Export files are staged together and published by a same-filesystem directory rename. Existing destinations are rejected rather than replaced; this prevents a failed export from leaving a partially updated bundle, but does not provide authenticity or tamper evidence.
- The prototype has no remote MCP transport or MCP authentication/authorization.
- There is no automatic rebuild, file watching, autonomous self-healing, or automatic rollback.

## Requirements for future host capabilities

Before introducing any guest-visible capability:

1. Define the WIT interface and keep it narrow.
2. Implement an explicit host-side capability gate, disabled by default unless operator policy permits it.
3. Enforce per-call and aggregate resource bounds.
4. Define candidate-verification isolation and side-effect behavior.
5. Add tests for denied access, malformed input, resource exhaustion, and host failure.
6. Update the synthesizer contract so generated code understands only supported operations.
7. Update README, ARCHITECTURE, AGENTS, and the relevant detailed documentation in the same PR.

For future outbound HTTP, validate DNS answers and every redirect target, including IPv4 and IPv6 private, loopback, link-local, and metadata ranges. For future database access, use parameterized operations and bound query time, row counts, and result sizes.


## MCP generation export

MCP `export_app` delegates to the same atomic artifact writer as the CLI. Its `out_dir` argument must be a single directory name beneath the configured `--export-root` (default `./.nosrc-exports`); absolute paths, traversal, and nested directories are rejected. The root is canonicalized before the destination is formed, and the artifact writer rejects an existing destination. Operator-mode bundles carry the immutable startup policy assertions. MCP-first draft-mode bundles explicitly record that no operator-trusted application assertions were configured; their generated draft assertions are rerun offline but are not represented as operator approval. New bundles embed the exact synthesis SPEC as `SPEC.md` and the loader verifies its digest. This does not sign or authenticate the bundle; protect the export root with operator-controlled filesystem permissions.
