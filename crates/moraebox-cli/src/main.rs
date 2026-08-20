#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use clap::{Args, Parser, Subcommand};
use moraebox_box::{
    BaseDiskSpec, BaseDiskStore, BoxMetadata, BoxStore, CreateBox, EphemeralDiskStore,
};
use moraebox_core::{
    BoxId, OutputChannel, OutputReadError, RunSpec, SessionState, Signal, TimeoutPolicy,
    resolve_cache_dir, resolve_state_dir,
};
use moraebox_image::{
    CacheUsage, CachedImage, CleanReport, Credentials, ImageCache, Platform, PreparedImage,
    PruneReport, RemoveReport, WorkspaceSnapshot, digest_tree,
};
use moraebox_runtime::{
    Backend, BoxRootSource, BoxRuntimeConfig, DoctorReport, LibkrunBackend, LibkrunConfig,
    NativeRuntimePaths, ProcessBackend, SessionError, SessionHandle, SessionManager, SessionStatus,
    Supervisor,
};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Parser)]
#[command(
    name = "morae",
    version,
    about = "Disposable microVM sandbox for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
}

#[derive(Debug, Args)]
struct DoctorArgs {
    #[arg(long)]
    json: bool,
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
    /// Override the automatically discovered signed VMM helper.
    #[arg(long, env = "MORAE_HELPER_PATH")]
    helper: Option<PathBuf>,
    /// Override the automatically discovered libkrun library.
    #[arg(long, env = "MORAE_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    /// Override the automatically discovered gvproxy network helper.
    #[arg(long, env = "MORAE_GVPROXY_PATH")]
    gvproxy: Option<PathBuf>,
    /// Use an already materialized guest root directory instead of a managed image.
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    /// OCI registry reference; uses the configured python:3.12 default when omitted.
    #[arg(long)]
    image: Option<String>,
    /// Reuse the persistent root filesystem identified by this `BoxId`.
    #[arg(long = "box", conflicts_with_all = ["rootfs", "image", "workspace"])]
    box_id: Option<BoxId>,
    /// Override the automatically discovered libkrun dependency directories.
    #[arg(long, env = "MORAE_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    /// Host directory to copy into an immutable read-only ext4 guest workspace.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, env = "MORAE_REGISTRY_USERNAME", requires = "registry_password")]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "MORAE_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
    /// Path to mke2fs; auto-detected when omitted.
    #[arg(long, env = "MORAE_MKE2FS")]
    mke2fs: Option<PathBuf>,
    /// Path to e2fsck; auto-detected when omitted.
    #[arg(long, env = "MORAE_E2FSCK")]
    e2fsck: Option<PathBuf>,
    /// Virtual root disk size for an ephemeral image-backed run.
    #[arg(long, default_value = "8GiB", value_parser = parse_disk_size)]
    disk_size: u64,
    /// Sandbox wall timeout, for example 30s, 1h, or none.
    #[arg(long, default_value = "1h")]
    timeout: String,
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
    /// Emit the structured final report rather than replaying raw output.
    #[arg(long)]
    json: bool,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct BoxCreateArgs {
    /// OCI registry reference; uses the configured default when omitted.
    #[arg(long)]
    image: Option<String>,
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, default_value = "8GiB", value_parser = parse_disk_size)]
    disk_size: u64,
    #[arg(long, env = "MORAE_MKE2FS")]
    mke2fs: Option<PathBuf>,
    #[arg(long, env = "MORAE_REGISTRY_USERNAME", requires = "registry_password")]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "MORAE_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BoxListArgs {
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BoxShowArgs {
    box_id: BoxId,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BoxDeleteArgs {
    box_id: BoxId,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Confirm permanent deletion of the Box disk.
    #[arg(long, required = true)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BoxResetArgs {
    box_id: BoxId,
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Confirm replacement of every change in the Box disk.
    #[arg(long, required = true)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BoxCloneArgs {
    box_id: BoxId,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Confirm creation of a new durable Box disk.
    #[arg(long, required = true)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImagePullArgs {
    reference: String,
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
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
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImageListArgs {
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImageRemoveArgs {
    /// Registry reference or sha256 manifest digest.
    target: String,
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Show what would be removed without changing the cache.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImageDefaultArgs {
    /// Registry image reference to use when run has no --rootfs or --image.
    #[arg(conflicts_with = "unset")]
    image: Option<String>,
    /// Remove the override and return to the built-in python:3.12 default.
    #[arg(long)]
    unset: bool,
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CacheInfoArgs {
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CachePruneArgs {
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Show what would be removed without changing the cache.
    #[arg(long, conflicts_with = "yes")]
    dry_run: bool,
    /// Apply the destructive cache operation without prompting.
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
struct CacheCleanArgs {
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Confirm that every managed cache category is in scope.
    #[arg(long, required = true)]
    all: bool,
    /// Show what would be removed without changing the cache.
    #[arg(long, conflicts_with = "yes")]
    dry_run: bool,
    /// Apply the destructive cache operation without prompting.
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BenchmarkArgs {
    /// Execution backend. Defaults to the isolated native microVM backend.
    #[arg(long, default_value = "libkrun", value_parser = ["process", "libkrun"])]
    backend: String,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    iterations: u32,
    #[arg(long, env = "MORAE_HELPER_PATH")]
    helper: Option<PathBuf>,
    #[arg(long, env = "MORAE_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    #[arg(long, conflicts_with = "rootfs")]
    image: Option<String>,
    #[arg(long = "box", conflicts_with_all = ["rootfs", "image"])]
    box_id: Option<BoxId>,
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, env = "MORAE_MKE2FS")]
    mke2fs: Option<PathBuf>,
    #[arg(long, env = "MORAE_E2FSCK")]
    e2fsck: Option<PathBuf>,
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
    #[arg(long, env = "MORAE_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    /// Command to measure; emit output immediately to populate `command_start_p95_micros`.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse_from(normalize_help_alias(std::env::args_os()));
    match execute(cli).await {
        Ok(code) => exit_code(code),
        Err(error) => {
            eprintln!("morae: {error}");
            ExitCode::FAILURE
        }
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

async fn execute(cli: Cli) -> Result<i32, Box<dyn std::error::Error>> {
    match cli.command {
        Command::Doctor(args) => doctor(&args),
        Command::Run(args) => run(*args).await,
        Command::Image {
            command: ImageCommand::Pull(args),
        } => image_pull(args).await,
        Command::Image {
            command: ImageCommand::List(args),
        } => image_list(&args),
        Command::Image {
            command: ImageCommand::Remove(args),
        } => image_remove(&args),
        Command::Image {
            command: ImageCommand::Default(args),
        } => image_default(&args),
        Command::Cache {
            command: CacheCommand::Info(args),
        } => cache_info(&args),
        Command::Cache {
            command: CacheCommand::Prune(args),
        } => cache_prune(&args),
        Command::Cache {
            command: CacheCommand::Clean(args),
        } => cache_clean(&args),
        Command::Box {
            command: BoxCommand::Create(args),
        } => box_create(args).await,
        Command::Box {
            command: BoxCommand::List(args),
        } => box_list(&args),
        Command::Box {
            command: BoxCommand::Show(args),
        } => box_show(&args),
        Command::Box {
            command: BoxCommand::Delete(args),
        } => box_delete(&args),
        Command::Box {
            command: BoxCommand::Reset(args),
        } => box_reset(&args),
        Command::Box {
            command: BoxCommand::Clone(args),
        } => box_clone(&args),
        Command::Benchmark(args) => benchmark(*args).await,
    }
}

fn doctor(args: &DoctorArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let report = DoctorReport::collect();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("host: {}/{}", report.os, report.architecture);
        println!(
            "libkrun: {}",
            report
                .libkrun
                .path
                .as_ref()
                .map_or_else(|| "missing".into(), |path| path.display().to_string())
        );
        println!(
            "libkrunfw: {}",
            report
                .libkrunfw
                .path
                .as_ref()
                .map_or_else(|| "missing".into(), |path| path.display().to_string())
        );
        println!(
            "hypervisor entitlement: {}",
            if report.hypervisor_entitlement {
                "present"
            } else {
                "missing"
            }
        );
        println!(
            "vmm helper: {}",
            report
                .helper_path
                .as_ref()
                .map_or_else(|| "missing".into(), |path| path.display().to_string())
        );
        println!("native backend ready: {}", report.native_backend_ready);
        println!(
            "gvproxy: {}",
            report
                .gvproxy
                .path
                .as_ref()
                .map_or_else(|| "missing".into(), |path| path.display().to_string())
        );
        println!("native network ready: {}", report.native_network_ready);
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
    }
    Ok(i32::from(args.strict && !report.native_backend_ready))
}

#[allow(clippy::too_many_lines)]
async fn run(args: RunArgs) -> Result<i32, Box<dyn std::error::Error>> {
    if args.interactive && args.json {
        return Err("--interactive cannot be combined with --json".into());
    }
    let mut spec = RunSpec::command(args.command);
    spec.box_id = args.box_id;
    spec.timeout = parse_timeout(&args.timeout)?;
    spec.tty = args.tty;
    spec.inherit_env = args.inherit_env;
    spec.network = args.network;
    spec.cwd = args.cwd;
    spec.env = args.env.into_iter().collect::<BTreeMap<_, _>>();
    if spec.inherit_env {
        spec.env.extend(std::env::vars());
    }
    if !args.interactive && !io::stdin().is_terminal() {
        io::stdin().read_to_end(&mut spec.stdin)?;
    }

    if args.rootfs.is_some() && args.image.is_some() {
        return Err("--rootfs and --image are mutually exclusive".into());
    }
    if spec.box_id.is_some() && args.backend != "libkrun" {
        return Err("--box requires --backend libkrun".into());
    }
    if spec.box_id.is_some()
        && (args.rootfs.is_some() || args.image.is_some() || args.workspace.is_some())
    {
        return Err("--box cannot be combined with --rootfs, --image, or --workspace".into());
    }
    validate_network_option(&args.backend, spec.network)?;
    let cache_dir = (args.backend == "libkrun")
        .then(|| resolve_cache_dir(args.cache_dir.as_deref()))
        .transpose()?;
    let state_dir = (args.backend == "libkrun")
        .then(|| resolve_state_dir(args.state_dir.as_deref()))
        .transpose()?;
    let image_reference = if spec.box_id.is_some() {
        None
    } else {
        select_image_reference(
            &args.backend,
            args.rootfs.is_some(),
            args.image,
            cache_dir.as_deref(),
        )?
    };
    let platform = Platform::host_linux();
    let prepared_image = if let Some(reference) = image_reference.as_deref() {
        let cache_dir = cache_dir
            .as_deref()
            .ok_or("libkrun image selection requires a cache directory")?;
        Some(
            resolve_or_pull(
                reference,
                cache_dir,
                &platform,
                credentials(args.registry_username, args.registry_password),
            )
            .await?,
        )
    } else {
        None
    };
    let mke2fs = args.mke2fs.unwrap_or_else(default_mke2fs);

    let workspace = if let Some(source) = args.workspace.as_deref() {
        if args.backend != "libkrun" {
            return Err("--workspace requires --backend libkrun".into());
        }
        if spec.cwd.is_some() {
            return Err("--cwd and --workspace cannot be combined in this version".into());
        }
        let cache_dir = cache_dir
            .as_deref()
            .ok_or("--workspace requires a cache directory")?;
        Some(WorkspaceSnapshot::create(source, cache_dir, &mke2fs)?)
    } else {
        None
    };

    let report = match args.backend.as_str() {
        "process" => {
            if args.interactive {
                return run_interactive(ProcessBackend, spec).await;
            }
            Supervisor::new(ProcessBackend).run(spec).await?
        }
        "libkrun" => {
            let cache_dir = cache_dir
                .as_deref()
                .ok_or("libkrun backend requires a cache directory")?;
            let state_dir = state_dir
                .as_deref()
                .ok_or("libkrun backend requires a state directory")?;
            let root_source = if spec.box_id.is_some() {
                None
            } else if let Some(prepared) = prepared_image.as_ref() {
                Some(BoxRootSource {
                    rootfs_path: prepared.rootfs.clone(),
                    manifest_digest: prepared.manifest_digest.clone(),
                    platform: platform_name(&platform),
                    virtual_size_bytes: args.disk_size,
                    mke2fs_path: mke2fs,
                })
            } else if let Some(rootfs) = args.rootfs.as_ref() {
                Some(BoxRootSource {
                    rootfs_path: rootfs.clone(),
                    manifest_digest: digest_tree(rootfs)?.to_string(),
                    platform: platform_name(&platform),
                    virtual_size_bytes: args.disk_size,
                    mke2fs_path: mke2fs,
                })
            } else {
                return Err("libkrun run requires an image, rootfs, or BoxId".into());
            };
            let mut config = native_config(
                args.helper,
                args.libkrun,
                args.lib_dir,
                args.gvproxy,
                root_source
                    .as_ref()
                    .map(|source| source.rootfs_path.clone()),
                "--rootfs, --image, or MORAE_ROOTFS",
                true,
            )?;
            config.vcpus = args.cpus;
            config.memory_mib = args.memory_mib;
            config.workspace_disk = workspace
                .as_ref()
                .map(|snapshot| snapshot.image_path.clone());
            config.network_runtime_dir = cache_dir.join("network");
            let ephemeral_disks = EphemeralDiskStore::new(cache_dir.join("runtime"));
            let _ = ephemeral_disks.garbage_collect()?;
            let runtime = BoxRuntimeConfig {
                boxes: BoxStore::new(state_dir),
                base_disks: BaseDiskStore::new(cache_dir),
                ephemeral_disks,
                source: root_source,
                e2fsck_path: args.e2fsck.unwrap_or_else(default_e2fsck),
            };
            let backend = LibkrunBackend::new(config).with_box_runtime(runtime);
            if args.interactive {
                return run_interactive(backend, spec).await;
            }
            Supervisor::new(backend).run(spec).await?
        }
        _ => unreachable!("clap validates backend values"),
    };
    if let Some(workspace) = &workspace {
        workspace.verify_source_unchanged()?;
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let mut stdout = io::stdout().lock();
        let mut stderr = io::stderr().lock();
        for chunk in &report.output {
            match chunk.channel {
                OutputChannel::Stdout | OutputChannel::Tty => stdout.write_all(&chunk.data)?,
                OutputChannel::Stderr => stderr.write_all(&chunk.data)?,
            }
        }
        stdout.flush()?;
        stderr.flush()?;
    }
    if report.timed_out {
        Ok(124)
    } else if let Some(code) = report.exit_code {
        Ok(code)
    } else if let Some(signal) = report.signal {
        Ok(128 + signal)
    } else {
        Ok(125)
    }
}

const INTERACTIVE_READ_BYTES: usize = 64 * 1024;

async fn run_interactive<B>(backend: B, spec: RunSpec) -> Result<i32, Box<dyn std::error::Error>>
where
    B: Backend + 'static,
{
    let _terminal = RawTerminalGuard::enter(spec.tty && io::stdin().is_terminal())?;
    let mut input = InteractiveInput::new()?;
    let mut signals = HostSignals::new()?;
    let session = SessionManager::new(Arc::new(backend)).start(spec).await?;
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut cursor = 0_u64;
    let mut input_open = true;

    let input_session = session.clone();
    let input_future = forward_interactive_input(&mut input, input_session);
    tokio::pin!(input_future);
    let wait_future = session.wait();
    tokio::pin!(wait_future);

    let status = loop {
        drain_interactive_output(&session, &mut cursor, &mut stdout, &mut stderr).await?;
        let output_future = session.wait_for_output(cursor);
        tokio::pin!(output_future);

        tokio::select! {
            status = &mut wait_future => break status?,
            input_result = &mut input_future, if input_open => {
                match input_result {
                    Ok(()) => input_open = false,
                    Err(_) if session.status().state == SessionState::Dead => {
                        input_open = false;
                    }
                    Err(error) => return Err(error),
                }
            }
            output_result = &mut output_future => {
                output_result?;
            }
            signal_result = signals.recv() => {
                let signal = signal_result?;
                if let Err(error) = session.signal(signal).await
                    && session.status().state != SessionState::Dead
                {
                    return Err(error.into());
                }
            }
        }
    };

    drain_interactive_output(&session, &mut cursor, &mut stdout, &mut stderr).await?;
    stdout.flush().await?;
    stderr.flush().await?;
    Ok(session_exit_code(&status))
}

async fn forward_interactive_input(
    input: &mut InteractiveInput,
    session: SessionHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            session.close_stdin().await?;
            return Ok(());
        }
        session.write(buffer[..count].to_vec()).await?;
    }
}

async fn drain_interactive_output(
    session: &SessionHandle,
    cursor: &mut u64,
    stdout: &mut tokio::io::Stdout,
    stderr: &mut tokio::io::Stderr,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let output = match session.read_output(*cursor, INTERACTIVE_READ_BYTES).await {
            Ok(output) => output,
            Err(SessionError::Output(OutputReadError::CursorExpired { earliest, .. })) => {
                let warning =
                    format!("morae: interactive output before cursor {earliest} was dropped\n");
                stderr.write_all(warning.as_bytes()).await?;
                *cursor = earliest;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if output.chunks.is_empty() {
            return Ok(());
        }
        for chunk in output.chunks {
            match chunk.channel {
                OutputChannel::Stdout | OutputChannel::Tty => {
                    stdout.write_all(&chunk.data).await?;
                }
                OutputChannel::Stderr => stderr.write_all(&chunk.data).await?,
            }
        }
        *cursor = output.next_cursor;
        stdout.flush().await?;
        stderr.flush().await?;
    }
}

fn session_exit_code(status: &SessionStatus) -> i32 {
    if status.timed_out {
        124
    } else if let Some(code) = status.exit_code {
        code
    } else if let Some(signal) = status.signal {
        128 + signal
    } else {
        125
    }
}

#[cfg(unix)]
struct RawTerminalGuard {
    original: nix::sys::termios::Termios,
}

#[cfg(unix)]
impl RawTerminalGuard {
    fn enter(enabled: bool) -> io::Result<Option<Self>> {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};

        if !enabled {
            return Ok(None);
        }
        let stdin = io::stdin();
        let original = tcgetattr(&stdin).map_err(io::Error::from)?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).map_err(io::Error::from)?;
        Ok(Some(Self { original }))
    }
}

#[cfg(unix)]
impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{SetArg, tcsetattr};

        let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, &self.original);
    }
}

#[cfg(not(unix))]
struct RawTerminalGuard;

#[cfg(not(unix))]
impl RawTerminalGuard {
    fn enter(_enabled: bool) -> io::Result<Option<Self>> {
        Ok(None)
    }
}

#[cfg(unix)]
struct InteractiveInput {
    inner: tokio::io::unix::AsyncFd<io::Stdin>,
    original_flags: nix::fcntl::OFlag,
}

#[cfg(unix)]
impl InteractiveInput {
    fn new() -> io::Result<Self> {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};

        let inner = tokio::io::unix::AsyncFd::new(io::stdin())?;
        let original_flags = OFlag::from_bits_truncate(
            fcntl(inner.get_ref(), FcntlArg::F_GETFL).map_err(io::Error::from)?,
        );
        fcntl(
            inner.get_ref(),
            FcntlArg::F_SETFL(original_flags | OFlag::O_NONBLOCK),
        )
        .map_err(io::Error::from)?;
        Ok(Self {
            inner,
            original_flags,
        })
    }

    async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.inner.readable_mut().await?;
            if let Ok(result) = guard.try_io(|inner| Read::read(inner.get_mut(), buffer)) {
                return result;
            }
        }
    }
}

