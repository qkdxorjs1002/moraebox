# Repository instructions

## Scope

fastmvm is a Rust workspace for a disposable coding-agent sandbox. Keep the portable lifecycle independent from the native libkrun adapter. Do not make the process backend appear to provide VM isolation.

## Required invariants

- A one-shot run follows prepare → start → ready → running → stop → dead.
- Timeout, cancellation, backend failure, and parent loss all lead to cleanup.
- The default timeout remains one hour and unlimited execution remains explicit.
- Commands are argv arrays. Do not introduce implicit shell parsing.
- Do not forward the host environment by default.
- Do not expose a host source directory directly through virtio-fs.
- Native networking must add vsock with TSI feature flags set to zero and must pass the egress tests.
- Never reuse an untrusted VM after its lease ends.
- Keep MCP stdout free of logs and diagnostics.

## Quality gate

Run before declaring a task complete:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Native macOS changes also require `fastmvm doctor --json` and the real-backend smoke suite when the signed dependencies are available. A skipped native check must state the exact missing capability.

The development helper may be ad-hoc signed with `assets/fastmvm-vmm.entitlements`; release artifacts require the project release-signing workflow.

## Dependency and file hygiene

- Pin native ABI compatibility to a released libkrun version; do not silently consume unreleased `main` ABI changes.
- Keep generated build output under `target/` and runtime state under `.fastmvm/` or the platform cache directory.
- Preserve user changes and never commit TAPL workflow data.
- Do not commit credentials, registry tokens, guest secrets, VM disks, or trace files containing content.
