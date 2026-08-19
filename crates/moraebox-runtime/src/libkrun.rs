use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use moraebox_core::{OutputChannel, RunSpec, Signal};
use tempfile::TempDir;
use tokio::{
    process::{Child, Command},
    time::{Instant, sleep},
};

use crate::{Backend, BackendController, BackendError, SpawnedSandbox};

const NETWORK_PROXY_START_TIMEOUT: Duration = Duration::from_secs(5);
const NETWORK_PROXY_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub struct LibkrunConfig {
    pub helper_path: PathBuf,
    pub library_path: PathBuf,
    pub root_path: PathBuf,
    pub library_search_path: Option<PathBuf>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub workspace_disk: Option<PathBuf>,
    pub gvproxy_path: Option<PathBuf>,
    pub network_runtime_dir: PathBuf,
}

impl LibkrunConfig {
    pub fn new(
        helper_path: impl Into<PathBuf>,
        library_path: impl Into<PathBuf>,
        root_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            helper_path: helper_path.into(),
            library_path: library_path.into(),
            root_path: root_path.into(),
            library_search_path: None,
            vcpus: 2,
            memory_mib: 512,
            workspace_disk: None,
            gvproxy_path: None,
            network_runtime_dir: PathBuf::from(".moraebox/network"),
        }
    }

    fn validate(&self) -> Result<(), BackendError> {
        if self.vcpus == 0 {
            return Err(BackendError::InvalidSpec("vCPU count must be non-zero"));
        }
        if self.memory_mib == 0 {
            return Err(BackendError::InvalidSpec("memory must be non-zero"));
        }
        for (name, path) in [
            ("VMM helper", &self.helper_path),
            ("libkrun", &self.library_path),
            ("root filesystem", &self.root_path),
        ] {
            if !path.exists() {
                return Err(BackendError::Control(format!(
                    "{name} does not exist: {}",
                    path.display()
                )));
            }
        }
        if let Some(path) = &self.workspace_disk
            && !path.is_file()
        {
            return Err(BackendError::Control(format!(
                "workspace disk does not exist: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn network_proxy_path(&self) -> Result<&Path, BackendError> {
        let path = self.gvproxy_path.as_deref().ok_or_else(|| {
            BackendError::Control(
                "network access requires --gvproxy, MORAE_GVPROXY_PATH, or gvproxy on PATH".into(),
            )
        })?;
        if !path.is_file() {
            return Err(BackendError::Control(format!(
                "gvproxy does not exist: {}",
                path.display()
            )));
        }
        Ok(path)
    }
}

#[derive(Debug, Clone)]
pub struct LibkrunBackend {
    config: LibkrunConfig,
}

impl LibkrunBackend {
    pub fn new(config: LibkrunConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Backend for LibkrunBackend {
    fn name(&self) -> &'static str {
        "libkrun"
    }

    async fn spawn(&self, spec: &RunSpec) -> Result<SpawnedSandbox, BackendError> {
        spec.validate().map_err(BackendError::InvalidSpec)?;
        self.config.validate()?;

        let network_proxy = if spec.network {
            Some(NetworkProxy::start(&self.config).await?)
        } else {
            None
        };

        let mut command = Command::new(&self.config.helper_path);
        command
            .arg("--libkrun")
            .arg(&self.config.library_path)
            .arg("--root")
            .arg(&self.config.root_path)
            .arg("--cpus")
            .arg(self.config.vcpus.to_string())
            .arg("--memory-mib")
            .arg(self.config.memory_mib.to_string())
            .arg("--parent-pid")
            .arg(std::process::id().to_string());
        if let Some(cwd) = &spec.cwd {
            command.arg("--cwd").arg(cwd);
        }
        if let Some(workspace) = &self.config.workspace_disk {
            command.arg("--workspace-disk").arg(workspace);
        }
        if let Some(proxy) = &network_proxy {
            command.arg("--network-socket").arg(&proxy.socket_path);
        }
        for (key, value) in &spec.env {
            command.arg("--env").arg(format!("{key}={value}"));
        }
        command.arg("--").args(&spec.argv);
        command.env_clear();
        if let Some(path) = &self.config.library_search_path {
            #[cfg(target_os = "macos")]
            command.env("DYLD_LIBRARY_PATH", path);
            #[cfg(not(target_os = "macos"))]
            command.env("LD_LIBRARY_PATH", path);
        }
        command.kill_on_drop(true);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        if spec.tty {
            spawn_pty(command, spec, network_proxy)
        } else {
            spawn_piped(command, network_proxy)
        }
    }
}

fn spawn_piped(
    mut command: Command,
    network_proxy: Option<NetworkProxy>,
) -> Result<SpawnedSandbox, BackendError> {
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let pid = child.id().ok_or(BackendError::MissingProcessId)?;
    let stdin = child.stdin.take().map(|writer| Box::pin(writer) as _);
    let stdout = child
        .stdout
        .take()
        .map(|reader| Box::pin(reader) as _)
        .ok_or_else(|| BackendError::Control("helper stdout pipe was not created".into()))?;
    let stderr = child.stderr.take().map(|reader| Box::pin(reader) as _);
    let exit = managed_exit(child, network_proxy);
    Ok(SpawnedSandbox {
        stdin,
        stdout,
        stdout_channel: OutputChannel::Stdout,
        stderr,
        exit,
        controller: Box::new(LibkrunController { pid: Arc::new(pid) }),
    })
}

#[cfg(unix)]
fn spawn_pty(
    mut command: Command,
    spec: &RunSpec,
    network_proxy: Option<NetworkProxy>,
) -> Result<SpawnedSandbox, BackendError> {
    use std::fs::File;

    use nix::pty::{Winsize, openpty};

    let window = Winsize {
        ws_row: spec.tty_rows,
        ws_col: spec.tty_columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(&window, None).map_err(std::io::Error::from)?;
    let master = File::from(pty.master);
    let master_reader = master.try_clone()?;
    let slave = File::from(pty.slave);
    command.stdin(Stdio::from(slave.try_clone()?));
    command.stdout(Stdio::from(slave.try_clone()?));
    command.stderr(Stdio::from(slave));

    let child = command.spawn()?;
    let pid = child.id().ok_or(BackendError::MissingProcessId)?;
    let exit = managed_exit(child, network_proxy);
    Ok(SpawnedSandbox {
        stdin: Some(Box::pin(tokio::fs::File::from_std(master))),
        stdout: Box::pin(tokio::fs::File::from_std(master_reader)),
        stdout_channel: OutputChannel::Tty,
        stderr: None,
        exit,
        controller: Box::new(LibkrunController { pid: Arc::new(pid) }),
    })
}

#[cfg(not(unix))]
fn spawn_pty(
    _command: Command,
    _spec: &RunSpec,
    _network_proxy: Option<NetworkProxy>,
) -> Result<SpawnedSandbox, BackendError> {
    Err(BackendError::Unsupported("PTY on this platform"))
}

fn managed_exit(
    mut child: Child,
    network_proxy: Option<NetworkProxy>,
) -> crate::backend::ExitFuture {
    Box::pin(async move {
        let status = child.wait().await;
        if let Some(proxy) = network_proxy {
            let cleanup = proxy.stop().await;
            if status.is_ok() {
                cleanup?;
            }
        }
        status
    })
}

#[derive(Debug)]
struct NetworkProxy {
    child: Child,
    state: TempDir,
    socket_path: PathBuf,
}

impl NetworkProxy {
    async fn start(config: &LibkrunConfig) -> Result<Self, BackendError> {
        let executable = config.network_proxy_path()?;
        std::fs::create_dir_all(&config.network_runtime_dir)?;
        let state = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(&config.network_runtime_dir)?;
        let socket_path = state.path().join("gvproxy.sock");
        let socket = socket_path.to_str().ok_or_else(|| {
            BackendError::Control("gvproxy socket path must be valid UTF-8".into())
        })?;

        let mut command = Command::new(executable);
        command
            .arg("--listen-vfkit")
            .arg(format!("unixgram://{socket}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear()
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|error| BackendError::Control(format!("failed to start gvproxy: {error}")))?;
        let started = Instant::now();
        loop {
            if socket_path.exists() {
                break;
            }
            if let Some(status) = child.try_wait()? {
                return Err(BackendError::Control(format!(
                    "gvproxy exited before creating its vfkit socket: {status}"
                )));
            }
            if started.elapsed() >= NETWORK_PROXY_START_TIMEOUT {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(BackendError::Control(
                    "gvproxy did not create its vfkit socket within 5 seconds".into(),
                ));
            }
            sleep(NETWORK_PROXY_POLL_INTERVAL).await;
        }

        Ok(Self {
            child,
            state,
            socket_path,
        })
    }

    async fn stop(mut self) -> io::Result<()> {
        if self.child.try_wait()?.is_none()
            && let Err(error) = self.child.start_kill()
            && self.child.try_wait()?.is_none()
        {
            return Err(error);
        }
        let _ = self.child.wait().await?;
        self.state.close()
    }
}

#[derive(Debug)]
struct LibkrunController {
    #[cfg_attr(
        not(unix),
        expect(
            dead_code,
            reason = "libkrun helper signals are unsupported on this platform"
        )
    )]
    pid: Arc<u32>,
}

#[async_trait]
impl BackendController for LibkrunController {
    async fn signal(&self, signal: Signal) -> Result<(), BackendError> {
        #[cfg(unix)]
        {
            use nix::{
                errno::Errno,
                sys::signal::{Signal as NixSignal, kill},
                unistd::Pid,
            };

            let raw_pid = i32::try_from(*self.pid)
                .map_err(|error| BackendError::Control(error.to_string()))?;
            let signal = match signal {
                Signal::Interrupt => NixSignal::SIGINT,
                Signal::Terminate => NixSignal::SIGTERM,
                Signal::Kill => NixSignal::SIGKILL,
                Signal::Hangup => NixSignal::SIGHUP,
            };
            match kill(Pid::from_raw(-raw_pid), signal) {
                Ok(()) | Err(Errno::ESRCH) => Ok(()),
                Err(error) => Err(BackendError::Control(error.to_string())),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            Err(BackendError::Unsupported(
                "libkrun helper signals on this platform",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn rejects_missing_native_paths() {
        let config = LibkrunConfig::new("missing-helper", "missing-lib", "missing-root");
        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn network_requires_gvproxy() {
        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        std::fs::write(&helper, []).unwrap();
        std::fs::write(&library, []).unwrap();
        std::fs::create_dir(&root).unwrap();
        let config = LibkrunConfig::new(helper, library, root);
        let mut spec = RunSpec::command(["true"]);
        spec.network = true;

        let error = LibkrunBackend::new(config).spawn(&spec).await;
        assert!(
            matches!(error, Err(BackendError::Control(message)) if message.contains("gvproxy"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn network_off_does_not_start_or_require_a_proxy() {
        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join(".moraebox/network");
        write_executable(&helper, "#!/bin/sh\nprintf 'network-off\\n'\n");
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.network_runtime_dir = network_runtime_dir.clone();
        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(RunSpec::command(["/usr/bin/true"]))
            .await
            .unwrap();

        assert_eq!(report.exit_code, Some(0));
        assert!(!network_runtime_dir.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn network_proxy_is_passed_to_the_helper_and_reaped() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let proxy_pid = state.path().join("gvproxy.pid");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join(".moraebox/network");
        write_executable(&helper, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nsocket=${2#unixgram://}\n: > \"$socket\"\nwhile :; do :; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy);
        config.network_runtime_dir = network_runtime_dir.clone();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;

        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(spec)
            .await
            .unwrap();
        let output = report
            .output
            .iter()
            .flat_map(|chunk| chunk.data.iter().copied())
            .collect::<Vec<_>>();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("--network-socket"));

        let pid = fs::read_to_string(proxy_pid)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH));
        assert_eq!(fs::read_dir(network_runtime_dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_reaps_the_network_proxy() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let proxy_pid = state.path().join("gvproxy.pid");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join(".moraebox/network");
        write_executable(&helper, "#!/bin/sh\nwhile :; do :; done\n");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nsocket=${2#unixgram://}\n: > \"$socket\"\nwhile :; do :; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy);
        config.network_runtime_dir = network_runtime_dir.clone();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;
        spec.timeout = moraebox_core::TimeoutPolicy::Limited(20);
        spec.kill_grace = Duration::from_millis(20);

        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(spec)
            .await
            .unwrap();

        assert!(report.timed_out);
        let pid = fs::read_to_string(proxy_pid)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH));
        assert_eq!(fs::read_dir(network_runtime_dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_spawned_sandbox_reaps_the_network_proxy() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let proxy_pid = state.path().join("gvproxy.pid");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join(".moraebox/network");
        write_executable(&helper, "#!/bin/sh\nwhile :; do :; done\n");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nsocket=${2#unixgram://}\n: > \"$socket\"\nwhile :; do :; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy);
        config.network_runtime_dir = network_runtime_dir.clone();
        let backend = LibkrunBackend::new(config);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;

        let spawned = backend.spawn(&spec).await.unwrap();
        let pid = fs::read_to_string(proxy_pid)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        drop(spawned);

        let pid = Pid::from_raw(pid);
        for _ in 0..100 {
            if kill(pid, None) == Err(Errno::ESRCH) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(kill(pid, None), Err(Errno::ESRCH));
        assert_eq!(fs::read_dir(network_runtime_dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }
}
