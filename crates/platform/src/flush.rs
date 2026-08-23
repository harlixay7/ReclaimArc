//! Durable flushes. The engine treats a failed flush as a failed job â€” never
//! silently continued.

use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::error::{PlatformError, PlatformErrorKind};
use crate::longpath::extend_path;

/// `FlushFileBuffers` on an open file. Fails precisely when the flush fails.
pub fn flush_file(file: &std::fs::File, path: &Path) -> Result<(), PlatformError> {
    let ok = unsafe { FlushFileBuffers(HANDLE(file.as_raw_handle() as *mut _)) };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "flush file buffers",
            Some(path),
            e.code().0 as u32,
        ));
    }
    Ok(())
}

/// Flush a directory so that renames/creates inside it are durable.
///
/// On NTFS/ReFS this works by opening the directory with
/// `FILE_FLAG_BACKUP_SEMANTICS` and flushing it. `FlushFileBuffers` on a
/// directory requires `FILE_WRITE_DATA` access on the handle (verified
/// empirically; read-only handles fail with ERROR_ACCESS_DENIED). On
/// filesystems that cannot flush directories this returns an explicit
/// `UnsupportedFilesystem` error — the engine records it in the journal and
/// treats the job accordingly.
pub fn flush_directory(path: &Path) -> Result<(), PlatformError> {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};

    let name: Vec<u16> = extend_path(path)?.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    };
    let handle = match result {
        Ok(h) => h,
        Err(e) => {
            return Err(PlatformError::from_os(
                PlatformErrorKind::Win32,
                "open directory for flush",
                Some(path),
                e.code().0 as u32,
            ))
        }
    };
    let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
    let ok = unsafe { FlushFileBuffers(HANDLE(file.as_raw_handle() as *mut _)) };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::UnsupportedFilesystem,
            "flush directory buffers",
            Some(path),
            e.code().0 as u32,
        ));
    }
    Ok(())
}

/// Whether the given filesystem name is known to support directory flushes.
pub fn directory_flush_supported(filesystem_name: &str) -> bool {
    let n = filesystem_name.to_ascii_lowercase();
    matches!(n.as_str(), "ntfs" | "refs")
}


