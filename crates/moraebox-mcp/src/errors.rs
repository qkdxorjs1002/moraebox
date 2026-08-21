use std::io;

use moraebox_box::BoxStoreError;
use moraebox_core::StoragePathError;
use moraebox_sdk::{NativeConfigurationError, SdkError};
use thiserror::Error;

#[derive(Debug, Error)]
pub(super) enum McpServerError {
    #[error("{0}")]
    InvalidConfiguration(String),
    #[error(transparent)]
    StoragePath(#[from] StoragePathError),
    #[error(transparent)]
    NativeConfiguration(#[from] NativeConfigurationError),
    #[error(transparent)]
    BoxStore(#[from] BoxStoreError),
}

impl From<&str> for McpServerError {
    fn from(message: &str) -> Self {
        Self::InvalidConfiguration(message.to_owned())
    }
}

impl From<String> for McpServerError {
    fn from(message: String) -> Self {
        Self::InvalidConfiguration(message)
    }
}

impl McpServerError {
    pub(super) fn stage(&self) -> &'static str {
        match self {
            Self::InvalidConfiguration(_)
            | Self::StoragePath(_)
            | Self::NativeConfiguration(_)
            | Self::BoxStore(_) => "server_configuration",
        }
    }

    pub(super) fn retryable(&self) -> bool {
        matches!(self, Self::BoxStore(BoxStoreError::Busy { .. }))
    }
}

#[derive(Debug, Error)]
pub(super) enum McpServeError {
    #[error("MCP transport I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("MCP request task failed: {0}")]
    RequestTask(#[source] tokio::task::JoinError),
    #[error("MCP writer task failed: {0}")]
    WriterTask(#[source] tokio::task::JoinError),
    #[error("MCP session cleanup failed: {0}")]
    SessionCleanup(#[source] SdkError),
}

impl McpServeError {
    pub(super) fn stage(&self) -> &'static str {
        match self {
            Self::Io(_) | Self::WriterTask(_) => "stdio_transport",
            Self::RequestTask(_) => "request_dispatch",
            Self::SessionCleanup(_) => "session_cleanup",
        }
    }

    pub(super) fn retryable(&self) -> bool {
        matches!(self, Self::Io(error) if matches!(
            error.kind(),
            io::ErrorKind::Interrupted
                | io::ErrorKind::TimedOut
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::ConnectionReset
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_error_preserves_stage_and_retryability() {
        let error = McpServeError::Io(io::Error::new(io::ErrorKind::TimedOut, "timeout"));
        assert_eq!(error.stage(), "stdio_transport");
        assert!(error.retryable());
    }

    #[test]
    fn server_configuration_error_is_typed_and_not_retryable() {
        let error = McpServerError::from("invalid backend combination");
        assert_eq!(error.stage(), "server_configuration");
        assert!(!error.retryable());
        assert_eq!(error.to_string(), "invalid backend combination");
    }
}
