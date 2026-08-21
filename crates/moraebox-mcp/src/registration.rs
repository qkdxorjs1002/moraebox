use std::{
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use clap::{Args, ValueEnum};
use moraebox_core::{resolve_cache_dir, resolve_state_dir};
use moraebox_image::ImageCache;
use moraebox_runtime::{DiskToolPaths, NativeRuntimePaths};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    task::JoinHandle,
    time::timeout,
};

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
const PREFLIGHT_CAPTURE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    #[error("registration configuration failed: {0}")]
    Configuration(String),
    #[error("registration dry-run rendering failed: {0}")]
    DryRun(String),
    #[error("server preflight failed before agent configuration was changed: {0}")]
    Preflight(String),
    #[error("failed to run {program}: {source}; install the agent CLI or use --dry-run")]
    AgentLaunch {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{program} failed with status {status}; agent configuration may have been partially updated. Inspect it, then roll back with: `{rollback}`"
    )]
    AgentRejected {
        program: String,
        status: std::process::ExitStatus,
        rollback: String,
    },
}

impl RegistrationError {
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "registration_configuration",
            Self::DryRun(_) => "registration_render",
            Self::Preflight(_) => "registration_preflight",
            Self::AgentLaunch { .. } | Self::AgentRejected { .. } => "agent_registration",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::Preflight(_))
    }
}

