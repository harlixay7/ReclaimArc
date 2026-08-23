//! Core engine errors. Every failure carries enough structure to be recorded
//! in the journal and surfaced with a recommended action — the engine never
//! silently swallows a filesystem error.

use reclaimarc_archive::ArchiveError;
use reclaimarc_journal::JournalError;
use reclaimarc_platform::PlatformError;

/// Core error type.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("archive error: {0}")]
    Archive(#[from] ArchiveError),

    #[error("filesystem error: {0}")]
    Platform(#[from] PlatformError),

    #[error("journal error: {0}")]
    Journal(#[from] JournalError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The extraction cannot be performed safely.
    #[error("infeasible: {0}")]
    Infeasible(String),

    /// A precondition failed (source changed, destination vanished, ...).
    #[error("precondition failed: {0}")]
    Precondition(String),

    /// The user cancelled the operation.
    #[error("operation cancelled")]
    Cancelled,

    /// A structured failure with recovery guidance.
    #[error("failed: {message}")]
    Failed {
        operation: String,
        path: Option<std::path::PathBuf>,
        os_error: Option<u32>,
        recovery_state: String,
        message: String,
        recommended_action: String,
    },
}

impl CoreError {
    pub fn failed(
        operation: impl Into<String>,
        path: Option<std::path::PathBuf>,
        os_error: Option<u32>,
        recovery_state: &str,
        message: impl Into<String>,
        recommended_action: impl Into<String>,
    ) -> Self {
        CoreError::Failed {
            operation: operation.into(),
            path,
            os_error,
            recovery_state: recovery_state.to_string(),
            message: message.into(),
            recommended_action: recommended_action.into(),
        }
    }
}

/// Structured fields of a failure, for journaling and UI display.
#[derive(Debug, Clone)]
pub struct FailureInfo {
    pub operation: String,
    pub path: Option<std::path::PathBuf>,
    pub os_error: Option<u32>,
    pub recovery_state: String,
    pub message: String,
    pub recommended_action: String,
}

impl From<&CoreError> for FailureInfo {
    fn from(e: &CoreError) -> Self {
        match e {
            CoreError::Failed {
                operation,
                path,
                os_error,
                recovery_state,
                message,
                recommended_action,
            } => FailureInfo {
                operation: operation.clone(),
                path: path.clone(),
                os_error: *os_error,
                recovery_state: recovery_state.clone(),
                message: message.clone(),
                recommended_action: recommended_action.clone(),
            },
            other => FailureInfo {
                operation: "engine".into(),
                path: None,
                os_error: None,
                recovery_state: "UNKNOWN".into(),
                message: other.to_string(),
                recommended_action: "Review the log for details.".into(),
            },
        }
    }
}