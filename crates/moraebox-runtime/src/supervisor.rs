use std::{
    future::Future,
    pin::Pin,
    process::ExitStatus,
    time::{Duration, Instant},
};

use moraebox_core::{
    Lifecycle, LifecycleEvent, OutputBuffer, OutputChannel, OutputChunk, RunSpec, SessionId,
    SessionState, Signal, TerminationReason,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
    time::{Sleep, sleep, timeout},
};

use crate::{Backend, BackendError, StartupMetrics, TraceEvent, TraceKind};

pub struct Supervisor<B> {
    backend: B,
}

impl<B> Supervisor<B>
where
    B: Backend,
{
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    #[allow(clippy::too_many_lines)]
    pub async fn run(&self, spec: RunSpec) -> Result<RunReport, SupervisorError> {
        spec.validate().map_err(BackendError::InvalidSpec)?;
        let started = Instant::now();
        let mut trace = TraceRecorder::new(started);
        let mut lifecycle = Lifecycle::default();
        lifecycle.apply(LifecycleEvent::Prepare)?;
        trace.push(TraceKind::PrepareStarted);
        lifecycle.apply(LifecycleEvent::Start)?;
        trace.push(TraceKind::BackendSpawnStarted);

        let spawned = match self.backend.spawn(&spec).await {
            Ok(spawned) => spawned,
            Err(error) => {
                lifecycle.apply(LifecycleEvent::Fail)?;
                lifecycle.apply(LifecycleEvent::CleanupComplete)?;
                return Err(error.into());
            }
        };
        lifecycle.apply(LifecycleEvent::AgentReady)?;
        trace.push(TraceKind::BackendSpawned);
        lifecycle.apply(LifecycleEvent::CommandStarted)?;
        trace.push(TraceKind::CommandStarted);
        let startup = spawned.startup;

        let mut exit = spawned.exit;
        let mut controller = Some(spawned.controller);
        let (output_sender, mut output_receiver) = mpsc::channel(64);
        let stdout_task = tokio::spawn(pump_output(
            spawned.stdout,
            spawned.stdout_channel,
            output_sender.clone(),
        ));
        let stderr_task = spawned.stderr.map(|stderr| {
            tokio::spawn(pump_output(
                stderr,
                OutputChannel::Stderr,
                output_sender.clone(),
            ))
        });
        drop(output_sender);
        let input_task = spawned.stdin.map(|mut stdin| {
            let input = spec.stdin.clone();
            tokio::spawn(async move {
                if !input.is_empty() {
                    stdin.write_all(&input).await?;
                }
                stdin.shutdown().await
            })
        });

        let mut output = OutputBuffer::new(spec.output_limit);
        let timeout = spec.timeout.duration();
        let mut timer = timeout.map(|duration| Box::pin(sleep(duration)));
        let mut output_open = true;
        let mut first_output_seen = false;
        let mut timed_out = false;
        let mut cleanup_errors = Vec::new();

        let status = loop {
            tokio::select! {
                status = &mut exit => {
                    break record_exit_result(status, &mut cleanup_errors);
                },
                chunk = output_receiver.recv(), if output_open => {
                    match chunk {
                        Some((channel, bytes)) => {
                            if !first_output_seen {
                                first_output_seen = true;
                                trace.push(TraceKind::FirstOutput);
                            }
                            output.push(channel, bytes);
                        }
                        None => { output_open = false; }
                    }
                }
                () = wait_for_timer(&mut timer), if timer.is_some() => {
                    timed_out = true;
                    lifecycle.apply(LifecycleEvent::Timeout)?;
                    trace.push(TraceKind::Timeout);
                    let term_result = bounded_control(
                        "TERM",
                        spec.kill_grace,
                        controller
                            .as_deref()
                            .expect("controller must exist before teardown")
                            .signal(Signal::Terminate),
                    )
                    .await;
                    let mut graceful_exit = None;
                    match term_result {
                        Ok(()) => {
                            trace.push(TraceKind::GracefulStop);
                            graceful_exit = wait_for_exit(
                                &mut exit,
                                spec.kill_grace,
                                &mut output_receiver,
                                &mut output,
                                &mut trace,
                                &mut first_output_seen,
                            )
                            .await;
                        }
                        Err(error) => cleanup_errors.push(error),
                    }
                    if let Some(result) = graceful_exit {
                        break record_exit_result(result, &mut cleanup_errors);
                    }

                    let force_result = bounded_control(
                        "force-stop",
                        spec.kill_grace,
                        controller
                            .as_deref()
                            .expect("controller must exist before force-stop")
                            .force_stop(),
                    )
                    .await;
                    if force_result.is_ok() {
                        trace.push(TraceKind::ForcedStop);
                    } else if let Err(error) = force_result {
                        cleanup_errors.push(error);
                    }
                    drop(controller.take());
                    if let Some(result) = wait_for_exit(
                        &mut exit,
                        spec.kill_grace,
                        &mut output_receiver,
                        &mut output,
                        &mut trace,
                        &mut first_output_seen,
                    )
                    .await
                    {
                        break record_exit_result(result, &mut cleanup_errors);
                    }
                    cleanup_errors.push(format!(
                        "backend exit did not complete within the {:?} hard cleanup deadline",
                        spec.kill_grace
                    ));
                    break None;
                }
            }
        };

        if !timed_out && status.is_some() {
            lifecycle.apply(LifecycleEvent::CommandExited)?;
        }
        if status.is_some() {
            trace.push(TraceKind::ProcessExited);
        }

        drop(controller.take());
        if let Some(task) = input_task
            && let Err(error) = abort_input_task(task).await
        {
            cleanup_errors.push(error);
        }
        cleanup_errors.extend(finish_output_tasks(stdout_task, stderr_task, spec.kill_grace).await);
        while let Ok((channel, bytes)) = output_receiver.try_recv() {
            if !first_output_seen {
                first_output_seen = true;
                trace.push(TraceKind::FirstOutput);
            }
            output.push(channel, bytes);
        }

        if !cleanup_errors.is_empty() {
            return Err(SupervisorError::Cleanup(cleanup_errors.join("; ")));
        }
        let status = status.expect("successful cleanup must include an exit status");

        lifecycle.apply(LifecycleEvent::CleanupComplete)?;
        trace.push(TraceKind::CleanupComplete);
        let all_output = output
            .read(output.earliest_cursor(), usize::MAX)
            .expect("earliest output cursor must be valid");
        let (exit_code, signal) = decode_exit_status(status);

        Ok(RunReport {
            session_id: spec.session_id,
            backend: self.backend.name().to_owned(),
            state: lifecycle.state(),
            termination_reason: lifecycle.termination_reason(),
            exit_code,
            signal,
            timed_out,
            output: all_output.chunks,
            output_earliest_cursor: output.earliest_cursor(),
            output_next_cursor: output.next_cursor(),
            output_truncated: all_output.truncated,
            elapsed_micros: duration_micros(started.elapsed()),
            startup,
            trace: trace.events,
        })
    }
}

