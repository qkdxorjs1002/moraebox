use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub sequence: u64,
    pub elapsed_micros: u64,
    pub kind: TraceKind,
}
