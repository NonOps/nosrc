# nosrc

**nosrc** is an experimental Rust project exploring a specification-driven runtime that synthesizes WebAssembly handlers and verifies candidate behavior before deployment.

The long-term design is described in [ARCHITECTURE.md](ARCHITECTURE.md). That document is a target architecture, not a statement that every described subsystem is implemented.

## Current status

This repository is an early prototype, not a production-ready application runtime.

Currently present:

- A Wasmtime engine with instruction-fuel metering and a per-instance linear-memory limiter
- A host ABI with initial embedded SQLite / redb capabilities and outbound HTTP support
- HTTP request and response envelope types
- An initial Markdown SPEC parser
- An OpenAI-compatible synthesis client and structured output types
- A shadow verifier for Wasm modules and contract assertions
- An Axum ingress router implementation with a hot-swappable active-generation pointer

Important gaps:

- The CLI currently initializes the Wasmtime engine and exits. It does **not** start the ingress server or load and deploy a SPEC.
- The end-to-end SPEC → synthesis → compile → verify → activation pipeline is not wired together.
- The embedded QuickJS / Wizer snapshotting path described in the architecture is not implemented.
- Autonomous regression capture, healing, and rollback are not implemented.
- The external-state mode and its external database/cache integrations are not implemented.
- The current verifier is a prototype; its isolation and state-lifecycle model still needs further design and testing.

Do not use this prototype to serve production traffic or execute untrusted generated code.

## Requirements

- A stable Rust toolchain supported by the pinned dependency versions
- Cargo

## Build and test

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Show the current CLI options:

```sh
cargo run -- --help
```

At this stage, invoking the binary initializes the Wasmtime engine and exits; it does not launch an HTTP service.

## Design and security

Read [ARCHITECTURE.md](ARCHITECTURE.md) for the intended system design and [AGENTS.md](AGENTS.md) for repository-specific engineering constraints.

The security model is not yet complete. In particular, outbound HTTP policy needs redirect-target and DNS-resolution protections before it can be treated as a robust SSRF boundary. Generated assertions are inputs to verification, not independent proof of correctness. A production deployment path must also establish artifact provenance, state migration semantics, bounded resource use, and safe rollback.

## License

Apache-2.0. See [LICENSE](LICENSE) if present.
