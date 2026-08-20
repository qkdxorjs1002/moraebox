//! Async process-owned API used by the CLI, MCP server, and embedders.

#![forbid(unsafe_code)]

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use moraebox_box::{BoxMetadata, BoxStore, BoxStoreError, CreateBox};
use moraebox_core::{BoxId, OutputChunk, RunSpec, SessionId, Signal};
use moraebox_runtime::{Backend, SessionError, SessionHandle, SessionManager, SessionStatus};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct SandboxSdk {
    manager: SessionManager,
    sessions: Arc<RwLock<HashMap<SessionId, SessionHandle>>>,
    box_store: Option<BoxStore>,
}

impl SandboxSdk {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            manager: SessionManager::new(backend),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            box_store: None,
        }
    }

    #[must_use]
    pub fn with_box_store(mut self, store: BoxStore) -> Self {
        self.box_store = Some(store);
        self
    }

    pub async fn start(&self, spec: RunSpec) -> Result<SessionStatus, SdkError> {
        let handle = self.manager.start(spec).await?;
        let status = handle.status();
        self.sessions.write().await.insert(handle.id(), handle);
        Ok(status)
    }

    pub async fn exec(&self, spec: RunSpec) -> Result<ExecutionResult, SdkError> {
        let handle = self.manager.start(spec).await?;
        if let Err(error) = handle.close_stdin().await
            && handle.status().state != moraebox_core::SessionState::Dead
        {
            return Err(error.into());
        }
        let status = handle.wait().await?;
        let output = handle.read_output(0, usize::MAX).await?;
        Ok(ExecutionResult {
            status,
            output: output.chunks,
            next_cursor: output.next_cursor,
            truncated: output.truncated,
        })
    }

    pub async fn io(
        &self,
        session_id: SessionId,
        request: IoRequest,
    ) -> Result<IoResult, SdkError> {
        let handle = self.session(session_id).await?;
        if let Some(input) = request.stdin {
            handle.write(input).await?;
        }
        if request.close_stdin {
            handle.close_stdin().await?;
        }
        if let Some((rows, columns)) = request.resize {
            handle.resize(rows, columns).await?;
        }
        if let Some(signal) = request.signal {
            handle.signal(signal).await?;
        }
        let output = handle
            .read_output(request.cursor, request.max_bytes)
            .await?;
        Ok(IoResult {
            status: handle.status(),
            output: output.chunks,
            next_cursor: output.next_cursor,
            truncated: output.truncated,
        })
    }

    pub async fn wait(&self, session_id: SessionId) -> Result<SessionStatus, SdkError> {
        self.session(session_id)
            .await?
            .wait()
            .await
            .map_err(Into::into)
    }

    pub async fn stop(&self, session_id: SessionId) -> Result<SessionStatus, SdkError> {
        let handle = self.session(session_id).await?;
        handle.stop().await?;
        Ok(handle.wait().await?)
    }

    pub async fn remove(&self, session_id: SessionId) -> bool {
        self.sessions.write().await.remove(&session_id).is_some()
    }

    pub async fn create_box(
        &self,
        request: CreateBox,
        source_disk: PathBuf,
    ) -> Result<BoxMetadata, SdkError> {
        let store = self.box_store()?;
        tokio::task::spawn_blocking(move || store.create(&request, &source_disk))
            .await
            .map_err(|error| SdkError::BoxTask(error.to_string()))?
            .map_err(Into::into)
    }

    pub async fn list_boxes(&self) -> Result<Vec<BoxMetadata>, SdkError> {
        let store = self.box_store()?;
        tokio::task::spawn_blocking(move || store.list())
            .await
            .map_err(|error| SdkError::BoxTask(error.to_string()))?
            .map_err(Into::into)
    }

    pub async fn get_box(&self, box_id: BoxId) -> Result<BoxMetadata, SdkError> {
        let store = self.box_store()?;
        tokio::task::spawn_blocking(move || store.get(box_id))
            .await
            .map_err(|error| SdkError::BoxTask(error.to_string()))?
            .map_err(Into::into)
    }

    pub async fn delete_box(&self, box_id: BoxId) -> Result<BoxMetadata, SdkError> {
        let store = self.box_store()?;
        tokio::task::spawn_blocking(move || store.delete(box_id))
            .await
            .map_err(|error| SdkError::BoxTask(error.to_string()))?
            .map_err(Into::into)
    }

    pub async fn reset_box(
        &self,
        box_id: BoxId,
        source_disk: PathBuf,
    ) -> Result<BoxMetadata, SdkError> {
        let store = self.box_store()?;
        tokio::task::spawn_blocking(move || store.reset(box_id, &source_disk))
            .await
            .map_err(|error| SdkError::BoxTask(error.to_string()))?
            .map_err(Into::into)
    }

    pub async fn clone_box(&self, box_id: BoxId) -> Result<BoxMetadata, SdkError> {
        let store = self.box_store()?;
        tokio::task::spawn_blocking(move || store.clone_box(box_id))
            .await
            .map_err(|error| SdkError::BoxTask(error.to_string()))?
            .map_err(Into::into)
    }

    async fn session(&self, session_id: SessionId) -> Result<SessionHandle, SdkError> {
        self.sessions
            .read()
            .await
            .get(&session_id)
            .cloned()
            .ok_or(SdkError::UnknownSession(session_id))
    }

    fn box_store(&self) -> Result<BoxStore, SdkError> {
        self.box_store
            .clone()
            .ok_or(SdkError::BoxStoreNotConfigured)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub status: SessionStatus,
    pub output: Vec<OutputChunk>,
    pub next_cursor: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoRequest {
    pub cursor: u64,
    pub max_bytes: usize,
    pub stdin: Option<Vec<u8>>,
    pub close_stdin: bool,
    pub resize: Option<(u16, u16)>,
    pub signal: Option<Signal>,
}

impl Default for IoRequest {
    fn default() -> Self {
        Self {
            cursor: 0,
            max_bytes: 1024 * 1024,
            stdin: None,
            close_stdin: false,
            resize: None,
            signal: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoResult {
    pub status: SessionStatus,
    pub output: Vec<OutputChunk>,
    pub next_cursor: u64,
    pub truncated: bool,
}

#[derive(Debug, Error)]
pub enum SdkError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("unknown sandbox session {0}")]
    UnknownSession(SessionId),
    #[error("Box store is not configured for this SDK instance")]
    BoxStoreNotConfigured,
    #[error("Box background task failed: {0}")]
    BoxTask(String),
    #[error(transparent)]
    BoxStore(#[from] BoxStoreError),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moraebox_runtime::ProcessBackend;

    use super::*;

    #[tokio::test]
    async fn process_owned_session_supports_incremental_io() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let status = sdk
            .start(RunSpec::command(stdin_echo_command()))
            .await
            .unwrap();
        let first = sdk
            .io(
                status.session_id,
                IoRequest {
                    stdin: Some(b"sdk\n".to_vec()),
                    close_stdin: true,
                    ..IoRequest::default()
                },
            )
            .await
            .unwrap();
        let status = sdk.wait(status.session_id).await.unwrap();
        let output = sdk
            .io(
                status.session_id,
                IoRequest {
                    cursor: first.next_cursor,
                    ..IoRequest::default()
                },
            )
            .await
            .unwrap();
        assert!(
            output
                .output
                .iter()
                .any(|chunk| chunk.data.starts_with(b"sdk"))
        );
    }

    #[tokio::test]
    async fn manages_boxes_when_a_store_is_configured() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("base.ext4");
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(1024 * 1024).unwrap();
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend))
            .with_box_store(BoxStore::new(temporary.path().join("state")));

        let created = sdk
            .create_box(
                CreateBox::new("sha256:test", "linux/arm64", 1024 * 1024),
                source,
            )
            .await
            .unwrap();

        assert_eq!(sdk.get_box(created.box_id).await.unwrap(), created);
        assert_eq!(sdk.list_boxes().await.unwrap(), vec![created.clone()]);
        sdk.delete_box(created.box_id).await.unwrap();
        assert!(sdk.list_boxes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn repeated_stop_returns_the_same_final_status() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let mut spec = RunSpec::command(long_running_command());
        spec.kill_grace = Duration::from_millis(20);
        let started = sdk.start(spec).await.unwrap();

        let first = sdk.stop(started.session_id).await.unwrap();
        let second = sdk.stop(started.session_id).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(second.state, moraebox_core::SessionState::Dead);
    }

    #[cfg(unix)]
    fn stdin_echo_command() -> Vec<String> {
        ["/bin/sh", "-c", "read value; printf '%s' \"$value\""]
            .map(String::from)
            .into()
    }

    #[cfg(unix)]
    fn long_running_command() -> Vec<String> {
        ["/bin/sh", "-c", "sleep 30"].map(String::from).into()
    }

    #[cfg(windows)]
    fn long_running_command() -> Vec<String> {
        vec![
            windows_system_executable("ping.exe"),
            "-n".into(),
            "31".into(),
            "127.0.0.1".into(),
        ]
    }

    #[cfg(windows)]
    fn stdin_echo_command() -> Vec<String> {
        vec![
            windows_system_executable("findstr.exe"),
            "/R".into(),
            ".*".into(),
        ]
    }

    #[cfg(windows)]
    fn windows_system_executable(name: &str) -> String {
        std::path::PathBuf::from(
            std::env::var_os("SystemRoot").expect("Windows must define SystemRoot"),
        )
        .join("System32")
        .join(name)
        .to_string_lossy()
        .into_owned()
    }
}
