use std::{
    process::ExitStatus,
    sync::Arc,
    time::{Duration, Instant},
};

use moraebox_core::{
    OutputBuffer, OutputChannel, OutputRead, OutputReadError, RunSpec, SessionId, SessionState,
    Signal, TerminationReason,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, mpsc, oneshot, watch},
    time::sleep,
};

use crate::{Backend, BackendError};

#[derive(Clone)]
pub struct SessionManager {
    backend: Arc<dyn Backend>,
}

impl SessionManager {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    pub async fn start(&self, spec: RunSpec) -> Result<SessionHandle, SessionError> {
        spec.validate().map_err(BackendError::InvalidSpec)?;
        let started = Instant::now();
        let spawned = self.backend.spawn(&spec).await?;
        let initial = SessionStatus {
            session_id: spec.session_id,
            backend: self.backend.name().into(),
            state: SessionState::Running,
            termination_reason: None,
            exit_code: None,
            signal: None,
            timed_out: false,
            elapsed_micros: 0,
        };
        let (status_sender, status_receiver) = watch::channel(initial);
        let output = Arc::new(Mutex::new(OutputBuffer::new(spec.output_limit)));
        let (command_sender, command_receiver) = mpsc::channel(32);
        let stdout_task = tokio::spawn(pump_output(
            spawned.stdout,
            spawned.stdout_channel,
            Arc::clone(&output),
        ));
        let stderr_task = spawned.stderr.map(|stderr| {
            tokio::spawn(pump_output(
                stderr,
                OutputChannel::Stderr,
                Arc::clone(&output),
            ))
        });

        let session_id = spec.session_id;
        tokio::spawn(drive_session(
            spec,
            started,
            spawned.stdin,
            spawned.exit,
            spawned.controller,
            command_receiver,
            status_sender,
            stdout_task,
            stderr_task,
        ));
        Ok(SessionHandle {
            session_id,
            output,
            status: status_receiver,
            commands: command_sender,
        })
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    session_id: SessionId,
    output: Arc<Mutex<OutputBuffer>>,
    status: watch::Receiver<SessionStatus>,
    commands: mpsc::Sender<SessionCommand>,
}

impl SessionHandle {
    pub fn id(&self) -> SessionId {
        self.session_id
    }

    pub fn status(&self) -> SessionStatus {
        self.status.borrow().clone()
    }

