#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand};
use fastmvm_core::{OutputChannel, RunSpec, TimeoutPolicy};
use fastmvm_image::{
    Cas, Credentials, ImageReference, Platform, RegistryClient, WorkspaceSnapshot,
};
use fastmvm_runtime::{
    Backend, DoctorReport, LibkrunBackend, LibkrunConfig, ProcessBackend, Supervisor,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "fastmvm",
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
    /// Measure cached one-shot sandbox latency.
    Benchmark(Box<BenchmarkArgs>),
}

#[derive(Debug, Subcommand)]
enum ImageCommand {
    /// Pull, verify, and materialize one OCI image.
    Pull(ImagePullArgs),
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
    /// Path to the signed VMM helper (required by the libkrun backend).
    #[arg(long, env = "FASTMVM_HELPER_PATH")]
    helper: Option<PathBuf>,
    /// Path to libkrun (required by the libkrun backend).
    #[arg(long, env = "FASTMVM_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    /// Dedicated guest root filesystem directory (required by the libkrun backend).
    #[arg(long, env = "FASTMVM_ROOTFS")]
    rootfs: Option<PathBuf>,
    /// OCI registry reference such as alpine@latest or debian:bookworm.
    #[arg(long)]
    image: Option<String>,
    /// Dynamic library dependency directory for libkrun.
    #[arg(long, env = "FASTMVM_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    /// Host directory to copy into an immutable read-only ext4 guest workspace.
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long, default_value = ".fastmvm/cache")]
    cache_dir: PathBuf,
    #[arg(
        long,
        env = "FASTMVM_REGISTRY_USERNAME",
        requires = "registry_password"
    )]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "FASTMVM_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
    /// Path to mke2fs; auto-detected when omitted.
    #[arg(long, env = "FASTMVM_MKE2FS")]
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
    #[arg(long, default_value = ".fastmvm/cache")]
    cache_dir: PathBuf,
    #[arg(long, default_value = "linux")]
    os: String,
    #[arg(long)]
    architecture: Option<String>,
    #[arg(
        long,
        env = "FASTMVM_REGISTRY_USERNAME",
        requires = "registry_password"
    )]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "FASTMVM_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BenchmarkArgs {
    #[arg(long, default_value = "process", value_parser = ["process", "libkrun"])]
    backend: String,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    iterations: u32,
    #[arg(long, env = "FASTMVM_HELPER_PATH")]
    helper: Option<PathBuf>,
    #[arg(long, env = "FASTMVM_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    #[arg(long, env = "FASTMVM_ROOTFS")]
    rootfs: Option<PathBuf>,
    #[arg(long, env = "FASTMVM_LIB_DIR")]
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
    match execute(Cli::parse()).await {
        Ok(code) => exit_code(code),
        Err(error) => {
            eprintln!("fastmvm: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(cli: Cli) -> Result<i32, Box<dyn std::error::Error>> {
    match cli.command {
        Command::Doctor(args) => doctor(&args),
        Command::Run(args) => run(*args).await,
        Command::Image {
            command: ImageCommand::Pull(args),
        } => image_pull(args).await,
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
    let credentials = credentials(args.registry_username, args.registry_password);
    let image_root = if let Some(reference) = args.image.as_deref() {
        if args.backend != "libkrun" {
            return Err("--image requires --backend libkrun".into());
        }
        Some(
            pull_and_materialize(
                reference,
                &args.cache_dir,
                &Platform::host_linux(),
                credentials,
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
            let helper = required_path(args.helper, "--helper or FASTMVM_HELPER_PATH")?;
            let library = required_path(args.libkrun, "--libkrun or FASTMVM_LIBKRUN_PATH")?;
            let root = required_path(
                args.rootfs.or(image_root),
                "--rootfs, --image, or FASTMVM_ROOTFS",
            )?;
            let mut config = LibkrunConfig::new(helper, library, root);
            config.library_search_path = args.lib_dir;
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
    let reference: ImageReference = reference.parse()?;
    let ImageReference::Registry(reference) = reference else {
        return Err("this build currently materializes registry references only".into());
    };
    let cas = Cas::new(cache_dir.join("oci"));
    let image = RegistryClient::new(credentials)?
        .pull(reference, platform, &cas)
        .await?;
    let rootfs = cache_dir
        .join("rootfs/sha256")
        .join(image.manifest_digest.hex());
    image.materialize_rootfs(&cas, &rootfs)?;
    Ok(PreparedImage {
        reference: image.reference.to_string(),
        manifest_digest: image.manifest_digest.to_string(),
        rootfs,
    })
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
            let helper = required_path(args.helper, "--helper or FASTMVM_HELPER_PATH")?;
            let library = required_path(args.libkrun, "--libkrun or FASTMVM_LIBKRUN_PATH")?;
            let root = required_path(args.rootfs, "--rootfs or FASTMVM_ROOTFS")?;
            let mut config = LibkrunConfig::new(helper, library, root);
            config.library_search_path = args.lib_dir;
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
struct PreparedImage {
    reference: String,
    manifest_digest: String,
    rootfs: PathBuf,
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
}
