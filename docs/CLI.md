# CLI reference

The executable has four operating modes. Run `cargo run -- --help` or `cargo run -- <subcommand> --help` for the Clap-generated command help from the exact build.

## HTTP runtime (default mode)

```sh
cargo run -- [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--spec PATH`, `-s PATH` | `./SPEC.md` | Operator service specification |
| `--openapi PATH` | none | Optional OpenAPI 3.x JSON contract alongside `SPEC.md` |
| `--port PORT`, `-p PORT` | `8080` | HTTP listen port |
| `--bind IP` | `127.0.0.1` | Listen address; external exposure must be explicit |
| `--verbose`, `-v` | off | Enable nosrc DEBUG diagnostics |
| `--cache-dir PATH` | `./.nosrc-cache` | Local content-addressed artifact cache |
| `--export-root PATH` | `./.nosrc-exports` | Root directory for MCP `export_app`; the tool accepts only one child directory name |
| `--no-cache` | off | Bypass cache reads and writes |
| `--shutdown-timeout SECONDS` | `30` | Maximum time to drain in-flight HTTP connections after SIGINT or (on Unix) SIGTERM; also settable with `NOSRC_SHUTDOWN_TIMEOUT_SECS` |

Example:

```sh
cargo run -- --spec ./examples/minimal/SPEC.md --bind 127.0.0.1 --port 8080 --verbose
```

## Runtime resource budgets

Operators may lower the guest execution budgets with environment variables. Unset values use the hard ceiling as the default; values above the ceiling, zero, empty values, and non-integers fail startup with the offending variable and accepted range.

| Environment variable | Default / hard ceiling | Meaning |
|---|---:|---|
| `NOSRC_MAX_FUEL` | `250000000` | Wasmtime fuel available to each guest invocation |
| `NOSRC_MAX_MEMORY_BYTES` | `67108864` (64 MiB) | Maximum linear-memory growth for each component instance |
| `NOSRC_MAX_RESPONSE_BYTES` | `1048576` (1 MiB) | Complete guest response envelope, including headers and WIT overhead |

The HTTP request-body limit remains a fixed 1 MiB hard ceiling. The 30-second guest epoch deadline and 5-second component startup deadline also remain fixed safety bounds in this release. These settings apply consistently to HTTP startup, MCP preparation/activation, verification, and offline bundle serving. MCP clients and draft SPECs cannot raise or replace the operator's selected budgets.

