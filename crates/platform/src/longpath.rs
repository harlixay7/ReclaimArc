//! Long-path-safe helpers.
//!
//! Windows paths longer than 260 characters require the extended-length
//! prefix `\\?\`. Note that `\\?\` disables dot-segment normalization, so this
//! layer is only ever fed already-validated clean paths (see
//! `reclaimarc-core` path security).

use std::path::{Path, PathBuf};

use crate::error::{PlatformError, PlatformErrorKind};

/// Convert a path to its extended-length form suitable for Windows APIs.
///
/// - Absolute drive paths: `C:\a\b` → `\\?\C:\a\b`
/// - UNC paths: `\\server\share\a` → `\\?\UNC\server\share\a`
/// - Relative paths: resolved against the current directory first.
pub fn extend_path(path: &Path) -> Result<String, PlatformError> {
    let p = path.to_path_buf();
    let s = p.to_string_lossy().replace('/', "\\");

    // Handle drive-relative paths (e.g. "B:foo\bar" or "B:") by ensuring a root backslash
    let normalized =
        if s.len() >= 2 && s.as_bytes()[1] == b':' && (s.len() == 2 || s.as_bytes()[2] != b'\\') {
            let drive_prefix = &s[..2];
            let rest = if s.len() > 2 { &s[2..] } else { "" };
            format!("{}\\{}", drive_prefix, rest)
        } else {
            s.to_string()
        };

    let p_norm = PathBuf::from(&normalized);
    let absolute = if p_norm.is_absolute() {
        p_norm
    } else {
        std::env::current_dir()
            .map_err(|e| {
                PlatformError::from_io(PlatformErrorKind::Io, "resolve current directory", None, &e)
            })?
            .join(&p_norm)
    };

    let raw = absolute.to_string_lossy().replace('/', "\\");
    let raw = if raw.len() >= 2
        && raw.as_bytes()[1] == b':'
        && (raw.len() == 2 || raw.as_bytes()[2] != b'\\')
    {
        let drive_prefix = &raw[..2];
        let rest = if raw.len() > 2 { &raw[2..] } else { "" };
        format!("{}\\{}", drive_prefix, rest)
    } else {
        raw.to_string()
    };

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

/// Atomically replace `dst` with `src` on the same volume, with
/// `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` so the rename is
/// durable.
///
/// Includes exponential backoff retries against transient sharing and lock
/// violations caused by real-time antivirus scanners and file indexers.
pub fn rename_existing(src: &Path, dst: &Path) -> Result<(), PlatformError> {
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let src_w: Vec<u16> = extend_path(src)?
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let dst_w: Vec<u16> = extend_path(dst)?
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut last_err = None;

    for &delay_ms in &[0, 5, 15, 30, 60, 100, 140] {
        if delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        let result = unsafe {
            MoveFileExW(
                windows::core::PCWSTR(src_w.as_ptr()),
                windows::core::PCWSTR(dst_w.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let code = e.code().0 as u32;
                last_err = Some(e);
                // ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33), ERROR_ACCESS_DENIED (5)
                let is_transient =
                    matches!(code, 32 | 33 | 5) || matches!(code & 0xFFFF, 32 | 33 | 5);
                if !is_transient {
                    break;
                }
            }
        }
    }

    Err(PlatformError::from_os(
        crate::error::PlatformErrorKind::Win32,
        "atomic rename",
        Some(dst),
        last_err.map(|e| e.code().0 as u32).unwrap_or(0),
    ))
}

/// Remove a file with transient lock retry support (handling antivirus scanners).
pub fn remove_file_existing(path: &Path) -> Result<(), PlatformError> {
    use windows::Win32::Storage::FileSystem::DeleteFileW;

    let path_w: Vec<u16> = extend_path(path)?
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut last_err = None;

    for &delay_ms in &[0, 5, 15, 30, 60, 100] {
        if delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        let result = unsafe { DeleteFileW(windows::core::PCWSTR(path_w.as_ptr())) };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let code = e.code().0 as u32;
                last_err = Some(e);
                // ERROR_FILE_NOT_FOUND (2) is not an error for idempotent cleanup
                if code == 2 || (code & 0xFFFF) == 2 {
                    return Ok(());
                }
                let is_transient =
                    matches!(code, 32 | 33 | 5) || matches!(code & 0xFFFF, 32 | 33 | 5);
                if !is_transient {
                    break;
                }
            }
        }
    }

    Err(PlatformError::from_os(
        crate::error::PlatformErrorKind::Win32,
        "remove file",
        Some(path),
        last_err.map(|e| e.code().0 as u32).unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rename_and_remove_existing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");

        std::fs::write(&src, b"hello rename").unwrap();
        rename_existing(&src, &dst).expect("rename_existing should succeed");
        assert!(!src.exists());
        assert!(dst.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello rename");

        remove_file_existing(&dst).expect("remove_file_existing should succeed");
        assert!(!dst.exists());

        // Idempotent removal on non-existent file
        remove_file_existing(&dst).expect("remove_file_existing on missing file is idempotent");
    }
}
