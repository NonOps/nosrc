# ABI v2 feasibility and conformance fixtures

These experiments are fixtures, not production ABI or capability changes.

## Existing feasibility and mapping probes

Run:

```sh
cargo test --test abi_v2_host_import -- --nocapture
cargo test --test abi_v2_mapping -- --nocapture
```

The host-import probe establishes that componentize-qjs can call an imported
host interface. The mapping probe covers nested records, variants, options,
`list<u8>` / `Uint8Array`, and raw 1 MiB binary mapping costs.

## Realistic JSON body representation comparison

Run:

```sh
cargo test --test abi_v2_json_benchmark -- --nocapture
```

The benchmark sends the same exactly 1 MiB ASCII JSON request through the two
candidate WIT body shapes:

1. A record containing authoritative `list<u8>` bytes plus
   `option<string>` UTF-8 text.
2. A bytes-only `list<u8>` request, decoded in the guest with the exact
   `decodeUtf8` helper documented to generated handlers, then passed to
   `JSON.parse`.
3. A tagged variant containing either text or binary; this benchmark measures
   the explicitly selected text case.

All three request paths execute guest `JSON.parse` and `JSON.stringify`;
the record keeps both byte and text representations live, while the bytes-only
path includes the guest-side UTF-8 conversion cost that was previously missing.
The test measures each path with a separate Wasmtime store and a 1,000,000,000-fuel
measurement ceiling, so one expensive path does not prevent the others from
being measured. It reports per-invocation fuel and limiter-observed peak
requested linear-memory size. The production-selected record path and response
serialization must remain within 250,000,000 fuel and 64 MiB linear-memory growth.
The bytes-only path is a deliberately rejected alternative: if guest-side UTF-8
decoding exhausts fuel, the test records that evidence without failing the
selected production path. It separately measures creation
of an exactly 1 MiB JSON response with `JSON.stringify`.

The benchmark intentionally does not infer text/binary intent from UTF-8
validity. The host selects the text variant explicitly. The binary round-trip
case includes NUL and invalid UTF-8 bytes and requires byte-for-byte equality.

The memory figure is Wasmtime limiter-observed linear-memory growth, not process
RSS or total host allocation. Fuel is measured around each exported invocation;
component instantiation is outside the per-invocation fuel measurement.

## HTTP no-body conformance already present in the runtime

The current host behavior is suppression, not rejection:

- `ResponseEnvelope::into_axum_response` suppresses guest-supplied bodies for
  `204`, `205`, and `304`.
- Ingress middleware strips bodies from every `HEAD` response, including
  guest responses that otherwise contain a body.

Existing tests cover the guest-supplied-body cases:
- `no_content_reset_content_and_not_modified_responses_have_no_body`
  in `src/engine/http_envelope.rs`.
- `ingress_dispatches_requests_to_the_active_quickjs_handler_and_strips_head_body`
  in `src/ingress/mod.rs` (the corresponding GET returns a non-empty body).

No production behavior is changed by these fixtures.
