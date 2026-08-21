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

The current prerelease channel targets Apple Silicon macOS. The tap also provides the pinned gvproxy, libkrun, and libkrunfw versions required by moraebox.

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

`doctor` is non-destructive: its temporary cache-volume and gvproxy socket probes are removed before it exits. Use `morae doctor --json` for named checks covering actual cache-volume reflink/free-space, helper and library architecture/signing, pinned released ABI versions and symbols, disk tools, and gvproxy socket creation. Every non-passing check includes a remediation.

### 3. Run a command

```sh
morae run -- python3 -c 'print("hello from moraebox")'
```

The built-in default image is `docker.io/library/python:3.12`. moraebox pulls and verifies it on the first run, then reuses the materialized local cache. Each execution still gets a fresh VM.

## Using the CLI

### Configure global options and shell completion

Storage, structured output, and native dependency overrides are global options. They may appear before or after a subcommand, so these forms are equivalent:

```sh
morae --cache-dir /var/tmp/morae-cache image list --json
morae image list --cache-dir /var/tmp/morae-cache --json
```

The precedence is an explicit CLI option, then its `MORAE_*` environment variable, then automatic discovery or the user-wide default. Storage uses `MORAE_CACHE_DIR` and `MORAE_STATE_DIR`. Native dependency overrides use `MORAE_HELPER_PATH`, `MORAE_LIBKRUN_PATH`, `MORAE_GVPROXY_PATH`, `MORAE_LIB_DIR`, `MORAE_MKE2FS`, and `MORAE_E2FSCK`. There is no implicit project configuration file, so changing directories does not change this resolution order. Registry credentials and `MORAE_ROOTFS` remain scoped to commands that consume them.

Generate completion code for the current shell with:

```sh
source <(morae completion bash)
source <(morae completion zsh)
morae completion fish | source
```

### Choose resources and a timeout

```sh
morae run \
  --cpus 2 \
  --memory-mib 512 \
  --timeout 30s \
  -- python3 -c 'print("isolated")'
```

The default wall-clock timeout is one hour. Unlimited execution must be explicit with `--timeout none` or `--timeout 0`.

Retained output defaults to 64 MiB and can be bounded per run with `--output-limit 8MiB` (maximum 1 GiB). The TERM-to-force-cleanup grace period defaults to five seconds and can be set with `--kill-grace 750ms` (maximum 60 seconds). The same MCP controls are `sandbox_exec.output_limit_bytes` and `sandbox_exec.kill_grace_ms`; their units are explicit bytes and milliseconds.

Image-backed `run`, `benchmark`, and `box create` commands accept `--pull missing|always|never`. `missing` preserves the cache-first default, `always` refreshes the reference from the registry, and `never` is cache-only and does not contact the registry. JSON run results expose the actual materialized manifest as `startup.resolved_image_digest`; benchmark results use `resolved_image_digest`, and Box metadata uses `manifest_digest`. MCP `sandbox_exec` and `sandbox_box_create` expose the same choices as `pull_policy`, with the resolved digest returned in session status or Box metadata.

After argument parsing, a command execution failure with `--json` writes one JSON document to stdout as `{"error":{"code":"...","stage":"...","retryable":false,"message":"...","remediation":"..."}}` and exits nonzero. Typed CLI errors preserve a more specific runtime stage and mark timeout, busy-resource, and transient I/O failures as retryable while keeping the stable command-level error codes. MCP startup and registration diagnostics likewise carry typed stage and retryability metadata on stderr. Without `--json`, failures retain the human-readable `morae: ...` stderr format. Clap help and argument-syntax errors keep Clap's standard output contract.

Everything after `--` is passed as argv. Shell syntax is interpreted only if a shell is the command:

```sh
morae run --image alpine:latest --env MESSAGE=hello \
  -- /bin/sh -c 'printf "%s\n" "$MESSAGE"'
```

Use `--env KEY=VALUE` to add individual values. `--inherit-env` forwards the host environment and should be used only when that exposure is intentional. Explicit `--env` values override inherited values; a non-Unicode host variable name or value rejects the run instead of being silently dropped or altered.

### Opt into outbound network access

Guest networking is disabled by default. Enable it for one native VM run with `--network`:

```sh
morae run --network -- curl -I https://example.com
```

