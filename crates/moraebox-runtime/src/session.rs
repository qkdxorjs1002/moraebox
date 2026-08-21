use std::{
    future::Future,
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
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::{sleep, timeout},
};

use crate::{Backend, BackendError};

const STDIN_QUEUE_ITEMS: usize = 32;
const STDIN_QUEUE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct SessionManager {
    backend: Arc<dyn Backend>,
}

impl SessionManager {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    pub async fn start(&self, mut spec: RunSpec) -> Result<SessionHandle, SessionError> {
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
        let (output_cursor_sender, output_cursor_receiver) = watch::channel(0_u64);
        let (command_sender, command_receiver) = mpsc::channel(32);
        let (stdin_sender, stdin_receiver) = mpsc::channel(STDIN_QUEUE_ITEMS);
        let stdin_bytes = Arc::new(Semaphore::new(STDIN_QUEUE_BYTES));
        let (stdin_shutdown_sender, stdin_shutdown_receiver) = watch::channel(false);
        let initial_stdin = std::mem::take(&mut spec.stdin);
        let stdin_task = tokio::spawn(pump_stdin(
            spawned.stdin,
            initial_stdin,
            stdin_receiver,
            stdin_shutdown_receiver,
            Arc::clone(&stdin_bytes),
        ));
        let stdout_task = tokio::spawn(pump_output(
            spawned.stdout,
            spawned.stdout_channel,
            Arc::clone(&output),
            output_cursor_sender.clone(),
        ));
        let stderr_task = spawned.stderr.map(|stderr| {
            tokio::spawn(pump_output(
                stderr,
                OutputChannel::Stderr,
                Arc::clone(&output),
                output_cursor_sender.clone(),
            ))
        });
        drop(output_cursor_sender);

        let session_id = spec.session_id;
        tokio::spawn(drive_session(
            spec,
            started,
            spawned.exit,
            spawned.controller,
            command_receiver,
            stdin_shutdown_sender,
            stdin_task,
            status_sender,
            stdout_task,
            stderr_task,
        ));
        Ok(SessionHandle {
            session_id,
            output,
            output_cursor: output_cursor_receiver,
            status: status_receiver,
            commands: command_sender,
            stdin: stdin_sender,
            stdin_bytes,
        })
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    session_id: SessionId,
    output: Arc<Mutex<OutputBuffer>>,
    output_cursor: watch::Receiver<u64>,
    status: watch::Receiver<SessionStatus>,
    commands: mpsc::Sender<SessionCommand>,
    stdin: mpsc::Sender<StdinRequest>,
    stdin_bytes: Arc<Semaphore>,
}

impl SessionHandle {
    pub fn id(&self) -> SessionId {
        self.session_id
    }

    pub fn status(&self) -> SessionStatus {
        self.status.borrow().clone()
    }

    /// Returns a completion future that does not retain command or I/O ownership.
    pub fn completion(
        &self,
    ) -> impl Future<Output = Result<SessionStatus, SessionError>> + Send + 'static {
        let mut receiver = self.status.clone();
        async move {
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
    }

    pub async fn read_output(
        &self,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<OutputRead, SessionError> {
        Ok(self.output.lock().await.read(cursor, max_bytes)?)
    }

    pub async fn wait_for_output(&self, cursor: u64) -> Result<u64, SessionError> {
        let mut receiver = self.output_cursor.clone();
        loop {
            let next_cursor = *receiver.borrow();
            if next_cursor > cursor {
                return Ok(next_cursor);
            }
            if receiver.changed().await.is_err() {
                self.wait().await?;
                return Ok(cursor);
            }
        }
    }

    pub async fn write(&self, bytes: impl Into<Vec<u8>>) -> Result<(), SessionError> {
        let bytes = bytes.into();
        if bytes.len() > STDIN_QUEUE_BYTES {
            return Err(SessionError::StdinWriteTooLarge {
                requested: bytes.len(),
                maximum: STDIN_QUEUE_BYTES,
            });
        }
        let permits = u32::try_from(bytes.len()).map_err(|_| SessionError::StdinWriteTooLarge {
            requested: bytes.len(),
            maximum: STDIN_QUEUE_BYTES,
        })?;
        let permit = Arc::clone(&self.stdin_bytes)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| SessionError::SessionClosed)?;
        let (reply, receiver) = oneshot::channel();
        self.stdin
            .send(StdinRequest::Write {
                bytes,
                permit,
                reply,
            })
            .await
            .map_err(|_| SessionError::SessionClosed)?;
        receive_reply(receiver).await
    }

