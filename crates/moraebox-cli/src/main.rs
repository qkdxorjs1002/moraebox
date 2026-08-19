#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand};
use moraebox_core::{OutputChannel, RunSpec, TimeoutPolicy};
use moraebox_image::{
    CacheUsage, CachedImage, CleanReport, Credentials, ImageCache, Platform, PreparedImage,
    PruneReport, RemoveReport, WorkspaceSnapshot,
};
use moraebox_runtime::{
    Backend, DoctorReport, LibkrunBackend, LibkrunConfig, NativeRuntimePaths, ProcessBackend,
    Supervisor,
};
use serde::Serialize;

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
    /// Use an already materialized guest root directory instead of a managed image.
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    /// OCI registry reference; uses the configured python:3.12 default when omitted.
    #[arg(long)]
    image: Option<String>,
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
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
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
struct ImagePullArgs {
    reference: String,
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
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
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImageRemoveArgs {
    /// Registry reference or sha256 manifest digest.
    target: String,
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
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
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CacheInfoArgs {
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CachePruneArgs {
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
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
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
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
    #[arg(long, default_value = "process", value_parser = ["process", "libkrun"])]
    backend: String,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    iterations: u32,
    #[arg(long, env = "MORAE_HELPER_PATH")]
    helper: Option<PathBuf>,
    #[arg(long, env = "MORAE_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    #[arg(long, env = "MORAE_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
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
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
    }
    Ok(i32::from(args.strict && !report.native_backend_ready))
}

async fn run(args: RunArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let mut spec = RunSpec::command(args.command);
    spec.timeout = parse_timeout(&args.timeout)?;
    spec.tty = args.tty;
    spec.inherit_env = args.inherit_env;
    spec.cwd = args.cwd;
    spec.env = args.env.into_iter().collect::<BTreeMap<_, _>>();
    if spec.inherit_env {
        spec.env.extend(std::env::vars());
    }
    if args.interactive || !io::stdin().is_terminal() {
        io::stdin().read_to_end(&mut spec.stdin)?;
    }

    if args.rootfs.is_some() && args.image.is_some() {
        return Err("--rootfs and --image are mutually exclusive".into());
    }
    let image_reference = select_image_reference(
        &args.backend,
        args.rootfs.is_some(),
        args.image,
        &args.cache_dir,
    )?;
    let image_root = if let Some(reference) = image_reference.as_deref() {
        Some(
            resolve_or_pull(
                reference,
                &args.cache_dir,
                &Platform::host_linux(),
                credentials(args.registry_username, args.registry_password),
            )
            .await?
            .rootfs,
        )
    } else {
        None
    };

    let workspace = if let Some(source) = args.workspace.as_deref() {
        if args.backend != "libkrun" {
            return Err("--workspace requires --backend libkrun".into());
        }
        if spec.cwd.is_some() {
            return Err("--cwd and --workspace cannot be combined in this version".into());
        }
        let mke2fs = args.mke2fs.unwrap_or_else(default_mke2fs);
        Some(WorkspaceSnapshot::create(source, &args.cache_dir, &mke2fs)?)
    } else {
        None
    };

    let report = match args.backend.as_str() {
        "process" => Supervisor::new(ProcessBackend).run(spec).await?,
        "libkrun" => {
            let mut config = native_config(
                args.helper,
                args.libkrun,
                args.lib_dir,
                args.rootfs.or(image_root),
                "--rootfs, --image, or MORAE_ROOTFS",
            )?;
            config.vcpus = args.cpus;
            config.memory_mib = args.memory_mib;
            config.workspace_disk = workspace
                .as_ref()
                .map(|snapshot| snapshot.image_path.clone());
            Supervisor::new(LibkrunBackend::new(config))
                .run(spec)
                .await?
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

fn select_image_reference(
    backend: &str,
    has_rootfs: bool,
    explicit_image: Option<String>,
    cache_dir: &std::path::Path,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if backend != "libkrun" {
        if explicit_image.is_some() {
            return Err("--image requires --backend libkrun".into());
        }
        return Ok(None);
    }
    if has_rootfs {
        return Ok(None);
    }
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

async fn image_pull(args: ImagePullArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let platform = Platform {
        os: args.os,
        architecture: args
            .architecture
            .unwrap_or_else(|| Platform::host_linux().architecture),
        variant: None,
    };
    let prepared = pull_and_materialize(
        &args.reference,
        &args.cache_dir,
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
    let images = ImageCache::new(&args.cache_dir).list()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&images)?);
    } else {
        print_image_list(&images);
    }
    Ok(0)
}

fn image_remove(args: &ImageRemoveArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let report = ImageCache::new(&args.cache_dir).remove(&args.target, !args.dry_run)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_remove_report(&report);
    }
    Ok(0)
}

fn image_default(args: &ImageDefaultArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let cache = ImageCache::new(&args.cache_dir);
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
    let usage = ImageCache::new(&args.cache_dir).usage()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&usage)?);
    } else {
        print_cache_usage(&usage);
    }
    Ok(0)
}