Network-enabled runs require `gvproxy`, which the Homebrew formula installs automatically. moraebox discovers `gvproxy` on `PATH`; for non-Homebrew installations, use `--gvproxy /path/to/gvproxy` or `MORAE_GVPROXY_PATH` when it is installed elsewhere. The native runtime starts a fresh gvproxy process and virtio-net endpoint for the run, then tears both down with the VM. The control vsock remains separate with all TSI feature flags disabled.

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

Registry manifests and blobs are digest-verified before layers are materialized. Private registries accept an explicit username/password pair through CLI options or `MORAE_REGISTRY_USERNAME` and `MORAE_REGISTRY_PASSWORD`. Bearer token realms must use HTTPS; credentials are sent only to the registry origin, its child auth origins, Docker Hub's token service, or an HTTPS origin explicitly trusted with the Rust SDK's `RegistryClient::with_allowed_credential_realm_origin` builder.

`--rootfs /path/to/rootfs` is an advanced alternative for an already materialized guest root directory. It bypasses image resolution and is mutually exclusive with `--image`.

### Continue work with a persistent Box

A normal run gets a new `SessionId`, a new microVM, and an ephemeral copy-on-write root disk that is deleted after cleanup. Create a Box only when later runs should keep filesystem changes:

```sh
BOX_ID=$(morae box create --image alpine:latest --json | jq -r .box_id)

morae run --box "$BOX_ID" -- /bin/sh -c 'echo retained > /root/result'
morae run --box "$BOX_ID" -- cat /root/result

morae box clone "$BOX_ID" --yes
morae box reset "$BOX_ID" --yes
morae box delete "$BOX_ID" --yes
```

`BoxId` identifies a persistent root filesystem lineage, not a VM or an authentication credential. Every `morae run --box` still creates a new microVM and `SessionId`; only files on that Box disk continue. Different Boxes have independent disks, and a second writer for the same Box fails immediately. `--box` cannot be combined with `--image`, `--rootfs`, or `--workspace`, and the non-isolating `process` backend rejects it.

Before a writable Box disk is exposed to a guest, moraebox atomically records `Dirty` metadata and flushes both the file and its parent directory. Only a clean helper shutdown returns it to `Ready`. A host crash, timeout, signal, helper failure, or failed spawn leaves it `Dirty`, so the next run executes `e2fsck -p` while holding the Box lease. Successful repair is recorded before the disk can be used again; an unrecoverable filesystem is marked `NeedsRepair` and blocked.

### Attach a read-only workspace

```sh
morae run \
  --workspace ./my-project \
  -- /bin/sh -c 'ls -la /workspace'
```

moraebox walks the host tree without following symlinks, rejects unsafe entries, sizes ext4 data and inodes from the scan, creates a read-only snapshot, and attaches it at `/workspace`. It does not expose the original host directory to the VM. Cache and state roots must remain outside the workspace source; overlapping paths are rejected before image preparation.

### Use an interactive terminal

```sh
morae run --image alpine:latest --tty --interactive -- /bin/sh
```

PTY allocation and live terminal resize are supported by the native backend. The CLI forwards host `SIGWINCH` events through the control protocol to the guest PTY.

### Manage local storage

```sh
morae image pull python:3.12
morae image list
morae image remove python:3.12

morae box create --image python:3.12
morae box list
morae box show BOX_ID
morae box clone BOX_ID --yes
morae box reset BOX_ID --yes
morae box delete BOX_ID --yes
morae box repair --dry-run
morae box repair --yes

morae cache info
morae cache reconcile --dry-run
morae cache reconcile --yes
morae cache prune --dry-run
morae cache prune --yes
morae cache clean --all --dry-run
morae cache clean --all --yes
```

By default, every command uses the user-wide `~/.moraebox/cache` and `~/.moraebox/state` directories, independent of the current working directory. Use `--cache-dir`, `--state-dir`, or their environment variables only when a command should use another location. When a storage-using command finds a matching project-local `.moraebox/cache` or `.moraebox/state`, it prints an explicit-use reminder on stderr. Existing project-local data is never selected or moved automatically; for example, continue using it with `morae box list --state-dir .moraebox/state`.