#[cfg(unix)]
impl Drop for InteractiveInput {
    fn drop(&mut self) {
        use nix::fcntl::{FcntlArg, fcntl};

        let _ = fcntl(self.inner.get_ref(), FcntlArg::F_SETFL(self.original_flags));
    }
}

#[cfg(not(unix))]
struct InteractiveInput {
    receiver: tokio::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
}

#[cfg(not(unix))]
impl InteractiveInput {
    fn new() -> io::Result<Self> {
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        std::thread::Builder::new()
            .name("morae-stdin".into())
            .spawn(move || {
                let mut stdin = io::stdin();
                loop {
                    let mut buffer = vec![0_u8; 16 * 1024];
                    let result = Read::read(&mut stdin, &mut buffer).map(|count| {
                        buffer.truncate(count);
                        buffer
                    });
                    let reached_eof = matches!(&result, Ok(bytes) if bytes.is_empty());
                    if sender.blocking_send(result).is_err() || reached_eof {
                        return;
                    }
                }
            })?;
        Ok(Self { receiver })
    }

    async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes = self.receiver.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "host stdin reader stopped")
        })??;
        if bytes.len() > buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "host stdin chunk exceeds the interactive input buffer",
            ));
        }
        let count = bytes.len();
        buffer[..count].copy_from_slice(&bytes);
        Ok(count)
    }
}

