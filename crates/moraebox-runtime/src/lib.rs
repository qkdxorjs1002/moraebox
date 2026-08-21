//! Backend and supervisor implementations shared by all public interfaces.

#![forbid(unsafe_code)]

mod backend;
mod budget;
mod doctor;
mod environment;
mod libkrun;
mod pool;
mod process;
mod session;
mod supervisor;
mod trace;

pub use backend::{
    Backend, BackendCapabilities, BackendController, BackendError, BoxedReader, BoxedWriter,
    CapabilitySupport, EnvironmentComponent, IsolationLevel, RootMode, SpawnedSandbox,
    StartupMetrics,
};
pub use budget::{RunBudget, RunBudgetTimeout, RunStage, StageError, StageTiming};
pub use doctor::{DoctorReport, LibraryProbe, NativeRuntimePaths, ToolProbe};
pub use libkrun::{BoxRootSource, BoxRuntimeConfig, LibkrunBackend, LibkrunConfig};
pub use pool::{PoolConfig, PoolError, PoolStats, PreparedKey, PreparedLease, PreparedPool};
pub use process::ProcessBackend;
pub use session::{
    MAX_SESSION_OUTPUT_READ_BYTES, SessionError, SessionHandle, SessionIoFailure,
    SessionIoFailureKind, SessionIoStream, SessionManager, SessionStatus,
};
pub use supervisor::{RunReport, Supervisor, SupervisorError};
pub use trace::{TraceEvent, TraceKind};