impl From<String> for RegistrationError {
    fn from(message: String) -> Self {
        Self::Configuration(message)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Agent {
    Codex,
    ClaudeCode,
}

impl Agent {
    fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RegistrationBackend {
    Libkrun,
    Process,
}

impl RegistrationBackend {
    fn as_str(self) -> &'static str {
        match self {
            Self::Libkrun => "libkrun",
            Self::Process => "process",
        }
    }
}

/// Register moraebox as a user-wide stdio MCP server.
#[derive(Debug, Args)]
pub struct InstallArgs {
    #[arg(value_enum)]
    agent: Agent,
    /// MCP server name in the agent configuration.
    #[arg(long, default_value = "moraebox", value_parser = parse_server_name)]
    name: String,
    /// Sandbox backend. `process` is deterministic but is not isolated.
    #[arg(long, value_enum, default_value_t = RegistrationBackend::Libkrun)]
    backend: RegistrationBackend,
    /// Register an already materialized guest root directory instead of an image.
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    /// OCI image registered for the libkrun backend; defaults to python:3.12.
    #[arg(long, conflicts_with = "rootfs")]
    image: Option<String>,
    /// Image and rootfs cache used by the registered server.
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Override the automatically discovered signed VMM helper.
    #[arg(long, env = "MORAE_HELPER_PATH")]
    helper: Option<PathBuf>,
    /// Override the automatically discovered libkrun library.
    #[arg(long, env = "MORAE_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    /// Override the automatically discovered gvproxy network helper.
    #[arg(long, env = "MORAE_GVPROXY_PATH")]
    gvproxy: Option<PathBuf>,
    /// Override the automatically discovered libkrun dependency directories.
    #[arg(long, env = "MORAE_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    /// Override the mke2fs utility used to prepare Box root disks.
    #[arg(long, env = "MORAE_MKE2FS")]
    mke2fs: Option<PathBuf>,
    /// Override the e2fsck utility used to recover dirty Boxes.
    #[arg(long, env = "MORAE_E2FSCK")]
    e2fsck: Option<PathBuf>,
    /// Override the debugfs utility used to restore the trusted guest agent.
    #[arg(long, env = "MORAE_DEBUGFS")]
    debugfs: Option<PathBuf>,
    /// Virtual root disk size for ephemeral runs and new Boxes.
    #[arg(long, default_value = "8GiB", value_parser = super::parse_disk_size)]
    disk_size: u64,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    /// Command agents should use to launch this MCP server.
    #[arg(long)]
    server_command: Option<OsString>,
    /// Print the exact agent CLI program and argv without changing configuration.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct CommandPlan {
    program: OsString,
    args: Vec<OsString>,
}

type Environment = Vec<(&'static str, OsString)>;
type ServerConfiguration = (Environment, Vec<OsString>);

#[derive(Debug, Default)]
struct NativeRegistrationPaths {
    helper: Option<PathBuf>,
    libkrun: Option<PathBuf>,
    libkrunfw: Option<PathBuf>,
    gvproxy: Option<PathBuf>,
    library_search_path: Option<PathBuf>,
    mke2fs: Option<PathBuf>,
    e2fsck: Option<PathBuf>,
    debugfs: Option<PathBuf>,
}

#[derive(Debug)]
struct ServerLaunch {
    program: PathBuf,
    environment: Environment,
    args: Vec<OsString>,
}

#[derive(Debug)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CommandPlan {
    fn print_json(&self) -> Result<(), String> {
        let value = json!({
            "program": self.program.to_string_lossy(),
            "args": self
                .args
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|error| format!("failed to render registration command: {error}"))?
        );
        Ok(())
    }
}

pub async fn install(args: &InstallArgs) -> Result<(), RegistrationError> {
    let server_command = resolve_server_command(args.server_command.as_deref())?;
    let native_paths = resolve_native_registration_paths(args, true)?;
    let (environment, server_args) = server_configuration_with_paths(args, &native_paths)?;
    let launch = ServerLaunch {
        program: server_command,
        environment,
        args: server_args,
    };
    let plan = build_command_plan_from_launch(args, &launch);

    if args.backend == RegistrationBackend::Process {
        eprintln!(
            "warning: the process backend is for deterministic development only; it does not provide VM isolation"
        );
    }
    if args.dry_run {
        return plan.print_json().map_err(RegistrationError::DryRun);
    }

    preflight_server(&launch)
        .await
        .map_err(RegistrationError::Preflight)?;
    let status = Command::new(&plan.program)
        .args(&plan.args)
        .status()
        .await
        .map_err(|source| RegistrationError::AgentLaunch {
            program: plan.program.to_string_lossy().into_owned(),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(RegistrationError::AgentRejected {
            program: plan.program.to_string_lossy().into_owned(),
            status,
            rollback: rollback_command(args.agent, &args.name),
        })
    }
}

#[cfg(test)]
fn build_command_plan(
    install: &InstallArgs,
    server_command: OsString,
) -> Result<CommandPlan, String> {
    let native_paths = resolve_native_registration_paths(install, false)?;
    let (environment, server_args) = server_configuration_with_paths(install, &native_paths)?;
    Ok(build_command_plan_from_launch(
        install,
        &ServerLaunch {
            program: PathBuf::from(server_command),
            environment,
            args: server_args,
        },
    ))
}

fn build_command_plan_from_launch(install: &InstallArgs, launch: &ServerLaunch) -> CommandPlan {
    let mut args = vec![OsString::from("mcp"), OsString::from("add")];

    match install.agent {
        Agent::Codex => {
            args.push(install.name.clone().into());
            for (key, value) in &launch.environment {
                args.push("--env".into());
                args.push(env_assignment(key, value));
            }
        }
        Agent::ClaudeCode => {
            args.extend(["--scope".into(), "user".into()]);
            if !launch.environment.is_empty() {
                args.push("--env".into());
                args.extend(
                    launch
                        .environment
                        .iter()
                        .map(|(key, value)| env_assignment(key, value)),
                );
            }
            args.extend([
                "--transport".into(),
                "stdio".into(),
                install.name.clone().into(),
            ]);
        }
    }

    args.push("--".into());
    args.push(launch.program.as_os_str().to_owned());
    args.extend(launch.args.iter().cloned());
    CommandPlan {
        program: install.agent.executable().into(),
        args,
    }
}

#[cfg(test)]
fn server_configuration(install: &InstallArgs) -> Result<ServerConfiguration, String> {
    let native_paths = resolve_native_registration_paths(install, false)?;
    server_configuration_with_paths(install, &native_paths)
}

fn server_configuration_with_paths(
    install: &InstallArgs,
    native_paths: &NativeRegistrationPaths,
) -> Result<ServerConfiguration, String> {
    let cache_dir =
        resolve_cache_dir(install.cache_dir.as_deref()).map_err(|error| error.to_string())?;
    let state_dir =
        resolve_state_dir(install.state_dir.as_deref()).map_err(|error| error.to_string())?;
    let cache_dir = absolute_path(&cache_dir)?;
    let state_dir = absolute_path(&state_dir)?;
    if install.backend == RegistrationBackend::Process {
        if install.rootfs.is_some() || install.image.is_some() {
            return Err("--rootfs and --image require --backend libkrun".into());
        }
        return Ok((
            Vec::new(),
            common_server_args(install, cache_dir, state_dir),
        ));
    }

    let mut environment = Vec::new();
    let mut server_args = common_server_args(install, cache_dir.clone(), state_dir);
    if let Some(rootfs) = &install.rootfs {
        environment.push(("MORAE_ROOTFS", absolute_path(rootfs)?.into_os_string()));
    } else {
        let reference = match &install.image {
            Some(reference) => reference.clone(),
            None => ImageCache::new(&cache_dir)
                .default_reference()
                .map_err(|error| error.to_string())?,
        };
        server_args.extend(["--image".into(), reference.into()]);
    }
    for (name, path) in [
        ("MORAE_HELPER_PATH", native_paths.helper.as_ref()),
        ("MORAE_LIBKRUN_PATH", native_paths.libkrun.as_ref()),
        ("MORAE_LIBKRUNFW_PATH", native_paths.libkrunfw.as_ref()),
        ("MORAE_GVPROXY_PATH", native_paths.gvproxy.as_ref()),
        ("MORAE_MKE2FS", native_paths.mke2fs.as_ref()),
        ("MORAE_E2FSCK", native_paths.e2fsck.as_ref()),
        ("MORAE_DEBUGFS", native_paths.debugfs.as_ref()),
    ] {
        if let Some(path) = path {
            environment.push((name, absolute_path(path)?.into_os_string()));
        }
    }
    if let Some(path) = &native_paths.library_search_path {
        environment.push(("MORAE_LIB_DIR", absolute_search_path(path)?));
    }
    Ok((environment, server_args))
}

fn resolve_native_registration_paths(
    install: &InstallArgs,
    discover: bool,
) -> Result<NativeRegistrationPaths, String> {
    if install.backend == RegistrationBackend::Process {
        return Ok(NativeRegistrationPaths::default());
    }
    let discovered = discover.then(|| {
        NativeRuntimePaths::discover_with_gvproxy(
            install.helper.clone(),
            install.libkrun.clone(),
            install.lib_dir.clone(),
            install.gvproxy.clone(),
        )
    });
    let discovered_disk_tools = discover.then(|| {
        DiskToolPaths::discover_with_debugfs(
            install.mke2fs.clone(),
            install.e2fsck.clone(),
            install.debugfs.clone(),
        )
    });
    let helper = discovered
        .as_ref()
        .and_then(|paths| paths.helper.clone())
        .or_else(|| install.helper.clone());
    let libkrun = discovered
        .as_ref()
        .and_then(|paths| paths.libkrun.clone())
        .or_else(|| install.libkrun.clone());
    let libkrunfw = discovered
        .as_ref()
        .and_then(|paths| paths.libkrunfw.clone());
    let gvproxy = discovered
        .as_ref()
        .and_then(|paths| paths.gvproxy.clone())
        .or_else(|| install.gvproxy.clone());
    let library_search_path = discovered
        .as_ref()
        .and_then(|paths| paths.library_search_path.clone())
        .or_else(|| install.lib_dir.clone());
    let mke2fs = if discover {
        resolve_tool_path(
            install.mke2fs.as_deref(),
            &discovered_disk_tools
                .as_ref()
                .expect("disk tools are discovered with native paths")
                .mke2fs_command(),
        )?
    } else {
        install.mke2fs.as_deref().map(absolute_path).transpose()?
    };
    let e2fsck = if discover {
        resolve_tool_path(
            install.e2fsck.as_deref(),
            &discovered_disk_tools
                .as_ref()
                .expect("disk tools are discovered with native paths")
                .e2fsck_command(),
        )?
    } else {
        install.e2fsck.as_deref().map(absolute_path).transpose()?
    };
    let debugfs = if discover {
        resolve_tool_path(
            install.debugfs.as_deref(),
            &discovered_disk_tools
                .as_ref()
                .expect("disk tools are discovered with native paths")
                .debugfs_command(),
        )?
    } else {
        install.debugfs.as_deref().map(absolute_path).transpose()?
    };
    Ok(NativeRegistrationPaths {
        helper,
        libkrun,
        libkrunfw,
        gvproxy,
        library_search_path,
        mke2fs,
        e2fsck,
        debugfs,
    })
}

fn common_server_args(
    install: &InstallArgs,
    cache_dir: PathBuf,
    state_dir: PathBuf,
) -> Vec<OsString> {
    let mut args = vec![
        "--backend".into(),
        install.backend.as_str().into(),
        "--cache-dir".into(),
        cache_dir.into_os_string(),
        "--state-dir".into(),
        state_dir.into_os_string(),
        "--disk-size".into(),
        install.disk_size.to_string().into(),
    ];
    if install.backend == RegistrationBackend::Libkrun {
        args.extend([
            "--cpus".into(),
            install.cpus.to_string().into(),
            "--memory-mib".into(),
            install.memory_mib.to_string().into(),
        ]);
    }
    args
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.into());
    }
    env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|error| format!("failed to resolve cache path: {error}"))
}