#[cfg(unix)]
struct HostSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl HostSignals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn recv(&mut self) -> io::Result<Signal> {
        tokio::select! {
            value = self.interrupt.recv() => signal_event(value, Signal::Interrupt),
            value = self.terminate.recv() => signal_event(value, Signal::Terminate),
        }
    }
}

#[cfg(unix)]
fn signal_event(received: Option<()>, signal: Signal) -> io::Result<Signal> {
    received
        .map(|()| signal)
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "host signal stream closed"))
}

#[cfg(windows)]
struct HostSignals {
    interrupt: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl HostSignals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c()?,
        })
    }

    async fn recv(&mut self) -> io::Result<Signal> {
        self.interrupt
            .recv()
            .await
            .map(|()| Signal::Interrupt)
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "host signal stream closed"))
    }
}

#[cfg(not(any(unix, windows)))]
struct HostSignals;

#[cfg(not(any(unix, windows)))]
impl HostSignals {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> io::Result<Signal> {
        tokio::signal::ctrl_c().await?;
        Ok(Signal::Interrupt)
    }
}

fn select_image_reference(
    backend: &str,
    has_rootfs: bool,
    explicit_image: Option<String>,
    cache_dir: Option<&std::path::Path>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if backend != "libkrun" {
        if has_rootfs {
            return Err("--rootfs requires --backend libkrun".into());
        }
        if explicit_image.is_some() {
            return Err("--image requires --backend libkrun".into());
        }
        return Ok(None);
    }
    if has_rootfs {
        return Ok(None);
    }
    let cache_dir = cache_dir.ok_or("libkrun image selection requires a cache directory")?;
    explicit_image
        .map_or_else(
            || ImageCache::new(cache_dir).default_reference().map(Some),
            |reference| Ok(Some(reference)),
        )
        .map_err(Into::into)
}

