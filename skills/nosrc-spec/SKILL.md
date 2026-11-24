---
name: nosrc-spec
description: Author valid SPEC.md contracts with ## Verification test cases and manage sandboxed WebAssembly endpoint generations through the nosrc MCP server.
---

# nosrc Spec & MCP Lifecycle

Use this skill whenever the user wants to create, test, verify, inspect, or export a `SPEC.md`-driven HTTP endpoint using `nosrc`.

## Prerequisites

- The `nosrc` binary must be installed on `PATH` (`cargo install --path .` from the `nosrc` repo).
- `LLM_API_KEY` must be set in the environment whenever synthesizing from `source: "spec"` or `source: "draft_spec"`.

## MCP Tools & Lifecycle Rules

`nosrc` exposes six in-process tools (`prepare_app`, `activate_app`, `invoke_app`, `get_app_status`, `rollback_app`, `export_app`). Respect these trust-boundary rules:

1. **Startup without `--spec` (default plugin mode):**
   - Use `prepare_app` with `source: "draft_spec"` and a complete `draft_spec` Markdown string to synthesize, compile to QuickJS Wasm, and run candidate verification.
   - **Never call `activate_app` on a `draft_spec` generation** when started without `--spec`—`nosrc` blocks activation until a human operator reviews the spec and restarts `nosrc mcp --spec /path/to/SPEC.md`.
   - Instead, call `export_app` with a **single directory name** (never a nested or absolute path) to export `SPEC.md`, `handler.js`, `component.wasm`, `assertions.json`, and `manifest.json` into `--export-root` (default `./.nosrc-exports`) for operator review.
2. **Startup with `--spec` (operator-trusted policy mode):**
   - Call `prepare_app` (`source: "spec"`), inspect results with `get_app_status`, promote via `activate_app`, test live requests via `invoke_app`, and revert a bad rollout via `rollback_app` (limited to the immediately previous retained generation).
3. **`source: "direct_js"` without `--spec`:**
   - Valid only as a compile/preparation check; it cannot be invoked, activated, or exported.

## Authoring Valid `SPEC.md` Files

### Guest Runtime Constraints
Design endpoints within `nosrc`'s current Wasmtime WIT sandbox:
- **No host capabilities:** Guest JS has no filesystem, network, process, database, or KV access. Keep handlers pure and self-contained.
- **Volatile state:** Module-level JS state persists across requests within one active generation, but resets on replacement or process restart.
- **Payload & execution limits:** UTF-8 request/response bodies up to 1 MiB, max 64 request headers, 30-second guest response deadline, and no message-framing/hop-by-hop response headers (`Content-Length`, `Transfer-Encoding`, `Connection`).

### `## Verification` Formatting Rules
Section headings (`## Capabilities`, `## Schema`, `## Endpoints`, `## Verification`) are matched **exactly** (case-insensitive). Unknown `## ` sections (such as `## Examples` or `## Notes`) are ignored as informational. Structured sections require `- ` bullets (`*`, `+`, and numbered lists are rejected) and `### <METHOD> <PATH>` headings.

Every declared endpoint **must** have at least one verification case under `## Verification`, or preparation will fail:
- Use a concrete request target in an `H3` code heading, replacing every `:parameter` segment with a sample value.
- **Never** repeat the same query parameter key in a verification heading (the handler ABI maps each query key to a single string).
- Allowed bullet assertions per case:
  - `- Header: <name>=<value>` (request header)
  - `- Response \`<status_code>\`` (scalar — at most once per case)
  - `- Response header: <name>=<value>` (exact match, case-insensitive header name)
  - `- Body: \`<exact_body>\`` (scalar — at most once per case)
  - `- Body contains: \`<substring>\`` (scalar — at most once per case)

### Template

```markdown
# <Service Name>

## Endpoints

### `GET /search`
Returns matching items for query `q` with optional `limit`.

## Verification

### `GET /search?q=hello%20world&limit=10`
- Header: accept=application/json
- Response `200`
- Response header: content-type=application/json
- Body contains: `hello`
```
