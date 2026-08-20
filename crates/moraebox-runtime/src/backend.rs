use std::{future::Future, io, pin::Pin, process::ExitStatus};

use async_trait::async_trait;
use moraebox_core::{OutputChannel, RunSpec, Signal};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

pub type BoxedReader = Pin<Box<dyn AsyncRead + Send>>;
pub type BoxedWriter = Pin<Box<dyn AsyncWrite + Send>>;
pub type ExitFuture = Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send>>;

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
    pub root_mode: Option<RootMode>,
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

    async fn spawn(&self, spec: &RunSpec) -> Result<SpawnedSandbox, BackendError>;
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
}