fn absolute_search_path(path: &Path) -> Result<OsString, String> {
    let paths = env::split_paths(path)
        .map(|component| absolute_path(&component))
        .collect::<Result<Vec<_>, _>>()?;
    env::join_paths(paths)
        .map_err(|error| format!("failed to resolve library search path: {error}"))
}

fn resolve_tool_path(explicit: Option<&Path>, fallback: &Path) -> Result<Option<PathBuf>, String> {
    if let Some(explicit) = explicit {
        return absolute_path(explicit).map(Some);
    }
    if fallback.components().count() > 1 {
        return absolute_path(fallback).map(Some);
    }
    Ok(find_in_path(fallback.as_os_str()))
}

fn env_assignment(key: &str, value: &OsStr) -> OsString {
    let mut assignment = OsString::from(key);
    assignment.push("=");
    assignment.push(value);
    assignment
}

fn resolve_server_command(explicit: Option<&OsStr>) -> Result<PathBuf, String> {
    if let Some(explicit) = explicit {
        let path = resolve_executable_path(explicit)?;
        validate_server_executable(&path)?;
        return Ok(path);
    }

    let invoked_as = env::args_os()
        .next()
        .unwrap_or_else(|| OsString::from("morae-mcp"));
    let path = resolve_executable_path(&invoked_as).or_else(|_| {
        env::current_exe()
            .map_err(|error| format!("failed to resolve morae-mcp executable: {error}"))
    })?;
    validate_server_executable(&path)?;
    Ok(path)
}

