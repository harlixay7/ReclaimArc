/// Error type for archive backends.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// The archive could not be opened / is not a supported format.
    #[error("archive cannot be opened: {0}")]
    Open(String),

    /// The archive is damaged (bad CRC, truncated, ...).
    #[error("archive data is corrupt: {0}")]
    Corrupt(String),

    /// Decoder failure (from the underlying library).
    #[error("decoder error: {0}")]
    Decoder(String),

    /// A password is required or was incorrect.
    #[error("password problem: {0}")]
    Password(String),

    /// A required volume is missing.
    #[error("missing volume: {0}")]
    MissingVolume(String),

    /// The archive uses features the backend does not support.
    #[error("unsupported archive feature: {0}")]
    Unsupported(String),

    /// The requested entry/unit does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Operation was cancelled by the caller.
    #[error("operation cancelled")]
    Cancelled,

    /// I/O failure while reading the archive.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid archive metadata (malformed headers).
    #[error("invalid archive metadata: {0}")]
    InvalidMetadata(String),
}

impl ArchiveError {
    pub fn open(what: impl Into<String>) -> Self {
        ArchiveError::Open(what.into())
    }
    pub fn corrupt(what: impl Into<String>) -> Self {
        ArchiveError::Corrupt(what.into())
    }
    pub fn unsupported(what: impl Into<String>) -> Self {
        ArchiveError::Unsupported(what.into())
    }
    pub fn missing_volume(what: impl Into<String>) -> Self {
        ArchiveError::MissingVolume(what.into())
    }
    pub fn invalid(what: impl Into<String>) -> Self {
        ArchiveError::InvalidMetadata(what.into())
    }
}