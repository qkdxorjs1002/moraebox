//! Async process-owned API used by the CLI, MCP server, and embedders.

#![forbid(unsafe_code)]

use std::{collections::HashMap, sync::Arc};

use fastmvm_core::{OutputChunk, RunSpec, SessionId, Signal};
use fastmvm_runtime::{Backend, SessionError, SessionHandle, SessionManager, SessionStatus};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct SandboxSdk {
    manager: SessionManager,
    sessions: Arc<RwLock<HashMap<SessionId, SessionHandle>>>,
}

impl SandboxSdk {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            manager: SessionManager::new(backend),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
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
            && handle.status().state != fastmvm_core::SessionState::Dead
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

    async fn session(&self, session_id: SessionId) -> Result<SessionHandle, SdkError> {
        self.sessions
            .read()
            .await
            .get(&session_id)
            .cloned()
            .ok_or(SdkError::UnknownSession(session_id))
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
}

#[cfg(test)]
mod tests {
    use fastmvm_runtime::ProcessBackend;

    use super::*;

    #[tokio::test]
    async fn process_owned_session_supports_incremental_io() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let status = sdk
            .start(RunSpec::command([
                "/bin/sh",
                "-c",
                "read value; printf '%s' \"$value\"",
            ]))
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
        assert_eq!(output.output[0].data, b"sdk");
    }
}