fn resolve_executable_path(program: &OsStr) -> Result<PathBuf, String> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return absolute_path(path);
    }
    find_in_path(program).ok_or_else(|| {
        format!(
            "server executable {} was not found on PATH",
            program.to_string_lossy()
        )
    })
}

fn find_in_path(program: &OsStr) -> Option<PathBuf> {
    let search_path = env::var_os("PATH")?;
    for directory in env::split_paths(&search_path) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return absolute_path(&candidate).ok();
        }
        #[cfg(windows)]
        if candidate.extension().is_none() {
            let executable = candidate.with_extension("exe");
            if executable.is_file() {
                return absolute_path(&executable).ok();
            }
        }
    }
    None
}

fn validate_server_executable(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "cannot inspect server executable {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "server executable is not a regular file: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "server executable does not have an execute permission bit: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

async fn preflight_server(launch: &ServerLaunch) -> Result<(), String> {
    validate_server_executable(&launch.program)?;
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in &launch.environment {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to launch {}: {error}", launch.program.display()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "server stdout was not captured".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "server stderr was not captured".to_owned())?;
    let stderr_task = tokio::spawn(capture_bounded(stderr));
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "server stdin was not captured".to_owned())?;
    let handshake = exchange_initialize(&mut stdin, &mut stdout).await;
    drop(stdin);
    if let Err(error) = handshake {
        terminate_child(&mut child).await;
        let stderr = collect_capture(stderr_task, "stderr").await?;
        return Err(format!("{error}; stderr: {}", render_capture(&stderr)));
    }

    let stdout_task = tokio::spawn(capture_bounded(stdout));
    let status = match timeout(PREFLIGHT_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            terminate_child(&mut child).await;
            let _ = collect_capture(stdout_task, "stdout").await;
            let _ = collect_capture(stderr_task, "stderr").await;
            return Err(format!(
                "failed while waiting for initialize response: {error}"
            ));
        }
        Err(_) => {
            terminate_child(&mut child).await;
            let _ = collect_capture(stdout_task, "stdout").await;
            let stderr = collect_capture(stderr_task, "stderr").await?;
            return Err(format!(
                "initialize handshake exceeded {PREFLIGHT_TIMEOUT:?}; stderr: {}",
                render_capture(&stderr)
            ));
        }
    };
    let trailing_stdout = collect_capture(stdout_task, "stdout").await?;
    let stderr = collect_capture(stderr_task, "stderr").await?;
    if !status.success() {
        return Err(format!(
            "server exited with {status}; stderr: {}",
            render_capture(&stderr)
        ));
    }
    if trailing_stdout.truncated
        || trailing_stdout
            .bytes
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
    {
        return Err(format!(
            "server wrote unexpected stdout after initialize response: {}",
            render_capture(&trailing_stdout)
        ));
    }
    Ok(())
}

