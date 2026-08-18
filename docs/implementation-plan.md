# Phase 0–5 implementation plan

## Implementation status

- Phase 0–1: complete for the portable foundation and macOS feasibility gate.
- Phase 2: native one-shot, separate stdio, PTY, timeout, TSI-off vsock, exit propagation, and parent watchdog implemented and exercised on macOS.
- Phase 3: registry OCI pull, bearer auth, platform selection, digest CAS, safe layers/whiteouts, materialized rootfs, and read-only ext4 workspace block implemented. Writable CoW/diff/copy-out is pending.
- Phase 4: native-first CLI, async SDK sessions, and stdio MCP exec/I/O/stop implemented. A separate sidecar binary is unnecessary because the MCP process is the owner sidecar.
- Phase 5: bounded digest-keyed one-use artifact pool, TTL/replenishment, strict tests, and 100-iteration benchmark implemented. A booted guest-agent pool and sleep/wake fault suite are pending.

## Phase 0: macOS feasibility gate

- Inspect host architecture, OS, toolchain, Hypervisor.framework, signing identity, and entitlement.
- Pin libkrun 1.19.4 with libkrunfw 5.5.0 and probe the required ABI.
- Prove explicit TSI-off vsock, read-only block, console/PTY, exit propagation, and teardown.
- Compare cached cold measurements with the same image on smolvm when available.
- Record missing system dependencies as a precise gate, never as a passing test.

Exit: native prerequisites and ABI pass, or the exact external installation/signing blocker is reported while the portable implementation remains testable.

## Phase 1: portable foundation

- Rust workspace, lifecycle state machine, request/exit models, bounded cursor output.
- Versioned protobuf protocol with length and version validation.
- Backend interface and deterministic process backend.
- Supervisor ownership, one-hour/unlimited deadline, TERM→grace→KILL, output pumps, trace.
- Doctor and process-backed CLI integration tests.

Exit: fmt, clippy, and workspace tests pass; timeout and process-group cleanup are observed.

## Phase 2: native macOS vertical slice

- Dedicated signed vmm-helper with a stable libkrun adapter.
- Static guest agent and agent-ready handshake over TSI-off vsock.
- One-shot argv exec, stdin EOF, separate output, exit mapping.
- Interactive PTY, resize, signal, parent-loss, timeout, and stale cleanup.

Exit: real `true`, output, non-zero exit, signal, PTY, timeout, network-none, and repeated cleanup smoke tests pass.

## Phase 3: OCI and workspace data path

- OCI reference aliases, manifest/index platform selection, authenticated registry pull, digest CAS.
- Safe layer application including whiteouts, hardlinks, metadata, and path validation.
- Immutable root template and session CoW clone.
- Safe workspace snapshot, read-only block attachment, guest read-only/CoW views, diff/copy-out.

Exit: malicious layer fixtures are rejected and host workspace digest remains unchanged after guest writes.

## Phase 4: public surfaces

- Native-first CLI run/session/image/doctor/benchmark commands.
- Async Rust SDK and process-owned sidecar contract.
- stdio MCP `sandbox_exec`, `sandbox_io`, and `sandbox_stop` with cursor output.
- Policy limits and redacted structured trace.

Exit: CLI and SDK lifecycle tests pass; MCP initialize/list/call works with no stdout contamination.

## Phase 5: prepared pool and hardening

- Image/workspace/policy keyed prepared units; one lease followed by destruction.
- Bounded pool size, idle TTL, owner-loss teardown, background replenishment.
- 100-iteration cold/prepared benchmark with p50/p95/p99.
- Protocol and OCI fuzz/property tests; output/disk/PID exhaustion and crash/sleep-wake coverage.

Exit: prepared p95 is at or below 100 ms on the reference host, or the report identifies the dominant measured segment and preserves correctness without claiming the SLO.
