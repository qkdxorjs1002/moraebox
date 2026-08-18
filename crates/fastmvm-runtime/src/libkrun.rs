use std::{path::PathBuf, process::Stdio, sync::Arc};

use async_trait::async_trait;
use fastmvm_core::{OutputChannel, RunSpec, Signal};
use tokio::process::Command;

use crate::{Backend, BackendController, BackendError, SpawnedSandbox};

#[derive(Debug, Clone)]
pub struct LibkrunConfig {
    pub helper_path: PathBuf,
    pub library_path: PathBuf,
    pub root_path: PathBuf,
    pub library_search_path: Option<PathBuf>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub workspace_disk: Option<PathBuf>,
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
            spawn_pty(command, spec)
        } else {
            spawn_piped(command)
        }
    }
}

fn spawn_piped(mut command: Command) -> Result<SpawnedSandbox, BackendError> {
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
    let exit = Box::pin(async move { child.wait().await });
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
fn spawn_pty(mut command: Command, spec: &RunSpec) -> Result<SpawnedSandbox, BackendError> {
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

    let mut child = command.spawn()?;
    let pid = child.id().ok_or(BackendError::MissingProcessId)?;
    let exit = Box::pin(async move { child.wait().await });
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
fn spawn_pty(_command: Command, _spec: &RunSpec) -> Result<SpawnedSandbox, BackendError> {
    Err(BackendError::Unsupported("PTY on this platform"))
}

#[derive(Debug)]
struct LibkrunController {
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

    #[test]
    fn rejects_missing_native_paths() {
        let config = LibkrunConfig::new("missing-helper", "missing-lib", "missing-root");
        assert!(config.validate().is_err());
    }
}