async fn resolve_or_pull(
    reference: &str,
    cache_dir: &std::path::Path,
    platform: &Platform,
    credentials: Option<Credentials>,
) -> Result<PreparedImage, Box<dyn std::error::Error>> {
    ImageCache::new(cache_dir)
        .resolve_or_pull(reference, platform, credentials)
        .await
        .map_err(Into::into)
}

async fn box_create(args: BoxCreateArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let cache = ImageCache::new(&cache_dir);
    let reference = match args.image {
        Some(reference) => reference,
        None => cache.default_reference()?,
    };
    let platform = Platform::host_linux();
    let prepared = cache
        .resolve_or_pull(
            &reference,
            &platform,
            credentials(args.registry_username, args.registry_password),
        )
        .await?;
    let spec = BaseDiskSpec::new(
        prepared.manifest_digest.clone(),
        platform_name(&platform),
        args.disk_size,
    );
    let base = BaseDiskStore::new(&cache_dir).prepare(
        &spec,
        &prepared.rootfs,
        &args.mke2fs.unwrap_or_else(default_mke2fs),
    )?;
    let metadata = BoxStore::new(state_dir).create(
        &CreateBox::new(
            prepared.manifest_digest,
            platform_name(&platform),
            args.disk_size,
        ),
        base.disk_path(),
    )?;
    print_box_result(&metadata, args.json)?;
    Ok(0)
}

fn box_list(args: &BoxListArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let boxes = BoxStore::new(state_dir).list()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&boxes)?);
    } else {
        println!("BOX ID\tSTATE\tIMAGE DIGEST\tPLATFORM\tSIZE\tGENERATION");
        for metadata in boxes {
            println!(
                "{}\t{:?}\t{}\t{}\t{}\t{}",
                metadata.box_id,
                metadata.state,
                metadata.manifest_digest,
                metadata.platform,
                format_bytes(metadata.virtual_size_bytes),
                metadata.generation
            );
        }
    }
    Ok(0)
}

fn box_show(args: &BoxShowArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let metadata = BoxStore::new(state_dir).get(args.box_id)?;
    print_box_result(&metadata, args.json)?;
    Ok(0)
}

fn box_delete(args: &BoxDeleteArgs) -> Result<i32, Box<dyn std::error::Error>> {
    debug_assert!(args.yes, "clap requires --yes");
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let metadata = BoxStore::new(state_dir).delete(args.box_id)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&BoxMutationReport {
                operation: "delete",
                box_id: metadata.box_id,
                generation: metadata.generation,
            })?
        );
    } else {
        println!("deleted Box {}", metadata.box_id);
    }
    Ok(0)
}

fn box_reset(args: &BoxResetArgs) -> Result<i32, Box<dyn std::error::Error>> {
    debug_assert!(args.yes, "clap requires --yes");
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let store = BoxStore::new(state_dir);
    let current = store.get(args.box_id)?;
    let spec = BaseDiskSpec::new(
        current.manifest_digest.clone(),
        current.platform.clone(),
        current.virtual_size_bytes,
    );
    let base = BaseDiskStore::new(cache_dir)
        .get(&spec)?
        .ok_or_else(|| {
            format!(
                "the immutable base disk for Box {} is not cached; recreate the image-backed Box instead",
                args.box_id
            )
        })?;
    let metadata = store.reset(args.box_id, base.disk_path())?;
    print_box_result(&metadata, args.json)?;
    Ok(0)
}

fn box_clone(args: &BoxCloneArgs) -> Result<i32, Box<dyn std::error::Error>> {
    debug_assert!(args.yes, "clap requires --yes");
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let metadata = BoxStore::new(state_dir).clone_box(args.box_id)?;
    print_box_result(&metadata, args.json)?;
    Ok(0)
}

