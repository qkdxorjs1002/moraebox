//! Backend and supervisor implementations shared by all public interfaces.

#![forbid(unsafe_code)]

mod backend;
mod doctor;
mod libkrun;
mod pool;
mod process;
mod session;
mod supervisor;
mod trace;

pub use backend::{
    Backend, BackendController, BackendError, BoxedReader, BoxedWriter, SpawnedSandbox,
};
pub use doctor::{DoctorReport, LibraryProbe, ToolProbe};
pub use libkrun::{LibkrunBackend, LibkrunConfig};
pub use pool::{PoolConfig, PoolError, PoolStats, PreparedKey, PreparedLease, PreparedPool};
pub use process::ProcessBackend;
pub use session::{SessionError, SessionHandle, SessionManager, SessionStatus};
pub use supervisor::{RunReport, Supervisor, SupervisorError};
pub use trace::{TraceEvent, TraceKind};
