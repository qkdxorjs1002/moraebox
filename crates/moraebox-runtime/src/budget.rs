use std::{
    error::Error,
    fmt,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use moraebox_core::TimeoutPolicy;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::timeout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStage {
    ImagePull,
    WorkspacePrepare,
    ProcessSpawn,
    NetworkSetup,
    RootPrepare,
    BoxLock,
    BoxRepair,
    CacheLookup,
    BaseDiskPrepare,
    EphemeralDiskClone,
    HelperSpawn,
    CommandRun,
}

impl fmt::Display for RunStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ImagePull => "image_pull",
            Self::WorkspacePrepare => "workspace_prepare",
            Self::ProcessSpawn => "process_spawn",
            Self::NetworkSetup => "network_setup",
            Self::RootPrepare => "root_prepare",
            Self::BoxLock => "box_lock",
            Self::BoxRepair => "box_repair",
            Self::CacheLookup => "cache_lookup",
            Self::BaseDiskPrepare => "base_disk_prepare",
            Self::EphemeralDiskClone => "ephemeral_disk_clone",
            Self::HelperSpawn => "helper_spawn",
            Self::CommandRun => "command_run",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: RunStage,
    pub elapsed_micros: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageEventKind {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StageEvent {
    pub kind: StageEventKind,
    pub stage: RunStage,
    pub elapsed_micros: u64,
}

#[derive(Debug, Clone)]
pub struct RunBudget {
    started: Instant,
    deadline: Option<Instant>,
    limit: Option<Duration>,
    state: Arc<Mutex<BudgetState>>,
    progress: Option<StageProgressReporter>,
}

#[derive(Clone)]
struct StageProgressReporter(Arc<dyn Fn(RunStage) + Send + Sync>);

impl fmt::Debug for StageProgressReporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StageProgressReporter(..)")
    }
}

#[derive(Debug, Default)]
struct BudgetState {
    timings: Vec<StageTiming>,
    events: Vec<StageEvent>,
    failure_stage: Option<RunStage>,
}

impl RunBudget {
    pub fn new(policy: TimeoutPolicy) -> Self {
        let started = Instant::now();
        let limit = policy.duration();
        Self {
            started,
            deadline: limit.map(|duration| started + duration),
            limit,
            state: Arc::new(Mutex::new(BudgetState::default())),
            progress: None,
        }
    }

    #[must_use]
    pub fn with_progress(mut self, progress: impl Fn(RunStage) + Send + Sync + 'static) -> Self {
        self.progress = Some(StageProgressReporter(Arc::new(progress)));
        self
    }

