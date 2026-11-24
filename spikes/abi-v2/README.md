# ABI v2 feasibility spike

This directory contains reproducible componentize-qjs experiments for the ABI v2
feasibility and mapping questions. These are test fixtures, not production
capabilities or a selected production ABI.

## Host-import feasibility gate

Run:

```sh
cargo test --test abi_v2_host_import -- --nocapture
```

The integration test uses the repository-pinned `componentize-qjs` dependency
and the same synchronous QuickJS runtime mode as the production componentizer.
It componentizes `host-import.js` against `host-import.wit`, links the imported
`nosrc:abi-v2-spike/host` interface in Wasmtime, calls the exported
`roundtrip`, and checks the exact result `host:componentize-qjs`.

This gate passed on Linux, Windows, and macOS in CI on commit
`810f4183d48f940a2250ed579629b037da97324d` (PR #86). No network, filesystem,
persistence, or other host authority is exposed.

## Mapping and binary-body measurements

Run:

```sh
cargo test --test abi_v2_mapping -- --nocapture
```

The second test exercises the exact JavaScript/WIT mapping against a dynamic
Wasmtime host import:

- Nested records use JavaScript objects and camelCase property names
  (`request.meta.requestId`, `request.meta.attempt`).
- A WIT variant is passed as a tagged object (`{ tag: "bytes", val: Uint8Array }`).
- `option<string>` none and some values are passed as JavaScript `null` and
  a string, respectively.
- `list<u8>` is passed as a `Uint8Array`; the host checks every byte.
- The test reports whether QuickJS exposes `TextEncoder` and `TextDecoder`
  and whether their UTF-8 round trip works. The result is printed, not assumed.
- A 1 MiB `list<u8>` request decode and 1 MiB response encode are measured in
  separate fresh Wasmtime stores. Fuel consumption is printed independently.

The test gives each measurement 100,000,000 fuel so it can print diagnostics
for an operation that exceeds the current production budget of 50,000,000.
It then explicitly fails if either the 1 MiB request decode or response encode
uses more than 50,000,000 fuel. Each store enforces the current 64 MiB
linear-memory growth limit and records the largest requested total linear-memory
size observed by the limiter. That number is the requested memory size, not a
growth delta, process RSS, or total component memory.

No measured values are checked into this document until the test has run on CI.
Review the printed fuel and memory measurements before choosing typed WIT
versus JSON/base64. The experiment does not by itself approve an ABI v2 design.