fn print_box_result(metadata: &BoxMetadata, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(metadata)?);
    } else {
        println!("Box ID: {}", metadata.box_id);
        println!("state: {:?}", metadata.state);
        println!("manifest: {}", metadata.manifest_digest);
        println!("platform: {}", metadata.platform);
        println!("disk size: {}", format_bytes(metadata.virtual_size_bytes));
        println!("generation: {}", metadata.generation);
    }
    Ok(())
}

async fn image_pull(args: ImagePullArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let platform = Platform {
        os: args.os,
        architecture: args
            .architecture
            .unwrap_or_else(|| Platform::host_linux().architecture),
        variant: None,
    };
    let prepared = pull_and_materialize(
        &args.reference,
        &cache_dir,
        &platform,
        credentials(args.registry_username, args.registry_password),
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&prepared)?);
    } else {
        println!("reference: {}", prepared.reference);
        println!("manifest: {}", prepared.manifest_digest);
        println!("rootfs: {}", prepared.rootfs.display());
    }
    Ok(0)
}

async fn pull_and_materialize(
    reference: &str,
    cache_dir: &std::path::Path,
    platform: &Platform,
    credentials: Option<Credentials>,
) -> Result<PreparedImage, Box<dyn std::error::Error>> {
    ImageCache::new(cache_dir)
        .pull(reference, platform, credentials)
        .await
        .map_err(Into::into)
}

fn image_list(args: &ImageListArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let images = ImageCache::new(cache_dir).list()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&images)?);
    } else {
        print_image_list(&images);
    }
    Ok(0)
}

fn image_remove(args: &ImageRemoveArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let report = ImageCache::new(cache_dir).remove(&args.target, !args.dry_run)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_remove_report(&report);
    }
    Ok(0)
}

fn image_default(args: &ImageDefaultArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let cache = ImageCache::new(cache_dir);
    let reference = if args.unset {
        cache.clear_default()?;
        cache.default_reference()?
    } else if let Some(reference) = args.image.as_deref() {
        cache.set_default(reference)?
    } else {
        cache.default_reference()?
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&DefaultImageReport { reference })?
        );
    } else {
        println!("default image: {reference}");
    }
    Ok(0)
}

fn print_image_list(images: &[CachedImage]) {
    println!("DEFAULT\tREFERENCE\tDIGEST\tPLATFORM\tSTATUS\tSIZE");
    for image in images {
        let reference = image.reference.as_deref().unwrap_or("<unknown>");
        let platform = image.platform.as_ref().map_or_else(
            || "<unknown>".into(),
            |platform| match &platform.variant {
                Some(variant) => format!("{}/{}/{}", platform.os, platform.architecture, variant),
                None => format!("{}/{}", platform.os, platform.architecture),
            },
        );
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            if image.default { "*" } else { "" },
            reference,
            image.manifest_digest,
            platform,
            if image.ready { "ready" } else { "missing" },
            format_bytes(image.size_bytes)
        );
    }
}

fn print_remove_report(report: &RemoveReport) {
    let action = if report.applied {
        "removed"
    } else {
        "would remove"
    };
    println!("target: {}", report.target);
    println!("references {action}: {}", report.references_removed.len());
    println!("rootfs {action}: {}", report.rootfs_removed.len());
    println!("space reclaimed: {}", format_bytes(report.reclaimed_bytes));
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut divisor = 1_u64;
    while bytes / divisor >= 1024 && unit < UNITS.len() - 1 {
        divisor = divisor.saturating_mul(1024);
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        let whole = bytes / divisor;
        let decimal = (bytes % divisor).saturating_mul(10) / divisor;
        format!("{whole}.{decimal} {}", UNITS[unit])
    }
}

fn cache_info(args: &CacheInfoArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let usage = ImageCache::new(cache_dir).usage()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&usage)?);
    } else {
        print_cache_usage(&usage);
    }
    Ok(0)
}

fn cache_prune(args: &CachePruneArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let apply = destructive_mode(args.dry_run, args.yes, "cache prune")?;
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let report = ImageCache::new(cache_dir).prune(apply)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_prune_report(&report);
    }
    Ok(0)
}

fn cache_clean(args: &CacheCleanArgs) -> Result<i32, Box<dyn std::error::Error>> {
    debug_assert!(args.all, "clap requires --all");
    let apply = destructive_mode(args.dry_run, args.yes, "cache clean --all")?;
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let report = ImageCache::new(cache_dir).clean(apply)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_clean_report(&report);
    }
    Ok(0)
}

fn destructive_mode(dry_run: bool, yes: bool, command: &str) -> Result<bool, String> {
    if dry_run {
        Ok(false)
    } else if yes {
        Ok(true)
    } else {
        Err(format!(
            "{command} requires --dry-run to preview or --yes to apply"
        ))
    }
}

fn print_cache_usage(usage: &CacheUsage) {
    println!("image references: {}", usage.references);
    println!("ready rootfs images: {}", usage.images);
    println!("rootfs size: {}", format_bytes(usage.rootfs_bytes));
    println!("immutable Box base disks: {}", usage.base_disks);
    println!(
        "Box base disk size: {}",
        format_bytes(usage.base_disk_bytes)
    );
    println!("OCI blobs: {}", usage.oci_blobs);
    println!("OCI size: {}", format_bytes(usage.oci_bytes));
    println!("workspace snapshots: {}", usage.workspaces);
    println!("workspace size: {}", format_bytes(usage.workspace_bytes));
    println!("total managed size: {}", format_bytes(usage.total_bytes));
}

fn print_prune_report(report: &PruneReport) {
    let action = if report.applied {
        "removed"
    } else {
        "would remove"
    };
    println!("OCI blobs {action}: {}", report.oci_blobs_removed);
    println!(
        "incomplete rootfs entries {action}: {}",
        report.incomplete_rootfs_removed
    );
    println!(
        "stale image records {action}: {}",
        report.stale_records_removed
    );
    println!("space reclaimed: {}", format_bytes(report.reclaimed_bytes));
}

fn print_clean_report(report: &CleanReport) {
    let action = if report.applied {
        "removed"
    } else {
        "would remove"
    };
    println!("managed cache entries {action}: {}", report.entries_removed);
    println!("space reclaimed: {}", format_bytes(report.reclaimed_bytes));
}

fn credentials(username: Option<String>, password: Option<String>) -> Option<Credentials> {
    username
        .zip(password)
        .map(|(username, password)| Credentials { username, password })
}