async fn exchange_initialize<W, R>(stdin: &mut W, stdout: &mut R) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let initialize = format!(
        "{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": "moraebox-install-preflight",
            "method": "initialize",
            "params": {
                "protocolVersion": super::PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "moraebox-installer", "version": env!("CARGO_PKG_VERSION") }
            }
        })
    );
    stdin
        .write_all(initialize.as_bytes())
        .await
        .map_err(|error| format!("failed to write initialize request: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("failed to flush initialize request: {error}"))?;

    let response = timeout(PREFLIGHT_TIMEOUT, capture_bounded_line(stdout))
        .await
        .map_err(|_| format!("initialize response exceeded {PREFLIGHT_TIMEOUT:?}"))?
        .map_err(|error| format!("failed to read initialize response: {error}"))?;
    validate_initialize_response(&response)?;

    let initialized = format!(
        "{}\n",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    );
    stdin
        .write_all(initialized.as_bytes())
        .await
        .map_err(|error| format!("failed to write initialized notification: {error}"))?;
    stdin
        .shutdown()
        .await
        .map_err(|error| format!("failed to close server stdin: {error}"))
}

async fn capture_bounded_line<R>(reader: &mut R) -> std::io::Result<CapturedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let count = reader.read(&mut byte).await?;
        if count == 0 {
            break;
        }
        if bytes.len() == PREFLIGHT_CAPTURE_BYTES {
            return Ok(CapturedOutput {
                bytes,
                truncated: true,
            });
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(CapturedOutput {
        bytes,
        truncated: false,
    })
}

async fn capture_bounded<R>(mut reader: R) -> std::io::Result<CapturedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = PREFLIGHT_CAPTURE_BYTES.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < count;
    }
    Ok(CapturedOutput { bytes, truncated })
}

async fn collect_capture(
    task: JoinHandle<std::io::Result<CapturedOutput>>,
    stream: &str,
) -> Result<CapturedOutput, String> {
    task.await
        .map_err(|error| format!("server {stream} capture task failed: {error}"))?
        .map_err(|error| format!("failed to read server {stream}: {error}"))
}

async fn terminate_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn validate_initialize_response(output: &CapturedOutput) -> Result<(), String> {
    if output.truncated {
        return Err(format!(
            "initialize response exceeded {PREFLIGHT_CAPTURE_BYTES} bytes"
        ));
    }
    let text = std::str::from_utf8(&output.bytes)
        .map_err(|error| format!("initialize response was not UTF-8: {error}"))?;
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(format!(
            "expected one initialize response on stdout, received {} lines",
            lines.len()
        ));
    }
    let response: Value = serde_json::from_str(lines[0])
        .map_err(|error| format!("initialize response was not valid JSON: {error}"))?;
    if let Some(error) = response.get("error") {
        return Err(format!("initialize returned a protocol error: {error}"));
    }
    if response.get("jsonrpc") != Some(&json!("2.0"))
        || response.get("id") != Some(&json!("moraebox-install-preflight"))
    {
        return Err("initialize response did not match the JSON-RPC request id".into());
    }
    if response.pointer("/result/protocolVersion") != Some(&json!(super::PROTOCOL_VERSION)) {
        return Err(format!(
            "initialize response did not confirm protocol version {}",
            super::PROTOCOL_VERSION
        ));
    }
    Ok(())
}

fn render_capture(output: &CapturedOutput) -> String {
    let mut rendered = String::from_utf8_lossy(&output.bytes).trim().to_owned();
    if output.truncated {
        rendered.push_str(" …[truncated]");
    }
    if rendered.is_empty() {
        "<empty>".into()
    } else {
        rendered
    }
}

fn rollback_command(agent: Agent, name: &str) -> String {
    match agent {
        Agent::Codex => format!("codex mcp remove {name}"),
        Agent::ClaudeCode => format!("claude mcp remove --scope user {name}"),
    }
}

