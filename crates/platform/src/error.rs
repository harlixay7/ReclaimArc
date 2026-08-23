use std::fmt;
use std::path::PathBuf;

/// Precise, structured errors. The engine turns these into journal records and
/// user-facing messages with operation, path, OS error and recommended action.
#[derive(Debug)]
pub struct PlatformError {
    /// Machine-readable error kind.
    pub kind: PlatformErrorKind,
    /// Human-readable detail. May contain paths, never secrets.
    pub message: String,
    /// Raw OS error code when the failure came from the OS.
    pub os: Option<u32>,
    /// The file system object involved, when relevant.
    pub path: Option<PathBuf>,
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)?;
        if let Some(os) = self.os {
            write!(f, " (OS error {os})")?;
        }
        Ok(())
    }
}

impl std::error::Error for PlatformError {}

impl PlatformError {
    /// Create an error from a raw Windows error code.
    pub fn from_os(kind: PlatformErrorKind, operation: &str, path: Option<&std::path::Path>, os_error: u32) -> Self {
        Self {
            kind,
            message: format!("{operation} failed for '{}'", path.map(|p| p.display().to_string()).unwrap_or_else(|| "<none>".into())),
            os: Some(os_error),
            path: path.map(|p| p.to_path_buf()),
        }
    }

    /// Create an error from a `std::io::Error`.
    pub fn from_io(kind: PlatformErrorKind, operation: &str, path: Option<&std::path::Path>, io: &std::io::Error) -> Self {
        Self {
            kind,
            message: format!("{operation} failed for '{}': {io}", path.map(|p| p.display().to_string()).unwrap_or_else(|| "<none>".into())),
            os: io.raw_os_error().map(|v| v as u32),
            path: path.map(|p| p.to_path_buf()),
        }
    }

    /// Simple non-OS error (policy violation, unsupported feature, ...).
    pub fn policy(kind: PlatformErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), os: None, path: None }
    }
}

/// Machine-readable classification of platform failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformErrorKind {
    /// Path does not exist or is not a file/directory as expected.
    NotFound,
    /// Access denied or sharing violation.
    AccessDenied,
    /// The volume/filesystem does not support the requested operation.
    UnsupportedFilesystem,
    /// The operation is not supported on this platform.
    Unsupported,
    /// Win32 call failed.
    Win32,
    /// I/O failure (read/write).
    Io,
    /// The requested range exceeds the file size.
    InvalidRange,
    /// A precondition check failed (e.g. identity mismatch).
    Precondition,
    /// Other policy violation.
    Policy,
}
