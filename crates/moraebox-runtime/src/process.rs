use std::{process::Stdio, sync::Arc};

use async_trait::async_trait;
use moraebox_core::{OutputChannel, RunSpec, Signal};
use tokio::process::Command;

use crate::{Backend, BackendController, BackendError, SpawnedSandbox};

#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessBackend;

#[async_trait]
impl Backend for ProcessBackend {
    fn name(&self) -> &'static str {
        "process"
    }

    async fn spawn(&self, spec: &RunSpec) -> Result<SpawnedSandbox, BackendError> {
        spec.validate().map_err(BackendError::InvalidSpec)?;
        if spec.tty {
            return Err(BackendError::Unsupported(
                "PTY on the deterministic process backend",
            ));
        }

        let mut command = Command::new(&spec.argv[0]);
        command.args(&spec.argv[1..]);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);
        if !spec.inherit_env {
            command.env_clear();
        }
        command.envs(&spec.env);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let mut child = command.spawn()?;
        let pid = child.id().ok_or(BackendError::MissingProcessId)?;
        let stdin = child.stdin.take().map(|writer| Box::pin(writer) as _);
        let stdout = child
            .stdout
            .take()
            .map(|reader| Box::pin(reader) as _)
            .ok_or_else(|| BackendError::Control("stdout pipe was not created".into()))?;
        let stderr = child.stderr.take().map(|reader| Box::pin(reader) as _);
        let exit = Box::pin(async move { child.wait().await });
        let controller = Box::new(ProcessController { pid: Arc::new(pid) });

        Ok(SpawnedSandbox {
            stdin,
            stdout,
            stdout_channel: OutputChannel::Stdout,
            stderr,
            exit,
            controller,
        })
    }
}

#[derive(Debug)]
struct ProcessController {
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "process signals are unsupported on this platform")
    )]
    pid: Arc<u32>,
}

#[async_trait]
impl BackendController for ProcessController {
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
                "process signals on this platform",
            ))
        }
    }
}
