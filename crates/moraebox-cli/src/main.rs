#![forbid(unsafe_code)]

use std::{
    ffi::OsString,
    fs,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use moraebox_box::{
    BaseDiskSpec, BaseDiskStore, BoxMetadata, BoxQuery, BoxRepairReport, BoxSortBy, BoxState,
    BoxStore, CheckpointId, CheckpointMetadata, CreateBox, CreateCheckpoint, ForkCheckpoint,
    UpdateBox,
};
use moraebox_core::{
    BoxId, CopyInSpec, CopyOutSpec, ImagePullPolicy, MAX_KILL_GRACE, MAX_OUTPUT_LIMIT, NetworkMode,
    NetworkPolicy, OutputChannel, OutputReadError, PublishProtocol, PublishRequest, RunSpec,
    SessionState, Signal, StoragePaths, TimeoutPolicy, WORKSPACE_DIFF_GUEST_PATH, WorkspaceMode,
    resolve_cache_dir, resolve_state_dir,
};
use moraebox_image::{
    CacheReconcileReport, CacheUsage, CachedImage, CleanReport, Credentials, ImageCache,
    ImageProgressStage, Platform, PreparedImage, PruneReport, RemoveReport,
    RootfsMetadataIssueKind, WorkspaceSnapshot, WorkspaceStage,
};
use moraebox_runtime::{
    Backend, BackendCapabilities, DiskToolPaths, DoctorReport, IsolationLevel, LibkrunBackend,
    NativeRuntimePaths, PoolConfig, PreparedRootPool, ProcessBackend, RunBudget, RunStage,
    SessionError, SessionHandle, SessionManager, SessionStatus, Supervisor,
};
use moraebox_sdk::{ManagedStorage, NativeRuntimeOverrides, NativeSandboxConfig};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

mod commands;
mod errors;
mod interactive;
mod profile;

use commands::{
    execute, exit_code, parse_box_label, parse_box_label_filter, parse_box_label_key,
    parse_box_name, parse_box_tag, parse_copy_in, parse_copy_out, parse_disk_size, parse_env,
    parse_kill_grace, parse_output_limit, parse_publish,
};
use errors::{CliError, CliErrorSource};
use interactive::run_interactive;

fn stderr_line_ending() -> &'static str {
    let stderr = io::stderr();
    let stderr_is_terminal = stderr.is_terminal();
    #[cfg(unix)]
    let output_processing = if stderr_is_terminal {
        use nix::sys::termios::{OutputFlags, tcgetattr};

        tcgetattr(&stderr)
            .ok()
            .map(|termios| termios.output_flags.contains(OutputFlags::OPOST))
    } else {
        None
    };
    #[cfg(not(unix))]
    let output_processing = None;

    terminal_line_ending(stderr_is_terminal, output_processing)
}

const fn terminal_line_ending(
    stderr_is_terminal: bool,
    output_processing: Option<bool>,
) -> &'static str {
    if !stderr_is_terminal || matches!(output_processing, Some(true)) {
        "\n"
    } else {
        "\r\n"
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "morae",
    version,
    about = "Disposable microVM sandbox for coding agents"
)]
struct Cli {
    #[command(flatten)]
    global: GlobalOptions,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Args, Default)]
