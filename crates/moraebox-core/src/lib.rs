//! Platform-neutral contracts for disposable moraebox sessions.

#![forbid(unsafe_code)]

mod output;
mod spec;
mod state;
mod storage;

pub use output::{OutputBuffer, OutputChannel, OutputChunk, OutputRead, OutputReadError};
pub use spec::{
    BoxId, DEFAULT_KILL_GRACE, DEFAULT_OUTPUT_LIMIT, DEFAULT_TIMEOUT, RunSpec, SessionId, Signal,
    TimeoutPolicy,
};
pub use state::{Lifecycle, LifecycleError, LifecycleEvent, SessionState, TerminationReason};
pub use storage::{StoragePathError, StoragePaths, resolve_cache_dir, resolve_state_dir};
