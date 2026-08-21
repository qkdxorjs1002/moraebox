use std::{
    fmt,
    future::Future,
    process::ExitStatus,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use moraebox_core::{
    Lifecycle, LifecycleEvent, OutputBuffer, OutputChannel, OutputRead, OutputReadError, RunSpec,
    SessionId, SessionState, Signal, TerminationReason,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::{sleep, timeout},
};

use crate::budget::StageEventKind;
use crate::{Backend, BackendError, StartupMetrics, TraceEvent, TraceKind};
use crate::{RunBudget, RunStage, StageTiming};

const STDIN_QUEUE_ITEMS: usize = 32;
const STDIN_QUEUE_BYTES: usize = 1024 * 1024;
pub const MAX_SESSION_OUTPUT_READ_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct SessionManager {
    backend: Arc<dyn Backend>,
}

impl SessionManager {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    pub async fn start(&self, spec: RunSpec) -> Result<SessionHandle, SessionError> {
        let budget = RunBudget::new(spec.timeout);
        self.start_with_budget(spec, budget).await
    }

    pub async fn start_with_budget(
        &self,
        spec: RunSpec,
        budget: RunBudget,
    ) -> Result<SessionHandle, SessionError> {
        start_session(Arc::clone(&self.backend), spec, budget).await
    }
}

pub(crate) async fn start_session<B: Backend + ?Sized>(
    backend: Arc<B>,
    mut spec: RunSpec,
    budget: RunBudget,
) -> Result<SessionHandle, SessionError> {
    spec.validate().map_err(BackendError::InvalidSpec)?;
    let started = budget.started();
    let trace = Arc::new(StdMutex::new(TraceRecorder::new(started)));
    let mut lifecycle = Lifecycle::default();
    lifecycle.apply(LifecycleEvent::Prepare)?;
    record_trace_at(&trace, TraceKind::PrepareStarted, 0);
    lifecycle.apply(LifecycleEvent::Start)?;
    record_trace(&trace, TraceKind::BackendSpawnStarted);
    let spawned = match backend.spawn(&spec, &budget).await {
        Ok(spawned) => spawned,
        Err(error) => {
            lifecycle.apply(LifecycleEvent::Fail)?;
            lifecycle.apply(LifecycleEvent::CleanupComplete)?;
            return Err(error.into());
        }
    };
    lifecycle.apply(LifecycleEvent::AgentReady)?;
    record_trace(&trace, TraceKind::BackendSpawned);
    lifecycle.apply(LifecycleEvent::CommandStarted)?;
    record_trace(&trace, TraceKind::CommandStarted);
    let startup = spawned.startup.clone();
    let (command_stage_started, command_timeout) = begin_command_stage(&budget);
    let initial = SessionStatus {
        session_id: spec.session_id,
        backend: backend.name().into(),
        resolved_image_digest: startup.resolved_image_digest.clone(),
        state: lifecycle.state(),
        termination_reason: lifecycle.termination_reason(),
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
    let terminal_errors = Arc::new(StdMutex::new(TerminalDiagnostics::default()));
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
        Arc::clone(&trace),
    ));
    let stderr_task = spawned.stderr.map(|stderr| {
        tokio::spawn(pump_output(
            stderr,
            OutputChannel::Stderr,
            Arc::clone(&output),
            output_cursor_sender.clone(),
            Arc::clone(&trace),
        ))
    });
    drop(output_cursor_sender);

    let session_id = spec.session_id;
    tokio::spawn(drive_session(
        spec,
        started,
        lifecycle,
        spawned.exit,
        spawned.controller,
        command_receiver,
        stdin_shutdown_sender,
        stdin_task,
        status_sender,
        stdout_task,
        stderr_task,
        Arc::clone(&trace),
        Arc::clone(&terminal_errors),
        budget.clone(),
        command_stage_started,
        command_timeout,
    ));
    Ok(SessionHandle {
        session_id,
        output,
        output_cursor: output_cursor_receiver,
        status: status_receiver,
        commands: command_sender,
        stdin: stdin_sender,
        stdin_bytes,
        startup,
        trace,
        terminal_errors,
        budget,
    })
}

