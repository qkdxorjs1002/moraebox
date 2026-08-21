use std::{future::Future, io, pin::Pin, process::ExitStatus, time::Duration};

use async_trait::async_trait;
use moraebox_core::{OutputChannel, RunSpec, Signal};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{RunBudget, RunStage, StageError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentComponent {
    Name,
    Value,
}

impl std::fmt::Display for EnvironmentComponent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Name => "name",
            Self::Value => "value",
        })
    }
}

pub type BoxedReader = Pin<Box<dyn AsyncRead + Send>>;
pub type BoxedWriter = Pin<Box<dyn AsyncWrite + Send>>;
pub type ExitFuture = Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    HostProcess,
    MicroVm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub isolation: IsolationLevel,
    pub tty: CapabilitySupport,
    pub network: CapabilitySupport,
    pub box_persistence: CapabilitySupport,
    pub workspace: CapabilitySupport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    Unsupported,
    Supported,
}

impl CapabilitySupport {
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

pub struct SpawnedSandbox {
    pub stdin: Option<BoxedWriter>,
    pub stdout: BoxedReader,
    pub stdout_channel: OutputChannel,
    pub stderr: Option<BoxedReader>,
    pub exit: ExitFuture,
    pub controller: Box<dyn BackendController>,
    pub startup: StartupMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootMode {
    Directory,
    StaticDisk,
    Ephemeral,
    Persistent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupMetrics {
    #[serde(default)]
    pub resolved_image_digest: Option<String>,
    pub root_mode: Option<RootMode>,
    #[serde(default)]
    pub prepared_pool_hit: Option<bool>,
    #[serde(default)]
    pub prepared_lease_micros: Option<u64>,
    pub network_setup_micros: Option<u64>,
    pub root_prepare_micros: Option<u64>,
    pub cache_lookup_micros: Option<u64>,
    pub box_lock_micros: Option<u64>,
    pub base_prepare_micros: Option<u64>,
    pub disk_clone_micros: Option<u64>,
    pub repair_micros: Option<u64>,
    pub helper_spawn_micros: Option<u64>,
}

#[async_trait]
pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> BackendCapabilities;

    async fn spawn(
        &self,
        spec: &RunSpec,
        budget: &RunBudget,
    ) -> Result<SpawnedSandbox, BackendError>;
}

#[async_trait]
pub trait BackendController: Send + Sync {
    async fn signal(&self, signal: Signal) -> Result<(), BackendError>;

    async fn resize(&self, _rows: u16, _cols: u16) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("terminal resize"))
    }

    async fn force_stop(&self) -> Result<(), BackendError> {
        self.signal(Signal::Kill).await
    }
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("invalid run specification: {0}")]
    InvalidSpec(&'static str),
    #[error("backend does not support {0}")]
    Unsupported(&'static str),
    #[error("backend process has no process id")]
    MissingProcessId,
    #[error("backend I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("backend control failed: {0}")]
    Control(String),
    #[error("image preparation failed: {0}")]
    ImagePreparation(String),
    #[error("run timed out after {limit:?} during {stage}")]
    Timeout { stage: RunStage, limit: Duration },
    #[error("host environment {component} for {variable} is not valid Unicode")]
    NonUnicodeEnvironment {
        variable: String,
        component: EnvironmentComponent,
    },
}

impl<E: std::fmt::Display> From<StageError<E>> for BackendError {
    fn from(error: StageError<E>) -> Self {
        match error {
            StageError::Timeout(error) => Self::Timeout {
                stage: error.stage,
                limit: error.limit,
            },
            StageError::Failed { stage, source } => {
                Self::Control(format!("{stage} failed: {source}"))
            }
        }
    }
}
