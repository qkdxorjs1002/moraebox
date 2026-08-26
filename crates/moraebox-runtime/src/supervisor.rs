use std::{future::Future, sync::Arc};

use moraebox_core::{OutputChunk, RunSpec, SessionId, SessionState, Signal, TerminationReason};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    Backend, BackendError, RunBudget, RunStage, SessionError, SessionIoFailure, StageTiming,
    StartupMetrics, TraceEvent, session::start_session,
};

pub struct Supervisor<B> {
    backend: Arc<B>,
}

impl<B> Supervisor<B>
where
    B: Backend,
{
    pub fn new(backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn backend_capabilities(&self) -> crate::BackendCapabilities {
        self.backend.capabilities()
    }

    pub async fn run(&self, spec: RunSpec) -> Result<RunReport, SupervisorError> {
        let budget = RunBudget::new(spec.timeout);
        self.run_with_budget(spec, budget).await
    }

    pub async fn run_with_budget(
        &self,
        spec: RunSpec,
        budget: RunBudget,
    ) -> Result<RunReport, SupervisorError> {
        let session = self.start_with_budget(spec, budget).await?;
        let _ = session.close_stdin().await;
        let status = wait_for_session(&session).await?;
        collect_report(&session, status).await
    }

    pub async fn run_with_budget_and_signal<F>(
        &self,
        spec: RunSpec,
        budget: RunBudget,
        signal: F,
    ) -> Result<RunReport, SupervisorError>
    where
        F: Future<Output = std::io::Result<Signal>>,
    {
        let signal_grace = spec.kill_grace;
        let session = self.start_with_budget(spec, budget).await?;
        let _ = session.close_stdin().await;
        tokio::pin!(signal);
        let status = tokio::select! {
            status = session.wait() => map_session_wait(&session, status)?,
            signal = &mut signal => {
                let signal = match signal {
                    Ok(signal) => signal,
                    Err(error) => {
                        stop_and_wait(&session).await?;
                        return Err(error.into());
                    }
                };
                if let Err(error) = session.signal(signal).await
                    && session.status().state != SessionState::Dead
                {
                    stop_and_wait(&session).await?;
                    return Err(error.into());
                }
                match tokio::time::timeout(signal_grace, session.wait()).await {
                    Ok(status) => map_session_wait(&session, status)?,
                    Err(_) => stop_and_wait(&session).await?,
                }
            }
        };
        collect_report(&session, status).await
    }

    async fn start_with_budget(
        &self,
        spec: RunSpec,
        budget: RunBudget,
    ) -> Result<crate::SessionHandle, SupervisorError> {
        start_session(Arc::clone(&self.backend), spec, budget)
            .await
            .map_err(map_session_start)
    }
}

async fn wait_for_session(
    session: &crate::SessionHandle,
) -> Result<crate::SessionStatus, SupervisorError> {
    map_session_wait(session, session.wait().await)
}

async fn stop_and_wait(
    session: &crate::SessionHandle,
) -> Result<crate::SessionStatus, SupervisorError> {
    if session.status().state != SessionState::Dead {
        let _ = session.stop().await;
    }
    let status = wait_for_session(session).await?;
    if let Some(details) = session.terminal_error() {
        return Err(SupervisorError::Cleanup(details));
    }
    Ok(status)
}

fn map_session_wait(
    session: &crate::SessionHandle,
    result: Result<crate::SessionStatus, SessionError>,
) -> Result<crate::SessionStatus, SupervisorError> {
    let status = match result {
        Ok(status) => status,
        Err(SessionError::Io(failure)) => return Err(SupervisorError::SessionIo(failure)),
        Err(error) => {
            if let Some(details) = session.terminal_error() {
                return Err(SupervisorError::Cleanup(details));
            }
            return Err(error.into());
        }
    };
    Ok(status)
}

async fn collect_report(
    session: &crate::SessionHandle,
    status: crate::SessionStatus,
) -> Result<RunReport, SupervisorError> {
    if let Some(details) = session.terminal_error() {
        return Err(SupervisorError::Cleanup(details));
    }
    let (all_output, output_earliest_cursor, output_next_cursor) = session.retained_output().await;

    Ok(RunReport {
        session_id: status.session_id,
        backend: status.backend,
        state: status.state,
        termination_reason: status.termination_reason,
        exit_code: status.exit_code,
        signal: status.signal,
        timed_out: status.timed_out,
        output: all_output.chunks,
        output_earliest_cursor,
        output_next_cursor,
        output_truncated: all_output.truncated,
        elapsed_micros: status.elapsed_micros,
        startup: session.startup(),
        trace: session.trace(),
        stages: session.stage_timings(),
        failure_stage: session.failure_stage(),
    })
}

fn map_session_start(error: SessionError) -> SupervisorError {
    match error {
        SessionError::Backend(error) => SupervisorError::Backend(error),
        SessionError::Lifecycle(error) => SupervisorError::Lifecycle(error),
        error => SupervisorError::Session(error),
    }
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
    #[serde(default)]
    pub stages: Vec<StageTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_stage: Option<RunStage>,
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Lifecycle(#[from] moraebox_core::LifecycleError),
    #[error("supervisor I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    SessionIo(#[from] SessionIoFailure),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("backend cleanup failed: {0}")]
    Cleanup(String),
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        process::ExitStatus,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use async_trait::async_trait;
    use moraebox_core::{OutputChannel, RunSpec, Signal, TimeoutPolicy};
    use tokio::{
        io::{AsyncRead, ReadBuf},
        sync::Notify,
    };

    use super::*;
    use crate::{ProcessBackend, SessionManager, TraceKind};

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
        assert!(
            report
                .stages
                .iter()
                .any(|timing| timing.stage == RunStage::ProcessSpawn)
        );
        assert!(
            report
                .stages
                .iter()
                .any(|timing| timing.stage == RunStage::CommandRun)
        );
        assert_eq!(report.failure_stage, None);
        assert!(report.trace.iter().any(|event| {
            event.kind == TraceKind::StageCompleted && event.stage == Some(RunStage::ProcessSpawn)
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
    async fn one_shot_and_session_share_exit_and_trace_semantics() {
        let spec = RunSpec::command(output_and_exit_command());
        let report = Supervisor::new(ProcessBackend)
            .run(spec.clone())
            .await
            .unwrap();
        let session = SessionManager::new(Arc::new(ProcessBackend))
            .start(spec)
            .await
            .unwrap();
        let _ = session.close_stdin().await;
        let status = session.wait().await.unwrap();

        assert_eq!(report.session_id, status.session_id);
        assert_eq!(report.backend, status.backend);
        assert_eq!(report.state, status.state);
        assert_eq!(report.termination_reason, status.termination_reason);
        assert_eq!(report.exit_code, status.exit_code);
        assert_eq!(report.signal, status.signal);
        assert_eq!(report.timed_out, status.timed_out);
        for trace in [report.trace, session.trace()] {
            let kinds = trace
                .iter()
                .filter(|event| event.stage.is_none())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert_eq!(
                &kinds[..4],
                &[
                    TraceKind::PrepareStarted,
                    TraceKind::BackendSpawnStarted,
                    TraceKind::BackendSpawned,
                    TraceKind::CommandStarted,
                ]
            );
            assert!(kinds.contains(&TraceKind::FirstOutput));
            assert!(kinds.contains(&TraceKind::ProcessExited));
            assert_eq!(kinds.last(), Some(&TraceKind::CleanupComplete));
        }
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
        assert_eq!(report.failure_stage, Some(RunStage::CommandRun));
        assert!(report.trace.iter().any(|event| {
            event.kind == TraceKind::StageFailed && event.stage == Some(RunStage::CommandRun)
        }));
    }

    #[tokio::test]
    async fn one_shot_and_session_share_timeout_semantics() {
        let mut spec = RunSpec::command(long_running_command());
        spec.timeout = TimeoutPolicy::Limited(30);
        spec.kill_grace = Duration::from_millis(30);
        let report = Supervisor::new(ProcessBackend)
            .run(spec.clone())
            .await
            .unwrap();
        let session = SessionManager::new(Arc::new(ProcessBackend))
            .start(spec)
            .await
            .unwrap();
        let status = session.wait().await.unwrap();

        assert_eq!(report.state, status.state);
        assert_eq!(report.termination_reason, status.termination_reason);
        assert_eq!(report.exit_code, status.exit_code);
        assert_eq!(report.signal, status.signal);
        assert_eq!(report.timed_out, status.timed_out);
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

        assert!(matches!(
            error,
            SupervisorError::SessionIo(SessionIoFailure {
                stream: crate::SessionIoStream::Stdout,
                kind: crate::SessionIoFailureKind::Operation,
                io_kind: Some(io::ErrorKind::Other),
                ref message,
            }) if message == "injected read failure"
        ));
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

        fn capabilities(&self) -> crate::BackendCapabilities {
            ProcessBackend::CAPABILITIES
        }

        async fn spawn(
            &self,
            _spec: &RunSpec,
            _budget: &RunBudget,
        ) -> Result<crate::SpawnedSandbox, BackendError> {
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

        fn capabilities(&self) -> crate::BackendCapabilities {
            ProcessBackend::CAPABILITIES
        }

        async fn spawn(
            &self,
            _spec: &RunSpec,
            _budget: &RunBudget,
        ) -> Result<crate::SpawnedSandbox, BackendError> {
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
