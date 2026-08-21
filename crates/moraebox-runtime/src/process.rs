use std::{io, process::Stdio};

#[cfg(unix)]
use std::sync::Arc;

use async_trait::async_trait;
use moraebox_core::{OutputChannel, RunSpec, Signal};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::{Backend, BackendController, BackendError, RunBudget, RunStage, SpawnedSandbox};

#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessBackend;

#[async_trait]
impl Backend for ProcessBackend {
    fn name(&self) -> &'static str {
        "process"
    }

    async fn spawn(
        &self,
        spec: &RunSpec,
        budget: &RunBudget,
    ) -> Result<SpawnedSandbox, BackendError> {
        spec.validate().map_err(BackendError::InvalidSpec)?;
        if spec.box_id.is_some() {
            return Err(BackendError::Unsupported(
                "Box persistence on the process backend; it does not provide VM isolation",
            ));
        }
        if spec.tty {
            return Err(BackendError::Unsupported(
                "PTY on the deterministic process backend",
            ));
        }
        if spec.network {
            return Err(BackendError::Unsupported(
                "network opt-in on the process backend; it already uses the host network without VM isolation",
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

        let mut child = budget
            .run_sync(RunStage::ProcessSpawn, || command.spawn())
            .map_err(BackendError::from)?;
        let pid = child.id().ok_or(BackendError::MissingProcessId)?;
        let stdin = child.stdin.take().map(|writer| Box::pin(writer) as _);
        let stdout = child
            .stdout
            .take()
            .map(|reader| Box::pin(reader) as _)
            .ok_or_else(|| BackendError::Control("stdout pipe was not created".into()))?;
        let stderr = child.stderr.take().map(|reader| Box::pin(reader) as _);
        let (exit, controller) = controlled_process(child, pid);

        Ok(SpawnedSandbox {
            stdin,
            stdout,
            stdout_channel: OutputChannel::Stdout,
            stderr,
            exit,
            controller,
            startup: crate::StartupMetrics::default(),
        })
    }
}

#[derive(Debug)]
struct ProcessController {
    #[cfg(unix)]
    pid: Arc<u32>,
    commands: mpsc::Sender<ProcessCommand>,
}

#[derive(Debug)]
enum ProcessCommand {
    Stop(Option<oneshot::Sender<io::Result<()>>>),
}

fn controlled_process(
    mut child: tokio::process::Child,
    pid: u32,
) -> (crate::backend::ExitFuture, Box<ProcessController>) {
    let (command_sender, mut command_receiver) = mpsc::channel(4);
    let process_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                status = child.wait() => return status,
                command = command_receiver.recv() => {
                    let Some(ProcessCommand::Stop(reply)) = command else {
                        let _ = child.start_kill();
                        return child.wait().await;
                    };
                    match child.start_kill() {
                        Ok(()) => {
                            if let Some(reply) = reply {
                                let _ = reply.send(Ok(()));
                            }
                        }
                        Err(error) => {
                            let reply_error = io::Error::new(error.kind(), error.to_string());
                            if let Some(reply) = reply {
                                let _ = reply.send(Err(reply_error));
                            }
                            return Err(error);
                        }
                    }
                }
            }
        }
    });
    let exit = Box::pin(async move {
        process_task
            .await
            .map_err(|error| io::Error::other(format!("process task failed: {error}")))?
    });
    let controller = Box::new(ProcessController {
        #[cfg(unix)]
        pid: Arc::new(pid),
        commands: command_sender,
    });
    #[cfg(not(unix))]
    let _ = pid;
    (exit, controller)
}

impl Drop for ProcessController {
    fn drop(&mut self) {
        let _ = self.commands.try_send(ProcessCommand::Stop(None));
    }
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
            match signal {
                Signal::Terminate | Signal::Kill => {
                    let (reply_sender, reply_receiver) = oneshot::channel();
                    if self
                        .commands
                        .send(ProcessCommand::Stop(Some(reply_sender)))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    match reply_receiver.await {
                        Ok(result) => result.map_err(BackendError::Io),
                        Err(_) => Ok(()),
                    }
                }
                Signal::Interrupt | Signal::Hangup => Err(BackendError::Unsupported(
                    "interrupt and hangup process signals on this platform",
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_vm_network_opt_in() {
        let mut spec = RunSpec::command(["true"]);
        spec.network = true;

        assert!(matches!(
            ProcessBackend
                .spawn(&spec, &RunBudget::new(spec.timeout))
                .await,
            Err(BackendError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn rejects_box_persistence() {
        let mut spec = RunSpec::command(["true"]);
        spec.box_id = Some(moraebox_core::BoxId::new());

        assert!(matches!(
            ProcessBackend
                .spawn(&spec, &RunBudget::new(spec.timeout))
                .await,
            Err(BackendError::Unsupported(message)) if message.contains("Box persistence")
        ));
    }
}