    pub async fn read_output(
        &self,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<OutputRead, SessionError> {
        Ok(self.output.lock().await.read(cursor, max_bytes)?)
    }

    pub async fn write(&self, bytes: impl Into<Vec<u8>>) -> Result<(), SessionError> {
        self.request(|reply| SessionCommand::Write(bytes.into(), reply))
            .await
    }

    pub async fn close_stdin(&self) -> Result<(), SessionError> {
        self.request(SessionCommand::CloseStdin).await
    }

    pub async fn signal(&self, signal: Signal) -> Result<(), SessionError> {
        self.request(|reply| SessionCommand::Signal(signal, reply))
            .await
    }

    pub async fn resize(&self, rows: u16, columns: u16) -> Result<(), SessionError> {
        self.request(|reply| SessionCommand::Resize(rows, columns, reply))
            .await
    }

    pub async fn stop(&self) -> Result<(), SessionError> {
        self.request(SessionCommand::Stop).await
    }

    pub async fn wait(&self) -> Result<SessionStatus, SessionError> {
        let mut receiver = self.status.clone();
        loop {
            let current = receiver.borrow().clone();
            if current.state == SessionState::Dead {
                return Ok(current);
            }
            receiver
                .changed()
                .await
                .map_err(|_| SessionError::SessionClosed)?;
        }
    }

    async fn request(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<(), String>>) -> SessionCommand,
    ) -> Result<(), SessionError> {
        let (sender, receiver) = oneshot::channel();
        self.commands
            .send(build(sender))
            .await
            .map_err(|_| SessionError::SessionClosed)?;
        receiver
            .await
            .map_err(|_| SessionError::SessionClosed)?
            .map_err(SessionError::Control)
    }
}

enum SessionCommand {
    Write(Vec<u8>, oneshot::Sender<Result<(), String>>),
    CloseStdin(oneshot::Sender<Result<(), String>>),
    Signal(Signal, oneshot::Sender<Result<(), String>>),
    Resize(u16, u16, oneshot::Sender<Result<(), String>>),
    Stop(oneshot::Sender<Result<(), String>>),
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn drive_session(
    spec: RunSpec,
    started: Instant,
    mut stdin: Option<crate::BoxedWriter>,
    mut exit: crate::backend::ExitFuture,
    controller: Box<dyn crate::BackendController>,
    mut commands: mpsc::Receiver<SessionCommand>,
    status_sender: watch::Sender<SessionStatus>,
    stdout_task: tokio::task::JoinHandle<std::io::Result<()>>,
    stderr_task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
) {
    if !spec.stdin.is_empty()
        && let Some(writer) = stdin.as_mut()
        && let Err(error) = writer.write_all(&spec.stdin).await
    {
        publish_failure(&status_sender, &spec, started, error.to_string());
        return;
    }
    let mut deadline = spec
        .timeout
        .duration()
        .map(|duration| Box::pin(sleep(duration)));
    let mut kill_deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut reason = TerminationReason::Exited;
    let mut timed_out = false;

    let exit_status = loop {
        tokio::select! {
            result = &mut exit => match result {
                Ok(status) => break Some(status),
                Err(error) => {
                    publish_failure(&status_sender, &spec, started, error.to_string());
                    break None;
                }
            },
            command = commands.recv() => {
                let Some(command) = command else { continue };
                match command {
                    SessionCommand::Write(bytes, reply) => {
                        let result = if let Some(writer) = stdin.as_mut() {
                            writer.write_all(&bytes).await.map_err(|error| error.to_string())
                        } else {
                            Err("stdin is closed".into())
                        };
                        let _ = reply.send(result);
                    }
                    SessionCommand::CloseStdin(reply) => {
                        let result = if let Some(mut writer) = stdin.take() {
                            writer.shutdown().await.map_err(|error| error.to_string())
                        } else {
                            Ok(())
                        };
                        let _ = reply.send(result);
                    }
                    SessionCommand::Signal(signal, reply) => {
                        let result = controller.signal(signal).await.map_err(|error| error.to_string());
                        let _ = reply.send(result);
                    }
                    SessionCommand::Resize(rows, columns, reply) => {
                        let result = controller.resize(rows, columns).await.map_err(|error| error.to_string());
                        let _ = reply.send(result);
                    }
                    SessionCommand::Stop(reply) => {
                        reason = TerminationReason::Cancelled;
                        publish_running_state(&status_sender, &spec, started, SessionState::Stopping, reason, false);
                        let result = controller.signal(Signal::Terminate).await.map_err(|error| error.to_string());
                        if result.is_ok() {
                            kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                            deadline = None;
                        }
                        let _ = reply.send(result);
                    }
                }
            }
            () = wait_timer(&mut deadline), if deadline.is_some() => {
                timed_out = true;
                reason = TerminationReason::TimedOut;
                publish_running_state(&status_sender, &spec, started, SessionState::TimedOut, reason, true);
                let _ = controller.signal(Signal::Terminate).await;
                kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                deadline = None;
            }
            () = wait_timer(&mut kill_deadline), if kill_deadline.is_some() => {
                let _ = controller.force_stop().await;
                kill_deadline = None;
            }
        }
    };
    drop(stdin);
    let _ = stdout_task.await;
    if let Some(task) = stderr_task {
        let _ = task.await;
    }
    if let Some(status) = exit_status {
        let (exit_code, signal) = decode_exit_status(status);
        let backend = status_sender.borrow().backend.clone();
        let _ = status_sender.send(SessionStatus {
            session_id: spec.session_id,
            backend,
            state: SessionState::Dead,
            termination_reason: Some(reason),
            exit_code,
            signal,
            timed_out,
            elapsed_micros: duration_micros(started.elapsed()),
        });
    }
}

async fn wait_timer(timer: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>) {
    if let Some(timer) = timer {
        timer.as_mut().await;
    }
}

fn publish_running_state(
    sender: &watch::Sender<SessionStatus>,
    spec: &RunSpec,
    started: Instant,
    state: SessionState,
    reason: TerminationReason,
    timed_out: bool,
) {
    let backend = sender.borrow().backend.clone();
    let _ = sender.send(SessionStatus {
        session_id: spec.session_id,
        backend,
        state,
        termination_reason: Some(reason),
        exit_code: None,
        signal: None,
        timed_out,
        elapsed_micros: duration_micros(started.elapsed()),
    });
}

fn publish_failure(
    sender: &watch::Sender<SessionStatus>,
    spec: &RunSpec,
    started: Instant,
    _error: String,
) {
    let backend = sender.borrow().backend.clone();
    let _ = sender.send(SessionStatus {
        session_id: spec.session_id,
        backend,
        state: SessionState::Dead,
        termination_reason: Some(TerminationReason::Failed),
        exit_code: None,
        signal: None,
        timed_out: false,
        elapsed_micros: duration_micros(started.elapsed()),
    });
}

async fn pump_output<R>(
    mut reader: R,
    channel: OutputChannel,
    output: Arc<Mutex<OutputBuffer>>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        output.lock().await.push(channel, &buffer[..count]);
    }
}

#[cfg(unix)]
fn decode_exit_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    use std::os::unix::process::ExitStatusExt;
    (status.code(), status.signal())
}