    pub fn remaining(&self, stage: RunStage) -> Result<Option<Duration>, RunBudgetTimeout> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        let now = Instant::now();
        if now >= deadline {
            self.record_failure(stage);
            return Err(self.timeout_error(stage));
        }
        Ok(Some(deadline - now))
    }

    pub async fn run<F, T, E>(&self, stage: RunStage, future: F) -> Result<T, StageError<E>>
    where
        F: Future<Output = Result<T, E>>,
    {
        let remaining = self.remaining(stage).map_err(StageError::Timeout)?;
        let stage_started = Instant::now();
        self.record_event(StageEventKind::Started, stage);
        let result = if let Some(remaining) = remaining {
            if let Ok(result) = timeout(remaining, future).await {
                result.map_err(|source| StageError::Failed { stage, source })
            } else {
                self.record_stage_failure(stage, stage_started);
                return Err(StageError::Timeout(self.timeout_error(stage)));
            }
        } else {
            future
                .await
                .map_err(|source| StageError::Failed { stage, source })
        };
        self.finish_result(stage, stage_started, result)
    }

    /// Records a stage whose implementation enforces the supplied remaining deadline itself.
    pub async fn observe<F, T, E>(&self, stage: RunStage, future: F) -> Result<T, StageError<E>>
    where
        F: Future<Output = Result<T, E>>,
    {
        self.remaining(stage).map_err(StageError::Timeout)?;
        let stage_started = Instant::now();
        self.record_event(StageEventKind::Started, stage);
        let result = future
            .await
            .map_err(|source| StageError::Failed { stage, source });
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.record_stage_failure(stage, stage_started);
            return Err(StageError::Timeout(self.timeout_error(stage)));
        }
        self.finish_result(stage, stage_started, result)
    }

    pub fn run_sync<T, E>(
        &self,
        stage: RunStage,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, StageError<E>> {
        self.remaining(stage).map_err(StageError::Timeout)?;
        let stage_started = Instant::now();
        self.record_event(StageEventKind::Started, stage);
        let result = operation().map_err(|source| StageError::Failed { stage, source });
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.record_stage_failure(stage, stage_started);
            return Err(StageError::Timeout(self.timeout_error(stage)));
        }
        self.finish_result(stage, stage_started, result)
    }

    pub fn timings(&self) -> Vec<StageTiming> {
        self.state
            .lock()
            .expect("run budget lock must not be poisoned")
            .timings
            .clone()
    }

    pub fn failure_stage(&self) -> Option<RunStage> {
        self.state
            .lock()
            .expect("run budget lock must not be poisoned")
            .failure_stage
    }

    pub(crate) fn started(&self) -> Instant {
        self.started
    }

    pub(crate) fn events(&self) -> Vec<StageEvent> {
        self.state
            .lock()
            .expect("run budget lock must not be poisoned")
            .events
            .clone()
    }

    pub(crate) fn begin_stage(&self, stage: RunStage) -> Instant {
        self.record_event(StageEventKind::Started, stage);
        Instant::now()
    }

    pub(crate) fn complete_stage(&self, stage: RunStage, started: Instant) {
        self.record_timing(stage, started);
        self.record_event(StageEventKind::Completed, stage);
    }

    pub(crate) fn fail_stage(&self, stage: RunStage, started: Instant) {
        self.record_stage_failure(stage, started);
    }

    fn finish_result<T, E>(
        &self,
        stage: RunStage,
        started: Instant,
        result: Result<T, StageError<E>>,
    ) -> Result<T, StageError<E>> {
        match result {
            Ok(value) => {
                self.record_timing(stage, started);
                self.record_event(StageEventKind::Completed, stage);
                Ok(value)
            }
            Err(error) => {
                self.record_stage_failure(stage, started);
                Err(error)
            }
        }
    }

    fn record_stage_failure(&self, stage: RunStage, started: Instant) {
        self.record_timing(stage, started);
        self.record_failure(stage);
        self.record_event(StageEventKind::Failed, stage);
    }

    fn record_timing(&self, stage: RunStage, started: Instant) {
        self.state
            .lock()
            .expect("run budget lock must not be poisoned")
            .timings
            .push(StageTiming {
                stage,
                elapsed_micros: duration_micros(started.elapsed()),
            });
    }

    fn record_failure(&self, stage: RunStage) {
        self.state
            .lock()
            .expect("run budget lock must not be poisoned")
            .failure_stage
            .get_or_insert(stage);
    }

    fn record_event(&self, kind: StageEventKind, stage: RunStage) {
        if let (StageEventKind::Started, Some(progress)) = (kind, self.progress.as_ref()) {
            (progress.0)(stage);
        }
        let elapsed_micros = duration_micros(self.started.elapsed());
        self.state
            .lock()
            .expect("run budget lock must not be poisoned")
            .events
            .push(StageEvent {
                kind,
                stage,
                elapsed_micros,
            });
    }

    fn timeout_error(&self, stage: RunStage) -> RunBudgetTimeout {
        RunBudgetTimeout {
            stage,
            limit: self.limit.expect("limited budget must have a timeout"),
        }
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("run timed out after {limit:?} during {stage}")]
pub struct RunBudgetTimeout {
    pub stage: RunStage,
    pub limit: Duration,
}

#[derive(Debug)]
pub enum StageError<E> {
    Timeout(RunBudgetTimeout),
    Failed { stage: RunStage, source: E },
}

impl<E: fmt::Display> fmt::Display for StageError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(error) => error.fmt(formatter),
            Self::Failed { stage, source } => write!(formatter, "{stage} failed: {source}"),
        }
    }
}

impl<E: Error + 'static> Error for StageError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeout(error) => Some(error),
            Self::Failed { source, .. } => Some(source),
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stages_share_one_deadline() {
        let budget = RunBudget::new(TimeoutPolicy::Limited(40));
        budget
            .run(RunStage::ImagePull, async {
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok::<_, std::convert::Infallible>(())
            })
            .await
            .unwrap();

        let error = budget
            .run(RunStage::CommandRun, async {
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok::<_, std::convert::Infallible>(())
            })
            .await
            .unwrap_err();

        assert!(matches!(error, StageError::Timeout(_)));
        assert_eq!(budget.failure_stage(), Some(RunStage::CommandRun));
        assert_eq!(budget.timings().len(), 2);
    }

    #[tokio::test]
    async fn unlimited_budget_never_synthesizes_a_deadline() {
        let budget = RunBudget::new(TimeoutPolicy::Unlimited);
        assert_eq!(budget.remaining(RunStage::CommandRun).unwrap(), None);
        budget
            .run(RunStage::CommandRun, async {
                Ok::<_, std::convert::Infallible>(())
            })
            .await
            .unwrap();
        assert_eq!(budget.failure_stage(), None);
    }

    #[tokio::test]
    async fn progress_reports_each_started_stage_once() {
        let stages = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&stages);
        let budget = RunBudget::new(TimeoutPolicy::Unlimited).with_progress(move |stage| {
            observed.lock().unwrap().push(stage);
        });

        budget
            .run(RunStage::ImagePull, async {
                Ok::<_, std::convert::Infallible>(())
            })
            .await
            .unwrap();
        budget
            .run_sync(RunStage::HelperSpawn, || {
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();

        assert_eq!(
            *stages.lock().unwrap(),
            vec![RunStage::ImagePull, RunStage::HelperSpawn]
        );
    }
}