Example (lower the budgets while retaining the runtime's hard ceilings):

```sh
NOSRC_MAX_FUEL=200000000 \\
NOSRC_MAX_MEMORY_BYTES=33554432 \\
NOSRC_MAX_RESPONSE_BYTES=524288 \\
cargo run -- --spec ./SPEC.md
```

## Artifact export

```sh
cargo run -- export [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--spec PATH`, `-s PATH` | `./SPEC.md` | Operator service specification |
| `--openapi PATH` | none | Optional OpenAPI 3.x JSON contract alongside `SPEC.md` |
| `--out-dir PATH`, `-o PATH` | `./dist` | Destination directory |
| `--cache-dir PATH` | `./.nosrc-cache` | Local content-addressed artifact cache |
| `--no-cache` | off | Bypass cache reads and writes |
| `--verbose`, `-v` | off | Enable nosrc DEBUG diagnostics |

Example:

```sh
LLM_API_KEY="your-api-key" cargo run -- export --spec ./examples/minimal/SPEC.md --out-dir ./dist
```

Export runs synthesis or cache lookup, componentization/compilation, trusted and generated verification, and production-instance creation before writing the export files. It exits without opening an HTTP listener. See [ARTIFACTS.md](ARTIFACTS.md).

## MCP stdio server

```sh
cargo run -- mcp [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--spec PATH`, `-s PATH` | none | Optional operator-controlled service specification; omit for MCP-first startup |
| `--http-port PORT` | disabled | Also expose HTTP ingress in this process; MCP activation and rollback share the active handler |
| `--http-bind IP` | `127.0.0.1` | HTTP bind address when `--http-port` is enabled |
| `--cache-dir PATH` | `./.nosrc-cache` | Local content-addressed artifact cache |
| `--no-cache` | off | Bypass cache reads and writes |
| `--verbose`, `-v` | off | Enable diagnostics on stderr |
| `--shutdown-timeout SECONDS` | `30` | Maximum time to drain shared HTTP connections after SIGINT or (on Unix) SIGTERM; also settable with `NOSRC_SHUTDOWN_TIMEOUT_SECS` |

Example:

```sh
LLM_API_KEY="your-api-key" cargo run -- mcp --spec ./examples/minimal/SPEC.md
```

To launch the server from an MCP client such as Claude Desktop or Cursor, use the `mcpServers` entry in [`examples/mcp/mcp.json`](../examples/mcp/mcp.json), which is also shown in the README. Use absolute paths for `--spec` and `--cache-dir`, because the client chooses the working directory. A unit test in `src/main.rs` checks that the CLI accepts that entry's arguments.

By default MCP uses stdio and does not open an HTTP listener. Omit `--spec` to start in MCP-first mode with empty application state and baseline runtime limits. `--export-root DIR` sets the root for MCP `export_app` (default `./.nosrc-exports`). The tool accepts only one directory name beneath this root; absolute paths, parent traversal, and nested paths are rejected. In MCP-first mode, the agent can prepare direct JavaScript as a compile check or prepare a draft SPEC and export that generation for operator review. Direct-JavaScript candidates cannot be invoked, activated, or exported without a startup SPEC. Activation is blocked until the reviewed draft is supplied on restart with `--spec`. No agent-provided assertion can substitute for operator-trusted acceptance criteria. To share the same active generation with HTTP ingress, use:

```sh
LLM_API_KEY="your-api-key" cargo run -- mcp --spec ./examples/minimal/SPEC.md --http-port 8080
```

The HTTP listener defaults to loopback. Set `--http-bind 0.0.0.0` only when external exposure is intended. MCP activation and rollback publish to the same generation state used by HTTP; preparing a generation does not activate it. The API key is needed for `source: "spec"` or `source: "draft_spec"` synthesis; direct-JavaScript preparation does not itself require an LLM call. The MCP process has no MCP-level authentication or authorization. Generation state and rollback history are process-local.

## Offline bundle serving

```sh
cargo run -- serve --bundle ./dist [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--bundle PATH` | required | Export directory containing all four artifacts |
| `--port PORT`, `-p PORT` | `8080` | HTTP listen port |
| `--bind IP` | `127.0.0.1` | Listen address; external binding must be explicit |
| `--verbose`, `-v` | off | Enable nosrc DEBUG diagnostics |
| `--shutdown-timeout SECONDS` | `30` | Maximum time to drain in-flight HTTP connections after SIGINT or (on Unix) SIGTERM; also settable with `NOSRC_SHUTDOWN_TIMEOUT_SECS` |

This mode does not read `SPEC.md`, require `LLM_API_KEY`, synthesize code, or contact a model provider. It validates the component digest and manifest fields, checks bounded regular bundle files, compiles the component, reruns the trusted and generated assertion suites in separate candidate instances according to the manifest's `policy_mode`, and runs the fixed host-owned ABI probes on another disposable instance. It creates a fresh serving instance and binds the HTTP listener only after all checks pass. The manifest is unsigned; these checks do not authenticate the bundle or protect against an attacker who can replace the component, assertions, and manifest together. Artifact symlinks/reparse points are rejected on Unix and Windows; other operating-system targets fail closed because safe no-follow file opening is unavailable.

Example:

```sh
cargo run -- serve --bundle ./dist --bind 127.0.0.1 --port 8080
```

`serve` still opens an HTTP listener for application traffic; “offline” means the startup path has no LLM/provider dependency or outbound provider request.

## HTTP probes

All HTTP modes expose the same probe routes:

- `GET /healthz` is a liveness probe and returns `200 OK` while the listener responds.
- `GET /readyz` returns `200 OK` only after the listener has bound successfully and a verified handler generation is active; otherwise it returns `503 Service Unavailable`. In MCP shared-HTTP mode, an empty MCP-first runtime is not ready until a generation is activated.
- `GET /_nosrc/status` returns the active generation, module hash, versioned telemetry (when available), and a `ready` boolean.

These routes bypass the 32-request application admission limit and remain observable during overload. Listener readiness is cleared when the HTTP server stops.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `LLM_API_KEY` | none | Required whenever a model request is actually made (HTTP and export on a cache miss; MCP SPEC-driven synthesis). Not needed for a verified cache hit, direct-JavaScript MCP preparation, or `serve --bundle` |
| `LLM_BASE_URL` | `https://api.openai.com/v1` | OpenAI-compatible API base URL. Must use `https`, except that `http` is accepted for `localhost` and loopback IPs |
| `LLM_MODEL` | `gpt-4o-mini` | Model identifier |

`LLM_MODEL` must be non-empty. Invalid provider URLs and HTTP client initialization failures stop startup with an actionable configuration error rather than falling back to a default client.

HTTP and export read `LLM_API_KEY` lazily: the key is not part of the artifact cache identity, so a valid cache entry is loaded, recompiled, and fully re-verified without it. On a cache miss (or with `--no-cache`) startup fails with a missing-key error before any request is sent. MCP SPEC-driven synthesis still requires the key at startup. Offline bundle serving does not construct an LLM client or contact a provider.

Synthesis requests are retried up to three attempts total, with exponential backoff, on HTTP 429, 5xx, connection failures, and timeouts. Provider error bodies are scrubbed of the configured key and `sk-` style tokens before being reported.

`RUST_LOG` overrides the default scoped tracing filter. Without it, normal mode uses `warn,nosrc=info`; verbose mode uses `warn,nosrc=debug`. Diagnostics are emitted to stderr. Broad filters such as `RUST_LOG=trace` can enable verbose dependency logging.

## Exit and failure behavior

- Invalid/unreadable SPEC files, unsupported requested capabilities, missing synthesis credentials, synthesis/schema failures, componentization failures, malformed bundles, digest mismatches, and failed mandatory verification prevent the selected workflow from proceeding.
- HTTP ingress is not started until startup preparation and verification succeed.
- Each guest invocation has a 30-second wall-clock deadline enforced by Wasmtime epoch interruption, independently of fuel. The engine epoch ticker advances every 10 ms; the interrupt is observed at the next instrumented guest check, so the deadline is coarse-grained rather than an exact real-time cutoff. Timed-out instances are discarded and recreated; if recovery fails, the handler is marked unavailable.
- HTTP modes handle SIGINT and, on Unix, SIGTERM by stopping accepts and draining in-flight connections for up to the configured shutdown timeout (30 seconds by default). Remaining connections are aborted when the deadline expires. Set the timeout to `0` to skip draining and abort active connections immediately.
- In MCP shared-HTTP mode, a shutdown signal ends the HTTP listener and then the MCP process; if the stdio client disconnects first, the shared HTTP task is stopped.
- Export files are staged in a temporary sibling directory and published together by directory rename. The destination must not already exist; an existing file, directory, or symlink is left untouched and causes export to fail. A staging or publication failure does not expose a partial bundle.
- MCP preparation does not activate a candidate implicitly. Activation is a separate tool call.
- Cache errors are treated as cache misses or best-effort store failures where appropriate; cache data never bypasses verification.