async fn wait_for_timer(timer: &mut Option<Pin<Box<Sleep>>>) {
    if let Some(timer) = timer {
        timer.as_mut().await;
    }
}

async fn wait_for_exit(
    exit: &mut crate::backend::ExitFuture,
    deadline: Duration,
    output_receiver: &mut mpsc::Receiver<(OutputChannel, Vec<u8>)>,
    output: &mut OutputBuffer,
    trace: &mut TraceRecorder,
    first_output_seen: &mut bool,
) -> Option<std::io::Result<ExitStatus>> {
    let deadline = sleep(deadline);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            status = &mut *exit => return Some(status),
            chunk = output_receiver.recv() => {
                if let Some((channel, bytes)) = chunk {
                    if !*first_output_seen {
                        *first_output_seen = true;
                        trace.push(TraceKind::FirstOutput);
                    }
                    output.push(channel, bytes);
                }
            }
            () = &mut deadline => return None,
        }
    }
}

async fn bounded_control<F>(
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

fn record_exit_result(
    result: std::io::Result<ExitStatus>,
    errors: &mut Vec<String>,
) -> Option<ExitStatus> {
    match result {
        Ok(status) => Some(status),
        Err(error) => {
            errors.push(format!("backend exit wait failed: {error}"));
            None
        }
    }
}

async fn abort_input_task(task: JoinHandle<std::io::Result<()>>) -> Result<(), String> {
    if task.is_finished() {
        return flatten_io_task("stdin", task.await);
    }
    task.abort();
    match task.await {
        Err(error) if error.is_cancelled() => Ok(()),
        result => flatten_io_task("stdin", result),
    }
}

async fn finish_output_tasks(
    stdout: JoinHandle<std::io::Result<()>>,
    stderr: Option<JoinHandle<std::io::Result<()>>>,
    deadline: Duration,
) -> Vec<String> {
    let stdout = finish_io_task("stdout", stdout, deadline);
    let stderr = async {
        match stderr {
            Some(task) => finish_io_task("stderr", task, deadline).await,
            None => Ok(()),
        }
    };
    let (stdout, stderr) = tokio::join!(stdout, stderr);
    [stdout, stderr]
        .into_iter()
        .filter_map(Result::err)
        .collect()
}

async fn finish_io_task(
    name: &'static str,
    mut task: JoinHandle<std::io::Result<()>>,
    deadline: Duration,
) -> Result<(), String> {
    if let Ok(result) = timeout(deadline, &mut task).await {
        flatten_io_task(name, result)
    } else {
        task.abort();
        let _ = task.await;
        Err(format!(
            "{name} pump did not stop within the {deadline:?} cleanup deadline"
        ))
    }
}

fn flatten_io_task(
    name: &'static str,
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<(), String> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("{name} pump failed: {error}")),
        Err(error) => Err(format!("{name} pump task failed: {error}")),
    }
}

