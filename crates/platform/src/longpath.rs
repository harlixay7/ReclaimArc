//! Long-path-safe helpers.
//!
//! Windows paths longer than 260 characters require the extended-length
//! prefix `\\?\`. Note that `\\?\` disables dot-segment normalization, so this
//! layer is only ever fed already-validated clean paths (see
//! `spacextract-core` path security).

use std::path::{Path, PathBuf};

use crate::error::{PlatformError, PlatformErrorKind};

/// Convert a path to its extended-length form suitable for Windows APIs.
///
/// - Absolute drive paths: `C:\a\b` → `\\?\C:\a\b`
/// - UNC paths: `\\server\share\a` → `\\?\UNC\server\share\a`
/// - Relative paths: resolved against the current directory first.
pub fn extend_path(path: &Path) -> Result<String, PlatformError> {
    let p = path.to_path_buf();
    let absolute = if p.is_absolute() {
        p
    } else {
        std::env::current_dir()
            .map_err(|e| PlatformError::from_io(PlatformErrorKind::Io, "resolve current directory", None, &e))?
            .join(&p)
    };

    let raw = absolute.to_string_lossy().replace('/', "\\");
    if raw.starts_with("\\\\?\\") {
        return Ok(raw);
    }
    if let Some(rest) = raw.strip_prefix("\\\\") {
        // UNC: \\server\share\... → \\?\UNC\server\share\...
        if rest.starts_with("?\\") {
            return Ok(raw);
        }
        return Ok(format!("\\\\?\\UNC\\{rest}"));
    }
    Ok(format!("\\\\?\\{raw}"))
}

/// The destination path used by the engine: always the extended form.
pub fn extended(path: &Path) -> Result<PathBuf, PlatformError> {
    Ok(PathBuf::from(extend_path(path)?))
}
