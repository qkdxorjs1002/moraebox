# moraebox full functional test — 2026-08-22

## Verdict

The host quality gate, the signed Apple Silicon smoke suite, and the full Rust
workspace test suite running inside a moraebox microVM passed. The live
functional suite found one release-blocking persistent Box durability defect
and one Box bundle sparsity/performance defect candidate.

## Environment

- Host: Apple Silicon macOS 26.5 (`Darwin 25.5.0 arm64`)
- Host Rust: 1.97.1
- Dogfood guest: `rust:1.85-bookworm`, Rust 1.85.1, Go 1.23.12
- libkrun: 1.19.4
- libkrunfw: 5.5.0
- gvproxy: 0.8.9
- Test commit: `aba6197` (`fix/morae-regression-failures-20260822`)
- OCI image digest used for native CLI, Box, and MCP checks:
  `sha256:45458a89e698e0d3e2abc0fac50a233e19ce6a34fa9b3b12c43b4a4d88ddd421`

## Passed checks

### Host and signed native gates

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`: 382 passed, 0 failed
- `scripts/ci-native-smoke.sh`
  - helper ad-hoc signing and entitlement validation
  - strict JSON doctor: native backend and network ready
  - released ABI and symbols for libkrun/libkrunfw
  - gvproxy socket handshake and egress
  - signed real-backend egress test

The first host test attempt was blocked only because the execution sandbox did
not permit the local mock registry to bind a loopback socket. The same complete
command passed outside that policy boundary.

### Full test suite inside moraebox

The committed source tree and the cached Go 1.23 toolchain were copied into a
fresh `rust:1.85-bookworm` microVM. Host environment values were not inherited;
guest-only `PATH`, `CARGO_HOME`, `RUSTUP_HOME`, `GOROOT`, and `HOME=/root` were
provided explicitly.

- `cargo fmt --all -- --check`: passed
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed
- `cargo test --workspace --locked`: 382 passed, 0 failed

The source was compiled under `/src` on a 16 GiB ephemeral root disk. A writable
workspace overlay was not large enough for the complete test build and failed
with `No space left on device`; increasing `--disk-size` does not enlarge that
overlay.

### Live CLI, lifecycle, and isolation checks

- process backend argv preservation, explicit environment, environment
  non-inheritance, stdout/stderr ordering, exit status 7, timeout status 124,
  and rejection of the VM-only network option
- native argv preservation and environment non-inheritance
- native stdout/stderr separation, non-zero exit propagation, timeout cleanup,
  and retained-output truncation
- three simultaneous native microVM runs
- read-only workspace enforcement
- writable workspace overlay, add/modify diff, atomic workspace copy-out, and
  preservation of the host source
- ordinary directory/file copy-in and copy-out
- native PTY: stdin/stdout/stderr all reported as TTY
- host SIGINT cancellation: exit status 130 with cleanup
- network egress via the signed native suite
- process benchmark threshold check
- native warm benchmark: 4/4 completed, 0 failures, 2 prepared-pool hits,
  2 misses, 608,362 us completion p95

### MCP

- process-backend initialize and `sandbox_exec`
- native libkrun initialize and `sandbox_exec`
- native MCP result reported a fresh SessionId, the expected image digest,
  `backend=libkrun`, `state=dead`, and `mcp-native` stdout
- workspace tests covered async sessions, cursor I/O, cancellation, session
  cleanup, Box tools, schemas, and protocol-only stdout

### Image, cache, and Box management

- image list and default resolution
- cache info, reconcile dry-run, prune dry-run, and full clean dry-run
- Box create, list/filter/show, rename, label/tag update, single-writer rejection,
  clone, export, import, reset, and delete
- clone and imported Box contents were readable in independent new microVMs
  when the original guest explicitly flushed the filesystem
- all three test Boxes were deleted and the isolated test state was empty

## Finding 1 — normal Box exit can lose guest writes

Severity: release blocker candidate.

The README persistence example failed on the real backend:

```sh
morae run --box "$BOX_ID" -- /bin/sh -c 'echo readme-retained > /root/readme-result'
morae run --box "$BOX_ID" -- cat /root/readme-result
```

The first run exited 0 and the Box returned to `Ready`. The second run received
a new SessionId but exited 1 because the file did not exist. A separate Python
write/read pair failed the same way.

Adding an explicit guest flush made persistence work:

```sh
morae run --box "$BOX_ID" -- /bin/sh -c \
  'echo synced-retained > /root/synced-result; sync'
morae run --box "$BOX_ID" -- cat /root/synced-result
```

The synced file also survived Box clone and export/import. This strongly points
to missing guest filesystem flush or graceful shutdown before the host ends the
VM. In `guest-agent/main_linux.go`, `sendExitAndWait` sends the Exit frame and
waits forever for host termination; it does not sync the writable root before
the host helper exits.

Recommended regression: on the signed real backend, run the README example
without `sync` and require the second VM to read the file. The clean-exit path
should flush the writable filesystem before reporting a Box as reusable.

## Finding 2 — exported Box bundle was not physically sparse

Severity: performance and disk-usage defect candidate.

A 4 GiB virtual Box with approximately 1.22 GiB allocated produced a
4,294,971,392-byte bundle that also occupied approximately 4 GiB physically.
Export took about 6 minutes 20 seconds, and each import attempt took about
3 minutes. The export path enables `tar::Builder::sparse(true)`, but the
real ext4 disk image holes were not preserved in the resulting bundle.

The current round-trip unit test verifies content and imported disk sparsity but
does not bound the exported bundle's allocated blocks. Add a real sparse source
assertion for the bundle itself and avoid full virtual-size hashing passes where
possible.

An import with a duplicate Box name was safely rejected, but only after the
large bundle had been verified and extracted. Earlier name-conflict detection
would avoid that unnecessary work.

## Test setup notes, not product failures

- A 1 GiB Box disk is too small for the selected Python image; 4 GiB succeeded.
- OCI image environment variables are not inherited automatically. The dogfood
  VM required explicit Rust/Go paths, consistent with the secure environment
  contract.
- MCP requests piped into an immediately closed stdin are cancelled by design;
  keeping the connection open through the response succeeded.
- A direct control byte written through the automation PTY did not emulate a
  terminal-generated SIGINT reliably. Sending SIGINT to the exact morae host
  PID produced exit 130 and immediate cleanup.
