# Performance gates

Performance measurements separate preparation from execution. OCI download, layer conversion, and a new workspace snapshot are never hidden inside the boot number.

## Timelines

- `prepare`: reference resolution through reusable image/workspace templates.
- `cached cold`: helper spawn through guest agent `Hello`.
- `exec`: agent-ready through child process acknowledgement and first output byte.
- `cleanup`: child exit through helper, socket, and session directory removal.
- `prepared`: request receipt through execution in an already booted, never-leased ready unit.

## Initial gates

- cached cold helper spawn → agent ready: p50 target 100 ms, p95 gate 200 ms;
- agent ready → exec acknowledgement: p95 20 ms;
- prepared request → exec acknowledgement: p95 100 ms;
- command exit → cleanup complete: p95 100 ms.

Every benchmark records host OS/architecture, fastmvm commit, libkrun/libkrunfw versions, image digest, workspace digest, vCPU/RAM, sample count, and p50/p95/p99. At least 100 measured iterations follow warm-up.

## Current Phase 0 observation

The initial Apple Silicon development host is supported and has Rust, Clang, LLVM, and Hypervisor.framework. Official artifacts for libkrun 1.19.4, libkrunfw 5.5.0, and smolvm 1.8.3 were checksum-verified in a temporary Phase 0 directory. A development binary carrying `com.apple.security.hypervisor` passed the ABI doctor with all required symbols.

An Alpine image was packed once and then executed ten times through `smolvm pack run` with network disabled. Every run exited zero. Observed end-to-end wall times in seconds were:

```text
2.1535 2.1446 2.1670 2.1657 2.1665
2.1506 2.1448 2.1512 2.1525 2.1690
```

The ten-run p50 is approximately 2.153 seconds and the observed maximum is 2.169 seconds. This includes packed-asset handling, agent startup, container execution, and cleanup; it is not a libkrun kernel-only boot number. Phase 2 must add internal timestamps before using this result for optimization. The process backend numbers remain correctness fixtures only.

## Phase 5 native result

The built-in benchmark executed `/bin/true` 100 times sequentially. Every sample spawned a new signed helper and a new microVM against the already verified/materialized `alpine@latest` rootfs (`sha256:e7a1a92a5bfeee40966aea60f0796b0e7917cc35591542701834f03a68fa3d18`). OCI download and first materialization were excluded and the report therefore labels this `cached-cold`.

```text
samples  failures  min      p50      p95      p99      max
100      0         48.753ms 63.940ms 78.724ms 84.102ms 525.839ms
```

The measured p95 meets the 100 ms target on the reference Apple Silicon host. The single 525.839 ms maximum is a real tail outlier and remains an optimization/observability target; it is not removed from the report.
