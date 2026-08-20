<p align="center">
  <img src="assets/moraebox.png" alt="moraebox sandcastle logo" width="320">
</p>

# moraebox

[한국어](README.ko.md)

[![GitHub release](https://img.shields.io/github/v/release/qkdxorjs1002/moraebox?include_prereleases)](https://github.com/qkdxorjs1002/moraebox/releases)
[![CI](https://github.com/qkdxorjs1002/moraebox/actions/workflows/ci.yml/badge.svg)](https://github.com/qkdxorjs1002/moraebox/actions/workflows/ci.yml)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust&logoColor=white)](Cargo.toml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](#license)

**A disposable Linux microVM for every coding-agent command.**

moraebox is a daemonless Rust runtime that starts one fresh microVM for one command, streams its output, and tears the sandbox down when the command finishes—or when its timeout, owner, or backend fails.

> [!IMPORTANT]
> Native microVM execution is currently release-qualified on Apple Silicon macOS. The portable `process` backend is a test and development aid; it runs commands directly on the host and provides **no VM isolation**.

## Why moraebox

Coding agents execute unfamiliar code, install dependencies, and run build tools. A child process is convenient, but it is not an isolation boundary. Long-lived VMs, on the other hand, can retain untrusted state between jobs.

moraebox is built around a smaller lifecycle: one owner, one command, one VM, then cleanup.

- **Disposable by design.** A prepared sandbox is consumed once and is never returned to an untrusted VM pool.
- **Cleanup on every exit path.** Success, timeout, cancellation, backend failure, and parent loss all converge on teardown.
- **No hidden shell.** Commands stay argv arrays unless you explicitly invoke a shell.
- **No inherited host environment.** The guest starts with an empty environment by default.
- **Conservative workspace access.** Host source trees become immutable, read-only ext4 images instead of direct virtio-fs shares.
- **Agent-ready interfaces.** The CLI, async Rust SDK, and stdio MCP server use the same runtime lifecycle.

## Quick start

### 1. Install

The current prerelease channel targets Apple Silicon macOS. The tap also provides the pinned libkrun and libkrunfw versions required by moraebox.

```sh
brew tap qkdxorjs1002/tap
brew trust --tap qkdxorjs1002/tap
brew install moraebox@pre
```

Homebrew 6 requires trust for non-official taps. Whole-tap trust is used here because the moraebox formula depends on companion formulae from the same tap. Review the tap before trusting it; see Homebrew's [Tap Trust documentation](https://docs.brew.sh/Tap-Trust) for narrower trust options.

The formula builds `morae`, `morae-mcp`, and `morae-vmm-helper` from the checksummed release source. It also ad-hoc signs the helper with the Hypervisor entitlement. No prebuilt moraebox binary is installed.

### 2. Check the native runtime

```sh
morae --version
morae doctor --strict
```

`doctor` is read-only. Use `morae doctor --json` for a machine-readable report of missing libraries, symbols, frameworks, tools, or signing capabilities.

### 3. Run a command

```sh
morae run -- python3 -c 'print("hello from moraebox")'
```

The built-in default image is `docker.io/library/python:3.12`. moraebox pulls and verifies it on the first run, then reuses the materialized local cache. Each execution still gets a fresh VM.

## Using the CLI

### Choose resources and a timeout

```sh
morae run \
  --cpus 2 \
  --memory-mib 512 \
  --timeout 30s \
  -- python3 -c 'print("isolated")'
```

The default wall-clock timeout is one hour. Unlimited execution must be explicit with `--timeout none` or `--timeout 0`.

Everything after `--` is passed as argv. Shell syntax is interpreted only if a shell is the command:

```sh
morae run --image alpine:latest --env MESSAGE=hello \
  -- /bin/sh -c 'printf "%s\n" "$MESSAGE"'
```

Use `--env KEY=VALUE` to add individual values. `--inherit-env` forwards the host environment and should be used only when that exposure is intentional.

### Opt into outbound network access

Guest networking is disabled by default. Enable it for one native VM run with `--network`:

```sh
morae run --network -- curl -I https://example.com
```

Network-enabled runs require `gvproxy`. moraebox discovers `gvproxy` on `PATH`; use `--gvproxy /path/to/gvproxy` or `MORAE_GVPROXY_PATH` when it is installed elsewhere. The native runtime starts a fresh gvproxy process and virtio-net endpoint for the run, then tears both down with the VM. The control vsock remains separate with all TSI feature flags disabled.

The same opt-in is `RunSpec.network = true` in the Rust SDK and `"network": true` in an MCP `sandbox_exec` call. The `process` backend rejects this VM-specific option because it already runs directly in the host network context and does not provide VM isolation.

Enabling network access gives guest code outbound access available through the host user's network context. It is not a destination allowlist or a separate network security boundary.

### Select an OCI image

```sh
# Use another image once
morae run --image debian:bookworm -- cat /etc/os-release

# Change the default for future runs
morae image default debian:bookworm

# Restore the built-in python:3.12 default
morae image default --unset
```

Registry manifests and blobs are digest-verified before layers are materialized. Private registries accept an explicit username/password pair through CLI options or `MORAE_REGISTRY_USERNAME` and `MORAE_REGISTRY_PASSWORD`.

`--rootfs /path/to/rootfs` is an advanced alternative for an already materialized guest root directory. It bypasses image resolution and is mutually exclusive with `--image`.

### Attach a read-only workspace

```sh
morae run \
  --workspace ./my-project \
  -- /bin/sh -c 'ls -la /workspace'
```

moraebox walks the host tree without following symlinks, rejects unsafe entries, creates a read-only ext4 snapshot, and attaches it at `/workspace`. It does not expose the original host directory to the VM.

### Use an interactive terminal

```sh
morae run --image alpine:latest --tty --interactive -- /bin/sh
```

PTY allocation is supported by the native backend. Live terminal resize is not implemented yet.

### Manage local storage

```sh
morae image pull python:3.12
morae image list
morae image remove python:3.12

morae cache info
morae cache prune --dry-run
morae cache prune --yes
morae cache clean --all --dry-run
morae cache clean --all --yes
```

Destructive cache operations require either `--dry-run` or `--yes`. Image and cache commands support `--json` where structured output is useful.

### Exercise the lifecycle without isolation

```sh
morae run --backend process -- /usr/bin/printf 'portable path\n'
morae benchmark --backend process --iterations 100 -- /usr/bin/true
```

The `process` backend is useful for deterministic tests, CI, and integration development. It is not a sandbox and must not be presented as one.

## Connect a coding agent

`morae-mcp` is a newline-delimited stdio MCP server. Its stdout is reserved for protocol messages; diagnostics go to stderr.

Register the native server with Codex or Claude Code:

```sh
morae-mcp install codex
morae-mcp install claude-code
```

Preview the exact command and argv without changing agent configuration:

```sh
morae-mcp install codex --dry-run
```

The installer uses the agent's official CLI and does not edit configuration files directly. Use `--image`, `--cache-dir`, `--cpus`, `--memory-mib`, or `--gvproxy` to customize the registration. For lifecycle testing without isolation, opt in with `--backend process`.

The server exposes three tools:

| Tool | Purpose |
| --- | --- |
| `sandbox_exec` | Run one command or start an asynchronous session; network access defaults to off |
| `sandbox_io` | Read bounded output, write or close stdin, resize, or send a signal |
| `sandbox_stop` | Stop a session and wait for cleanup |

Commands remain argv arrays in the MCP schema. Output chunks are exposed as UTF-8 text so agents can read them directly; invalid UTF-8 bytes are replaced with `U+FFFD`. Stdin bytes remain base64-encoded.

## How it works

```text
CLI / Rust SDK / MCP server
             │
      runtime supervisor
   lifecycle · deadline · I/O
             │
    one VMM helper process
       released libkrun ABI
             │
   console + vsock (TSI off)
 optional virtio-net ↔ gvproxy
             │
      one Linux microVM
```

The helper is a process boundary because libkrun's start operation consumes its context and exits the calling process. Keeping it outside the CLI, SDK host, and MCP server gives the supervisor one unambiguous ownership handle for the VM.

Every one-shot run follows the same state machine:

```text
prepare → start → ready → running → stop → dead
   └──────── failure / timeout / cancellation ───────┘
```

`dead` means that helper processes, control sockets, I/O pumps, temporary files, and VM resources have been reclaimed.

### Workspace layout

| Crate | Responsibility |
| --- | --- |
| `moraebox-core` | Run specification, lifecycle states, signals, and bounded output |
| `moraebox-image` | OCI registry access, digest verification, cache, and workspace snapshots |
| `moraebox-runtime` | Backends, supervision, sessions, diagnostics, and traces |
| `moraebox-sdk` | Async embedding API |
| `moraebox-cli` | The `morae` command-line interface |
| `moraebox-mcp` | Stdio MCP server and agent registration |
| `moraebox-vmm-helper` | Signed native boundary around libkrun |
| `moraebox-protocol` | Bounded host/guest protocol types |

## Security model

The current threat model is untrusted Linux guest code launched by a local user on that same user's macOS host.

Security-relevant defaults include:

- no guest network interface unless one run explicitly opts in;
- a control vsock with Transparent Socket Impersonation flags set to zero;
- opt-in egress uses a per-run gvproxy virtio-net process that is cleaned up with the VM;
- no host environment forwarding;
- no implicit shell parsing;
- immutable, read-only workspace snapshots;
- digest-verified OCI content with traversal, device, unsafe-link, and symlink-parent checks;
- a one-hour default deadline with TERM-to-KILL escalation;
- single-use prepared units and cleanup after parent loss.

moraebox does **not** claim to protect against a hostile host user, a compromised hypervisor or VMM, or hostile multi-tenant operation. The process backend does not provide isolation.

## Platform support and current limits

| Area | Status |
| --- | --- |
| Apple Silicon macOS | Native libkrun execution; current release-qualified target |
| Linux and Windows | Compile-and-test targets; no native release runtime |
| libkrun stack | Validated with released libkrun 1.19.4 and libkrunfw 5.5.0 |
| Image sources | Remote OCI registries; local OCI layouts and Docker archives are not imported yet |
| VM reuse | Materialized artifacts may be cached; booted untrusted VMs are never reused |
| Workspaces | Read-only snapshots; writable overlays and copy-out/diff are future work |
| Interactive I/O | PTY supported; live resize is future work |

This is an early-stage project. Review the boundaries above before using it for security-sensitive workloads.

## Build from source

Rust 1.85 or newer is required.

```sh
cargo build --release --locked \
  -p moraebox-cli \
  -p moraebox-mcp \
  -p moraebox-vmm-helper

codesign --force --sign - \
  --entitlements assets/moraebox-vmm.entitlements \
  target/release/morae-vmm-helper
```

Native execution additionally needs compatible released libkrun/libkrunfw builds, Hypervisor.framework, `gvproxy` for opt-in networking, and `mke2fs` from `e2fsprogs` when a workspace is attached. `morae doctor --json` reports base native readiness and network readiness separately.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Native macOS changes also require `morae doctor --json` and the real-backend smoke suite when the signed helper and native dependencies are available. CI runs the portable quality gate on macOS, Linux, and Windows.

Bug reports and focused pull requests are welcome. Please include the backend, host platform, exact command, and `morae doctor --json` output when reporting native runtime problems. Remove local paths or other sensitive values before sharing diagnostics.

## License

moraebox is licensed under the [Apache License 2.0](LICENSE).
