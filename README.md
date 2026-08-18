# fastmvm

[한국어](README.ko.md)

[![GitHub release](https://img.shields.io/github/v/release/qkdxorjs1002/fastmvm?include_prereleases)](https://github.com/qkdxorjs1002/fastmvm/releases)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust&logoColor=white)](Cargo.toml)
[![CI](https://github.com/qkdxorjs1002/fastmvm/actions/workflows/ci.yml/badge.svg)](https://github.com/qkdxorjs1002/fastmvm/actions/workflows/ci.yml)

**Disposable microVM execution for coding agents.** fastmvm is a daemonless Rust runtime that gives each one-shot command its own Linux microVM, streams its output, and destroys the sandbox on completion, timeout, cancellation, backend failure, or owner loss.

The Phase 0–5 vertical slice is implemented. Native execution is currently release-qualified only on Apple Silicon macOS with compatible released libkrun and libkrunfw builds. Linux and Windows remain compile-and-test targets; the process backend is a deterministic test double and does **not** provide VM isolation.

[Get started](#quick-start) · [Install](#installation) · [Run a sandbox](#run-a-sandbox) · [Use the MCP server](#mcp-server) · [Review the security model](#security-model)

## Why fastmvm?

Coding-agent harnesses need more than a child process, but they should not need a long-running privileged daemon or a reusable VM that retains untrusted state.

| Need | fastmvm behavior |
| --- | --- |
| Strong one-shot ownership | One helper process and one fresh microVM belong to one sandbox run |
| Predictable cleanup | Completion, timeout, cancellation, failure, and parent loss all converge on cleanup |
| Safe command handling | Commands are argv arrays; no implicit shell parsing is introduced |
| Conservative host access | Host workspaces become immutable ext4 images instead of direct virtio-fs exports |
| Bounded execution | The default wall timeout is one hour; unlimited execution is explicit |
| Agent-friendly integration | The CLI, async Rust SDK, and stdio MCP server share the same lifecycle model |

## Quick start

### Install with Homebrew

The Homebrew release currently targets Apple Silicon macOS:

```sh
brew tap qkdxorjs1002/tap
brew install fastmvm
fastmvm --version
fastmvm doctor --json
```

The formula installs `fastmvm`, `fastmvm-mcp`, the signed `fastmvm-vmm-helper`, and `e2fsprogs`. Native execution still requires compatible released libkrun and libkrunfw libraries. `doctor` reports the exact missing path, symbol, framework, or signing capability without changing the host.

### Verify the portable path

```sh
fastmvm run --backend process -- /usr/bin/printf 'hello from fastmvm\n'
```

This verifies the lifecycle and output path only. The process backend runs directly on the host and is **not a security sandbox**.

### Run a native microVM

Point fastmvm at released native dependencies, then require the full readiness gate:

```sh
export FASTMVM_HELPER_PATH="$(brew --prefix fastmvm)/bin/fastmvm-vmm-helper"
export FASTMVM_LIBKRUN_PATH="/path/to/libkrun.dylib"
export FASTMVM_LIB_DIR="/path/to/native/library-directory"

fastmvm doctor --strict
fastmvm run --image alpine@latest -- /bin/uname -a
```

The currently validated development stack is libkrun 1.19.4 with libkrunfw 5.5.0 on Apple Silicon macOS. The adapter detects the released libkrun 1.x root API and the explicit-resource 2.0 ABI at runtime; unreleased `main` ABI changes are not a compatibility target.

## Installation

### Requirements

For native execution:

- Apple Silicon macOS with Hypervisor.framework.
- Compatible released libkrun and libkrunfw builds.
- A helper signed with the `com.apple.security.hypervisor` entitlement.
- `mke2fs` from `e2fsprogs` when attaching a host workspace.
- Network access to the selected OCI registry when pulling an image.

For source development:

- Rust 1.85 or newer.
- The platform build tools required by Rust.
- The native dependencies above only for real-backend checks.

### Build from source

```sh
cargo build --release --locked \
  -p fastmvm-cli \
  -p fastmvm-mcp \
  -p fastmvm-vmm-helper

codesign --force --sign - \
  --entitlements assets/fastmvm-vmm.entitlements \
  target/release/fastmvm-vmm-helper
```

Ad-hoc signing is for local development only. Published release helpers are signed by the release workflow.

## Run a sandbox

### Inspect native readiness

```sh
fastmvm doctor
fastmvm doctor --json
fastmvm doctor --strict
```

`--strict` returns a failure status unless the native backend is ready.

### Pull an OCI image

```sh
fastmvm image pull alpine@latest --json
fastmvm image pull ghcr.io/example/image:tag \
  --cache-dir .fastmvm/cache
```

Registry manifests and blobs are digest-verified before layers are materialized. Registry credentials are accepted only as an explicit username/password pair through options or `FASTMVM_REGISTRY_USERNAME` and `FASTMVM_REGISTRY_PASSWORD`.

`oci-layout:` and `docker-archive:` references are parsed by the image layer but are not yet imported by the public CLI.

### Execute a command

```sh
fastmvm run \
  --image alpine@latest \
  --cpus 2 \
  --memory-mib 512 \
  --timeout 30s \
  -- /bin/echo hello
```

Everything after `--` is passed as an argv array. Shell syntax is interpreted only when you explicitly run a shell:

```sh
fastmvm run --image alpine@latest -- /bin/sh -c 'printf "%s\n" "$HOME"'
```

The guest environment is empty by default. Add individual values with `--env KEY=VALUE`, or use `--inherit-env` only when host environment forwarding is intentional.

The default timeout is one hour:

```sh
fastmvm run --backend process --timeout 10m -- /usr/bin/true
fastmvm run --backend process --timeout none -- /usr/bin/true
```

`none` (or `0`) is the explicit unlimited setting.

### Attach a read-only workspace

```sh
fastmvm run \
  --rootfs /path/to/materialized-rootfs \
  --workspace ./project \
  -- /bin/sh -c 'cat /workspace/Cargo.toml'
```

fastmvm walks the host tree without following symlinks, rejects unsafe entries, creates a mode-0444 ext4 image, and attaches it read-only at `/workspace`. The original host directory is never exposed directly through virtio-fs.

### Stream through a PTY

```sh
fastmvm run --image alpine@latest --tty --interactive -- /bin/sh
```

PTY allocation is available on the native backend. Live PTY resize on the macOS controller is not implemented yet.

### Benchmark the lifecycle

```sh
fastmvm benchmark \
  --backend process \
  --iterations 100 \
  -- /usr/bin/true
```

The JSON report includes minimum, p50, p95, p99, and maximum latency. The current prepared pool caches verified and materialized artifacts rather than booted guest-agent VMs, so reports call this mode `cached-cold`.

## MCP server

Start the newline-delimited stdio server with either backend:

```sh
fastmvm-mcp --backend process

FASTMVM_HELPER_PATH="$(brew --prefix fastmvm)/bin/fastmvm-vmm-helper" \
FASTMVM_LIBKRUN_PATH="/path/to/libkrun.dylib" \
FASTMVM_ROOTFS="/path/to/materialized-rootfs" \
FASTMVM_LIB_DIR="/path/to/native/library-directory" \
fastmvm-mcp --backend libkrun
```

The MCP server keeps stdout exclusively for protocol messages; diagnostics go to stderr.

| Tool | Purpose |
| --- | --- |
| `sandbox_exec` | Start a one-shot command or an asynchronous session |
| `sandbox_io` | Read cursor-based output, write or close stdin, resize, or signal |
| `sandbox_stop` | Stop a session and wait for cleanup |

Commands remain argv arrays in the MCP schema. Output bytes and stdin are base64-encoded, and output reads are bounded.

## Architecture

```text
CLI / Rust SDK / MCP
          |
   runtime supervisor
state, deadline, I/O, cleanup
          |
 per-VM vmm-helper process
     stable libkrun ABI
          |
 console + vsock (TSI off)
          |
    one Linux microVM
```

The helper is a process boundary because `krun_start_enter()` consumes the libkrun context and exits its calling process. Keeping it outside the CLI, SDK host, and MCP server gives the supervisor an unambiguous ownership handle.

The public one-shot lifecycle is:

```text
New → Preparing → Starting → Ready → Running → Stopping → Dead
                      \          \       \
                       └─────────── Failed ─────→ Dead
                                   TimedOut ────→ Dead
```

`Dead` means process handles, control sockets, I/O pumps, temporary files, and VM resources have been reclaimed. See [the architecture document](docs/architecture.md) for crate ownership and storage flow.

## Security model

The v1 threat model is untrusted Linux guest code invoked by a local user on that same user's macOS host. It does not claim protection from a hostile host user, a compromised hypervisor/VMM, or hostile multi-tenant operation.

Security defaults:

- No guest network interface is added by default.
- The control vsock is created with Transparent Socket Impersonation flags set to zero.
- Host source directories are converted to immutable read-only block images.
- The guest environment starts empty.
- Commands never gain implicit shell parsing.
- The default wall deadline is one hour.
- Stop escalates from TERM to KILL after a five-second grace period.
- OCI content is digest-verified and layers reject traversal, devices, unsafe links, and symlink-parent escapes.
- A prepared unit is consumed once; an untrusted VM is never reused after its lease.
- Parent loss terminates the helper and converges on cleanup.

Read [docs/security.md](docs/security.md) for trust boundaries and [docs/protocol.md](docs/protocol.md) for the bounded host/guest frame contract.

## Current boundaries

- Native execution is release-qualified only on Apple Silicon macOS.
- Linux and Windows compile and test in CI but do not ship native release binaries.
- The process backend is useful for deterministic tests, not isolation.
- Registry images are materialized; local OCI layouts and Docker archives are not yet imported.
- Native root filesystems use dedicated materialized virtio-fs directories, while host workspaces use read-only block images.
- Writable workspace overlays, copy-out/diff, a custom guest-agent handshake, and live PTY resize remain follow-up work.
- The artifact pool is `cached-cold`; it does not claim a pool of already booted reusable VMs.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Native macOS changes additionally require:

```sh
fastmvm doctor --json
```

Run the real-backend smoke suite when the signed helper, released libkrun/libkrunfw builds, and Hypervisor.framework capability are available. A skipped native check should name the exact missing capability.

More project detail:

- [Implementation plan](docs/implementation-plan.md)
- [Architecture](docs/architecture.md)
- [Security model](docs/security.md)
- [Protocol](docs/protocol.md)
- [Performance](docs/performance.md)

## Release

Pushing a stable tag that exactly matches the workspace version, for example `0.1.0`, starts [the release workflow](.github/workflows/release.yml). It:

1. Runs the full Rust quality gate on an Apple Silicon macOS runner.
2. Builds and release-signs `fastmvm`, `fastmvm-mcp`, and the entitled VMM helper.
3. Verifies the signatures and packaged process-backend smoke path.
4. Publishes the archive and SHA-256 file to GitHub Releases.
5. Generates, validates, and pushes `Formula/fastmvm.rb` to `qkdxorjs1002/homebrew-tap`.

Repository release secrets:

| Secret | Purpose |
| --- | --- |
| `HOMEBREW_TAP_TOKEN` | Write access to `qkdxorjs1002/homebrew-tap` |
| `MACOS_CERTIFICATE_P12` | Base64-encoded signing certificate and private key |
| `MACOS_CERTIFICATE_PASSWORD` | Password for the PKCS#12 bundle |
| `MACOS_SIGNING_IDENTITY` | Identity passed to `codesign` |

The workflow does not publish Linux, Windows, or Intel macOS binaries because those native adapters are not currently release-qualified.

## License

fastmvm is licensed under Apache-2.0.
