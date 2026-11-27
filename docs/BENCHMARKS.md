# v0.1 benchmark baseline

This is a reproducible, measurement-only baseline for the current nosrc implementation. It is intended for before/after comparisons on the same GitHub Actions runner class and toolchain. It is not a performance target, a capacity claim, or a general model-quality evaluation.

## Workloads

The required `SPEC evaluation` workflow runs the following deterministic workloads in the Rust test profile, sequentially (`--test-threads=1`):

| Workload | Coverage | Recorded measurements |
| --- | --- | --- |
| Offline corpus | Ten checked-in SPECs; reference JavaScript componentization and operator-authored verification | Per-SPEC elapsed preparation time, fuel high-water, memory high-water |
| Mock-provider corpus | The same ten SPECs through the real HTTP synthesizer client, componentization, model assertions, and trusted verification | Per-SPEC elapsed preparation time, fuel high-water, memory high-water |
| Repair regression | Deliberately invalid first candidate followed by a valid replacement | Functional regression only; not included in timing aggregates |

The offline and mock reports are written to `target/spec-evaluation/offline-report.json` and `target/spec-evaluation/mock-report.json`. The CI job uploads both reports together with runner/toolchain metadata as the `spec-evaluation-baseline` workflow artifact. Each artifact is tied to the workflow run and source commit; retain the artifact when recording a baseline.

## Reproduce locally

Use the same stable Rust toolchain and a quiet machine. From the repository root:

```sh
rustc -Vv
cargo -V
uname -a
cargo test --bin nosrc spec_evaluation -- --ignored --nocapture --test-threads=1
```

The command is offline: the mock-provider tests use a loopback HTTP server and do not require an LLM key or external provider access. The ignored live-provider test is not selected by this command.

Reports are emitted under `target/spec-evaluation/`. Preserve the JSON files alongside the command output and environment details when comparing runs. For meaningful comparisons, use the same commit, Rust toolchain, runner/CPU class, build profile, and test-thread count. Run more than once if investigating variance; the committed CI artifact is a reference observation, not a statistically rigorous benchmark suite.

## Methodology and interpretation

- Each per-SPEC timer starts immediately before `prepare_from_js` or `prepare_from_spec_with_synthesizer` and stops after that preparation succeeds. It excludes SPEC parsing, lifecycle construction, provider-server startup, and report serialization.
- Offline preparation includes componentization and trusted verification. Mock preparation additionally includes the loopback mock HTTP completion request and model-assertion verification.
- Fuel and memory are high-water telemetry across disposable verification instances for that generation. They are maxima, not sums, and are not wall-clock runtime throughput measurements.
- The corpus is intentionally small and representative, not statistically comprehensive. It does not measure concurrent HTTP throughput, tail latency, cold process startup, release-profile performance, or live-provider/network latency.
- The repair-path test guards correctness of replacement behavior; its duration is not part of the baseline.
- No pooling, optimization, or performance threshold is introduced by this issue. The baseline exists to make later measurements comparable.

## Baseline artifact

The GitHub Actions `SPEC evaluation` run on a commit produces the versioned-by-run `spec-evaluation-baseline` artifact containing the raw per-SPEC reports and environment metadata. Use the artifact from the merged `main` run as the v0.1 reference and cite its workflow run URL in release notes or follow-up performance work. Do not copy values from a different runner or toolchain into this document as universal performance claims.