fn cache_prune(args: &CachePruneArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let apply = destructive_mode(args.dry_run, args.yes, "cache prune")?;
    let report = ImageCache::new(&args.cache_dir).prune(apply)?;
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
    let report = ImageCache::new(&args.cache_dir).clean(apply)?;
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
            run_benchmark(&Supervisor::new(ProcessBackend), command, args.iterations).await?
        }
        "libkrun" => {
            let mut config = native_config(
                args.helper,
                args.libkrun,
                args.lib_dir,
                args.rootfs,
                "--rootfs or MORAE_ROOTFS",
            )?;
            config.vcpus = args.cpus;
            config.memory_mib = args.memory_mib;
            run_benchmark(
                &Supervisor::new(LibkrunBackend::new(config)),
                command,
                args.iterations,
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
) -> Result<BenchmarkReport, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(iterations as usize);
    let mut failures = 0_u32;
    for _ in 0..iterations {
        let report = supervisor.run(RunSpec::command(command.clone())).await?;
        if report.exit_code != Some(0) || report.timed_out {
            failures += 1;
        }
        samples.push(report.elapsed_micros);
    }
    samples.sort_unstable();
    Ok(BenchmarkReport {
        backend: supervisor.backend_name().into(),
        mode: "cached-cold".into(),
        iterations,
        failures,
        min_micros: samples[0],
        p50_micros: percentile(&samples, 50),
        p95_micros: percentile(&samples, 95),
        p99_micros: percentile(&samples, 99),
        max_micros: *samples.last().expect("iterations is non-zero"),
    })
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
}

fn native_config(
    helper: Option<PathBuf>,
    libkrun: Option<PathBuf>,
    lib_dir: Option<PathBuf>,
    root: Option<PathBuf>,
    root_description: &'static str,
) -> Result<LibkrunConfig, Box<dyn std::error::Error>> {
    let paths = NativeRuntimePaths::discover(helper, libkrun, lib_dir);
    let helper = required_path(
        paths.helper,
        "--helper, MORAE_HELPER_PATH, or a sibling morae-vmm-helper",
    )?;
    let library = required_path(
        paths.libkrun,
        "--libkrun, MORAE_LIBKRUN_PATH, or a supported Homebrew libkrun",
    )?;
    let mut config = LibkrunConfig::new(helper, library, required_path(root, root_description)?);
    config.library_search_path = paths.library_search_path;
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
    fn selects_explicit_rootfs_then_image_then_python_default() {
        let cache_dir =
            std::env::temp_dir().join(format!("moraebox-cli-default-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);

        assert_eq!(
            select_image_reference("process", false, None, &cache_dir).unwrap(),
            None
        );
        assert_eq!(
            select_image_reference("libkrun", true, None, &cache_dir).unwrap(),
            None
        );
        assert_eq!(
            select_image_reference("libkrun", false, Some("debian:bookworm".into()), &cache_dir)
                .unwrap()
                .as_deref(),
            Some("debian:bookworm")
        );
        assert_eq!(
            select_image_reference("libkrun", false, None, &cache_dir)
                .unwrap()
                .as_deref(),
            Some("docker.io/library/python:3.12")
        );

        ImageCache::new(&cache_dir)
            .set_default("debian:bookworm")
            .unwrap();
        assert_eq!(
            select_image_reference("libkrun", false, None, &cache_dir)
                .unwrap()
                .as_deref(),
            Some("docker.io/library/debian:bookworm")
        );
        std::fs::remove_dir_all(cache_dir).unwrap();
    }
}
