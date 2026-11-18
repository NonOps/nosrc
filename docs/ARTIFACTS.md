# Exported artifact format

## Overview

Run:

```sh
LLM_API_KEY="your-api-key" cargo run -- export --spec ./SPEC.md --out-dir ./dist
```

The command executes the normal verified preparation pipeline and writes four files into the destination directory. It does not start the HTTP listener.

```text
dist/
├── handler.js
├── component.wasm
├── assertions.json
└── manifest.json
```

## Files

### `handler.js`

The self-contained JavaScript ES module synthesized for the SPEC. The handler exports `handle(requestJson)` and follows the JSON request/response envelope contract. It is source intended for inspection and review.

### `component.wasm`

The compiled QuickJS WebAssembly component produced from the handler source and current WIT contract. The component is compiled with the current Wasmtime engine before export.

### `assertions.json`

Contains two arrays:

```json
{
  "trusted_assertions": [],
  "generated_assertions": []
}
```

- `trusted_assertions` are derived from the operator-authored SPEC verification cases.
- `generated_assertions` are returned by the synthesizer. The export pipeline rejects an empty generated assertion suite. The export CLI uses SPEC-driven synthesis; direct-JavaScript preparation is an MCP workflow, not an export input mode.

These arrays describe the assertion inputs. They are not a record of individual execution results.

### `manifest.json`

Current manifest fields:

| Field | Meaning |
|---|---|
| `service_name` | Service name parsed from the SPEC |
| `spec_sha256` | SHA-256 digest of the exact SPEC text read for this run |
| `module_hash` | Hash identifying the compiled component in the engine |
| `component_sha256` | SHA-256 digest of exported component bytes |
| `trusted_assertions_count` | Number of trusted SPEC assertions |
| `generated_assertions_count` | Number of generated supplemental assertions |
| `rationale` | Optional synthesis rationale |

The current manifest does not include a signature, signer identity, explicit per-assertion outcomes, effective runtime policy, compiler/toolchain bill of materials, provider response, or reproducibility guarantee.

## Verification semantics

Export occurs only after:
1. SPEC parsing and runtime capability validation.
2. Trusted SPEC verification.
3. Generated supplemental verification.
4. Creation of a fresh production handler instance.

Both required suites must pass for this export path. Generated assertions supplement, but do not replace, trusted SPEC assertions.

A successful export means the artifact passed these configured checks in the current build/runtime environment. It is not a cryptographic attestation, formal proof, security certification, or guarantee that the handler is correct for all inputs.

## Cache and reproducibility

The artifact cache is separate from the export directory. It is content-addressed using inputs and toolchain settings that affect generated artifacts. A cache hit does not skip component compilation or verification.

The manifest records SPEC and component digests, but it does not capture every input needed to independently reproduce the build. Provider-side model aliases may be mutable, and the manifest does not record a signed provider transcript or complete toolchain provenance. Do not treat the manifest as a reproducible-build statement.

## Filesystem behavior and handling

The export function creates the destination's parent directory if necessary, writes all four files into a uniquely named temporary sibling directory, and publishes the completed directory with a same-filesystem rename. Readers see either no destination or the complete bundle; a failure while staging leaves the destination untouched and the temporary directory is cleaned up.

The destination itself must not already exist. Export intentionally refuses to overwrite an existing file, directory, or symlink. This avoids claiming atomic replacement where portable filesystems do not provide an atomic replacement operation for non-empty directories. To export again, remove or move the previous destination first. Concurrent exports targeting the same absent destination are not a supported coordination mechanism; only one publication can win.

Treat exported source and components as build artifacts. Review them before deployment, restrict write access to export locations, and do not assume that the manifest is tamper-evident. Hashes detect accidental/mismatched content only when compared against a trusted manifest copy; the manifest itself is not signed.