async fn pump_output<R>(
    mut reader: R,
    channel: OutputChannel,
    sender: mpsc::Sender<(OutputChannel, Vec<u8>)>,
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
        if sender
            .send((channel, buffer[..count].to_vec()))
            .await
            .is_err()
        {
            return Ok(());
        }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub session_id: SessionId,
    pub backend: String,
    pub state: SessionState,
    pub termination_reason: Option<TerminationReason>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub output: Vec<OutputChunk>,
    pub output_earliest_cursor: u64,
    pub output_next_cursor: u64,
    pub output_truncated: bool,
    pub elapsed_micros: u64,
    pub startup: StartupMetrics,
    pub trace: Vec<TraceEvent>,
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
        });
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Lifecycle(#[from] moraebox_core::LifecycleError),
    #[error("supervisor I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend cleanup failed: {0}")]
    Cleanup(String),
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use async_trait::async_trait;
    use moraebox_core::{RunSpec, TimeoutPolicy};
    use tokio::{io::ReadBuf, sync::Notify};

    use super::*;
    use crate::ProcessBackend;

    #[tokio::test]
    async fn captures_stdout_stderr_and_exit_code() {
        let supervisor = Supervisor::new(ProcessBackend);
        let spec = RunSpec::command(output_and_exit_command());
        let report = supervisor.run(spec).await.unwrap();
        assert_eq!(report.exit_code, Some(7));
        assert_eq!(report.state, SessionState::Dead);
        assert!(report.output.iter().any(|chunk| {
            chunk.channel == OutputChannel::Stdout && chunk.data.starts_with(b"out")
        }));
        assert!(report.output.iter().any(|chunk| {
            chunk.channel == OutputChannel::Stderr && chunk.data.starts_with(b"err")
        }));
        let trace_kinds = report
            .trace
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        let command_started = trace_kinds
            .iter()
            .position(|kind| *kind == TraceKind::CommandStarted)
            .unwrap();
        let first_output = trace_kinds
            .iter()
            .position(|kind| *kind == TraceKind::FirstOutput)
            .unwrap();
        assert!(command_started < first_output);
    }

    #[tokio::test]
    async fn enforces_timeout_and_kills_the_process_group() {
        let supervisor = Supervisor::new(ProcessBackend);
        let mut spec = RunSpec::command(long_running_command());
        spec.timeout = TimeoutPolicy::Limited(30);
        spec.kill_grace = Duration::from_millis(30);
        let report = supervisor.run(spec).await.unwrap();
        assert!(report.timed_out);
        assert_eq!(report.termination_reason, Some(TerminationReason::TimedOut));
    }

    #[tokio::test]
    async fn closes_stdin_after_writing() {
        let supervisor = Supervisor::new(ProcessBackend);
        let mut spec = RunSpec::command(stdin_echo_command());
        spec.stdin = b"input".to_vec();
        let report = supervisor.run(spec).await.unwrap();
        assert!(report.output.iter().any(|chunk| {
            chunk.channel == OutputChannel::Stdout && chunk.data.starts_with(b"input")
        }));
    }

    #[tokio::test]
    async fn term_and_force_errors_do_not_skip_exit_cleanup() {
        let state = Arc::new(FailingTeardownState::default());
        let supervisor = Supervisor::new(FailingTeardownBackend {
            state: Arc::clone(&state),
        });
        let mut spec = RunSpec::command(["fake"]);
        spec.timeout = TimeoutPolicy::Limited(1);
        spec.kill_grace = Duration::from_millis(50);

        let error = supervisor.run(spec).await.unwrap_err();

        let message = error.to_string();
        assert!(message.contains("TERM failed"));
        assert!(message.contains("force-stop failed"));
        assert_eq!(state.term_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.force_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.exit_cleanups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn output_pump_error_is_reported() {
        let supervisor = Supervisor::new(OutputFailureBackend);

        let error = supervisor
            .run(RunSpec::command(["fake"]))
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("stdout pump failed: injected read failure")
        );
    }

    #[derive(Default)]
    struct FailingTeardownState {
        term_calls: AtomicUsize,
        force_calls: AtomicUsize,
        exit_cleanups: AtomicUsize,
        forced: Notify,
    }

    struct FailingTeardownBackend {
        state: Arc<FailingTeardownState>,
    }

    #[async_trait]
    impl Backend for FailingTeardownBackend {
        fn name(&self) -> &'static str {
            "failing-teardown"
        }

        async fn spawn(&self, _spec: &RunSpec) -> Result<crate::SpawnedSandbox, BackendError> {
            let exit_state = Arc::clone(&self.state);
            let exit = Box::pin(async move {
                exit_state.forced.notified().await;
                exit_state.exit_cleanups.fetch_add(1, Ordering::SeqCst);
                Ok(success_status())
            });
            Ok(crate::SpawnedSandbox {
                stdin: None,
                stdout: Box::pin(tokio::io::empty()),
                stdout_channel: OutputChannel::Stdout,
                stderr: None,
                exit,
                controller: Box::new(FailingTeardownController {
                    state: Arc::clone(&self.state),
                }),
                startup: StartupMetrics::default(),
            })
        }
    }

    struct FailingTeardownController {
        state: Arc<FailingTeardownState>,
    }

    #[async_trait]
    impl crate::BackendController for FailingTeardownController {
        async fn signal(&self, signal: Signal) -> Result<(), BackendError> {
            assert_eq!(signal, Signal::Terminate);
            self.state.term_calls.fetch_add(1, Ordering::SeqCst);
            Err(BackendError::Control("injected TERM failure".into()))
        }

        async fn force_stop(&self) -> Result<(), BackendError> {
            self.state.force_calls.fetch_add(1, Ordering::SeqCst);
            self.state.forced.notify_one();
            Err(BackendError::Control("injected force-stop failure".into()))
        }
    }

    struct OutputFailureBackend;

    #[async_trait]
    impl Backend for OutputFailureBackend {
        fn name(&self) -> &'static str {
            "output-failure"
        }

        async fn spawn(&self, _spec: &RunSpec) -> Result<crate::SpawnedSandbox, BackendError> {
            Ok(crate::SpawnedSandbox {
                stdin: None,
                stdout: Box::pin(FailingReader),
                stdout_channel: OutputChannel::Stdout,
                stderr: None,
                exit: Box::pin(async { Ok(success_status()) }),
                controller: Box::new(NoopController),
                startup: StartupMetrics::default(),
            })
        }
    }

    struct FailingReader;

    impl AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("injected read failure")))
        }
    }

    struct NoopController;

    #[async_trait]
    impl crate::BackendController for NoopController {
        async fn signal(&self, _signal: Signal) -> Result<(), BackendError> {
            Ok(())
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
    fn output_and_exit_command() -> Vec<String> {
        ["/bin/sh", "-c", "printf out; printf err >&2; exit 7"]
            .map(String::from)
            .into()
    }

    #[cfg(windows)]
    fn output_and_exit_command() -> Vec<String> {
        vec![
            windows_system_executable("cmd.exe"),
            "/D".into(),
            "/C".into(),
            "echo out&echo err>&2&exit /b 7".into(),
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

    #[cfg(unix)]
    fn stdin_echo_command() -> Vec<String> {
        ["/bin/sh", "-c", "cat"].map(String::from).into()
    }

    #[cfg(windows)]
    fn stdin_echo_command() -> Vec<String> {
        vec![
            windows_system_executable("findstr.exe"),
            "/R".into(),
            ".*".into(),
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