Cache size output distinguishes logical bytes from filesystem-allocated bytes. Rootfs sizes come from publish-time indexed metadata, so `image list` and `cache info` do not rescan every tree. Use `cache reconcile --dry-run` to detect missing, invalid, stale, or orphan metadata and `cache reconcile --yes` (also available as `cache repair --yes`) to repair it. `cache prune` also reclaims exact moraebox rootfs staging directories left by an interrupted pull while preserving complete roots and unknown entries.

Mutating cache operations require either `--dry-run` or `--yes`; durable Box mutations require `--yes`. `morae cache clean` removes rebuildable image, immutable base-disk, and ephemeral data, but never persistent Box disks under `~/.moraebox/state`. Image, Box, and cache commands support `--json` where structured output is useful.

`morae box list` keeps returning healthy Boxes when another entry is corrupt and includes an `errors` array in JSON output. `morae box repair --dry-run` previews those entries; `--yes` acquires each valid Box lock and moves corrupt entries into a private `state/quarantine` batch directory. Quarantine never deletes or reconstructs disk data, and busy entries remain in place with a per-entry failure.

Native CLI and MCP startup garbage-collect crash leftovers only after they are at least one hour old and their base, Box, or session lock is available. The collector recognizes only moraebox-generated `.creating`, `.deleting`, delete tombstone, reset temporary-disk, and atomic Box metadata names; it leaves active, recent, quarantined, and unknown entries untouched.

### Exercise the lifecycle without isolation

```sh
morae run --backend process -- /usr/bin/printf 'portable path\n'
morae benchmark --backend process --iterations 100 -- /usr/bin/true
```

`morae run`, `morae benchmark`, and `morae-mcp` default to the `libkrun` microVM backend. The `process` backend is available only through the explicit `--backend process` development opt-in shown above. It is useful for deterministic tests, CI, and integration development, but it is not a sandbox and must not be presented as one. Guest root options such as `--image`, `--rootfs`, and `--box` are rejected with the process backend instead of being ignored.

For native cached-start qualification, use a command that writes immediately so the report can measure the first guest output as a conservative command-start signal:

```sh
morae benchmark --image alpine:latest \
  --iterations 100 -- /bin/echo ready
```

The JSON report separates immutable-base lookup, Box lock, CoW clone, root preparation, helper spawn, first guest output, and full completion percentiles. Native runs report `mode: "cached-one-shot"`; an explicit process benchmark reports `mode: "host-process"` so host execution cannot be mistaken for microVM performance.

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

The installer uses the agent's official CLI and does not edit configuration files directly. It records the server executable, storage, rootfs, and discovered native dependency paths as absolute paths so the registration does not depend on the agent's working directory or `PATH`. Before invoking the agent CLI, it verifies that the server is executable and completes a bounded MCP `initialize` handshake with the exact registered argv and environment. A preflight failure leaves agent configuration untouched. If the agent CLI itself fails, follow the printed inspection and rollback guidance; the installer does not blindly remove a possibly pre-existing registration.

Use `--image`, `--cache-dir`, `--state-dir`, `--disk-size`, `--cpus`, `--memory-mib`, `--gvproxy`, or `--server-command` to customize the registration. For lifecycle testing without isolation, opt in with `--backend process`. Manual rollback commands are:

```sh
codex mcp remove moraebox
claude mcp remove --scope user moraebox
```

The server exposes execution tools plus persistent Box management:

| Tool | Purpose |
| --- | --- |
| `sandbox_exec` | Run one command or start an asynchronous session; optional `box_id` reuses persistent files |
| `sandbox_io` | Read bounded output, optionally long-poll with `wait_ms`, write or close stdin, resize, or send a signal |
| `sandbox_session_list` / `sandbox_session_status` | List connection-owned sessions or read one current status without waiting |
| `sandbox_stop` | Stop a session and wait for cleanup while retaining its record |
| `sandbox_remove` | Stop if needed and immediately remove retained session status and output |
| `sandbox_box_create` | Create a persistent Box from an OCI image |
| `sandbox_box_list` / `sandbox_box_get` | Inspect persistent Box metadata |
| `sandbox_box_delete` / `sandbox_box_reset` | Permanently mutate an idle Box with explicit confirmation |
| `sandbox_box_clone` | Create a new independent durable Box with explicit confirmation |