fn parse_server_name(input: &str) -> Result<String, String> {
    if input.is_empty()
        || !input
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("server name must use only ASCII letters, numbers, '-' or '_'".into());
    }
    Ok(input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn registration_errors_preserve_stage_and_retryability() {
        let preflight = RegistrationError::Preflight("handshake timed out".into());
        assert_eq!(preflight.stage(), "registration_preflight");
        assert!(preflight.retryable());

        let configuration = RegistrationError::Configuration("invalid path".into());
        assert_eq!(configuration.stage(), "registration_configuration");
        assert!(!configuration.retryable());
    }

    fn install_args(agent: Agent) -> InstallArgs {
        InstallArgs {
            agent,
            name: "moraebox".into(),
            backend: RegistrationBackend::Libkrun,
            rootfs: Some("/rootfs".into()),
            image: None,
            cache_dir: None,
            state_dir: None,
            helper: None,
            libkrun: None,
            gvproxy: None,
            lib_dir: None,
            mke2fs: None,
            e2fsck: None,
            debugfs: None,
            disk_size: 8 * 1024 * 1024 * 1024,
            cpus: 2,
            memory_mib: 512,
            server_command: None,
            dry_run: true,
        }
    }

    fn string_args(plan: &CommandPlan) -> Vec<String> {
        plan.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn builds_codex_user_registration() {
        let cache = resolve_cache_dir(None).unwrap();
        let state = resolve_state_dir(None).unwrap();
        let plan =
            build_command_plan(&install_args(Agent::Codex), "/bin/morae-mcp".into()).unwrap();
        assert_eq!(plan.program, "codex");
        assert_eq!(
            string_args(&plan),
            [
                "mcp",
                "add",
                "moraebox",
                "--env",
                "MORAE_ROOTFS=/rootfs",
                "--",
                "/bin/morae-mcp",
                "--backend",
                "libkrun",
                "--cache-dir",
                cache.to_str().unwrap(),
                "--state-dir",
                state.to_str().unwrap(),
                "--disk-size",
                "8589934592",
                "--cpus",
                "2",
                "--memory-mib",
                "512",
            ]
        );
    }

    #[test]
    fn builds_claude_user_registration() {
        let cache = resolve_cache_dir(None).unwrap();
        let state = resolve_state_dir(None).unwrap();
        let plan =
            build_command_plan(&install_args(Agent::ClaudeCode), "/bin/morae-mcp".into()).unwrap();
        assert_eq!(plan.program, "claude");
        assert_eq!(
            string_args(&plan),
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "--env",
                "MORAE_ROOTFS=/rootfs",
                "--transport",
                "stdio",
                "moraebox",
                "--",
                "/bin/morae-mcp",
                "--backend",
                "libkrun",
                "--cache-dir",
                cache.to_str().unwrap(),
                "--state-dir",
                state.to_str().unwrap(),
                "--disk-size",
                "8589934592",
                "--cpus",
                "2",
                "--memory-mib",
                "512",
            ]
        );
    }

    #[test]
    fn process_registration_is_explicit_and_has_no_native_environment() {
        let mut install = install_args(Agent::Codex);
        install.backend = RegistrationBackend::Process;
        install.rootfs = None;
        let (environment, server_args) = server_configuration(&install).unwrap();
        assert!(environment.is_empty());
        let rendered = string_args_from(&server_args);
        assert_eq!(rendered[0..2], ["--backend", "process"]);
        assert!(rendered.contains(&"--cache-dir".into()));
        assert!(rendered.contains(&"--state-dir".into()));
        assert!(!rendered.contains(&"--cpus".into()));
        assert!(!rendered.contains(&"--memory-mib".into()));
    }

    #[test]
    fn libkrun_registration_uses_python_default_without_a_rootfs() {
        let cache = TestCache::new("default");
        let state = resolve_state_dir(None).unwrap();
        let mut install = install_args(Agent::Codex);
        install.rootfs = None;
        install.cache_dir = Some(cache.path.clone());
        let (environment, server_args) = server_configuration(&install).unwrap();
        assert!(environment.is_empty());
        assert_eq!(
            string_args_from(&server_args),
            [
                "--backend",
                "libkrun",
                "--cache-dir",
                cache.path.to_str().unwrap(),
                "--state-dir",
                state.to_str().unwrap(),
                "--disk-size",
                "8589934592",
                "--cpus",
                "2",
                "--memory-mib",
                "512",
                "--image",
                "docker.io/library/python:3.12",
            ]
        );
    }

    #[test]
    fn explicit_image_is_registered_without_credentials() {
        let cache = TestCache::new("explicit");
        let mut install = install_args(Agent::ClaudeCode);
        install.rootfs = None;
        install.image = Some("debian:bookworm".into());
        install.cache_dir = Some(cache.path.clone());
        let (environment, server_args) = server_configuration(&install).unwrap();
        assert!(environment.is_empty());
        let rendered = string_args_from(&server_args);
        let image = rendered
            .iter()
            .position(|value| value == "--image")
            .unwrap();
        assert_eq!(rendered[image + 1], "debian:bookworm");
    }

    #[test]
    fn explicit_gvproxy_path_is_registered_as_environment() {
        let mut install = install_args(Agent::Codex);
        install.gvproxy = Some("/opt/tools/gvproxy".into());

        let (environment, _) = server_configuration(&install).unwrap();

        assert!(
            environment.contains(&("MORAE_GVPROXY_PATH", OsString::from("/opt/tools/gvproxy")))
        );
    }

    #[test]
    fn discovered_libkrunfw_path_is_registered_as_environment() {
        let install = install_args(Agent::Codex);
        let native_paths = NativeRegistrationPaths {
            libkrunfw: Some("/opt/native/libkrunfw.dylib".into()),
            ..NativeRegistrationPaths::default()
        };

        let (environment, _) = server_configuration_with_paths(&install, &native_paths).unwrap();

        assert!(environment.contains(&(
            "MORAE_LIBKRUNFW_PATH",
            OsString::from("/opt/native/libkrunfw.dylib")
        )));
    }

    #[test]
    fn explicit_relative_storage_paths_are_resolved_from_the_install_directory() {
        let mut install = install_args(Agent::Codex);
        install.cache_dir = Some("custom-cache".into());
        install.state_dir = Some("custom-state".into());

        let (_, server_args) = server_configuration(&install).unwrap();
        let rendered = string_args_from(&server_args);
        let current = env::current_dir().unwrap();

        assert!(rendered.contains(&current.join("custom-cache").to_string_lossy().into_owned()));
        assert!(rendered.contains(&current.join("custom-state").to_string_lossy().into_owned()));
    }

    #[test]
    fn persistent_runtime_paths_are_registered_as_absolute_paths() {
        let mut install = install_args(Agent::Codex);
        install.rootfs = Some("guest-root".into());
        install.helper = Some("tools/helper".into());
        install.libkrun = Some("lib/libkrun.dylib".into());
        install.gvproxy = Some("tools/gvproxy".into());
        install.lib_dir = Some("lib".into());
        install.mke2fs = Some("tools/mke2fs".into());
        install.e2fsck = Some("tools/e2fsck".into());
        install.debugfs = Some("tools/debugfs".into());

        let (environment, _) = server_configuration(&install).unwrap();

        for (name, value) in environment {
            if name == "MORAE_LIB_DIR" {
                assert!(env::split_paths(&value).all(|path| path.is_absolute()));
            } else {
                assert!(Path::new(&value).is_absolute(), "{name} was not absolute");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn server_command_must_be_a_regular_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let server = temporary.path().join("morae-mcp");
        fs::write(&server, b"stub").unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            resolve_server_command(Some(server.as_os_str()))
                .unwrap_err()
                .contains("execute permission")
        );

        fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            resolve_server_command(Some(server.as_os_str())).unwrap(),
            server
        );
    }

    #[test]
    fn process_registration_rejects_guest_root_sources() {
        let mut install = install_args(Agent::Codex);
        install.backend = RegistrationBackend::Process;
        install.rootfs = None;
        install.image = Some("python:3.12".into());
        assert_eq!(
            server_configuration(&install).unwrap_err(),
            "--rootfs and --image require --backend libkrun"
        );

        install.image = None;
        install.rootfs = Some("/rootfs".into());
        assert_eq!(
            server_configuration(&install).unwrap_err(),
            "--rootfs and --image require --backend libkrun"
        );
    }

    #[test]
    fn validates_portable_server_names() {
        assert_eq!(parse_server_name("moraebox_2").unwrap(), "moraebox_2");
        assert!(parse_server_name("morae box").is_err());
    }

    fn string_args_from(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    struct TestCache {
        path: PathBuf,
    }

    impl TestCache {
        fn new(label: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "moraebox-mcp-registration-{label}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestCache {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
