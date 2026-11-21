# Exported artifact format

## Overview

Run:

```sh
LLM_API_KEY="your-api-key" cargo run -- export --spec ./SPEC.md --out-dir ./dist
```

The command executes the normal verified preparation pipeline and writes five files into the destination directory. MCP's `export_app` tool uses this same exporter for retained generations. Export does not start the HTTP listener.

```text
dist/
├── SPEC.md
├── handler.js
├── component.wasm
├── assertions.json
└── manifest.json
```

## Files

### `SPEC.md`

The exact SPEC text used to synthesize the handler. Its SHA-256 is recorded in the manifest and verified when this file is present. Draft-policy bundles must include `SPEC.md` so the draft contract remains available alongside its generated assertions. Older operator-policy bundles without `SPEC.md` remain loadable for backward compatibility.

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
- `generated_assertions` are returned by the synthesizer. Draft-policy bundles require a non-empty generated suite. Operator-policy bundles may have an empty generated suite when exporting an MCP direct-JavaScript generation; their operator-trusted suite remains mandatory.

These arrays describe the assertion inputs. They are not a record of individual execution results. Host-owned ABI probes are built into the runtime and are not serialized into the bundle; offline serving reruns them using the current runtime version.

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
| `policy_mode` | `operator` for an operator-controlled trusted suite or `draft` when no operator-specific assertions exist |

The current manifest does not include a signature, signer identity, explicit per-assertion outcomes, effective runtime policy, compiler/toolchain bill of materials, provider response, or reproducibility guarantee.

## Verification semantics

Export occurs only after:
1. SPEC parsing and runtime capability validation.
2. Trusted SPEC verification.
3. Generated supplemental verification.
4. Creation of a fresh production handler instance.

Operator-policy exports require the trusted operator suite to pass; a generated suite may be empty for direct-JavaScript MCP generations. Draft-policy exports have no operator-trusted suite and require the draft SPEC verification cases plus model-generated assertions to pass. The manifest records this distinction; draft assertions are never relabeled as operator trust.

When an operator SPEC is loaded at startup and an agent supplies a different
`draft_spec` for synthesis, the generation can still be prepared and used by the
runtime, but `export_app` rejects that generation. The current bundle format has
one `SPEC.md`; exporting only the draft would fail to preserve the provenance of
the operator-trusted assertions. Export becomes available once the bundle format
can represent both SPEC sources explicitly.

A successful export means the artifact passed these configured checks in the current build/runtime environment. It is not a cryptographic attestation, formal proof, security certification, or guarantee that the handler is correct for all inputs.

## Offline bundle serving

Run a previously exported bundle without a SPEC file or model-provider credentials:

```sh
cargo run -- serve --bundle ./dist
```

Before opening the HTTP listener, the loader:

1. Requires the bundle directory and four core artifacts to be real directories/regular files (not symlinks), and applies file-size limits. Draft-policy bundles must also include `SPEC.md`; when present, it is opened without following symlinks and its digest is checked.
2. Parses the manifest and assertion document, validates SHA-256 field syntax, and checks assertion counts against the manifest.
3. Recomputes the component SHA-256 and requires it to match both `component_sha256` and `module_hash`.
4. Compiles the component with the current Wasmtime engine.
5. Runs the trusted and generated assertion suites on separate disposable component instances, then runs the runtime's fixed host-owned ABI probes on another disposable instance.
6. Creates a fresh serving instance only after every check succeeds.

The loader does not execute `handler.js`; the prebuilt `component.wasm` is the serving artifact. `handler.js` is required as an audit/review artifact and is checked for presence, regular-file type, size, and non-empty content, but the current manifest has no handler-source digest.

New exports include the exact SPEC text as `SPEC.md`, allowing the loader to recompute `spec_sha256`. Legacy operator-policy bundles without that file remain supported; for those, the digest can only be syntax-checked. Draft-policy bundles require `SPEC.md` because their embedded assertions are tied to that draft contract. The trusted assertions are the exported assertion inputs, not a separately authenticated copy of the original SPEC policy.

The manifest and assertions are unsigned. Recomputing hashes and rerunning assertions detects accidental corruption and inconsistencies, but does not authenticate provenance or stop an attacker who can replace the component, assertions, and manifest together. Keep bundle directories operator-controlled and use a trusted external signing/attestation mechanism if authenticity is required. Offline serving still opens the configured HTTP listener; it only removes the LLM/provider dependency from startup.

## Cache and reproducibility

The artifact cache is separate from the export directory. It is content-addressed using inputs and toolchain settings that affect generated artifacts. A cache hit does not skip component compilation or verification.

The manifest records SPEC and component digests, but it does not capture every input needed to independently reproduce the build. Provider-side model aliases may be mutable, and the manifest does not record a signed provider transcript or complete toolchain provenance. Do not treat the manifest as a reproducible-build statement.

## Filesystem behavior and handling

The export function creates the destination's parent directory if necessary, writes all five files into a uniquely named temporary sibling directory, and publishes the completed directory with a same-filesystem rename. Readers see either no destination or the complete bundle; a failure while staging leaves the destination untouched and the temporary directory is cleaned up.

The destination itself must not already exist. Export intentionally refuses to overwrite an existing file, directory, or symlink. This avoids claiming atomic replacement where portable filesystems do not provide an atomic replacement operation for non-empty directories. To export again, remove or move the previous destination first. Concurrent exports targeting the same absent destination are not a supported coordination mechanism; only one publication can win.

Treat exported source and components as build artifacts. Review them before deployment, restrict write access to export locations, and do not assume that the manifest is tamper-evident. Hashes detect accidental/mismatched content only when compared against a trusted manifest copy; the manifest itself is not signed.

