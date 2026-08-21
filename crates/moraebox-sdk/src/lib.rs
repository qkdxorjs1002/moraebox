//! Async process-owned API used by the CLI, MCP server, and embedders.

#![forbid(unsafe_code)]

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use moraebox_box::{BoxMetadata, BoxStore, BoxStoreError, CreateBox};
use moraebox_core::{BoxId, OutputChunk, OutputReadError, RunSpec, SessionId, Signal};
use moraebox_runtime::{Backend, SessionError, SessionHandle, SessionManager, SessionStatus};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{RwLock, oneshot},
    task::JoinSet,
};

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

    /// Starts a connection-owned session unless its creating request is cancelled first.
    ///
    /// Once this returns successfully, the session remains owned by this SDK until it is
    /// explicitly removed or [`Self::shutdown`] is called.
    pub async fn start_cancellable(
        &self,
        spec: RunSpec,
        cancellation: oneshot::Receiver<()>,
    ) -> Result<SessionStatus, SdkError> {
        let handle = self.start_handle_cancellable(spec, cancellation).await?;
        let status = handle.status();
        self.sessions.write().await.insert(handle.id(), handle);
        Ok(status)
    }

    pub async fn exec(&self, spec: RunSpec) -> Result<ExecutionResult, SdkError> {
        let handle = self.manager.start(spec).await?;
        Self::finish_execution(&handle).await
    }

    /// Runs a request-owned one-shot session and completes cleanup before reporting cancellation.
    pub async fn exec_cancellable(
        &self,
        spec: RunSpec,
        mut cancellation: oneshot::Receiver<()>,
    ) -> Result<ExecutionResult, SdkError> {
        if cancellation.try_recv().is_ok() {
            return Err(SdkError::RequestCancelled);
        }
        let mut start = Box::pin(self.manager.start(spec));
        let handle = tokio::select! {
            result = &mut start => result?,
            _ = &mut cancellation => {
                let handle = start.await?;
                Self::stop_handle(&handle).await?;
                return Err(SdkError::RequestCancelled);
            }
        };
        if cancellation.try_recv().is_ok() {
            Self::stop_handle(&handle).await?;
            return Err(SdkError::RequestCancelled);
        }
        tokio::select! {
            result = Self::finish_execution(&handle) => result,
            _ = &mut cancellation => {
                Self::stop_handle(&handle).await?;
                Err(SdkError::RequestCancelled)
            }
        }
    }

    async fn finish_execution(handle: &SessionHandle) -> Result<ExecutionResult, SdkError> {
        if let Err(error) = handle.close_stdin().await
            && handle.status().state != moraebox_core::SessionState::Dead
        {
            return Err(error.into());
        }
        let status = handle.wait().await?;
        let output = match handle.read_output(0, usize::MAX).await {
            Ok(output) => output,
            Err(SessionError::Output(OutputReadError::CursorExpired { earliest, .. })) => {
                handle.read_output(earliest, usize::MAX).await?
            }
            Err(error) => return Err(error.into()),
        };
        Ok(ExecutionResult {
            status,
            output: output.chunks,
            next_cursor: output.next_cursor,
            truncated: output.truncated,
        })
    }

    async fn start_handle_cancellable(
        &self,
        spec: RunSpec,
        mut cancellation: oneshot::Receiver<()>,
    ) -> Result<SessionHandle, SdkError> {
        if cancellation.try_recv().is_ok() {
            return Err(SdkError::RequestCancelled);
        }
        let mut start = Box::pin(self.manager.start(spec));
        let handle = tokio::select! {
            result = &mut start => result?,
            _ = &mut cancellation => {
                let handle = start.await?;
                Self::stop_handle(&handle).await?;
                return Err(SdkError::RequestCancelled);
            }
        };
        if cancellation.try_recv().is_ok() {
            Self::stop_handle(&handle).await?;
            return Err(SdkError::RequestCancelled);
        }
        Ok(handle)
    }

    async fn stop_handle(handle: &SessionHandle) -> Result<SessionStatus, SdkError> {
        let stop_error = handle.stop().await.err();
        let status = handle.wait().await?;
        if let Some(error) = stop_error {
            Err(error.into())
        } else {
            Ok(status)
        }
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

    /// Stops and forgets every connection-owned session, continuing after individual failures.
    pub async fn shutdown(&self) -> Result<(), SdkError> {
        let sessions = std::mem::take(&mut *self.sessions.write().await);
        let mut cleanup = JoinSet::new();
        for handle in sessions.into_values() {
            cleanup.spawn(async move { Self::stop_handle(&handle).await });
        }

        let mut first_error = None;
        while let Some(result) = cleanup.join_next().await {
            let result = result
                .map_err(|error| SdkError::SessionTask(error.to_string()))
                .and_then(|result| result.map(|_| ()));
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
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
    #[error("sandbox request was cancelled")]
    RequestCancelled,
    #[error("session cleanup task failed: {0}")]
    SessionTask(String),
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

    #[tokio::test]
    async fn exec_returns_retained_mixed_output_after_truncation() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let mut spec = RunSpec::command(mixed_output_command());
        spec.output_limit = 8;

        let result = sdk.exec(spec).await.unwrap();

        assert!(result.truncated);
        assert_eq!(result.next_cursor, 12);
        assert_eq!(
            result
                .output
                .iter()
                .map(|chunk| chunk.data.len())
                .sum::<usize>(),
            8
        );
        let mut cursor = 4;
        for chunk in &result.output {
            assert_eq!(chunk.cursor, cursor);
            cursor += chunk.data.len() as u64;
        }
        assert_eq!(cursor, result.next_cursor);
        assert!(
            result
                .output
                .iter()
                .any(|chunk| chunk.channel == moraebox_core::OutputChannel::Stdout)
        );
        assert!(
            result
                .output
                .iter()
                .any(|chunk| chunk.channel == moraebox_core::OutputChannel::Stderr)
        );
    }

    #[tokio::test]
    async fn cancellable_exec_waits_for_session_cleanup() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let mut spec = RunSpec::command(long_running_command());
        spec.kill_grace = Duration::from_millis(20);
        let (cancel, cancellation) = oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = cancel.send(());
        });

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            sdk.exec_cancellable(spec, cancellation),
        )
        .await
        .expect("cancelled execution cleanup timed out");

        assert!(matches!(result, Err(SdkError::RequestCancelled)));
    }

    #[tokio::test]
    async fn pre_cancelled_start_does_not_create_a_session() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let (cancel, cancellation) = oneshot::channel();
        cancel.send(()).unwrap();

        let result = sdk
            .start_cancellable(RunSpec::command(long_running_command()), cancellation)
            .await;

        assert!(matches!(result, Err(SdkError::RequestCancelled)));
        sdk.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_and_forgets_connection_owned_sessions() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let mut spec = RunSpec::command(long_running_command());
        spec.kill_grace = Duration::from_millis(20);
        let first = sdk.start(spec.clone()).await.unwrap();
        let second = sdk.start(spec).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), sdk.shutdown())
            .await
            .expect("SDK shutdown timed out")
            .unwrap();

        assert!(matches!(
            sdk.wait(first.session_id).await,
            Err(SdkError::UnknownSession(id)) if id == first.session_id
        ));
        assert!(matches!(
            sdk.wait(second.session_id).await,
            Err(SdkError::UnknownSession(id)) if id == second.session_id
        ));
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

    #[cfg(unix)]
    fn mixed_output_command() -> Vec<String> {
        ["/bin/sh", "-c", "printf abcdef; printf UVWXYZ >&2"]
            .map(String::from)
            .into()
    }

    #[cfg(windows)]
    fn mixed_output_command() -> Vec<String> {
        vec![
            std::path::PathBuf::from(
                std::env::var_os("SystemRoot").expect("Windows must define SystemRoot"),
            )
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe")
            .to_string_lossy()
            .into_owned(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "[Console]::Out.Write('abcdef'); [Console]::Error.Write('UVWXYZ')".into(),
        ]
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