Commands remain argv arrays in the MCP schema. Output chunks are exposed as UTF-8 text so agents can read them directly; invalid UTF-8 bytes are replaced with `U+FFFD`. Stdin bytes remain base64-encoded. A waiting `sandbox_exec` response includes at most 1 MiB of output. When `has_more` is true, pass its `status.session_id` and `continuation_cursor` to `sandbox_io` within five minutes; executions whose output fits inline are removed immediately. `sandbox_io.wait_ms` can wait up to 30 seconds for output or session completion without a fixed polling interval, and reports `wait_timed_out` when the bound expires. The server permits up to 32 active runs. Completed asynchronous sessions remain readable for five minutes, can be inspected with `sandbox_session_list` and `sandbox_session_status`, or are released explicitly by `sandbox_remove`; disconnecting the stdio client removes all connection-owned sessions.

## How it works

```text
CLI / Rust SDK / MCP server
             │
      runtime supervisor
   lifecycle · deadline · I/O
             │
 image rootfs → immutable base ext4
       ├─ no BoxId: per-run CoW disk → delete
       └─ BoxId: persistent disk → retain
             │
    one VMM helper process
       released libkrun ABI
             │
 diagnostic console + control vsock (TSI off)
             │
 bounded versioned protocol
 session · stream · sequence
             │
  trusted embedded guest agent
       argv · I/O · signals · PTY
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
| `moraebox-box` | Persistent Box metadata, leases, immutable base disks, and ephemeral CoW disks |
| `moraebox-image` | OCI registry access, digest verification, cache orchestration, metadata/storage, and workspace snapshots |
| `moraebox-runtime` | Backends with separated native root/network lifecycles, supervision, sessions, diagnostics, and traces |
| `moraebox-sdk` | Async embedding API |
| `moraebox-cli` | The `morae` interface with separated command and interactive I/O modules |
| `moraebox-mcp` | MCP tool protocol, stdio transport, and agent registration |
| `moraebox-vmm-helper` | Signed native boundary around libkrun plus the embedded Linux guest agent |
| `moraebox-protocol` | Bounded, versioned host/guest framing, messages, and state validation |

## Security model

The current threat model is untrusted Linux guest code launched by a local user on that same user's macOS host.

Security-relevant defaults include:

- no guest network interface unless one run explicitly opts in;
- a control vsock with Transparent Socket Impersonation flags set to zero;
- version, session, stream, sequence, direction, and state validation on every control message, with an 8 MiB frame limit;
- a guest agent embedded in the signed helper and re-injected, mode-checked, and byte-verified for every root-disk lease, including persistent Boxes;
- opt-in egress uses a per-run gvproxy virtio-net process that is cleaned up with the VM;
- no host environment forwarding;
- no implicit shell parsing;
- immutable, read-only workspace snapshots;
- digest-verified OCI content with traversal, device, unsafe-link, and symlink-parent checks;
- a one-hour default deadline with TERM-to-KILL escalation;
- single-use prepared units and cleanup after parent loss.

Persistent Boxes are an explicit exception to filesystem disposal, not to VM disposal. They can retain untrusted guest changes across runs, so reuse a Box only for related work and delete or reset it before crossing a trust boundary. Exclusive leases prevent concurrent writers to one Box.

moraebox does **not** claim to protect against a hostile host user, a compromised hypervisor or VMM, or hostile multi-tenant operation. The process backend does not provide isolation.

## Platform support and current limits

| Area | Status |
| --- | --- |
| Apple Silicon macOS | Native libkrun execution; current release-qualified target |
| Linux and Windows | Compile-and-test targets; no native release runtime |
| libkrun stack | Validated with released libkrun 1.19.4 and libkrunfw 5.5.0 |
| Image sources | Remote OCI registries; local OCI layouts and Docker archives are not imported yet |
| VM reuse | Materialized artifacts may be cached; booted untrusted VMs are never reused |
| Box persistence | Opt-in full root filesystem persistence; each run still uses a fresh microVM |
| Workspaces | Read-only snapshots; writable overlays and copy-out/diff are future work |
| Interactive I/O | PTY and live terminal resize supported over the bounded control protocol |

This is an early-stage project. Review the boundaries above before using it for security-sensitive workloads.

## Build from source

Rust 1.85 or newer and Go 1.23 or newer are required. The helper build cross-compiles a static Linux/arm64 guest agent with the Go standard library and embeds it in the signed host helper.

```sh
cargo build --release --locked \
  -p moraebox-cli \
  -p moraebox-mcp \
  -p moraebox-vmm-helper

