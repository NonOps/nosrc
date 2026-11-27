# SPEC evaluation corpus

This corpus is a small, versioned regression set for nosrc synthesis and verification. It is deliberately representative rather than statistically comprehensive.

## Contents

Ten SPECs and reference JavaScript handlers cover JSON normalization, required-field validation, status codes and response headers, malformed JSON, query parameters, resource creation, plain text, empty responses, list filtering, and path parameters.

Each reference handler is checked offline through the real componentization and trusted verification path. The required mock-provider test then returns those same reference handlers through the actual synthesis, componentization, model-assertion verification, and operator-verification pipeline. It asserts that every SPEC has a non-empty model-visible assertion suite covering every verification case and that those model assertions pass. A separate deterministic repair-path test deliberately returns a componentizable but incorrect first candidate, checks that the host sends only a safe `verification_failed` repair summary, then returns a valid replacement and asserts exactly one replacement attempt. The mock tests also check that operator verification details are absent from the model-visible prompt.

## Run

The offline and mock-provider corpus tests run in a dedicated required CI job so their CPU-heavy componentization work does not compete with the general regression suite. That job selects only tests whose names contain `spec_evaluation`; it never runs the live-provider test. The live-provider test is ignored, is not selected by the required CI command, and requires an explicitly configured `LLM_API_KEY`. It is opt-in local evaluation only. Run the deterministic tests locally with:

```sh
cargo test --bin nosrc spec_evaluation -- --ignored --nocapture --test-threads=1
```

To evaluate a configured live provider, set LLM_API_KEY and optionally LLM_BASE_URL / LLM_MODEL, then run:

```sh
cargo test live_provider_evaluation -- --ignored --nocapture --test-threads=1
```

The offline and mock modes write per-SPEC JSON reports beneath `target/spec-evaluation/`; reports include first-pass verification, replacement attempts, canonical failed-check categories, fuel and memory high-water telemetry across disposable trusted, host-probe, model, and supplemental verification instances, and elapsed duration. High-water values are maxima across instances (not sums); last-invocation fuel is taken from the latest verification instance that reports it. Report categories are stable values: `cache_artifact_invalid`, `cache_component_invalid`, `candidate_invalid`, `componentization_failed`, `verification_failed`, and `other` (unknown internal values are grouped as `other`). Duplicate categories are removed per SPEC. Reports intentionally omit prompts, generated source, request bodies, and credentials.

The dedicated CI job uploads the offline and mock reports plus runner/toolchain metadata as the `spec-evaluation-baseline` artifact. See [the v0.1 benchmark methodology](../docs/BENCHMARKS.md) for workload boundaries, interpretation, and comparison guidance.

The corpus uses the current ABI and verification semantics. Repeated-header cases are deferred until the ABI v2 contract can represent them without loss. Results describe only these checked-in SPECs and must not be presented as a general model-quality claim.