fn begin_command_stage(budget: &RunBudget) -> (Instant, Option<Duration>) {
    let timeout = budget
        .remaining(RunStage::CommandRun)
        .unwrap_or(Some(Duration::ZERO));
    (budget.begin_stage(RunStage::CommandRun), timeout)
}

#[derive(Debug, Default)]
struct TerminalDiagnostics {
    messages: Vec<String>,
    io_failures: Vec<SessionIoFailure>,
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
    startup: StartupMetrics,
    trace: Arc<StdMutex<TraceRecorder>>,
    terminal_errors: Arc<StdMutex<TerminalDiagnostics>>,
    budget: RunBudget,
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
        let terminal_errors = Arc::clone(&self.terminal_errors);
        async move {
            loop {
                let current = receiver.borrow().clone();
                if current.state == SessionState::Dead {
                    if let Some(failure) = terminal_errors
                        .lock()
                        .expect("session error lock must not be poisoned")
                        .io_failures
                        .first()
                        .cloned()
                    {
                        return Err(SessionError::Io(failure));
                    }
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
        if max_bytes > MAX_SESSION_OUTPUT_READ_BYTES {
            return Err(SessionError::OutputReadTooLarge {
                requested: max_bytes,
                maximum: MAX_SESSION_OUTPUT_READ_BYTES,
            });
        }
        let snapshot = self.output.lock().await.snapshot(cursor, max_bytes)?;
        Ok(snapshot.materialize())
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

    pub(crate) fn startup(&self) -> StartupMetrics {
        self.startup.clone()
    }

    pub(crate) fn trace(&self) -> Vec<TraceEvent> {
        let mut events = self
            .trace
            .lock()
            .expect("session trace lock must not be poisoned")
            .events
            .clone();
        events.extend(self.budget.events().into_iter().map(|event| TraceEvent {
            sequence: 0,
            elapsed_micros: event.elapsed_micros,
            kind: match event.kind {
                StageEventKind::Started => TraceKind::StageStarted,
                StageEventKind::Completed => TraceKind::StageCompleted,
                StageEventKind::Failed => TraceKind::StageFailed,
            },
            stage: Some(event.stage),
        }));
        events.sort_by_key(|event| event.elapsed_micros);
        for (sequence, event) in events.iter_mut().enumerate() {
            event.sequence = sequence as u64;
        }
        events
    }

    pub(crate) fn terminal_error(&self) -> Option<String> {
        let errors = self
            .terminal_errors
            .lock()
            .expect("session error lock must not be poisoned");
        (!errors.messages.is_empty()).then(|| errors.messages.join("; "))
    }

    pub(crate) async fn retained_output(&self) -> (OutputRead, u64, u64) {
        let (snapshot, earliest, next) = {
            let output = self.output.lock().await;
            let earliest = output.earliest_cursor();
            let next = output.next_cursor();
            let snapshot = output
                .snapshot(earliest, usize::MAX)
                .expect("earliest retained output cursor must be readable");
            (snapshot, earliest, next)
        };
        let retained = snapshot.materialize();
        (retained, earliest, next)
    }

    pub(crate) fn stage_timings(&self) -> Vec<StageTiming> {
        self.budget.timings()
    }

    pub(crate) fn failure_stage(&self) -> Option<RunStage> {
        self.budget.failure_stage()
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
    mut lifecycle: Lifecycle,
    mut exit: crate::backend::ExitFuture,
    controller: Box<dyn crate::BackendController>,
    mut commands: mpsc::Receiver<SessionCommand>,
    stdin_shutdown: watch::Sender<bool>,
    mut stdin_task: tokio::task::JoinHandle<std::io::Result<()>>,
    status_sender: watch::Sender<SessionStatus>,
    stdout_task: tokio::task::JoinHandle<std::io::Result<()>>,
    stderr_task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    trace: Arc<StdMutex<TraceRecorder>>,
    terminal_errors: Arc<StdMutex<TerminalDiagnostics>>,
    budget: RunBudget,
    command_stage_started: Instant,
    command_timeout: Option<Duration>,
) {
    let mut controller = Some(controller);
    let mut deadline = command_timeout.map(|duration| Box::pin(sleep(duration)));
    let mut kill_deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut cleanup_deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut timed_out = false;
    let mut errors = Vec::new();
    let mut io_failures = Vec::new();
    let mut commands_open = true;
    let mut shutdown_started = false;
    let mut stdin_open = true;

    let exit_status = loop {
        tokio::select! {
            result = &mut exit => {
                match result {
                    Ok(status) => {
                        if lifecycle.state() == SessionState::Running {
                            apply_and_publish(
                                &mut lifecycle,
                                LifecycleEvent::CommandExited,
                                &status_sender,
                                &spec,
                                started,
                                timed_out,
                            );
                        }
                        record_trace(&trace, TraceKind::ProcessExited);
                        break Some(status);
                    }
                    Err(error) => {
                        errors.push(format!("backend exit wait failed: {error}"));
                        mark_failed(
                            &mut lifecycle,
                            &status_sender,
                            &spec,
                            started,
                            timed_out,
                        );
                        break None;
                    }
                }
            },
            result = &mut stdin_task, if stdin_open => {
                stdin_open = false;
                if let Err(error) = flatten_stdin_task(result) {
                    errors.push(format!("stdin pump failed: {error}"));
                    shutdown_started = true;
                    mark_failed(
                        &mut lifecycle,
                        &status_sender,
                        &spec,
                        started,
                        false,
                    );
                    deadline = None;
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
                        record_trace(&trace, TraceKind::GracefulStop);
                        kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                    } else {
                        errors.push(term_result.expect_err("TERM result was checked as an error"));
                        let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                        if force_result.is_ok() {
                            record_trace(&trace, TraceKind::ForcedStop);
                        } else {
                            errors.push(force_result.expect_err("force result was checked as an error"));
                        }
                        cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                    }
                }
            }
            command = commands.recv(), if commands_open => {
                let Some(command) = command else {
                    commands_open = false;
                    if !shutdown_started {
                        shutdown_started = true;
                        apply_and_publish(
                            &mut lifecycle,
                            LifecycleEvent::StopRequested,
                            &status_sender,
                            &spec,
                            started,
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
                            record_trace(&trace, TraceKind::GracefulStop);
                            kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                        } else {
                            errors.push(term_result.expect_err("TERM result was checked as an error"));
                            mark_failed(
                                &mut lifecycle,
                                &status_sender,
                                &spec,
                                started,
                                false,
                            );
                            let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                            if force_result.is_ok() {
                                record_trace(&trace, TraceKind::ForcedStop);
                            } else {
                                errors.push(force_result.expect_err("force result was checked as an error"));
                            }
                            cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                        }
                        deadline = None;
                    } else if kill_deadline.is_none()
                        && cleanup_deadline.is_none()
                        && controller.is_some()
                    {
                        let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                        if force_result.is_ok() {
                            record_trace(&trace, TraceKind::ForcedStop);
                        } else {
                            errors.push(force_result.expect_err("force result was checked as an error"));
                            mark_failed(
                                &mut lifecycle,
                                &status_sender,
                                &spec,
                                started,
                                timed_out,
                            );
                        }
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
                        apply_and_publish(
                            &mut lifecycle,
                            LifecycleEvent::StopRequested,
                            &status_sender,
                            &spec,
                            started,
                            false,
                        );
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
                            record_trace(&trace, TraceKind::GracefulStop);
                            kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                            deadline = None;
                        } else if controller.is_some() {
                            errors.push(result.clone().expect_err("TERM result was checked as an error"));
                            mark_failed(
                                &mut lifecycle,
                                &status_sender,
                                &spec,
                                started,
                                false,
                            );
                            let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                            if force_result.is_ok() {
                                record_trace(&trace, TraceKind::ForcedStop);
                            } else {
                                errors.push(force_result.expect_err("force result was checked as an error"));
                            }
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
                apply_and_publish(
                    &mut lifecycle,
                    LifecycleEvent::Timeout,
                    &status_sender,
                    &spec,
                    started,
                    true,
                );
                record_trace(&trace, TraceKind::Timeout);
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
                    record_trace(&trace, TraceKind::GracefulStop);
                    kill_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                } else {
                    errors.push(term_result.expect_err("TERM result was checked as an error"));
                    mark_failed(
                        &mut lifecycle,
                        &status_sender,
                        &spec,
                        started,
                        true,
                    );
                    let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                    if force_result.is_ok() {
                        record_trace(&trace, TraceKind::ForcedStop);
                    } else {
                        errors.push(force_result.expect_err("force result was checked as an error"));
                    }
                    cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
                }
                deadline = None;
            }
            () = wait_timer(&mut kill_deadline), if kill_deadline.is_some() => {
                let force_result = force_and_release(&mut controller, spec.kill_grace).await;
                if force_result.is_ok() {
                    record_trace(&trace, TraceKind::ForcedStop);
                } else {
                    errors.push(force_result.expect_err("force result was checked as an error"));
                    mark_failed(
                        &mut lifecycle,
                        &status_sender,
                        &spec,
                        started,
                        timed_out,
                    );
                }
                kill_deadline = None;
                cleanup_deadline = Some(Box::pin(sleep(spec.kill_grace)));
            }
            () = wait_timer(&mut cleanup_deadline), if cleanup_deadline.is_some() => {
                errors.push(format!(
                    "backend exit did not complete within the {:?} hard cleanup deadline",
                    spec.kill_grace
                ));
                mark_failed(
                    &mut lifecycle,
                    &status_sender,
                    &spec,
                    started,
                    timed_out,
                );
                break None;
            }
        }
    };
    stdin_shutdown.send_replace(true);
    if stdin_open {
        if let Err(error) =
            finish_session_io_task(SessionIoStream::Stdin, stdin_task, spec.kill_grace).await
        {
            errors.push(error.to_string());
            io_failures.push(error);
        }
    }
    let output_failures =
        finish_session_output_tasks(stdout_task, stderr_task, spec.kill_grace).await;
    errors.extend(output_failures.iter().map(ToString::to_string));
    io_failures.extend(output_failures);
    drop(controller.take());
    if !errors.is_empty() {
        mark_failed(&mut lifecycle, &status_sender, &spec, started, timed_out);
    }
    if timed_out || !errors.is_empty() {
        budget.fail_stage(RunStage::CommandRun, command_stage_started);
    } else {
        budget.complete_stage(RunStage::CommandRun, command_stage_started);
    }
    *terminal_errors
        .lock()
        .expect("session error lock must not be poisoned") = TerminalDiagnostics {
        messages: errors,
        io_failures,
    };
    if let Some(status) = exit_status {
        let (exit_code, signal) = decode_exit_status(status);
        apply_lifecycle(&mut lifecycle, LifecycleEvent::CleanupComplete);
        record_trace(&trace, TraceKind::CleanupComplete);
        publish_status(
            &status_sender,
            &spec,
            started,
            &lifecycle,
            timed_out,
            exit_code,
            signal,
        );
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
) -> Vec<SessionIoFailure> {
    let stdout = finish_session_io_task(SessionIoStream::Stdout, stdout, deadline);
    let stderr = async {
        match stderr {
            Some(task) => finish_session_io_task(SessionIoStream::Stderr, task, deadline).await,
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
    stream: SessionIoStream,
    mut task: tokio::task::JoinHandle<std::io::Result<()>>,
    deadline: Duration,
) -> Result<(), SessionIoFailure> {
    if let Ok(result) = timeout(deadline, &mut task).await {
        flatten_session_io_task(stream, result)
    } else {
        task.abort();
        let _ = task.await;
        Err(SessionIoFailure::cleanup_timeout(stream, deadline))
    }
}

fn flatten_session_io_task(
    stream: SessionIoStream,
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<(), SessionIoFailure> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(SessionIoFailure::operation(stream, &error)),
        Err(error) => Err(SessionIoFailure::task(stream, &error)),
    }
}

async fn wait_timer(timer: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>) {
    if let Some(timer) = timer {
        timer.as_mut().await;
    }
}

fn apply_lifecycle(lifecycle: &mut Lifecycle, event: LifecycleEvent) {
    lifecycle
        .apply(event)
        .expect("common lifecycle engine emitted an invalid transition");
}

fn apply_and_publish(
    lifecycle: &mut Lifecycle,
    event: LifecycleEvent,
    sender: &watch::Sender<SessionStatus>,
    spec: &RunSpec,
    started: Instant,
    timed_out: bool,
) {
    apply_lifecycle(lifecycle, event);
    publish_status(sender, spec, started, lifecycle, timed_out, None, None);
}

fn mark_failed(
    lifecycle: &mut Lifecycle,
    sender: &watch::Sender<SessionStatus>,
    spec: &RunSpec,
    started: Instant,
    timed_out: bool,
) {
    if lifecycle.state() != SessionState::Failed {
        apply_lifecycle(lifecycle, LifecycleEvent::Fail);
    }
    publish_status(sender, spec, started, lifecycle, timed_out, None, None);
}

#[allow(clippy::too_many_arguments)]
fn publish_status(
    sender: &watch::Sender<SessionStatus>,
    spec: &RunSpec,
    started: Instant,
    lifecycle: &Lifecycle,
    timed_out: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
) {
    let current = sender.borrow();
    let backend = current.backend.clone();
    let resolved_image_digest = current.resolved_image_digest.clone();
    drop(current);
    let _ = sender.send(SessionStatus {
        session_id: spec.session_id,
        backend,
        resolved_image_digest,
        state: lifecycle.state(),
        termination_reason: lifecycle.termination_reason(),
        exit_code,
        signal,
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
    if !initial.is_empty() {
        let writer = writer
            .as_mut()
            .expect("non-empty initial input requires backend stdin");
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
    trace: Arc<StdMutex<TraceRecorder>>,
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
        let (next_cursor, first_output) = {
            let mut output = output.lock().await;
            let first_output = output.next_cursor() == 0;
            output.push(channel, &buffer[..count]);
            (output.next_cursor(), first_output)
        };
        if first_output {
            record_trace(&trace, TraceKind::FirstOutput);
        }
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

struct TraceRecorder {
    started: Instant,
    events: Vec<TraceEvent>,
}

impl TraceRecorder {
    fn new(started: Instant) -> Self {
        Self {
            started,
            events: Vec::new(),
        }
    }

    fn push(&mut self, kind: TraceKind) {
        self.events.push(TraceEvent {
            sequence: self.events.len() as u64,
            elapsed_micros: duration_micros(self.started.elapsed()),
            kind,
            stage: None,
        });
    }

    fn push_at(&mut self, kind: TraceKind, elapsed_micros: u64) {
        self.events.push(TraceEvent {
            sequence: self.events.len() as u64,
            elapsed_micros,
            kind,
            stage: None,
        });
    }
}

fn record_trace(trace: &Arc<StdMutex<TraceRecorder>>, kind: TraceKind) {
    trace
        .lock()
        .expect("session trace lock must not be poisoned")
        .push(kind);
}

fn record_trace_at(trace: &Arc<StdMutex<TraceRecorder>>, kind: TraceKind, elapsed_micros: u64) {
    trace
        .lock()
        .expect("session trace lock must not be poisoned")
        .push_at(kind, elapsed_micros);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: SessionId,
    pub backend: String,
    #[serde(default)]
    pub resolved_image_digest: Option<String>,
    pub state: SessionState,
    pub termination_reason: Option<TerminationReason>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub elapsed_micros: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIoStream {
    Stdin,
    Stdout,
    Stderr,
}

impl fmt::Display for SessionIoStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Stdin => "stdin",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIoFailureKind {
    Operation,
    Task,
    CleanupTimeout,
}

impl fmt::Display for SessionIoFailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Operation => "I/O failed",
            Self::Task => "task failed",
            Self::CleanupTimeout => "cleanup timed out",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{stream} pump {kind}: {message}")]
pub struct SessionIoFailure {
    pub stream: SessionIoStream,
    pub kind: SessionIoFailureKind,
    pub io_kind: Option<std::io::ErrorKind>,
    pub message: String,
}

impl SessionIoFailure {
    fn operation(stream: SessionIoStream, error: &std::io::Error) -> Self {
        Self {
            stream,
            kind: SessionIoFailureKind::Operation,
            io_kind: Some(error.kind()),
            message: error.to_string(),
        }
    }

    fn task(stream: SessionIoStream, error: &tokio::task::JoinError) -> Self {
        Self {
            stream,
            kind: SessionIoFailureKind::Task,
            io_kind: None,
            message: error.to_string(),
        }
    }

    fn cleanup_timeout(stream: SessionIoStream, deadline: Duration) -> Self {
        Self {
            stream,
            kind: SessionIoFailureKind::CleanupTimeout,
            io_kind: None,
            message: format!("did not stop within the {deadline:?} cleanup deadline"),
        }
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Io(#[from] SessionIoFailure),
    #[error(transparent)]
    Output(#[from] OutputReadError),
    #[error(transparent)]
    Lifecycle(#[from] moraebox_core::LifecycleError),
    #[error("session is no longer available")]
    SessionClosed,
    #[error("session control failed: {0}")]
    Control(String),
    #[error("stdin write is {requested} bytes, exceeding the {maximum}-byte queue limit")]
    StdinWriteTooLarge { requested: usize, maximum: usize },
    #[error("output read is {requested} bytes, exceeding the {maximum}-byte request limit")]
    OutputReadTooLarge { requested: usize, maximum: usize },
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
        let trace = session
            .trace()
            .into_iter()
            .filter(|event| event.stage.is_none())
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            &trace[..4],
            &[
                TraceKind::PrepareStarted,
                TraceKind::BackendSpawnStarted,
                TraceKind::BackendSpawned,
                TraceKind::CommandStarted,
            ]
        );
        assert!(trace.contains(&TraceKind::FirstOutput));
        assert_eq!(trace[trace.len() - 2], TraceKind::ProcessExited);
        assert_eq!(trace.last(), Some(&TraceKind::CleanupComplete));
    }

    #[tokio::test]
    async fn resolved_image_digest_survives_status_updates() {
        let manager = SessionManager::new(Arc::new(ResolvedImageProcessBackend));
        let session = manager
            .start(RunSpec::command(stdin_echo_command()))
            .await
            .unwrap();
        session.close_stdin().await.unwrap();

        let status = session.wait().await.unwrap();

        assert_eq!(
            status.resolved_image_digest.as_deref(),
            Some("sha256:resolved")
        );
        assert_eq!(
            session.startup().resolved_image_digest.as_deref(),
            Some("sha256:resolved")
        );
    }

    struct ResolvedImageProcessBackend;

    #[async_trait]
    impl Backend for ResolvedImageProcessBackend {
        fn name(&self) -> &'static str {
            "resolved-image-process"
        }

        fn capabilities(&self) -> crate::BackendCapabilities {
            ProcessBackend::CAPABILITIES
        }

        async fn spawn(
            &self,
            spec: &RunSpec,
            budget: &RunBudget,
        ) -> Result<crate::SpawnedSandbox, BackendError> {
            let mut spawned = ProcessBackend.spawn(spec, budget).await?;
            spawned.startup.resolved_image_digest = Some("sha256:resolved".into());
            Ok(spawned)
        }
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
    async fn rejects_a_single_output_read_larger_than_the_api_limit() {
        let manager = SessionManager::new(Arc::new(ProcessBackend));
        let session = manager
            .start(RunSpec::command(long_running_command()))
            .await
            .unwrap();

        let error = session
            .read_output(0, MAX_SESSION_OUTPUT_READ_BYTES + 1)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SessionError::OutputReadTooLarge {
                requested,
                maximum: MAX_SESSION_OUTPUT_READ_BYTES,
            } if requested == MAX_SESSION_OUTPUT_READ_BYTES + 1
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
        let error = session.wait().await.unwrap_err();
        let status = session.status();

        assert_eq!(status.state, SessionState::Dead);
        assert_eq!(status.termination_reason, Some(TerminationReason::Failed));
        assert!(matches!(
            error,
            SessionError::Io(SessionIoFailure {
                stream: SessionIoStream::Stdout,
                kind: SessionIoFailureKind::Operation,
                io_kind: Some(io::ErrorKind::Other),
                ref message,
            }) if message == "injected read failure"
        ));
        assert!(
            session
                .terminal_error()
                .is_some_and(|error| error.contains("injected read failure"))
        );
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
    async fn output_cleanup_timeout_is_a_typed_io_failure() {
        let manager = SessionManager::new(Arc::new(SessionTestBackend {
            mode: SessionBackendMode::OutputGate,
            state: Arc::new(SessionBackendState::default()),
        }));
        let mut spec = RunSpec::command(["fake"]);
        spec.kill_grace = Duration::from_millis(10);
        let session = manager.start(spec).await.unwrap();

        assert!(matches!(
            session.wait().await,
            Err(SessionError::Io(SessionIoFailure {
                stream: SessionIoStream::Stdout,
                kind: SessionIoFailureKind::CleanupTimeout,
                io_kind: None,
                ..
            }))
        ));
        assert_eq!(session.status().state, SessionState::Dead);
    }

    #[tokio::test]
    async fn output_task_panic_is_a_typed_io_failure() {
        let manager = SessionManager::new(Arc::new(SessionTestBackend {
            mode: SessionBackendMode::OutputPanic,
            state: Arc::new(SessionBackendState::default()),
        }));
        let session = manager.start(RunSpec::command(["fake"])).await.unwrap();

        assert!(matches!(
            session.wait().await,
            Err(SessionError::Io(SessionIoFailure {
                stream: SessionIoStream::Stdout,
                kind: SessionIoFailureKind::Task,
                io_kind: None,
                ..
            }))
        ));
        assert_eq!(session.status().state, SessionState::Dead);
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
        OutputPanic,
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

        fn capabilities(&self) -> crate::BackendCapabilities {
            ProcessBackend::CAPABILITIES
        }

        async fn spawn(
            &self,
            _spec: &RunSpec,
            _budget: &RunBudget,
        ) -> Result<crate::SpawnedSandbox, BackendError> {
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
                SessionBackendMode::OutputFailure
                | SessionBackendMode::OutputGate
                | SessionBackendMode::OutputPanic => Box::pin(async { Ok(success_status()) }),
            };
            let stdout: crate::BoxedReader = match self.mode {
                SessionBackendMode::OutputFailure => Box::pin(SessionFailingReader),
                SessionBackendMode::OutputPanic => Box::pin(SessionPanickingReader),
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

    struct SessionPanickingReader;

    impl AsyncRead for SessionPanickingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            panic!("injected output task panic")
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
