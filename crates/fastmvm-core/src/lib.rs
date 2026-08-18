//! Platform-neutral contracts for disposable fastmvm sessions.

#![forbid(unsafe_code)]

mod output;
mod spec;
mod state;

pub use output::{OutputBuffer, OutputChannel, OutputChunk, OutputRead, OutputReadError};
pub use spec::{
    DEFAULT_KILL_GRACE, DEFAULT_OUTPUT_LIMIT, DEFAULT_TIMEOUT, RunSpec, SessionId, Signal,
    TimeoutPolicy,
};
pub use state::{Lifecycle, LifecycleError, LifecycleEvent, SessionState, TerminationReason};