struct GlobalOptions {
    /// Cache root. CLI value overrides `MORAE_CACHE_DIR`, then ~/.moraebox/cache is used.
    #[arg(long, global = true, env = "MORAE_CACHE_DIR")]
    cache_dir: Option<PathBuf>,
    /// State root. CLI value overrides `MORAE_STATE_DIR`, then ~/.moraebox/state is used.
    #[arg(long, global = true, env = "MORAE_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Emit structured JSON where the selected command supports it.
    #[arg(long, global = true)]
    json: bool,
    /// Override the automatically discovered signed VMM helper.
    #[arg(long, global = true, env = "MORAE_HELPER_PATH")]
    helper: Option<PathBuf>,
    /// Override the automatically discovered libkrun library.
    #[arg(long, global = true, env = "MORAE_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    /// Override the automatically discovered gvproxy network helper.
    #[arg(long, global = true, env = "MORAE_GVPROXY_PATH")]
    gvproxy: Option<PathBuf>,
    /// Override the automatically discovered libkrun dependency directories.
    #[arg(long, global = true, env = "MORAE_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    /// Path to mke2fs; auto-detected when omitted.
    #[arg(long, global = true, env = "MORAE_MKE2FS")]
    mke2fs: Option<PathBuf>,
    /// Path to e2fsck; auto-detected when omitted.
    #[arg(long, global = true, env = "MORAE_E2FSCK")]
    e2fsck: Option<PathBuf>,
    /// Path to debugfs; auto-detected when omitted.
    #[arg(long, global = true, env = "MORAE_DEBUGFS")]
    debugfs: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect native libkrun prerequisites without changing the host.
    Doctor(DoctorArgs),
    /// Run one command and destroy its sandbox when it exits.
    Run(Box<RunArgs>),
    /// Inspect and validate explicit morae.toml execution profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Pull and prepare OCI images without a container daemon.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Inspect or clean local image and workspace caches.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Create and manage persistent Box root filesystems.
    Box {
        #[command(subcommand)]
        command: BoxCommand,
    },
    /// Measure cached one-shot sandbox latency.
    Benchmark(Box<BenchmarkArgs>),
    /// Generate shell completion code on stdout.
    Completion(CompletionArgs),
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// List profile names after validating the entire file.
    #[command(visible_alias = "ls")]
    List(ProfileFileArgs),
    /// Parse and semantically validate the entire profile file.
    Validate(ProfileFileArgs),
}

#[derive(Debug, Args)]
struct ProfileFileArgs {
    /// Exact profile file path. Defaults to ./morae.toml without parent discovery.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ImageCommand {
    /// Pull, verify, and materialize one OCI image.
    Pull(ImagePullArgs),
    /// List locally cached images.
    #[command(visible_alias = "ls")]
    List(ImageListArgs),
    /// Remove a cached image reference or manifest digest.
    #[command(visible_alias = "rm")]
    Remove(ImageRemoveArgs),
    /// Show, set, or reset the default image reference.
    Default(ImageDefaultArgs),
}

#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// Show cache entry counts and disk usage.
    Info(CacheInfoArgs),
    /// Check rootfs size metadata and optionally repair missing, stale, or orphan indexes.
    #[command(visible_alias = "repair")]
    Reconcile(CacheReconcileArgs),
    /// Remove OCI blobs and incomplete entries not referenced by a ready rootfs.
    Prune(CachePruneArgs),
    /// Remove all managed image, rootfs, OCI, and workspace cache entries.
    Clean(CacheCleanArgs),
}

#[derive(Debug, Subcommand)]
enum BoxCommand {
    /// Create a persistent Box from an OCI image.
    Create(BoxCreateArgs),
    /// List persistent Boxes.
    #[command(visible_alias = "ls")]
    List(BoxListArgs),
    /// Show one Box.
    Show(BoxShowArgs),
    /// Delete one idle Box.
    #[command(visible_alias = "rm")]
    Delete(BoxDeleteArgs),
    /// Reset one idle Box to its cached immutable base disk.
    Reset(BoxResetArgs),
    /// Clone one idle Box into a new independent Box.
    Clone(BoxCloneArgs),
    /// Assign a unique display name to one idle Box.
    Rename(BoxRenameArgs),
    /// Add or remove labels and tags on one idle Box.
    Update(BoxUpdateArgs),
    /// Export an idle Box as a verified sparse tar bundle.
    #[command(visible_alias = "backup")]
    Export(BoxExportArgs),
    /// Verify a Box bundle and restore it under a new `BoxId`.
    Import(BoxImportArgs),
    /// Preview or quarantine corrupt Box entries without deleting their data.
    #[command(visible_alias = "quarantine")]
    Repair(BoxRepairArgs),
    /// Create, inspect, delete, or fork immutable Box disk checkpoints.
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
}

