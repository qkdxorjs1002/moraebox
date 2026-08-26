//! Platform-neutral contracts for disposable moraebox sessions.

#![forbid(unsafe_code)]

mod output;
mod spec;
mod state;
mod storage;

pub use output::{
    OutputBuffer, OutputChannel, OutputChunk, OutputRead, OutputReadError, OutputReadSnapshot,
};
pub use spec::{
    BoxId, CopyInSpec, CopyOutSpec, DEFAULT_COPY_LIMIT, DEFAULT_KILL_GRACE, DEFAULT_OUTPUT_LIMIT,
    DEFAULT_TIMEOUT, ImagePullPolicy, MAX_COPY_LIMIT, MAX_KILL_GRACE, MAX_NETWORK_CIDRS,
    MAX_NETWORK_DOMAINS, MAX_OUTPUT_LIMIT, MAX_PUBLISH_REQUESTS, NetworkMode, NetworkPolicy,
    PublishProtocol, PublishRequest, RunSpec, SessionId, Signal, TimeoutPolicy,
    WORKSPACE_DIFF_GUEST_PATH, WorkspaceMode,
};
pub use state::{Lifecycle, LifecycleError, LifecycleEvent, SessionState, TerminationReason};
pub use storage::{
    StoragePathError, StoragePaths, StorageRootError, ensure_private_storage_root,
    resolve_cache_dir, resolve_state_dir,
};