codesign --force --sign - \
  --entitlements assets/moraebox-vmm.entitlements \
  target/release/morae-vmm-helper
```

Native execution additionally needs compatible released libkrun/libkrunfw builds, Hypervisor.framework, `mke2fs`, `e2fsck`, and `debugfs` from `e2fsprogs`, and `gvproxy` for opt-in networking. `debugfs` installs the trusted agent into each leased root disk without exposing a host source directory to the guest. `morae doctor --json` reports base native readiness and network readiness separately, and probes the effective `--cache-dir` volume rather than an unrelated system temporary volume. Both doctor and runtime require a successful Unix datagram connection to the bound gvproxy vfkit endpoint; a path that merely exists is not ready. Startup failures retain only the last 16 KiB of gvproxy stderr for bounded diagnostics.

Native startup finishes root disk preparation before starting gvproxy. A root preparation failure therefore creates no network process, and a later helper spawn failure or cancelled network setup explicitly kills and reaps gvproxy before its runtime state is removed.

Every native run repeats the same prerequisite checks reported by `morae doctor --json` before preparing a root disk or starting gvproxy. The helper must be executable, signed for the host architecture, and carry the Hypervisor entitlement. libkrun 1.19.4 and libkrunfw 5.5.0 must be signed host-architecture files whose canonical Homebrew paths prove the pinned released versions; libkrun must also export `krun_add_vsock_port` and the other required ABI, plus `krun_add_net_unixgram` for networked runs. An unverifiable copied or custom library is rejected with doctor-based remediation instead of reaching helper spawn.

The advanced `--rootfs /path/to/rootfs` directory mode remains a low-level compatibility path and uses libkrun's direct exec interface. Managed image and Box root-disk runs use the bounded host/guest protocol by default.

CLI and MCP native execution share the `moraebox-sdk` configuration layer. It resolves disk tools and native helpers with the same override precedence, opens the same image/Box/base/ephemeral stores with startup garbage collection, and constructs image or rootfs sources with one platform, disk-size, and filesystem-tool policy. Frontends retain only command- and transport-specific choices.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny --all-features --locked check
```

CI runs the locked portable quality gate on macOS, Linux, and Windows. A separate Ubuntu job compiles and tests the locked workspace with the declared Rust 1.85 MSRV. The dependency-policy job uses cargo-deny to reject advisories, unapproved licenses, wildcard dependencies, and unknown package sources; duplicate transitive versions remain visible as warnings. Every external GitHub Action is pinned to an immutable commit SHA and retains its release tag in a comment for maintainers. The Apple Silicon macOS job installs the pinned native dependencies and runs the signed real-backend suite; if the runner lacks a required native capability, the job records the exact missing capability and dependency-setup outcome in the GitHub Step Summary. Once the capabilities are present, build, image preparation, doctor, or smoke failures fail the job instead of being reported as skips.

Native macOS changes also require `morae doctor --json` and the real-backend smoke suite when the signed helper and native dependencies are available.

On Apple Silicon macOS, run the signed network security gate after the portable checks:

```sh
scripts/native-egress-e2e.sh
```

The gate ad-hoc signs the debug helper with `assets/moraebox-vmm.entitlements`, requires a ready default cached image, and verifies network-off DNS/TCP/UDP denial, network-on egress, and cleanup after timeout, cancellation, and helper failure. Set `MORAE_NATIVE_E2E_IMAGE` to select another ready cached image, or `MORAE_NATIVE_E2E_CACHE_DIR` for a non-default cache. `MORAE_EGRESS_HOST` and `MORAE_EGRESS_UDP_DNS` override the external TCP/DNS probe targets when required by the test environment.

Bug reports and focused pull requests are welcome. Please include the backend, host platform, exact command, and `morae doctor --json` output when reporting native runtime problems. Remove local paths or other sensitive values before sharing diagnostics.

## License

moraebox is licensed under the [Apache License 2.0](LICENSE).