async fn benchmark(args: BenchmarkArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let command = args.command;
    let report = match args.backend.as_str() {
        "process" => {
            if args.box_id.is_some() || args.image.is_some() || args.rootfs.is_some() {
                return Err("--box, --image, and --rootfs require --backend libkrun".into());
            }
            run_benchmark(
                &Supervisor::new(ProcessBackend),
                command,
                args.iterations,
                None,
            )
            .await?
        }
        "libkrun" => {
            let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
            let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
            let platform = Platform::host_linux();
            let prepared_image = if args.box_id.is_none() && args.rootfs.is_none() {
                let cache = ImageCache::new(&cache_dir);
                let reference = match args.image {
                    Some(reference) => reference,
                    None => cache.default_reference()?,
                };
                Some(
                    cache
                        .resolve_or_pull(
                            &reference,
                            &platform,
                            credentials(args.registry_username, args.registry_password),
                        )
                        .await?,
                )
            } else {
                None
            };
            let mke2fs = args.mke2fs.unwrap_or_else(default_mke2fs);
            let root_source = if args.box_id.is_some() {
                None
            } else if let Some(prepared) = prepared_image {
                Some(BoxRootSource {
                    rootfs_path: prepared.rootfs,
                    manifest_digest: prepared.manifest_digest,
                    platform: platform_name(&platform),
                    virtual_size_bytes: args.disk_size,
                    mke2fs_path: mke2fs,
                })
            } else if let Some(rootfs) = args.rootfs {
                Some(BoxRootSource {
                    manifest_digest: digest_tree(&rootfs)?.to_string(),
                    rootfs_path: rootfs,
                    platform: platform_name(&platform),
                    virtual_size_bytes: args.disk_size,
                    mke2fs_path: mke2fs,
                })
            } else {
                return Err("libkrun benchmark requires an image, rootfs, or BoxId".into());
            };
            let mut config = native_config(
                args.helper,
                args.libkrun,
                args.lib_dir,
                None,
                root_source
                    .as_ref()
                    .map(|source| source.rootfs_path.clone()),
                "--rootfs, --image, or --box",
                true,
            )?;
            config.vcpus = args.cpus;
            config.memory_mib = args.memory_mib;
            config.network_runtime_dir = cache_dir.join("network");
            let ephemeral_disks = EphemeralDiskStore::new(cache_dir.join("runtime"));
            let _ = ephemeral_disks.garbage_collect()?;
            let runtime = BoxRuntimeConfig {
                boxes: BoxStore::new(state_dir),
                base_disks: BaseDiskStore::new(&cache_dir),
                ephemeral_disks,
                source: root_source,
                e2fsck_path: args.e2fsck.unwrap_or_else(default_e2fsck),
            };
            run_benchmark(
                &Supervisor::new(LibkrunBackend::new(config).with_box_runtime(runtime)),
                command,
                args.iterations,
                args.box_id,
            )
            .await?
        }
        _ => unreachable!("clap validates backend values"),
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(i32::from(report.failures > 0))
}

async fn run_benchmark<B: Backend>(
    supervisor: &Supervisor<B>,
    command: Vec<String>,
    iterations: u32,
    box_id: Option<BoxId>,
) -> Result<BenchmarkReport, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(iterations as usize);
    let mut backend_ready_samples = Vec::with_capacity(iterations as usize);
    let mut command_start_samples = Vec::with_capacity(iterations as usize);
    let mut root_prepare_samples = Vec::new();
    let mut cache_lookup_samples = Vec::new();
    let mut box_lock_samples = Vec::new();
    let mut disk_clone_samples = Vec::new();
    let mut helper_spawn_samples = Vec::new();
    let mut failures = 0_u32;
    for _ in 0..iterations {
        let mut spec = RunSpec::command(command.clone());
        spec.box_id = box_id;
        let report = supervisor.run(spec).await?;
        if report.exit_code != Some(0) || report.timed_out {
            failures += 1;
        }
        samples.push(report.elapsed_micros);
        if let Some(command_started) = report
            .trace
            .iter()
            .find(|event| event.kind == moraebox_runtime::TraceKind::CommandStarted)
        {
            backend_ready_samples.push(command_started.elapsed_micros);
        }
        if let Some(first_output) = report
            .trace
            .iter()
            .find(|event| event.kind == moraebox_runtime::TraceKind::FirstOutput)
        {
            command_start_samples.push(first_output.elapsed_micros);
        }
        extend_if_some(
            &mut root_prepare_samples,
            report.startup.root_prepare_micros,
        );
        extend_if_some(
            &mut cache_lookup_samples,
            report.startup.cache_lookup_micros,
        );
        extend_if_some(&mut box_lock_samples, report.startup.box_lock_micros);
        extend_if_some(&mut disk_clone_samples, report.startup.disk_clone_micros);
        extend_if_some(
            &mut helper_spawn_samples,
            report.startup.helper_spawn_micros,
        );
    }
    samples.sort_unstable();
    let backend = supervisor.backend_name();
    Ok(BenchmarkReport {
        backend: backend.into(),
        mode: benchmark_mode(backend).into(),
        iterations,
        failures,
        min_micros: samples[0],
        p50_micros: percentile(&samples, 50),
        p95_micros: percentile(&samples, 95),
        p99_micros: percentile(&samples, 99),
        max_micros: *samples.last().expect("iterations is non-zero"),
        command_start_p95_micros: optional_percentile(&mut command_start_samples, 95),
        backend_ready_p95_micros: optional_percentile(&mut backend_ready_samples, 95),
        root_prepare_p95_micros: optional_percentile(&mut root_prepare_samples, 95),
        cache_lookup_p95_micros: optional_percentile(&mut cache_lookup_samples, 95),
        box_lock_p95_micros: optional_percentile(&mut box_lock_samples, 95),
        disk_clone_p95_micros: optional_percentile(&mut disk_clone_samples, 95),
        helper_spawn_p95_micros: optional_percentile(&mut helper_spawn_samples, 95),
    })
}

fn benchmark_mode(backend: &str) -> &'static str {
    if backend == "libkrun" {
        "cached-one-shot"
    } else {
        "host-process"
    }
}

fn extend_if_some(samples: &mut Vec<u64>, value: Option<u64>) {
    if let Some(value) = value {
        samples.push(value);
    }
}

