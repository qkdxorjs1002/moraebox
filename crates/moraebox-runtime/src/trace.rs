use serde::{Deserialize, Serialize};

use crate::RunStage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    PrepareStarted,
    BackendSpawnStarted,
    BackendSpawned,
    CommandStarted,
    FirstOutput,
    Timeout,
    GracefulStop,
    ForcedStop,
    ProcessExited,
    CleanupComplete,
    StageStarted,
    StageCompleted,
    StageFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub sequence: u64,
    pub elapsed_micros: u64,
    pub kind: TraceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<RunStage>,
}
