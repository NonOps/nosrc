# ABI v2 feasibility spike

This directory contains a deliberately minimal end-to-end test for the first
mandatory ABI v2 feasibility gate: a `componentize-qjs` component importing a
host-defined WIT interface and calling it from JavaScript.

## Run

From the repository root:

```sh
cargo test --test abi_v2_host_import -- --nocapture
```

The integration test uses the repository-pinned `componentize-qjs` dependency and the
same synchronous QuickJS runtime mode as the production componentizer. It:

1. Componentizes `host-import.js` against `host-import.wit`.
2. Defines the imported `nosrc:abi-v2-spike/host` interface in a Wasmtime
   component linker.
3. Instantiates the component and calls its exported `roundtrip` function.
4. Verifies that JavaScript called the host function and returned its result.

A successful run exits successfully. The assertion checks the exact host-provided value returned through JavaScript.

This is a feasibility harness, not a production capability implementation.
No network, filesystem, persistence, or other host authority is exposed.

## Gate

Do not finalize the ABI v2 representation or implement capabilities until this
test succeeds on the pinned dependency and supported CI platforms. If the
component cannot import and call the host interface, stop and revisit the
capability architecture.

The remaining spike work (records, variants, options, lists, binary values,
text codecs, and fuel/memory measurements) follows only after this gate is
demonstrated.