#[cfg(not(unix))]
fn decode_exit_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), None)
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: SessionId,
    pub backend: String,
    pub state: SessionState,
    pub termination_reason: Option<TerminationReason>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub elapsed_micros: u64,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Output(#[from] OutputReadError),
    #[error("session is no longer available")]
    SessionClosed,
    #[error("session control failed: {0}")]
    Control(String),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moraebox_core::RunSpec;

    use super::*;
    use crate::ProcessBackend;

    #[tokio::test]
    async fn supports_incremental_stdin_and_cursor_output() {
        let manager = SessionManager::new(Arc::new(ProcessBackend));
        let session = manager
            .start(RunSpec::command(stdin_echo_command()))
            .await
            .unwrap();
        session.write(b"hello\n".to_vec()).await.unwrap();
        session.close_stdin().await.unwrap();
        let status = session.wait().await.unwrap();
        assert_eq!(status.exit_code, Some(0));
        let output = session.read_output(0, 1024).await.unwrap();
        assert!(output.chunks.iter().any(|chunk| {
            chunk.channel == OutputChannel::Stdout && chunk.data.starts_with(b"hello")
        }));
    }

    #[tokio::test]
    async fn stop_terminates_a_long_running_session() {
        let manager = SessionManager::new(Arc::new(ProcessBackend));
        let session = manager
            .start(RunSpec::command(long_running_command()))
            .await
            .unwrap();
        session.stop().await.unwrap();
        let status = session.wait().await.unwrap();
        assert_eq!(
            status.termination_reason,
            Some(TerminationReason::Cancelled)
        );
    }

    #[cfg(unix)]
    fn stdin_echo_command() -> Vec<String> {
        ["/bin/sh", "-c", "read line; printf '%s' \"$line\""]
            .map(String::from)
            .into()
    }

    #[cfg(windows)]
    fn stdin_echo_command() -> Vec<String> {
        vec![
            windows_system_executable("findstr.exe"),
            "/R".into(),
            ".*".into(),
        ]
    }

    #[cfg(unix)]
    fn long_running_command() -> Vec<String> {
        ["/bin/sh", "-c", "sleep 30"].map(String::from).into()
    }

    #[cfg(windows)]
    fn long_running_command() -> Vec<String> {
        vec![
            windows_system_executable("ping.exe"),
            "-n".into(),
            "31".into(),
            "127.0.0.1".into(),
        ]
    }

    #[cfg(windows)]
    fn windows_system_executable(name: &str) -> String {
        std::path::PathBuf::from(
            std::env::var_os("SystemRoot").expect("Windows must define SystemRoot"),
        )
        .join("System32")
        .join(name)
        .to_string_lossy()
        .into_owned()
    }
}
