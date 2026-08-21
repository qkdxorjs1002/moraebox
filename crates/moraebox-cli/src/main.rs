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

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use moraebox_box::{
    BaseDiskSpec, BaseDiskStore, BoxMetadata, BoxRepairReport, BoxStore, CreateBox,
};
use moraebox_core::{
    BoxId, ImagePullPolicy, MAX_KILL_GRACE, MAX_OUTPUT_LIMIT, OutputChannel, OutputReadError,
    RunSpec, SessionState, Signal, StoragePaths, TimeoutPolicy, resolve_cache_dir,
    resolve_state_dir,
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

use commands::{
    execute, exit_code, parse_disk_size, parse_env, parse_kill_grace, parse_output_limit,
};
use errors::{CliError, CliErrorSource};
use interactive::run_interactive;

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
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect native libkrun prerequisites without changing the host.
    Doctor(DoctorArgs),
    /// Run one command and destroy its sandbox when it exits.
    Run(Box<RunArgs>),
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
    /// Preview or quarantine corrupt Box entries without deleting their data.
    #[command(visible_alias = "quarantine")]
    Repair(BoxRepairArgs),
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
    /// Execution backend. `process` is deterministic but is not isolated.
    #[arg(long, default_value = "libkrun", value_parser = ["process", "libkrun"])]
    backend: String,
    /// Use an already materialized guest root directory instead of a managed image.
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    /// OCI registry reference; uses the configured python:3.12 default when omitted.
    #[arg(long)]
    image: Option<String>,
    /// Image acquisition policy: cache-first, forced refresh, or cache-only.
    #[arg(long = "pull", default_value_t = ImagePullPolicy::Missing)]
    pull_policy: ImagePullPolicy,
    /// Reuse the persistent root filesystem identified by this `BoxId`.
    #[arg(long = "box", conflicts_with_all = ["rootfs", "image", "workspace"])]
    box_id: Option<BoxId>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    /// Host directory to copy into an immutable read-only ext4 guest workspace.
    #[arg(long)]
    workspace: Option<PathBuf>,
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
    #[arg(long, default_value = "8GiB", value_parser = parse_disk_size)]
    disk_size: u64,
    /// Sandbox wall timeout, for example 30s, 1h, or none.
    #[arg(long, default_value = "1h")]
    timeout: String,
    /// Maximum retained output, for example 8MiB or 128MB.
    #[arg(long, default_value = "64MiB", value_parser = parse_output_limit)]
    output_limit: usize,
    /// Grace period between graceful termination and forced cleanup.
    #[arg(long, default_value = "5s", value_parser = parse_kill_grace)]
    kill_grace: Duration,
    /// Allocate a pseudo-terminal (native backend).
    #[arg(short = 't', long)]
    tty: bool,
    /// Forward stdin even when the host stdin is a terminal.
    #[arg(short = 'i', long)]
    interactive: bool,
    /// Preserve the host environment. The secure default is an empty environment.
    #[arg(long)]
    inherit_env: bool,
    /// Allow outbound network access from the native VM.
    #[arg(long)]
    network: bool,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Add one guest environment value as KEY=VALUE.
    #[arg(short = 'e', long = "env", value_parser = parse_env)]
    env: Vec<(String, String)>,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct BoxCreateArgs {
    /// OCI registry reference; uses the configured default when omitted.
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
}

#[derive(Debug, Args)]
struct BoxListArgs {}

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
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    #[arg(long, conflicts_with = "rootfs")]
    image: Option<String>,
    /// Image acquisition policy: cache-first, forced refresh, or cache-only.
    #[arg(long = "pull", default_value_t = ImagePullPolicy::Missing)]
    pull_policy: ImagePullPolicy,
    #[arg(long = "box", conflicts_with_all = ["rootfs", "image"])]
    box_id: Option<BoxId>,
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
            command: BoxCommand::Repair(_),
        } => "box_repair",
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
