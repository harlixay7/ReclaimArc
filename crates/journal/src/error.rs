/// Error type for journal operations.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// Underlying SQLite failure.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A required record was missing or inconsistent.
    #[error("journal record missing/inconsistent: {0}")]
    Missing(String),

    /// Serialization of a record failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// The journal could not be opened because the database is corrupted.
    #[error("journal corrupted: {0}")]
    Corrupt(String),

    /// The journal file is not a ReclaimArc journal (schema mismatch).
    #[error("not a ReclaimArc journal (unexpected schema): {0}")]
    Schema(String),

    /// I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A precondition (e.g. expected state) was not met.
    #[error("journal state precondition failed: {0}")]
    State(String),
}

impl JournalError {
    pub fn missing(what: impl Into<String>) -> Self {
        JournalError::Missing(what.into())
    }
    pub fn state(what: impl Into<String>) -> Self {
        JournalError::State(what.into())
    }
    pub fn schema(what: impl Into<String>) -> Self {
        JournalError::Schema(what.into())
    }
}

pub(crate) type Result<T> = std::result::Result<T, JournalError>;
