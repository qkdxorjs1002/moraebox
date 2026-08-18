use std::{
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
    time::{Sleep, sleep},
};

use crate::{Backend, BackendError, TraceEvent, TraceKind};

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

        let mut exit = spawned.exit;
        let controller = spawned.controller;
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
        let mut timed_out = false;

        let status = loop {
            tokio::select! {
                status = &mut exit => break status?,
                chunk = output_receiver.recv(), if output_open => {
                    match chunk {
                        Some((channel, bytes)) => { output.push(channel, bytes); }
                        None => { output_open = false; }
                    }
                }
                () = wait_for_timer(&mut timer), if timer.is_some() => {
                    timed_out = true;
                    lifecycle.apply(LifecycleEvent::Timeout)?;
                    trace.push(TraceKind::Timeout);
                    controller.signal(Signal::Terminate).await?;
                    trace.push(TraceKind::GracefulStop);
                    break wait_with_grace(
                        &mut exit,
                        &*controller,
                        spec.kill_grace,
                        &mut output_receiver,
                        &mut output,
                        &mut trace,
                    ).await?;
                }
            }
        };

        if !timed_out {
            lifecycle.apply(LifecycleEvent::CommandExited)?;
        }
        trace.push(TraceKind::ProcessExited);

        let _ = input_task.map(|task| task.abort());
        let _ = stdout_task.await;
        if let Some(task) = stderr_task {
            let _ = task.await;
        }
        while let Ok((channel, bytes)) = output_receiver.try_recv() {
            output.push(channel, bytes);
        }

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
            trace: trace.events,
        })
    }
}

async fn wait_for_timer(timer: &mut Option<Pin<Box<Sleep>>>) {
    if let Some(timer) = timer {
        timer.as_mut().await;
    }
}

async fn wait_with_grace(
    exit: &mut crate::backend::ExitFuture,
    controller: &dyn crate::BackendController,
    grace: Duration,
    output_receiver: &mut mpsc::Receiver<(OutputChannel, Vec<u8>)>,
    output: &mut OutputBuffer,
    trace: &mut TraceRecorder,
) -> Result<ExitStatus, SupervisorError> {
    let grace_timer = sleep(grace);
    tokio::pin!(grace_timer);
    loop {
        tokio::select! {
            status = &mut *exit => return Ok(status?),
            chunk = output_receiver.recv() => {
                if let Some((channel, bytes)) = chunk {
                    output.push(channel, bytes);
                }
            }
            () = &mut grace_timer => {
                controller.force_stop().await?;
                trace.push(TraceKind::ForcedStop);
                return Ok(exit.await?);
            }
        }
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moraebox_core::{RunSpec, TimeoutPolicy};

    use super::*;
    use crate::ProcessBackend;

    #[tokio::test]
    async fn captures_stdout_stderr_and_exit_code() {
        let supervisor = Supervisor::new(ProcessBackend);
        let spec = RunSpec::command(["/bin/sh", "-c", "printf out; printf err >&2; exit 7"]);
        let report = supervisor.run(spec).await.unwrap();
        assert_eq!(report.exit_code, Some(7));
        assert_eq!(report.state, SessionState::Dead);
        assert!(report.output.iter().any(|chunk| chunk.data == b"out"));
        assert!(report.output.iter().any(|chunk| chunk.data == b"err"));
    }

    #[tokio::test]
    async fn enforces_timeout_and_kills_the_process_group() {
        let supervisor = Supervisor::new(ProcessBackend);
        let mut spec = RunSpec::command(["/bin/sh", "-c", "sleep 30"]);
        spec.timeout = TimeoutPolicy::Limited(30);
        spec.kill_grace = Duration::from_millis(30);
        let report = supervisor.run(spec).await.unwrap();
        assert!(report.timed_out);
        assert_eq!(report.termination_reason, Some(TerminationReason::TimedOut));
    }

    #[tokio::test]
    async fn closes_stdin_after_writing() {
        let supervisor = Supervisor::new(ProcessBackend);
        let mut spec = RunSpec::command(["/bin/sh", "-c", "cat"]);
        spec.stdin = b"input".to_vec();
        let report = supervisor.run(spec).await.unwrap();
        assert!(report.output.iter().any(|chunk| chunk.data == b"input"));
    }
}