#[derive(Debug, Subcommand)]
enum CheckpointCommand {
    /// Capture one idle Ready Box disk as an immutable checkpoint.
    Create(CheckpointCreateArgs),
    /// List immutable checkpoints.
    #[command(visible_alias = "ls")]
    List,
    /// Show one checkpoint.
    Show(CheckpointShowArgs),
    /// Permanently delete one idle checkpoint.
    #[command(visible_alias = "rm")]
    Delete(CheckpointDeleteArgs),
    /// Create an independent writable Box from one checkpoint.
    Fork(CheckpointForkArgs),
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Return a failure exit status unless the native backend is ready.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
struct RunArgs {
    /// Load NAME from an explicitly selected morae.toml execution profile.
    #[arg(long)]
    profile: Option<String>,
    /// Profile file path. Requires --profile; defaults to ./morae.toml.
    #[arg(long, requires = "profile")]
    config: Option<PathBuf>,
    /// Execution backend. `process` is deterministic but is not isolated.
    #[arg(long, value_parser = ["process", "libkrun"])]
    backend: Option<String>,
    /// Use an already materialized guest root directory instead of a managed image.
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    /// Registry, oci-layout:PATH, or docker-archive:PATH#REPO:TAG image reference.
    #[arg(long)]
    image: Option<String>,
    /// Image acquisition policy: cache-first, forced refresh, or cache-only.
    #[arg(long = "pull")]
    pull_policy: Option<ImagePullPolicy>,
    /// Reuse the persistent root filesystem identified by this `BoxId` or name.
    #[arg(long = "box", conflicts_with_all = ["rootfs", "image", "workspace"])]
    box_id: Option<String>,
    #[arg(long)]
    cpus: Option<u8>,
    #[arg(long)]
    memory_mib: Option<u32>,
    /// Host directory to copy into an immutable read-only ext4 guest workspace.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Give /workspace a disposable writable overlay while preserving the snapshot lower.
    #[arg(long, conflicts_with = "workspace_read_only")]
    workspace_writable: bool,
    /// Override a writable profile workspace with read-only attachment.
    #[arg(long, conflicts_with = "workspace_writable")]
    workspace_read_only: bool,
    /// Atomically copy the final /workspace tree to this new host path.
    #[arg(long)]
    workspace_copy_out: Option<PathBuf>,
    /// Write an add/modify/delete JSON manifest to this new host path.
    #[arg(long)]
    workspace_diff: Option<PathBuf>,
    /// Copy HOST source to an absolute GUEST destination, formatted HOST=GUEST.
    #[arg(long = "copy-in", value_parser = parse_copy_in)]
    copy_in: Vec<CopyInSpec>,
    /// Copy absolute GUEST source to a new HOST destination, formatted GUEST=HOST.
    #[arg(long = "copy-out", value_parser = parse_copy_out)]
    copy_out: Vec<CopyOutSpec>,
    /// Maximum encoded bytes for each copy operation.
    #[arg(long, default_value = "64MiB", value_parser = parse_output_limit)]
    copy_limit: usize,
    #[arg(long, env = "MORAE_REGISTRY_USERNAME", requires = "registry_password")]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "MORAE_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
    /// Virtual root disk size for an ephemeral image-backed run.
    #[arg(long, value_parser = parse_disk_size)]
    disk_size: Option<u64>,
    /// Sandbox wall timeout, for example 30s, 1h, or none.
    #[arg(long)]
    timeout: Option<String>,
    /// Maximum retained output, for example 8MiB or 128MB.
    #[arg(long, value_parser = parse_output_limit)]
    output_limit: Option<usize>,
    /// Grace period between graceful termination and forced cleanup.
    #[arg(long, value_parser = parse_kill_grace)]
    kill_grace: Option<Duration>,
    /// Allocate a pseudo-terminal (native backend).
    #[arg(short = 't', long, conflicts_with = "no_tty")]
    tty: bool,
    /// Override a TTY-enabled profile without allocating a pseudo-terminal.
    #[arg(long, conflicts_with = "tty")]
    no_tty: bool,
    /// Forward stdin even when the host stdin is a terminal.
    #[arg(short = 'i', long)]
    interactive: bool,
    /// Preserve the host environment. The secure default is an empty environment.
    #[arg(long)]
    inherit_env: bool,
    /// Allow outbound network access from the native VM.
    #[arg(long, conflicts_with_all = ["no_network", "allow_cidrs", "allow_domains"])]
    network: bool,
    /// Disable profile networking and published preview ports.
    #[arg(long, conflicts_with_all = ["network", "allow_cidrs", "allow_domains", "publish"])]
    no_network: bool,
    /// Allow outbound traffic to a CIDR (repeatable, native VM only).
    #[arg(long = "allow-cidr")]
    allow_cidrs: Vec<String>,
    /// Allow HTTPS and DNS traffic for a domain pattern such as example.com or *.example.com.
    #[arg(long = "allow-domain")]
    allow_domains: Vec<String>,
    /// Publish loopback TCP `HOST_PORT:GUEST_PORT` for a local preview.
    #[arg(long, value_parser = parse_publish)]
    publish: Vec<PublishRequest>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Add one guest environment value as KEY=VALUE.
    #[arg(short = 'e', long = "env", value_parser = parse_env)]
    env: Vec<(String, String)>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
struct ResolvedRunArgs {
    backend: String,
    rootfs: Option<PathBuf>,
    image: Option<String>,
    pull_policy: ImagePullPolicy,
    box_id: Option<String>,
    cpus: u8,
    memory_mib: u32,
    workspace: Option<PathBuf>,
    workspace_writable: bool,
    workspace_copy_out: Option<PathBuf>,
    workspace_diff: Option<PathBuf>,
    copy_in: Vec<CopyInSpec>,
    copy_out: Vec<CopyOutSpec>,
    copy_limit: usize,
    registry_username: Option<String>,
    registry_password: Option<String>,
    disk_size: u64,
    timeout: String,
    output_limit: usize,
    kill_grace: Duration,
    tty: bool,
    interactive: bool,
    inherit_env: bool,
    network: bool,
    allow_cidrs: Vec<String>,
    allow_domains: Vec<String>,
    publish: Vec<PublishRequest>,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct BoxCreateArgs {
    /// Registry, oci-layout:PATH, or docker-archive:PATH#REPO:TAG image reference.
    #[arg(long)]
    image: Option<String>,
    /// Image acquisition policy: cache-first, forced refresh, or cache-only.
    #[arg(long = "pull", default_value_t = ImagePullPolicy::Missing)]
    pull_policy: ImagePullPolicy,
    #[arg(long, default_value = "8GiB", value_parser = parse_disk_size)]
    disk_size: u64,
    #[arg(long, env = "MORAE_REGISTRY_USERNAME", requires = "registry_password")]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "MORAE_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
    /// Unique display name for the Box.
    #[arg(long, value_parser = parse_box_name)]
    name: Option<String>,
    /// Add a key/value label as KEY=VALUE.
    #[arg(long = "label", value_parser = parse_box_label)]
    labels: Vec<(String, String)>,
    /// Add a tag.
    #[arg(long = "tag", value_parser = parse_box_tag)]
    tags: Vec<String>,
}

#[derive(Debug, Args)]
struct BoxListArgs {
    /// Match a unique Box name exactly, case-insensitively.
    #[arg(long, value_parser = parse_box_name)]
    name: Option<String>,
    /// Require a label key, optionally with an exact value: KEY or KEY=VALUE.
    #[arg(long = "label", value_parser = parse_box_label_filter)]
    labels: Vec<(String, Option<String>)>,
    /// Require a tag.
    #[arg(long = "tag", value_parser = parse_box_tag)]
    tags: Vec<String>,
    /// Match one lifecycle state.
    #[arg(long, value_enum)]
    state: Option<BoxStateArg>,
    /// Stable Box list ordering.
    #[arg(long, value_enum, default_value_t = BoxSortArg::Id)]
    sort: BoxSortArg,
    /// Reverse the selected stable order.
    #[arg(long)]
    reverse: bool,
}

#[derive(Debug, Args)]
struct BoxShowArgs {
    box_id: BoxId,
}

#[derive(Debug, Args)]
struct BoxDeleteArgs {
    box_id: BoxId,
    /// Confirm permanent deletion of the Box disk.
    #[arg(long, required = true)]
    yes: bool,
}

#[derive(Debug, Args)]
struct BoxResetArgs {
    box_id: BoxId,
    /// Confirm replacement of every change in the Box disk.
    #[arg(long, required = true)]
    yes: bool,
}

#[derive(Debug, Args)]
struct BoxCloneArgs {
    box_id: BoxId,
    /// Confirm creation of a new durable Box disk.
    #[arg(long, required = true)]
    yes: bool,
}

#[derive(Debug, Args)]
struct CheckpointCreateArgs {
    box_id: BoxId,
    /// Optional checkpoint display name.
    #[arg(long, value_parser = parse_box_name)]
    name: Option<String>,
    /// Add a key/value label as KEY=VALUE.
    #[arg(long = "label", value_parser = parse_box_label)]
    labels: Vec<(String, String)>,
    /// Add a tag.
    #[arg(long = "tag", value_parser = parse_box_tag)]
    tags: Vec<String>,
}

#[derive(Debug, Args)]
struct CheckpointShowArgs {
    checkpoint_id: CheckpointId,
}

#[derive(Debug, Args)]
struct CheckpointDeleteArgs {
    checkpoint_id: CheckpointId,
    /// Confirm permanent deletion of the checkpoint disk.
    #[arg(long, required = true)]
    yes: bool,
}

#[derive(Debug, Args)]
struct CheckpointForkArgs {
    checkpoint_id: CheckpointId,
    /// Confirm creation of a new durable Box disk.
    #[arg(long, required = true)]
    yes: bool,
    /// Optional unique name for the new Box.
    #[arg(long, value_parser = parse_box_name)]
    name: Option<String>,
    /// Replace inherited checkpoint labels with KEY=VALUE values.
    #[arg(long = "label", value_parser = parse_box_label)]
    labels: Vec<(String, String)>,
    /// Replace inherited checkpoint tags.
    #[arg(long = "tag", value_parser = parse_box_tag)]
    tags: Vec<String>,
}

#[derive(Debug, Args)]
struct BoxRenameArgs {
    box_id: BoxId,
    #[arg(value_parser = parse_box_name)]
    name: String,
}

#[derive(Debug, Args)]
struct BoxUpdateArgs {
    box_id: BoxId,
    /// Set a key/value label as KEY=VALUE.
    #[arg(long = "label", value_parser = parse_box_label)]
    set_labels: Vec<(String, String)>,
    /// Remove a label by key.
    #[arg(long = "remove-label", value_parser = parse_box_label_key)]
    remove_labels: Vec<String>,
    /// Add a tag.
    #[arg(long = "tag", value_parser = parse_box_tag)]
    add_tags: Vec<String>,
    /// Remove a tag.
    #[arg(long = "remove-tag", value_parser = parse_box_tag)]
    remove_tags: Vec<String>,
}

#[derive(Debug, Args)]
struct BoxExportArgs {
    box_id: BoxId,
    /// New tar bundle path; existing files are never overwritten.
    destination: PathBuf,
}

#[derive(Debug, Args)]
struct BoxImportArgs {
    /// Verified tar bundle to import under a new `BoxId`.
    source: PathBuf,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BoxStateArg {
    Ready,
    Dirty,
    NeedsRepair,
}

impl From<BoxStateArg> for BoxState {
    fn from(value: BoxStateArg) -> Self {
        match value {
            BoxStateArg::Ready => Self::Ready,
            BoxStateArg::Dirty => Self::Dirty,
            BoxStateArg::NeedsRepair => Self::NeedsRepair,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum BoxSortArg {
    Name,
    Created,
    Updated,
    LastUsed,
    PhysicalSize,
    VirtualSize,
    #[default]
    Id,
}

impl From<BoxSortArg> for BoxSortBy {
    fn from(value: BoxSortArg) -> Self {
        match value {
            BoxSortArg::Name => Self::Name,
            BoxSortArg::Created => Self::Created,
            BoxSortArg::Updated => Self::Updated,
            BoxSortArg::LastUsed => Self::LastUsed,
            BoxSortArg::PhysicalSize => Self::PhysicalSize,
            BoxSortArg::VirtualSize => Self::VirtualSize,
            BoxSortArg::Id => Self::Id,
        }
    }
}

#[derive(Debug, Args)]
struct BoxRepairArgs {
    /// Report corrupt entries without changing the store.
    #[arg(long, conflicts_with = "yes")]
    dry_run: bool,
    /// Move corrupt entries into the private quarantine directory.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct ImagePullArgs {
    reference: String,
    #[arg(long, default_value = "linux")]
    os: String,
    #[arg(long)]
    architecture: Option<String>,
    #[arg(long, env = "MORAE_REGISTRY_USERNAME", requires = "registry_password")]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "MORAE_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
}

#[derive(Debug, Args)]
struct ImageListArgs {}

#[derive(Debug, Args)]
struct ImageRemoveArgs {
    /// Registry reference or sha256 manifest digest.
    target: String,
    /// Show what would be removed without changing the cache.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ImageDefaultArgs {
    /// Registry image reference to use when run has no --rootfs or --image.
    #[arg(conflicts_with = "unset")]
    image: Option<String>,
    /// Remove the override and return to the built-in python:3.12 default.
    #[arg(long)]
    unset: bool,
}

#[derive(Debug, Args)]
struct CacheInfoArgs {}

#[derive(Debug, Args)]
struct CacheReconcileArgs {
    /// Check and report changes without updating metadata.
    #[arg(long, conflicts_with = "yes")]
    dry_run: bool,
    /// Repair rootfs metadata and remove orphan metadata without prompting.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct CachePruneArgs {
    /// Show what would be removed without changing the cache.
    #[arg(long, conflicts_with = "yes")]
    dry_run: bool,
    /// Apply the destructive cache operation without prompting.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
struct CacheCleanArgs {
    /// Confirm that every managed cache category is in scope.
    #[arg(long, required = true)]
    all: bool,
    /// Show what would be removed without changing the cache.
    #[arg(long, conflicts_with = "yes")]
    dry_run: bool,
    /// Apply the destructive cache operation without prompting.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct BenchmarkArgs {
    /// Execution backend. Defaults to the isolated native microVM backend.
    #[arg(long, default_value = "libkrun", value_parser = ["process", "libkrun"])]
    backend: String,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    iterations: u32,
    /// Measurement population: mixed includes the first cold run, cold disables prepared roots,
    /// and warm performs one unmeasured warm-up.
    #[arg(long, value_enum, default_value_t = BenchmarkModeArg::Mixed)]
    mode: BenchmarkModeArg,
    /// Maximum measured runs in flight.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=256))]
    concurrency: u16,
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    #[arg(long, conflicts_with = "rootfs")]
    image: Option<String>,
    /// Image acquisition policy: cache-first, forced refresh, or cache-only.
    #[arg(long = "pull", default_value_t = ImagePullPolicy::Missing)]
    pull_policy: ImagePullPolicy,
    /// Reuse the persistent root filesystem identified by this `BoxId` or name.
    #[arg(long = "box", conflicts_with_all = ["rootfs", "image"])]
    box_id: Option<String>,
    #[arg(long, default_value = "8GiB", value_parser = parse_disk_size)]
    disk_size: u64,
    #[arg(long, env = "MORAE_REGISTRY_USERNAME", requires = "registry_password")]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "MORAE_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    /// Maximum retained output per measured run.
    #[arg(long, default_value = "64MiB", value_parser = parse_output_limit)]
    output_limit: usize,
    /// Grace period between graceful termination and forced cleanup.
    #[arg(long, default_value = "5s", value_parser = parse_kill_grace)]
    kill_grace: Duration,
    /// Command to measure; emit output immediately to populate `command_start_p95_micros`.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum BenchmarkModeArg {
    #[default]
    Mixed,
    Cold,
    Warm,
}

#[derive(Debug, Args)]
struct CompletionArgs {
    /// Shell whose completion code should be generated.
    shell: Shell,
}

#[derive(Debug, Serialize)]
struct CliErrorDocument {
    error: CliErrorEnvelope,
}

#[derive(Debug, Serialize)]
struct CliErrorEnvelope {
    code: &'static str,
    stage: String,
    retryable: bool,
    message: String,
    remediation: &'static str,
}

impl CliErrorEnvelope {
    fn from_error(error: &CliError) -> Self {
        Self {
            code: error.code,
            stage: error.stage.clone(),
            retryable: error.retryable,
            message: error.to_string(),
            remediation: error.remediation,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse_from(normalize_help_alias(std::env::args_os()));
    let json = cli.global.json;
    match execute(cli).await {
        Ok(code) => exit_code(code),
        Err(error) => {
            if json {
                let document = CliErrorDocument {
                    error: CliErrorEnvelope::from_error(&error),
                };
                println!(
                    "{}",
                    serde_json::to_string(&document)
                        .expect("CLI error envelope serialization must succeed")
                );
            } else {
                eprintln!("morae: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn command_stage(command: &Command) -> &'static str {
    match command {
        Command::Doctor(_) => "doctor",
        Command::Run(_) => "run",
        Command::Profile {
            command: ProfileCommand::List(_),
        } => "profile_list",
        Command::Profile {
            command: ProfileCommand::Validate(_),
        } => "profile_validate",
        Command::Image {
            command: ImageCommand::Pull(_),
        } => "image_pull",
        Command::Image {
            command: ImageCommand::List(_),
        } => "image_list",
        Command::Image {
            command: ImageCommand::Remove(_),
        } => "image_remove",
        Command::Image {
            command: ImageCommand::Default(_),
        } => "image_default",
        Command::Cache {
            command: CacheCommand::Info(_),
        } => "cache_info",
        Command::Cache {
            command: CacheCommand::Reconcile(_),
        } => "cache_reconcile",
        Command::Cache {
            command: CacheCommand::Prune(_),
        } => "cache_prune",
        Command::Cache {
            command: CacheCommand::Clean(_),
        } => "cache_clean",
        Command::Box {
            command: BoxCommand::Create(_),
        } => "box_create",
        Command::Box {
            command: BoxCommand::List(_),
        } => "box_list",
        Command::Box {
            command: BoxCommand::Show(_),
        } => "box_show",
        Command::Box {
            command: BoxCommand::Delete(_),
        } => "box_delete",
        Command::Box {
            command: BoxCommand::Reset(_),
        } => "box_reset",
        Command::Box {
            command: BoxCommand::Clone(_),
        } => "box_clone",
        Command::Box {
            command: BoxCommand::Rename(_),
        } => "box_rename",
        Command::Box {
            command: BoxCommand::Update(_),
        } => "box_update",
        Command::Box {
            command: BoxCommand::Export(_),
        } => "box_export",
        Command::Box {
            command: BoxCommand::Import(_),
        } => "box_import",
        Command::Box {
            command: BoxCommand::Repair(_),
        } => "box_repair",
        Command::Box {
            command: BoxCommand::Checkpoint { command },
        } => match command {
            CheckpointCommand::Create(_) => "checkpoint_create",
            CheckpointCommand::List => "checkpoint_list",
            CheckpointCommand::Show(_) => "checkpoint_show",
            CheckpointCommand::Delete(_) => "checkpoint_delete",
            CheckpointCommand::Fork(_) => "checkpoint_fork",
        },
        Command::Benchmark(_) => "benchmark",
        Command::Completion(_) => "completion",
    }
}

fn normalize_help_alias<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    if args.len() == 3 && args[1] == "run" && args[2] == "help" {
        args[2] = "--help".into();
    }
    args
}
