# Architecture

## Component ownership

```text
CLI / Rust SDK / sidecar SDK / MCP
                |
          runtime supervisor
 state, deadline, output, cleanup, trace
                |
        per-VM vmm-helper process
          stable libkrun C ABI
                |
  explicit console + vsock (TSI disabled)
                |
     libkrun injected init / workload
       argv, env, cwd, stdio, exit
```

The helper is a process boundary, not just a crate boundary. `krun_start_enter()` consumes the libkrun context and exits the calling process with the workload status. Keeping the call in a per-VM child prevents a workload from terminating a CLI, SDK host, or MCP server and gives the supervisor an unambiguous ownership handle.

## Crates

- `fastmvm-core`: portable request, timeout, output cursor, and lifecycle contracts.
- `fastmvm-protocol`: bounded, versioned host/guest protobuf frames.
- `fastmvm-image`: OCI reference/auth/platform selection, digest CAS, safe layer application, and immutable workspace images.
- `fastmvm-runtime`: process/libkrun backends, supervisor, incremental sessions, prepared artifact pool, doctor, and trace.
- `fastmvm-vmm-helper`: signed per-VM libkrun 1.x/2.0 adapter and parent-loss watchdog.
- `fastmvm-sdk`: async process-owned start/exec/I/O/stop API.
- `fastmvm-cli`: doctor, OCI image pull, native/process run, and percentile benchmark.
- `fastmvm-mcp`: newline-delimited stdio MCP with `sandbox_exec`, `sandbox_io`, and `sandbox_stop`.

## Lifecycle

The public state machine is:

```text
New → Preparing → Starting → Ready → Running → Stopping → Dead
                      \          \       \
                       └─────────── Failed ─────→ Dead
                                   TimedOut ────→ Dead
```

`Dead` means process handles, control sockets, I/O pumps, temporary files, and VM resources have been reclaimed. Exit metadata is stored separately so cleanup does not erase the reason.

## Storage path

OCI blobs and derived rootfs trees are content-addressed. Layer application rejects traversal, device/FIFO entries, unsafe links, and existing symlink parents while implementing OCI whiteouts. A host workspace is walked without following symlinks and materialized as a separate ext4 image. The image is mode 0444, opened by libkrun with `read_only=true`, and mounted read-only at `/workspace`.

No Phase 0–5 path requires a global daemon. An MCP session has one explicit owner process. The generic prepared pool is bounded, keyed by image/workspace/policy digests, expires idle units, and consumes every lease once. Today those units represent prepared artifacts; a booted guest-agent pool is intentionally not claimed.
