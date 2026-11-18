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
| `--no-cache` | off | Bypass cache reads and writes |

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
| `--spec PATH`, `-s PATH` | `./SPEC.md` | Operator-controlled service specification |
| `--http-port PORT` | disabled | Also expose HTTP ingress in this process; MCP activation and rollback share the active handler |
| `--http-bind IP` | `127.0.0.1` | HTTP bind address when `--http-port` is enabled |
| `--cache-dir PATH` | `./.nosrc-cache` | Local content-addressed artifact cache |
| `--no-cache` | off | Bypass cache reads and writes |
| `--verbose`, `-v` | off | Enable diagnostics on stderr |

Example:

```sh
LLM_API_KEY="your-api-key" cargo run -- mcp --spec ./examples/minimal/SPEC.md
```

By default MCP uses stdio and does not open an HTTP listener. To share the same active generation with HTTP ingress, use:

```sh
LLM_API_KEY="your-api-key" cargo run -- mcp --spec ./examples/minimal/SPEC.md --http-port 8080
```

The HTTP listener defaults to loopback. Set `--http-bind 0.0.0.0` only when external exposure is intended. MCP activation and rollback publish to the same generation state used by HTTP; preparing a generation does not activate it. The API key is needed when using SPEC-driven synthesis; direct-JavaScript preparation does not itself require an LLM call. The MCP process has no MCP-level authentication or authorization. Generation state and rollback history are process-local.

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

This mode does not read `SPEC.md`, require `LLM_API_KEY`, synthesize code, or contact a model provider. It validates the component digest and manifest fields, checks bounded regular bundle files, compiles the component, and reruns trusted and generated assertions in separate candidate instances. The listener starts only after all checks pass. The manifest is unsigned; these checks do not authenticate the bundle or protect against an attacker who can replace the component, assertions, and manifest together. Artifact symlinks/reparse points are rejected on Unix and Windows; other operating-system targets fail closed because safe no-follow file opening is unavailable.

Example:

```sh
cargo run -- serve --bundle ./dist --bind 127.0.0.1 --port 8080
```

`serve` still opens an HTTP listener for application traffic; “offline” means the startup path has no LLM/provider dependency or outbound provider request.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `LLM_API_KEY` | none | Required by HTTP and export startup, including cache hits; MCP requires it when using SPEC-driven synthesis. Direct-JavaScript MCP preparation does not require an LLM call |
| `LLM_BASE_URL` | `https://api.openai.com/v1` | OpenAI-compatible API base URL |
| `LLM_MODEL` | `gpt-4o-mini` | Model identifier |

HTTP and export currently construct the LLM configuration before cache lookup, so `LLM_API_KEY` must be set even when a valid cache entry exists. MCP direct-JavaScript preparation and `serve --bundle` can run without it; offline bundle serving does not construct an LLM client or contact a provider.

`RUST_LOG` overrides the default scoped tracing filter. Without it, normal mode uses `warn,nosrc=info`; verbose mode uses `warn,nosrc=debug`. Diagnostics are emitted to stderr. Broad filters such as `RUST_LOG=trace` can enable verbose dependency logging.

## Exit and failure behavior

- Invalid/unreadable SPEC files, unsupported requested capabilities, missing synthesis credentials, synthesis/schema failures, componentization failures, malformed bundles, digest mismatches, and failed mandatory verification prevent the selected workflow from proceeding.
- HTTP ingress is not started until startup preparation and verification succeed.
- Export files are staged in a temporary sibling directory and published together by directory rename. The destination must not already exist; an existing file, directory, or symlink is left untouched and causes export to fail. A staging or publication failure does not expose a partial bundle.
- MCP preparation does not activate a candidate implicitly. Activation is a separate tool call.
- Cache errors are treated as cache misses or best-effort store failures where appropriate; cache data never bypasses verification.

