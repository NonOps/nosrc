# CLI reference

The executable has four operating modes. Run `cargo run -- --help` or `cargo run -- <subcommand> --help` for the Clap-generated command help from the exact build.

## HTTP runtime (default mode)

```sh
cargo run -- [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--spec PATH`, `-s PATH` | `./SPEC.md` | Operator service specification |
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

## Artifact export

```sh
cargo run -- export [OPTIONS]
```

| Option | Default | Description |
|---|---|---|
| `--spec PATH`, `-s PATH` | `./SPEC.md` | Operator service specification |
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

This mode does not read `SPEC.md`, require `LLM_API_KEY`, synthesize code, or contact a model provider. It validates the component digest and manifest fields, checks bounded regular bundle files, compiles the component, and reruns the assertion suites in separate candidate instances according to the manifest's `policy_mode`. The listener starts only after all checks pass. The manifest is unsigned; these checks do not authenticate the bundle or protect against an attacker who can replace the component, assertions, and manifest together. Artifact symlinks/reparse points are rejected on Unix and Windows; other operating-system targets fail closed because safe no-follow file opening is unavailable.

Example:

```sh
cargo run -- serve --bundle ./dist --bind 127.0.0.1 --port 8080
```

`serve` still opens an HTTP listener for application traffic; “offline” means the startup path has no LLM/provider dependency or outbound provider request.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `LLM_API_KEY` | none | Required whenever a model request is actually made (HTTP and export on a cache miss; MCP SPEC-driven synthesis). Not needed for a verified cache hit, direct-JavaScript MCP preparation, or `serve --bundle` |
| `LLM_BASE_URL` | `https://api.openai.com/v1` | OpenAI-compatible API base URL. Must use `https`, except that `http` is accepted for `localhost` and loopback IPs |
| `LLM_MODEL` | `gpt-4o-mini` | Model identifier |

HTTP and export read `LLM_API_KEY` lazily: the key is not part of the artifact cache identity, so a valid cache entry is loaded, recompiled, and fully re-verified without it. On a cache miss (or with `--no-cache`) startup fails with a missing-key error before any request is sent. MCP SPEC-driven synthesis still requires the key at startup. Offline bundle serving does not construct an LLM client or contact a provider.

Synthesis requests are retried up to three attempts total, with exponential backoff, on HTTP 429, 5xx, connection failures, and timeouts. Provider error bodies are scrubbed of the configured key and `sk-` style tokens before being reported.

`RUST_LOG` overrides the default scoped tracing filter. Without it, normal mode uses `warn,nosrc=info`; verbose mode uses `warn,nosrc=debug`. Diagnostics are emitted to stderr. Broad filters such as `RUST_LOG=trace` can enable verbose dependency logging.

## Exit and failure behavior

- Invalid/unreadable SPEC files, unsupported requested capabilities, missing synthesis credentials, synthesis/schema failures, componentization failures, malformed bundles, digest mismatches, and failed mandatory verification prevent the selected workflow from proceeding.
- HTTP ingress is not started until startup preparation and verification succeed.
- HTTP modes handle SIGINT and, on Unix, SIGTERM by stopping accepts and draining in-flight connections for up to the configured shutdown timeout (30 seconds by default). Remaining connections are aborted when the deadline expires.
- In MCP shared-HTTP mode, a shutdown signal ends the HTTP listener and then the MCP process; if the stdio client disconnects first, the shared HTTP task is stopped.
- Export files are staged in a temporary sibling directory and published together by directory rename. The destination must not already exist; an existing file, directory, or symlink is left untouched and causes export to fail. A staging or publication failure does not expose a partial bundle.
- MCP preparation does not activate a candidate implicitly. Activation is a separate tool call.
- Cache errors are treated as cache misses or best-effort store failures where appropriate; cache data never bypasses verification.

