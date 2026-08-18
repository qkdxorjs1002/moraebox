# moraebox

[한국어](README.ko.md)

[![GitHub release](https://img.shields.io/github/v/release/qkdxorjs1002/moraebox?include_prereleases)](https://github.com/qkdxorjs1002/moraebox/releases)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust&logoColor=white)](Cargo.toml)
[![CI](https://github.com/qkdxorjs1002/moraebox/actions/workflows/ci.yml/badge.svg)](https://github.com/qkdxorjs1002/moraebox/actions/workflows/ci.yml)

**Disposable microVM execution for coding agents.** moraebox is a daemonless Rust runtime that gives each one-shot command its own Linux microVM, streams its output, and destroys the sandbox on completion, timeout, cancellation, backend failure, or owner loss.

The Phase 0–5 vertical slice is implemented. Native execution is currently release-qualified only on Apple Silicon macOS with compatible released libkrun and libkrunfw builds. Linux and Windows remain compile-and-test targets; the process backend is a deterministic test double and does **not** provide VM isolation.

[Get started](#quick-start) · [Install](#installation) · [Run a sandbox](#run-a-sandbox) · [Use the MCP server](#mcp-server) · [Review the security model](#security-model)

## Why moraebox?

Coding-agent harnesses need more than a child process, but they should not need a long-running privileged daemon or a reusable VM that retains untrusted state.

| Need | moraebox behavior |
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

# Stable release
brew install moraebox

# Newest stable or prerelease
brew install moraebox@pre

morae --version
morae doctor --json
```

Both formulae download a checksummed release source archive and compile `morae`, `morae-mcp`, and `morae-vmm-helper` on the Mac running `brew install`; no prebuilt moraebox bottle or binary is downloaded. Homebrew supplies Rust as a build dependency, installs `e2fsprogs`, and ad-hoc signs the locally built helper with the Hypervisor entitlement. This signature does not identify a developer and is not Apple-notarized.

The stable and prerelease formulae conflict because they install the same executables; uninstall the current formula before switching channels. Native execution still requires compatible released libkrun and libkrunfw libraries. `doctor` reports the exact missing path, symbol, framework, or signing capability without changing the host.

### Verify the portable path

```sh
morae run --backend process -- /usr/bin/printf 'hello from moraebox\n'
```

This verifies the lifecycle and output path only. The process backend runs directly on the host and is **not a security sandbox**.

### Run a native microVM

Point moraebox at released native dependencies, then require the full readiness gate:

```sh
export MORAE_HELPER_PATH="$(brew --prefix moraebox)/bin/morae-vmm-helper"
export MORAE_LIBKRUN_PATH="/path/to/libkrun.dylib"
export MORAE_LIB_DIR="/path/to/native/library-directory"

morae doctor --strict
morae run --image alpine@latest -- /bin/uname -a
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
  -p moraebox-cli \
  -p moraebox-mcp \
  -p moraebox-vmm-helper

codesign --force --sign - \
  --entitlements assets/moraebox-vmm.entitlements \
  target/release/morae-vmm-helper
```

This is also the signing model used by the Homebrew formula: the helper is built and ad-hoc signed on the installation Mac. It carries the required entitlement but no Developer ID identity or Apple notarization.

## Run a sandbox

### Inspect native readiness

```sh
morae doctor
morae doctor --json
morae doctor --strict
```

`--strict` returns a failure status unless the native backend is ready.

### Pull an OCI image

```sh
morae image pull alpine@latest --json
morae image pull ghcr.io/example/image:tag \
  --cache-dir .moraebox/cache
```

Registry manifests and blobs are digest-verified before layers are materialized. Registry credentials are accepted only as an explicit username/password pair through options or `MORAE_REGISTRY_USERNAME` and `MORAE_REGISTRY_PASSWORD`.

`oci-layout:` and `docker-archive:` references are parsed by the image layer but are not yet imported by the public CLI.

### Execute a command

```sh
morae run \
  --image alpine@latest \
  --cpus 2 \
  --memory-mib 512 \
  --timeout 30s \
  -- /bin/echo hello
```

Everything after `--` is passed as an argv array. Shell syntax is interpreted only when you explicitly run a shell:

```sh
morae run --image alpine@latest -- /bin/sh -c 'printf "%s\n" "$HOME"'
```

The guest environment is empty by default. Add individual values with `--env KEY=VALUE`, or use `--inherit-env` only when host environment forwarding is intentional.

The default timeout is one hour:

```sh
morae run --backend process --timeout 10m -- /usr/bin/true
morae run --backend process --timeout none -- /usr/bin/true
```

`none` (or `0`) is the explicit unlimited setting.

### Attach a read-only workspace

```sh
morae run \
  --rootfs /path/to/materialized-rootfs \
  --workspace ./project \
  -- /bin/sh -c 'cat /workspace/Cargo.toml'
```

moraebox walks the host tree without following symlinks, rejects unsafe entries, creates a mode-0444 ext4 image, and attaches it read-only at `/workspace`. The original host directory is never exposed directly through virtio-fs.

### Stream through a PTY

```sh
morae run --image alpine@latest --tty --interactive -- /bin/sh
```

PTY allocation is available on the native backend. Live PTY resize on the macOS controller is not implemented yet.

### Benchmark the lifecycle

```sh
morae benchmark \
  --backend process \
  --iterations 100 \
  -- /usr/bin/true
```

The JSON report includes minimum, p50, p95, p99, and maximum latency. The current prepared pool caches verified and materialized artifacts rather than booted guest-agent VMs, so reports call this mode `cached-cold`.

## MCP server

Start the newline-delimited stdio server with either backend:

```sh
morae-mcp --backend process

MORAE_HELPER_PATH="$(brew --prefix moraebox)/bin/morae-vmm-helper" \
MORAE_LIBKRUN_PATH="/path/to/libkrun.dylib" \
MORAE_ROOTFS="/path/to/materialized-rootfs" \
MORAE_LIB_DIR="/path/to/native/library-directory" \
morae-mcp --backend libkrun
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
morae doctor --json
```

Run the real-backend smoke suite when the signed helper, released libkrun/libkrunfw builds, and Hypervisor.framework capability are available. A skipped native check should name the exact missing capability.

More project detail:

- [Implementation plan](docs/implementation-plan.md)
- [Architecture](docs/architecture.md)
- [Security model](docs/security.md)
- [Protocol](docs/protocol.md)
- [Performance](docs/performance.md)

## Release

Pushing a validated tag without a leading `v` starts [the release workflow](.github/workflows/release.yml). Stable tags use `x.y.z`; prerelease tags use `x.y.z-alphaN`, `x.y.z-betaN`, or `x.y.z-rcN`, for example `0.0.0-alpha1`. The tag is the release version and is synchronized into the runner's temporary workspace before the quality gate; the source commit is not rewritten. The workflow:

1. Runs the full Rust quality gate on an Apple Silicon macOS runner.
2. Creates a versioned source archive containing the synchronized workspace manifest and locked dependency set, then verifies its contents.
3. Publishes only the source archive and SHA-256 file to GitHub Releases, marking prerelease tags accordingly.
4. Updates the rolling `Formula/moraebox-pre.rb` and `moraebox@pre` alias in `qkdxorjs1002/homebrew-tap`; stable tags also update `Formula/moraebox.rb`.
5. On each `brew install`, the formula builds the three executables from that source with Cargo's locked dependencies and ad-hoc signs the installed VMM helper with `assets/moraebox-vmm.entitlements`.

The workflow does not publish moraebox binaries or use Developer ID signing and Apple notarization. Each installation therefore performs its own source build; the resulting ad-hoc signature proves neither developer identity nor notarization.

Repository release secrets:

| Secret | Purpose |
| --- | --- |
| `HOMEBREW_TAP_TOKEN` | Write access to `qkdxorjs1002/homebrew-tap` |

## License

moraebox is licensed under Apache-2.0.