fn optional_percentile(samples: &mut [u64], percentile_value: usize) -> Option<u64> {
    if samples.is_empty() {
        None
    } else {
        samples.sort_unstable();
        Some(percentile(samples, percentile_value))
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (sorted.len() * percentile).div_ceil(100).saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}

#[derive(Debug, Serialize)]
struct DefaultImageReport {
    reference: String,
}

#[derive(Debug, Serialize)]
struct BoxMutationReport {
    operation: &'static str,
    box_id: BoxId,
    generation: u64,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    backend: String,
    mode: String,
    iterations: u32,
    failures: u32,
    min_micros: u64,
    p50_micros: u64,
    p95_micros: u64,
    p99_micros: u64,
    max_micros: u64,
    command_start_p95_micros: Option<u64>,
    backend_ready_p95_micros: Option<u64>,
    root_prepare_p95_micros: Option<u64>,
    cache_lookup_p95_micros: Option<u64>,
    box_lock_p95_micros: Option<u64>,
    disk_clone_p95_micros: Option<u64>,
    helper_spawn_p95_micros: Option<u64>,
}

fn native_config(
    helper: Option<PathBuf>,
    libkrun: Option<PathBuf>,
    lib_dir: Option<PathBuf>,
    gvproxy: Option<PathBuf>,
    root: Option<PathBuf>,
    root_description: &'static str,
    managed_root: bool,
) -> Result<LibkrunConfig, Box<dyn std::error::Error>> {
    let paths = NativeRuntimePaths::discover_with_gvproxy(helper, libkrun, lib_dir, gvproxy);
    let helper = required_path(
        paths.helper,
        "--helper, MORAE_HELPER_PATH, or a sibling morae-vmm-helper",
    )?;
    let library = required_path(
        paths.libkrun,
        "--libkrun, MORAE_LIBKRUN_PATH, or a supported Homebrew libkrun",
    )?;
    let root = if managed_root {
        root.unwrap_or_default()
    } else {
        required_path(root, root_description)?
    };
    let mut config = LibkrunConfig::new(helper, library, root);
    config.library_search_path = paths.library_search_path;
    config.gvproxy_path = paths.gvproxy;
    Ok(config)
}

fn required_path(
    path: Option<PathBuf>,
    description: &'static str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    path.ok_or_else(|| format!("libkrun backend requires {description}").into())
}

fn default_mke2fs() -> PathBuf {
    for path in [
        "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs",
        "/usr/local/opt/e2fsprogs/sbin/mke2fs",
        "/usr/sbin/mke2fs",
        "/sbin/mke2fs",
    ] {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("mke2fs")
}

fn default_e2fsck() -> PathBuf {
    for path in [
        "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
        "/usr/local/opt/e2fsprogs/sbin/e2fsck",
        "/usr/sbin/e2fsck",
        "/sbin/e2fsck",
    ] {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("e2fsck")
}

fn platform_name(platform: &Platform) -> String {
    match &platform.variant {
        Some(variant) => format!("{}/{}/{}", platform.os, platform.architecture, variant),
        None => format!("{}/{}", platform.os, platform.architecture),
    }
}

fn parse_disk_size(input: &str) -> Result<u64, String> {
    let input = input.trim();
    let split = input
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(input.len());
    let (number, suffix) = input.split_at(split);
    let value = number
        .parse::<u64>()
        .map_err(|_| "disk size must start with a positive integer".to_owned())?;
    if value == 0 {
        return Err("disk size must be greater than zero".into());
    }
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "kb" => 1000,
        "mb" => 1000 * 1000,
        "gb" => 1000 * 1000 * 1000,
        _ => return Err("disk size suffix must be B, KiB, MiB, GiB, KB, MB, or GB".into()),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| "disk size is too large".into())
}

fn parse_timeout(input: &str) -> Result<TimeoutPolicy, Box<dyn std::error::Error>> {
    if input.eq_ignore_ascii_case("none") || input == "0" {
        return Ok(TimeoutPolicy::Unlimited);
    }
    let duration: Duration = humantime::parse_duration(input)?;
    let milliseconds = u64::try_from(duration.as_millis())?;
    if milliseconds == 0 {
        return Err("timeout must be non-zero or 'none'".into());
    }
    Ok(TimeoutPolicy::Limited(milliseconds))
}

fn validate_network_option(backend: &str, network: bool) -> Result<(), &'static str> {
    if network && backend != "libkrun" {
        return Err("--network requires --backend libkrun");
    }
    Ok(())
}

fn parse_env(input: &str) -> Result<(String, String), String> {
    let Some((key, value)) = input.split_once('=') else {
        return Err("environment values must use KEY=VALUE".into());
    };
    if key.is_empty() || key.contains('\0') || value.contains('\0') {
        return Err("environment keys and values must be non-empty and NUL-free".into());
    }
    Ok((key.to_owned(), value.to_owned()))
}

fn exit_code(code: i32) -> ExitCode {
    let clamped = code.clamp(0, 255);
    ExitCode::from(u8::try_from(clamped).expect("value was clamped to u8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timeout_and_unlimited() {
        assert_eq!(
            parse_timeout("1h").unwrap(),
            TimeoutPolicy::Limited(3_600_000)
        );
        assert_eq!(parse_timeout("none").unwrap(), TimeoutPolicy::Unlimited);
    }

    #[test]
    fn parses_network_as_an_explicit_opt_in() {
        let default = Cli::try_parse_from(["morae", "run", "--", "/usr/bin/true"]).unwrap();
        let Command::Run(default) = default.command else {
            panic!("expected run command");
        };
        assert!(!default.network);

        let enabled = Cli::try_parse_from([
            "morae",
            "run",
            "--network",
            "--gvproxy",
            "/opt/tools/gvproxy",
            "--",
            "/usr/bin/true",
        ])
        .unwrap();
        let Command::Run(enabled) = enabled.command else {
            panic!("expected run command");
        };
        assert!(enabled.network);
        assert_eq!(enabled.gvproxy, Some(PathBuf::from("/opt/tools/gvproxy")));
    }

    #[test]
    fn user_execution_commands_default_to_libkrun() {
        let run = Cli::try_parse_from(["morae", "run", "--", "/bin/true"]).unwrap();
        let Command::Run(run) = run.command else {
            panic!("expected run command");
        };
        assert_eq!(run.backend, "libkrun");

        let benchmark = Cli::try_parse_from(["morae", "benchmark", "--", "/bin/true"]).unwrap();
        let Command::Benchmark(benchmark) = benchmark.command else {
            panic!("expected benchmark command");
        };
        assert_eq!(benchmark.backend, "libkrun");

        let process = Cli::try_parse_from([
            "morae",
            "benchmark",
            "--backend",
            "process",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let Command::Benchmark(process) = process.command else {
            panic!("expected benchmark command");
        };
        assert_eq!(process.backend, "process");
    }

    #[test]
    fn storage_paths_are_optional_explicit_overrides() {
        let default = Cli::try_parse_from(["morae", "run", "--", "/bin/true"]).unwrap();
        let Command::Run(default) = default.command else {
            panic!("expected run command");
        };
        assert!(default.cache_dir.is_none());
        assert!(default.state_dir.is_none());

        let explicit = Cli::try_parse_from([
            "morae",
            "run",
            "--cache-dir",
            "custom-cache",
            "--state-dir",
            "custom-state",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let Command::Run(explicit) = explicit.command else {
            panic!("expected run command");
        };
        assert_eq!(explicit.cache_dir, Some("custom-cache".into()));
        assert_eq!(explicit.state_dir, Some("custom-state".into()));
    }

    #[test]
    fn parses_environment_without_shell_expansion() {
        assert_eq!(
            parse_env("A=hello world").unwrap(),
            ("A".into(), "hello world".into())
        );
        assert!(parse_env("MISSING").is_err());
    }

    #[test]
    fn run_help_alias_preserves_literal_help_command() {
        for args in [["morae", "run", "help"], ["morae", "run", "--help"]] {
            let error = Cli::try_parse_from(normalize_help_alias(args)).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
            assert!(
                error
                    .to_string()
                    .contains("Usage: morae run [OPTIONS] <COMMAND>...")
            );
        }

        let cli =
            Cli::try_parse_from(normalize_help_alias(["morae", "run", "--", "help"])).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.command, ["help"]);
    }

    #[test]
    fn parses_image_management_aliases() {
        let list = Cli::try_parse_from(["morae", "image", "ls", "--json"]).unwrap();
        let Command::Image {
            command: ImageCommand::List(args),
        } = list.command
        else {
            panic!("expected image list command");
        };
        assert!(args.json);

        let remove =
            Cli::try_parse_from(["morae", "image", "rm", "python:3.12", "--dry-run"]).unwrap();
        let Command::Image {
            command: ImageCommand::Remove(args),
        } = remove.command
        else {
            panic!("expected image remove command");
        };
        assert_eq!(args.target, "python:3.12");
        assert!(args.dry_run);

        let default = Cli::try_parse_from(["morae", "image", "default", "python:3.13"]).unwrap();
        let Command::Image {
            command: ImageCommand::Default(args),
        } = default.command
        else {
            panic!("expected image default command");
        };
        assert_eq!(args.image.as_deref(), Some("python:3.13"));
        assert!(!args.unset);
    }

    #[test]
    fn formats_cache_sizes_for_humans() {
        assert_eq!(format_bytes(42), "42 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn parses_cache_management_and_requires_all_for_clean() {
        let prune = Cli::try_parse_from(["morae", "cache", "prune", "--dry-run"]).unwrap();
        let Command::Cache {
            command: CacheCommand::Prune(args),
        } = prune.command
        else {
            panic!("expected cache prune command");
        };
        assert!(args.dry_run);

        let clean = Cli::try_parse_from(["morae", "cache", "clean", "--all", "--yes"]).unwrap();
        let Command::Cache {
            command: CacheCommand::Clean(args),
        } = clean.command
        else {
            panic!("expected cache clean command");
        };
        assert!(args.all);
        assert!(args.yes);

        assert!(Cli::try_parse_from(["morae", "cache", "clean", "--yes"]).is_err());
    }

    #[test]
    fn destructive_cache_operations_require_an_explicit_mode() {
        assert!(!destructive_mode(true, false, "cache prune").unwrap());
        assert!(destructive_mode(false, true, "cache prune").unwrap());
        assert!(destructive_mode(false, false, "cache prune").is_err());
    }

    #[test]
    fn parses_box_management_and_run_selection() {
        let create = Cli::try_parse_from([
            "morae",
            "box",
            "create",
            "--image",
            "alpine:latest",
            "--disk-size",
            "512MiB",
            "--json",
        ])
        .unwrap();
        let Command::Box {
            command: BoxCommand::Create(create),
        } = create.command
        else {
            panic!("expected box create command");
        };
        assert_eq!(create.disk_size, 512 * 1024 * 1024);
        assert!(create.json);

        let box_id = BoxId::new();
        let run = Cli::try_parse_from([
            "morae",
            "run",
            "--box",
            &box_id.to_string(),
            "--",
            "/bin/true",
        ])
        .unwrap();
        let Command::Run(run) = run.command else {
            panic!("expected run command");
        };
        assert_eq!(run.box_id, Some(box_id));

        assert!(
            Cli::try_parse_from([
                "morae",
                "run",
                "--box",
                &box_id.to_string(),
                "--image",
                "alpine:latest",
                "--",
                "/bin/true",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["morae", "box", "delete", &box_id.to_string()]).is_err());
        assert!(Cli::try_parse_from(["morae", "box", "reset", &box_id.to_string()]).is_err());
        assert!(Cli::try_parse_from(["morae", "box", "clone", &box_id.to_string()]).is_err());
    }

    #[test]
    fn parses_disk_size_units_and_rejects_zero() {
        assert_eq!(parse_disk_size("8GiB").unwrap(), 8 * 1024 * 1024 * 1024);
        assert_eq!(parse_disk_size("500MB").unwrap(), 500_000_000);
        assert!(parse_disk_size("0").is_err());
        assert!(parse_disk_size("1TB").is_err());
    }

    #[test]
    fn selects_explicit_rootfs_then_image_then_python_default() {
        let cache_dir =
            std::env::temp_dir().join(format!("moraebox-cli-default-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);

        assert_eq!(
            select_image_reference("process", false, None, Some(&cache_dir)).unwrap(),
            None
        );
        assert_eq!(
            select_image_reference("process", true, None, Some(&cache_dir))
                .unwrap_err()
                .to_string(),
            "--rootfs requires --backend libkrun"
        );
        assert_eq!(
            select_image_reference("libkrun", true, None, Some(&cache_dir)).unwrap(),
            None
        );
        assert_eq!(
            select_image_reference(
                "libkrun",
                false,
                Some("debian:bookworm".into()),
                Some(&cache_dir),
            )
            .unwrap()
            .as_deref(),
            Some("debian:bookworm")
        );
        assert_eq!(
            select_image_reference("libkrun", false, None, Some(&cache_dir))
                .unwrap()
                .as_deref(),
            Some("docker.io/library/python:3.12")
        );

        ImageCache::new(&cache_dir)
            .set_default("debian:bookworm")
            .unwrap();
        assert_eq!(
            select_image_reference("libkrun", false, None, Some(&cache_dir))
                .unwrap()
                .as_deref(),
            Some("docker.io/library/debian:bookworm")
        );
        std::fs::remove_dir_all(cache_dir).unwrap();
    }

    #[test]
    fn benchmark_modes_distinguish_microvms_from_host_processes() {
        assert_eq!(benchmark_mode("libkrun"), "cached-one-shot");
        assert_eq!(benchmark_mode("process"), "host-process");
    }
}