    pub async fn close_stdin(&self) -> Result<(), SessionError> {
        let (reply, receiver) = oneshot::channel();
        self.stdin
            .send(StdinRequest::Close(reply))
            .await
            .map_err(|_| SessionError::SessionClosed)?;
        receive_reply(receiver).await
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
        if matches!(
            self.status().state,
            SessionState::Stopping
                | SessionState::Failed
                | SessionState::TimedOut
                | SessionState::Dead
        ) {
            return Ok(());
        }
        self.request(SessionCommand::Stop).await
    }

    pub async fn wait(&self) -> Result<SessionStatus, SessionError> {
        self.completion().await
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

async fn receive_reply(
    receiver: oneshot::Receiver<Result<(), String>>,
) -> Result<(), SessionError> {
    receiver
        .await
        .map_err(|_| SessionError::SessionClosed)?
        .map_err(SessionError::Control)
}

enum SessionCommand {
    Signal(Signal, oneshot::Sender<Result<(), String>>),
    Resize(u16, u16, oneshot::Sender<Result<(), String>>),
    Stop(oneshot::Sender<Result<(), String>>),
}

enum StdinRequest {
    Write {
        bytes: Vec<u8>,
        permit: OwnedSemaphorePermit,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Close(oneshot::Sender<Result<(), String>>),
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn drive_session(
    spec: RunSpec,
    started: Instant,
    mut exit: crate::backend::ExitFuture,
    controller: Box<dyn crate::BackendController>,
    mut commands: mpsc::Receiver<SessionCommand>,
    stdin_shutdown: watch::Sender<bool>,
    mut stdin_task: tokio::task::JoinHandle<std::io::Result<()>>,
    status_sender: watch::Sender<SessionStatus>,
    stdout_task: tokio::task::JoinHandle<std::io::Result<()>>,
    stderr_task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
) {
    let mut controller = Some(controller);
    let mut deadline = spec
        .timeout
        .duration()
        .map(|duration| Box::pin(sleep(duration)));
    let mut kill_deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut cleanup_deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut reason = TerminationReason::Exited;
    let mut timed_out = false;
    let mut cleanup_failed = false;
    let mut commands_open = true;
    let mut shutdown_started = false;
    let mut stdin_open = true;

    let exit_status = loop {
        tokio::select! {
            result = &mut exit => if let Ok(status) = result {
                break Some(status);
            } else {
                cleanup_failed = true;
                reason = TerminationReason::Failed;
                publish_running_state(
                    &status_sender,
                    &spec,
                    started,
                    SessionState::Failed,
                    reason,
                    timed_out,
                );
                break None;
            },
            result = &mut stdin_task, if stdin_open => {
                stdin_open = false;
                if flatten_stdin_task(result).is_err() {
                    shutdown_started = true;
                    reason = TerminationReason::Failed;
                    publish_running_state(
                        &status_sender,
                        &spec,
                        started,
                        SessionState::Failed,
                        reason,
                        false,
                    );
                    deadline = None;
                    let term_result = bounded_session_control(
                        "TERM",
                        spec.kill_grace,
                        controller
                            .as_deref()
                            .expect("controller must exist before teardown")
                            .signal(Signal::Terminate),
                    )
                    .await;
                    if term_result.is_ok() {
                        kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                    } else {
                        cleanup_failed = true;
                        let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                        cleanup_failed |= force_result.is_err();
                        cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                    }
                }
            }
            command = commands.recv(), if commands_open => {
                let Some(command) = command else {
                    commands_open = false;
                    if !shutdown_started {
                        shutdown_started = true;
                        reason = TerminationReason::Cancelled;
                        publish_running_state(
                            &status_sender,
                            &spec,
                            started,
                            SessionState::Stopping,
                            reason,
                            false,
                        );
                        stdin_shutdown.send_replace(true);
                        let term_result = bounded_session_control(
                            "TERM",
                            spec.kill_grace,
                            controller
                                .as_deref()
                                .expect("controller must exist before teardown")
                                .signal(Signal::Terminate),
                        )
                        .await;
                        if term_result.is_ok() {
                            kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                        } else {
                            cleanup_failed = true;
                            reason = TerminationReason::Failed;
                            publish_running_state(
                                &status_sender,
                                &spec,
                                started,
                                SessionState::Failed,
                                reason,
                                false,
                            );
                            let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                            cleanup_failed |= force_result.is_err();
                            cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                        }
                        deadline = None;
                    } else if kill_deadline.is_none()
                        && cleanup_deadline.is_none()
                        && controller.is_some()
                    {
                        let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                        cleanup_failed |= force_result.is_err();
                        cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                    }
                    continue;
                };
                match command {
                    SessionCommand::Signal(signal, reply) => {
                        let result = match controller.as_deref() {
                            Some(controller) => controller.signal(signal).await.map_err(|error| error.to_string()),
                            None => Err("session teardown is already forced".into()),
                        };
                        let _ = reply.send(result);
                    }
                    SessionCommand::Resize(rows, columns, reply) => {
                        let result = match controller.as_deref() {
                            Some(controller) => controller.resize(rows, columns).await.map_err(|error| error.to_string()),
                            None => Err("session teardown is already forced".into()),
                        };
                        let _ = reply.send(result);
                    }
                    SessionCommand::Stop(reply) => {
                        if shutdown_started {
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                        shutdown_started = true;
                        reason = TerminationReason::Cancelled;
                        publish_running_state(&status_sender, &spec, started, SessionState::Stopping, reason, false);
                        stdin_shutdown.send_replace(true);
                        let result = match controller.as_deref() {
                            Some(controller) => bounded_session_control(
                                "TERM",
                                spec.kill_grace,
                                controller.signal(Signal::Terminate),
                            )
                            .await,
                            None => Err("session teardown is already forced".into()),
                        };
                        if result.is_ok() {
                            kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                            deadline = None;
                        } else if controller.is_some() {
                            cleanup_failed = true;
                            reason = TerminationReason::Failed;
                            publish_running_state(
                                &status_sender,
                                &spec,
                                started,
                                SessionState::Failed,
                                reason,
                                false,
                            );
                            let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                            cleanup_failed |= force_result.is_err();
                            cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                            deadline = None;
                        }
                        let _ = reply.send(result);
                    }
                }
            }
            () = wait_timer(&mut deadline), if deadline.is_some() => {
                shutdown_started = true;
                timed_out = true;
                reason = TerminationReason::TimedOut;
                publish_running_state(&status_sender, &spec, started, SessionState::TimedOut, reason, true);
                stdin_shutdown.send_replace(true);
                let term_result = bounded_session_control(
                    "TERM",
                    spec.kill_grace,
                    controller
                        .as_deref()
                        .expect("controller must exist before teardown")
                        .signal(Signal::Terminate),
                )
                .await;
                if term_result.is_ok() {
                    kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                } else {
                    cleanup_failed = true;
                    reason = TerminationReason::Failed;
                    publish_running_state(
                        &status_sender,
                        &spec,
                        started,
                        SessionState::Failed,
                        reason,
                        true,
                    );
                    let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                    cleanup_failed |= force_result.is_err();
                    cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                }
                deadline = None;
            }
            () = wait_timer(&mut kill_deadline), if kill_deadline.is_some() => {
                let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                if force_result.is_err() {
                    cleanup_failed = true;
                    reason = TerminationReason::Failed;
                    publish_running_state(
                        &status_sender,
                        &spec,
                        started,
                        SessionState::Failed,
                        reason,
                        timed_out,
                    );
                }
                kill_deadline = None;
                cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
            }
            () = wait_timer(&mut cleanup_deadline), if cleanup_deadline.is_some() => {
                cleanup_failed = true;
                reason = TerminationReason::Failed;
                publish_running_state(
                    &status_sender,
                    &spec,
                    started,
                    SessionState::Failed,
                    reason,
                    timed_out,
                );
                break None;
            }
        }
    };
    stdin_shutdown.send_replace(true);
    if stdin_open {
        cleanup_failed |= finish_session_io_task("stdin", stdin_task, spec.kill_grace)
            .await
            .is_err();
    }
    cleanup_failed |= !finish_session_output_tasks(stdout_task, stderr_task, spec.kill_grace)
        .await
        .is_empty();
    drop(controller.take());
    if cleanup_failed {
        reason = TerminationReason::Failed;
        publish_running_state(
            &status_sender,
            &spec,
            started,
            SessionState::Failed,
            reason,
            timed_out,
        );
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

async fn bounded_session_control<F>(
    operation: &'static str,
    deadline: Duration,
    future: F,
) -> Result<(), String>
where
    F: Future<Output = Result<(), BackendError>>,
{
    match timeout(deadline, future).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("backend {operation} failed: {error}")),
        Err(_) => Err(format!(
            "backend {operation} did not complete within {deadline:?}"
        )),
    }
}

async fn force_and_release(
    controller: &mut Option<Box<dyn crate::BackendController>>,
    deadline: Duration,
) -> Result<(), String> {
    let result = match controller.as_deref() {
        Some(controller) => {
            bounded_session_control("force-stop", deadline, controller.force_stop()).await
        }
        None => Err("backend controller was already released".into()),
    };
    drop(controller.take());
    result
}

async fn finish_session_output_tasks(
    stdout: tokio::task::JoinHandle<std::io::Result<()>>,
    stderr: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    deadline: Duration,
) -> Vec<String> {
    let stdout = finish_session_io_task("stdout", stdout, deadline);
    let stderr = async {
        match stderr {
            Some(task) => finish_session_io_task("stderr", task, deadline).await,
            None => Ok(()),
        }
    };
    let (stdout, stderr) = tokio::join!(stdout, stderr);
    [stdout, stderr]
        .into_iter()
        .filter_map(Result::err)
        .collect()
}

async fn finish_session_io_task(
    name: &'static str,
    mut task: tokio::task::JoinHandle<std::io::Result<()>>,
    deadline: Duration,
) -> Result<(), String> {
    if let Ok(result) = timeout(deadline, &mut task).await {
        flatten_session_io_task(name, result)
    } else {
        task.abort();
        let _ = task.await;
        Err(format!(
            "{name} pump did not stop within the {deadline:?} cleanup deadline"
        ))
    }
}

fn flatten_session_io_task(
    name: &'static str,
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<(), String> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("{name} pump failed: {error}")),
        Err(error) => Err(format!("{name} pump task failed: {error}")),
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

fn flatten_stdin_task(
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> std::io::Result<()> {
    result.map_err(|error| std::io::Error::other(format!("stdin task failed: {error}")))?
}

async fn pump_stdin(
    mut writer: Option<crate::BoxedWriter>,
    initial: Vec<u8>,
    mut requests: mpsc::Receiver<StdinRequest>,
    mut shutdown: watch::Receiver<bool>,
    byte_budget: Arc<Semaphore>,
) -> std::io::Result<()> {
    let result = pump_stdin_inner(&mut writer, &initial, &mut requests, &mut shutdown).await;
    requests.close();
    byte_budget.close();
    while let Ok(request) = requests.try_recv() {
        reply_stdin_closed(request);
    }
    result
}

async fn pump_stdin_inner(
    writer: &mut Option<crate::BoxedWriter>,
    initial: &[u8],
    requests: &mut mpsc::Receiver<StdinRequest>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()> {
    if writer.is_none() && !initial.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "backend stdin is unavailable",
        ));
    }
    if !initial.is_empty()
        && let Some(writer) = writer.as_mut()
    {
        tokio::select! {
            biased;
            () = stdin_shutdown(shutdown) => return Ok(()),
            result = writer.write_all(initial) => result?,
        }
    }

    loop {
        tokio::select! {
            biased;
            () = stdin_shutdown(shutdown) => return Ok(()),
            request = requests.recv() => {
                let Some(request) = request else {
                    return Ok(());
                };
                match request {
                    StdinRequest::Write { bytes, permit, reply } => {
                        let result = if let Some(writer) = writer.as_mut() {
                            tokio::select! {
                                biased;
                                () = stdin_shutdown(shutdown) => Err("stdin is closed".into()),
                                result = writer.write_all(&bytes) => {
                                    result.map_err(|error| error.to_string())
                                },
                            }
                        } else {
                            Err("stdin is closed".into())
                        };
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        drop(permit);
                        if failed {
                            return Ok(());
                        }
                    }
                    StdinRequest::Close(reply) => {
                        let result = if let Some(mut writer) = writer.take() {
                            writer.shutdown().await.map_err(|error| error.to_string())
                        } else {
                            Ok(())
                        };
                        let _ = reply.send(result);
                    }
                }
            }
        }
    }
}

async fn stdin_shutdown(receiver: &mut watch::Receiver<bool>) {
    if !*receiver.borrow() {
        let _ = receiver.changed().await;
    }
}

fn reply_stdin_closed(request: StdinRequest) {
    let reply = match request {
        StdinRequest::Write { reply, .. } | StdinRequest::Close(reply) => reply,
    };
    let _ = reply.send(Err("stdin is closed".into()));
}

async fn pump_output<R>(
    mut reader: R,
    channel: OutputChannel,
    output: Arc<Mutex<OutputBuffer>>,
    output_cursor: watch::Sender<u64>,
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
        let next_cursor = {
            let mut output = output.lock().await;
            output.push(channel, &buffer[..count]);
            output.next_cursor()
        };
        output_cursor.send_replace(next_cursor);
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
    #[error("stdin write is {requested} bytes, exceeding the {maximum}-byte queue limit")]
    StdinWriteTooLarge { requested: usize, maximum: usize },
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        sync::{
            Arc, Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

    #[cfg(unix)]
    use std::fs;

    use async_trait::async_trait;
    use moraebox_core::{RunSpec, TimeoutPolicy};
    use tokio::{io::ReadBuf, sync::Notify};

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

    #[tokio::test]
    async fn initial_stdin_cannot_block_the_wall_timeout() {
        let manager = SessionManager::new(Arc::new(ProcessBackend));
        let mut spec = RunSpec::command(long_running_command());
        spec.stdin = vec![b'x'; STDIN_QUEUE_BYTES * 4];
        spec.timeout = TimeoutPolicy::Limited(50);
        spec.kill_grace = Duration::from_millis(20);

        let session = manager.start(spec).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(2), session.wait())
            .await
            .expect("blocked initial stdin must not stop the wall timeout")
            .unwrap();

        assert!(status.timed_out);
        assert_eq!(status.termination_reason, Some(TerminationReason::TimedOut));
    }

    #[tokio::test]
    async fn blocked_incremental_stdin_does_not_block_stop() {
        let manager = SessionManager::new(Arc::new(ProcessBackend));
        let mut spec = RunSpec::command(long_running_command());
        spec.kill_grace = Duration::from_millis(20);
        let session = manager.start(spec).await.unwrap();
        assert_eq!(session.stdin.max_capacity(), STDIN_QUEUE_ITEMS);
        let writer = session.clone();
        let write = tokio::spawn(async move { writer.write(vec![b'x'; STDIN_QUEUE_BYTES]).await });
        sleep(Duration::from_millis(50)).await;
        assert!(!write.is_finished(), "test write must fill the guest pipe");
        assert_eq!(session.stdin_bytes.available_permits(), 0);

        let status = tokio::time::timeout(Duration::from_secs(2), async {
            session.stop().await.unwrap();
            session.wait().await.unwrap()
        })
        .await
        .expect("stop must remain responsive while stdin is blocked");
        let write_result = tokio::time::timeout(Duration::from_secs(2), write)
            .await
            .expect("blocked write must be released during stop")
            .unwrap();

        assert_eq!(
            status.termination_reason,
            Some(TerminationReason::Cancelled)
        );
        assert!(write_result.is_err());
    }

    #[tokio::test]
    async fn rejects_a_single_write_larger_than_the_byte_budget() {
        let manager = SessionManager::new(Arc::new(ProcessBackend));
        let session = manager
            .start(RunSpec::command(long_running_command()))
            .await
            .unwrap();

        let error = session
            .write(vec![0; STDIN_QUEUE_BYTES + 1])
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SessionError::StdinWriteTooLarge {
                requested,
                maximum: STDIN_QUEUE_BYTES,
            } if requested == STDIN_QUEUE_BYTES + 1
        ));
        session.stop().await.unwrap();
        session.wait().await.unwrap();
    }

    #[tokio::test]
    async fn control_errors_still_force_and_reach_dead_after_cleanup() {
        let state = Arc::new(SessionBackendState::default());
        let manager = SessionManager::new(Arc::new(SessionTestBackend {
            mode: SessionBackendMode::ControlFailure,
            state: Arc::clone(&state),
        }));
        let mut spec = RunSpec::command(["fake"]);
        spec.kill_grace = Duration::from_millis(50);
        let session = manager.start(spec).await.unwrap();

        let error = session.stop().await.unwrap_err();
        let status = session.wait().await.unwrap();

        assert!(error.to_string().contains("TERM failed"));
        assert_eq!(state.term_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.force_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.exit_cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(status.state, SessionState::Dead);
        assert_eq!(status.termination_reason, Some(TerminationReason::Failed));
    }

    #[tokio::test]
    async fn hard_cleanup_deadline_never_publishes_dead_without_exit() {
        let state = Arc::new(SessionBackendState::default());
        let manager = SessionManager::new(Arc::new(SessionTestBackend {
            mode: SessionBackendMode::ExitHangs,
            state: Arc::clone(&state),
        }));
        let mut spec = RunSpec::command(["fake"]);
        spec.timeout = TimeoutPolicy::Limited(1);
        spec.kill_grace = Duration::from_millis(10);
        let session = manager.start(spec).await.unwrap();

        sleep(Duration::from_millis(80)).await;

        assert_eq!(state.force_calls.load(Ordering::SeqCst), 1);
        assert_eq!(session.status().state, SessionState::Failed);
        assert!(matches!(
            session.wait().await,
            Err(SessionError::SessionClosed)
        ));
    }

    #[tokio::test]
    async fn output_pump_error_marks_the_completed_session_failed() {
        let manager = SessionManager::new(Arc::new(SessionTestBackend {
            mode: SessionBackendMode::OutputFailure,
            state: Arc::new(SessionBackendState::default()),
        }));

        let session = manager.start(RunSpec::command(["fake"])).await.unwrap();
        let status = session.wait().await.unwrap();

        assert_eq!(status.state, SessionState::Dead);
        assert_eq!(status.termination_reason, Some(TerminationReason::Failed));
    }

    #[tokio::test]
    async fn dead_is_published_only_after_output_pump_is_reclaimed() {
        let state = Arc::new(SessionBackendState::default());
        let manager = SessionManager::new(Arc::new(SessionTestBackend {
            mode: SessionBackendMode::OutputGate,
            state: Arc::clone(&state),
        }));
        let mut spec = RunSpec::command(["fake"]);
        spec.kill_grace = Duration::from_secs(1);
        let session = manager.start(spec).await.unwrap();

        sleep(Duration::from_millis(20)).await;
        assert_ne!(session.status().state, SessionState::Dead);
        drop(state.output_writer.lock().unwrap().take());

        assert_eq!(session.wait().await.unwrap().state, SessionState::Dead);
    }

    #[tokio::test]
    async fn repeated_stop_preserves_the_first_kill_deadline() {
        let state = Arc::new(SessionBackendState::default());
        let manager = SessionManager::new(Arc::new(SessionTestBackend {
            mode: SessionBackendMode::GracefulThenForce,
            state: Arc::clone(&state),
        }));
        let mut spec = RunSpec::command(["fake"]);
        spec.kill_grace = Duration::from_millis(80);
        let session = manager.start(spec).await.unwrap();

        session.stop().await.unwrap();
        sleep(Duration::from_millis(50)).await;
        session.stop().await.unwrap();
        let status = timeout(Duration::from_millis(55), session.wait())
            .await
            .expect("a repeated stop must not extend the first kill deadline")
            .unwrap();

        assert_eq!(state.term_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.force_calls.load(Ordering::SeqCst), 1);
        assert_eq!(status.state, SessionState::Dead);
        session.stop().await.unwrap();
        assert_eq!(session.status(), status);
    }

    #[tokio::test]
    async fn queued_stop_race_sends_term_only_once() {
        let state = Arc::new(SessionBackendState::default());
        let manager = SessionManager::new(Arc::new(SessionTestBackend {
            mode: SessionBackendMode::GracefulThenForce,
            state: Arc::clone(&state),
        }));
        let mut spec = RunSpec::command(["fake"]);
        spec.kill_grace = Duration::from_millis(20);
        let session = manager.start(spec).await.unwrap();
        let (first_reply, first_result) = oneshot::channel();
        let (second_reply, second_result) = oneshot::channel();

        session
            .commands
            .send(SessionCommand::Stop(first_reply))
            .await
            .unwrap();
        session
            .commands
            .send(SessionCommand::Stop(second_reply))
            .await
            .unwrap();
        assert!(first_result.await.unwrap().is_ok());
        assert!(second_result.await.unwrap().is_ok());
        session.wait().await.unwrap();

        assert_eq!(state.term_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.force_calls.load(Ordering::SeqCst), 1);
    }

    #[derive(Clone, Copy)]
    enum SessionBackendMode {
        ControlFailure,
        ExitHangs,
        GracefulThenForce,
        OutputFailure,
        OutputGate,
    }

    #[derive(Default)]
    struct SessionBackendState {
        term_calls: AtomicUsize,
        force_calls: AtomicUsize,
        exit_cleanups: AtomicUsize,
        forced: Notify,
        output_writer: StdMutex<Option<tokio::io::DuplexStream>>,
    }

    struct SessionTestBackend {
        mode: SessionBackendMode,
        state: Arc<SessionBackendState>,
    }

    #[async_trait]
    impl Backend for SessionTestBackend {
        fn name(&self) -> &'static str {
            "session-test"
        }

        async fn spawn(&self, _spec: &RunSpec) -> Result<crate::SpawnedSandbox, BackendError> {
            let exit_state = Arc::clone(&self.state);
            let exit: crate::backend::ExitFuture = match self.mode {
                SessionBackendMode::ControlFailure | SessionBackendMode::GracefulThenForce => {
                    Box::pin(async move {
                        exit_state.forced.notified().await;
                        exit_state.exit_cleanups.fetch_add(1, Ordering::SeqCst);
                        Ok(success_status())
                    })
                }
                SessionBackendMode::ExitHangs => Box::pin(std::future::pending()),
                SessionBackendMode::OutputFailure | SessionBackendMode::OutputGate => {
                    Box::pin(async { Ok(success_status()) })
                }
            };
            let stdout: crate::BoxedReader = match self.mode {
                SessionBackendMode::OutputFailure => Box::pin(SessionFailingReader),
                SessionBackendMode::OutputGate => {
                    let (writer, reader) = tokio::io::duplex(8);
                    *self.state.output_writer.lock().unwrap() = Some(writer);
                    Box::pin(reader)
                }
                SessionBackendMode::ControlFailure
                | SessionBackendMode::ExitHangs
                | SessionBackendMode::GracefulThenForce => Box::pin(tokio::io::empty()),
            };
            Ok(crate::SpawnedSandbox {
                stdin: None,
                stdout,
                stdout_channel: OutputChannel::Stdout,
                stderr: None,
                exit,
                controller: Box::new(SessionTestController {
                    mode: self.mode,
                    state: Arc::clone(&self.state),
                }),
                startup: crate::StartupMetrics::default(),
            })
        }
    }

    struct SessionTestController {
        mode: SessionBackendMode,
        state: Arc<SessionBackendState>,
    }

    #[async_trait]
    impl crate::BackendController for SessionTestController {
        async fn signal(&self, signal: Signal) -> Result<(), BackendError> {
            if signal == Signal::Terminate {
                self.state.term_calls.fetch_add(1, Ordering::SeqCst);
            }
            if matches!(self.mode, SessionBackendMode::ControlFailure) {
                return Err(BackendError::Control("injected TERM failure".into()));
            }
            Ok(())
        }

        async fn force_stop(&self) -> Result<(), BackendError> {
            self.state.force_calls.fetch_add(1, Ordering::SeqCst);
            if matches!(
                self.mode,
                SessionBackendMode::ControlFailure | SessionBackendMode::GracefulThenForce
            ) {
                self.state.forced.notify_one();
            }
            if matches!(self.mode, SessionBackendMode::ControlFailure) {
                return Err(BackendError::Control("injected force-stop failure".into()));
            }
            Ok(())
        }
    }

    struct SessionFailingReader;

    impl AsyncRead for SessionFailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("injected read failure")))
        }
    }

    #[cfg(unix)]
    fn success_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    fn success_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_last_handle_terminates_and_reaps_the_process() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let pid_path = state.path().join("child.pid");
        let script = format!(
            "printf '%s' \"$$\" > '{}'; while :; do :; done",
            pid_path.display()
        );
        let manager = SessionManager::new(Arc::new(ProcessBackend));
        let session = manager
            .start(RunSpec::command(["/bin/sh", "-c", &script]))
            .await
            .unwrap();
        let output = Arc::downgrade(&session.output);
        wait_for_path(&pid_path).await;
        let pid = fs::read_to_string(&pid_path)
            .unwrap()
            .parse::<i32>()
            .unwrap();

        drop(session);

        let pid = Pid::from_raw(pid);
        tokio::time::timeout(Duration::from_secs(2), async {
            while kill(pid, None) != Err(Errno::ESRCH) || output.upgrade().is_some() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owner loss must terminate the process and output pumps");
    }

    #[cfg(unix)]
    async fn wait_for_path(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !path.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child did not publish its pid");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_notification_arrives_before_session_exit() {
        let manager = SessionManager::new(Arc::new(ProcessBackend));
        let session = manager
            .start(RunSpec::command(
                [
                    "/bin/sh",
                    "-c",
                    "printf ready; read line; printf ':%s' \"$line\"",
                ]
                .map(String::from)
                .to_vec(),
            ))
            .await
            .unwrap();

        let cursor = tokio::time::timeout(Duration::from_secs(2), session.wait_for_output(0))
            .await
            .expect("first output must arrive while the command is still running")
            .unwrap();
        assert_eq!(cursor, 5);
        assert_eq!(session.status().state, SessionState::Running);
        let first = session.read_output(0, 1024).await.unwrap();
        assert_eq!(first.chunks[0].data, b"ready");

        session.write(b"done\n".to_vec()).await.unwrap();
        session.close_stdin().await.unwrap();
        let status = session.wait().await.unwrap();
        assert_eq!(status.exit_code, Some(0));
        let rest = session.read_output(cursor, 1024).await.unwrap();
        assert_eq!(rest.chunks[0].data, b":done");
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
