use std::io;

use moraebox_box::BoxStoreError;
use moraebox_core::StoragePathError;
use moraebox_image::{ImageCacheError, WorkspaceError};
use moraebox_runtime::{
    BackendError, RunBudgetTimeout, RunStage, SessionError, StageError, SupervisorError,
};
use moraebox_sdk::NativeConfigurationError;
use thiserror::Error;

#[derive(Debug, Error)]
pub(super) enum CliErrorSource {
    #[error("{0}")]
    InvalidInput(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    StoragePath(#[from] StoragePathError),
    #[error(transparent)]
    BoxStore(#[from] BoxStoreError),
    #[error(transparent)]
    ImageCache(#[from] ImageCacheError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    NativeConfiguration(#[from] NativeConfigurationError),
    #[error("{message}")]
    Stage {
        stage: RunStage,
        retryable: bool,
        message: String,
    },
    #[error("background task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl CliErrorSource {
    pub(super) fn from_stage<E: std::fmt::Display>(error: StageError<E>) -> Self {
        match error {
            StageError::Timeout(error) => Self::Stage {
                stage: error.stage,
                retryable: true,
                message: error.to_string(),
            },
            StageError::Failed { stage, source } => Self::Stage {
                stage,
                retryable: false,
                message: source.to_string(),
            },
        }
    }

    fn stage(&self) -> Option<RunStage> {
        match self {
            Self::Stage { stage, .. }
            | Self::Backend(BackendError::Timeout { stage, .. })
            | Self::Supervisor(SupervisorError::Backend(BackendError::Timeout { stage, .. })) => {
                Some(*stage)
            }
            _ => None,
        }
    }

    fn retryable(&self) -> bool {
        match self {
            Self::Stage { retryable, .. } => *retryable,
            Self::Backend(BackendError::Timeout { .. })
            | Self::Supervisor(SupervisorError::Backend(BackendError::Timeout { .. }))
            | Self::BoxStore(BoxStoreError::Busy { .. } | BoxStoreError::BaseDiskBusy { .. })
            | Self::ImageCache(ImageCacheError::Busy { .. }) => true,
            Self::Io(error) | Self::Supervisor(SupervisorError::Io(error)) => {
                is_retryable_io(error)
            }
            _ => false,
        }
    }
}

impl From<String> for CliErrorSource {
    fn from(message: String) -> Self {
        Self::InvalidInput(message)
    }
}

impl From<&str> for CliErrorSource {
    fn from(message: &str) -> Self {
        Self::InvalidInput(message.to_owned())
    }
}

impl From<RunBudgetTimeout> for CliErrorSource {
    fn from(error: RunBudgetTimeout) -> Self {
        Self::Stage {
            stage: error.stage,
            retryable: true,
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Error)]
#[error("{source}")]
pub(super) struct CliError {
    pub(super) code: &'static str,
    pub(super) stage: String,
    pub(super) retryable: bool,
    pub(super) remediation: &'static str,
    #[source]
    pub(super) source: CliErrorSource,
}

impl CliError {
    pub(super) fn for_command(default_stage: &'static str, source: CliErrorSource) -> Self {
        let (default_code, remediation) = command_metadata(default_stage);
        let stage = source
            .stage()
            .map_or_else(|| default_stage.to_owned(), |stage| stage.to_string());
        Self {
            code: default_code,
            stage,
            retryable: source.retryable(),
            remediation,
            source,
        }
    }
}

fn command_metadata(stage: &str) -> (&'static str, &'static str) {
    if stage == "run" || stage == "benchmark" {
        (
            "execution_failed",
            "Review the command, backend options, and diagnostics, then retry.",
        )
    } else if stage.starts_with("image_") {
        (
            "image_operation_failed",
            "Check the image reference, registry access, cache permissions, and retry.",
        )
    } else if stage.starts_with("box_") {
        (
            "box_operation_failed",
            "Inspect Box state and storage permissions before retrying the operation.",
        )
    } else if stage.starts_with("cache_") {
        (
            "cache_operation_failed",
            "Inspect cache permissions and the requested operation, then retry.",
        )
    } else if stage == "doctor" {
        (
            "doctor_failed",
            "Review native dependency paths and run `morae doctor` for human-readable diagnostics.",
        )
    } else {
        (
            "command_failed",
            "Review the command arguments and diagnostics, then retry.",
        )
    }
}

fn is_retryable_io(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_preserves_runtime_stage_and_retryability() {
        let error = CliError::for_command(
            "run",
            BackendError::Timeout {
                stage: RunStage::HelperSpawn,
                limit: std::time::Duration::from_secs(1),
            }
            .into(),
        );
        assert_eq!(error.code, "execution_failed");
        assert_eq!(error.stage, "helper_spawn");
        assert!(error.retryable);
    }

    #[test]
    fn validation_is_typed_and_not_retryable() {
        let error = CliError::for_command("run", CliErrorSource::from("invalid option"));
        assert_eq!(error.code, "execution_failed");
        assert_eq!(error.stage, "run");
        assert!(!error.retryable);
    }
}
