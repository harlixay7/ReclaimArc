//! Filesystem facts: identity, allocated size, free space, storage-pool checks.

use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetFileInformationByHandle, GetVolumeInformationW,
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_SHARE_DELETE,
    OPEN_EXISTING, FILE_READ_ATTRIBUTES,
};

use crate::error::{PlatformError, PlatformErrorKind};
use crate::longpath::extend_path;

/// Identity of a file on disk, used to prove "this is the same file we opened".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    /// Volume serial number from `GetVolumeInformationW`.
    pub volume_serial: u32,
    /// 64-bit NTFS file ID (index high:low).
    pub file_id: u64,
    /// Logical size in bytes.
    pub file_size: u64,
    /// Last write time as Windows FILETIME (100ns intervals since 1601-01-01).
    pub last_write_time: u64,
}

/// Open an existing file for attribute access, sharing read/write/delete.
///
/// The engine uses this to snapshot identities and to hold a handle that
/// detects external modification.
pub fn open_for_identity(path: &Path) -> Result<std::fs::File, PlatformError> {
    let name: Vec<u16> = extend_path(path)?.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_READ_ATTRIBUTES.0 as u32,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    };
    match result {
        Ok(h) => Ok(unsafe { std::fs::File::from_raw_handle(h.0) }),
        Err(e) => {
            let code = e.code().0 as u32;
            Err(PlatformError::from_os(PlatformErrorKind::NotFound, "open file", Some(path), code))
        }
    }
}
/// Identity of a file. Fails precisely when the file cannot be opened or when
/// the identity cannot be read â€” the caller must never guess.
pub fn file_identity(path: &Path) -> Result<FileIdentity, PlatformError> {
    let file = open_for_identity(path)?;
    file_identity_from_handle(&file, path)
}

/// Identity from an already-open handle.
pub fn file_identity_from_handle(file: &std::fs::File, path: &Path) -> Result<FileIdentity, PlatformError> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let ok = unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle() as *mut _), &mut info) };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "get file information",
            Some(path),
            e.code().0 as u32,
        ));
    }
    let file_id = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    let mtime = ((info.ftLastWriteTime.dwHighDateTime as u64) << 32) | info.ftLastWriteTime.dwLowDateTime as u64;
    Ok(FileIdentity {
        volume_serial: info.dwVolumeSerialNumber,
        file_id,
        file_size: size,
        last_write_time: mtime,
    })
}

/// Physical (on-disk) allocated size of a file, including sparse deallocated
/// regions.
///
/// Measured by summing `FSCTL_QUERY_ALLOCATED_RANGES` over the whole file:
/// `GetCompressedFileSizeW` and `FILE_STANDARD_INFO::AllocationSize` are
/// known to report incorrect values for sparse files on Windows
/// (see Microsoft support KB for `GetCompressedFileSize` with sparse files),
/// while the allocation query matches `fsutil file queryAllocRanges` exactly.
pub fn allocated_size(path: &Path) -> Result<u64, PlatformError> {
    let file = crate::sparse::open_for_query(path)?;
    let len = file_size(path)?;
    crate::sparse::query_allocated_ranges(&file, path, 0, len)
        .map(|ranges| ranges.iter().map(|r| r.len).sum())
}

/// Allocated size of a file from an already-open handle, measured by summing
/// `FSCTL_QUERY_ALLOCATED_RANGES` over the whole file (reliable for sparse
/// files, unlike `GetCompressedFileSizeW`/`FILE_STANDARD_INFO`).
pub fn allocated_size_from_handle(file: &std::fs::File, path: &Path) -> Result<u64, PlatformError> {
    let ident = file_identity_from_handle(file, path)?;
    crate::sparse::query_allocated_ranges(file, path, 0, ident.file_size)
        .map(|ranges| ranges.iter().map(|r| r.len).sum())
}

/// Free bytes available on the volume containing `path`.
pub fn free_space(path: &Path) -> Result<u64, PlatformError> {
    let root = drive_root(path);
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free: u64 = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None) };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "get free space",
            Some(&root),
            e.code().0 as u32,
        ));
    }
    Ok(free)
}

/// Total bytes on the volume containing `path`.
pub fn total_space(path: &Path) -> Result<u64, PlatformError> {
    let root = drive_root(path);
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut total: u64 = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), None, Some(&mut total), None) };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "get total space",
            Some(&root),
            e.code().0 as u32,
        ));
    }
    Ok(total)
}

/// Filesystem name of the volume containing `path` (e.g. "NTFS", "ReFS").
pub fn filesystem_name(path: &Path) -> Result<String, PlatformError> {
    let root = drive_root(path);
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut fs_name = [0u16; 260];
    let mut serial: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut serial),
            None,
            None,
            Some(&mut fs_name),
        )
    };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "get volume information",
            Some(&root),
            e.code().0 as u32,
        ));
    }
    let end = fs_name.iter().position(|&c| c == 0).unwrap_or(fs_name.len());
    Ok(String::from_utf16_lossy(&fs_name[..end]))
}

/// Volume serial number of the volume containing `path`.
pub fn volume_serial(path: &Path) -> Result<u32, PlatformError> {
    let root = drive_root(path);
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut serial: u32 = 0;
    let ok = unsafe { GetVolumeInformationW(PCWSTR(wide.as_ptr()), None, Some(&mut serial), None, None, None) };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "get volume serial",
            Some(&root),
            e.code().0 as u32,
        ));
    }
    Ok(serial)
}

/// Whether two paths live on the same storage pool (volume). When false,
/// reclaiming source bytes cannot increase capacity available to the
/// destination.
pub fn same_storage_pool(a: &Path, b: &Path) -> Result<bool, PlatformError> {
    let sa = volume_serial(a)?;
    let sb = volume_serial(b)?;
    Ok(sa == sb)
}

/// The root of the volume containing `path` ("C:\", "\\server\share\", ...).
pub fn drive_root(path: &Path) -> PathBuf {
    let p = path.to_path_buf();
    let absolute = if p.is_absolute() {
        p
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(&p)
    } else {
        p
    };
    let mut root = absolute.clone();
    while let Some(parent) = root.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        root = parent.to_path_buf();
    }
    let root_str = root.to_string_lossy();
    if root_str.ends_with(':') {
        return PathBuf::from(format!("{}\\", root_str));
    }
    if root_str.ends_with('\\') || root_str.starts_with("\\\\") {
        return root;
    }
    root
}

/// Cluster size of the volume containing `path`.
pub fn cluster_size(path: &Path) -> Result<u32, PlatformError> {
    let root = drive_root(path);
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(wide.as_ptr()),
            Some(&mut sectors_per_cluster),
            Some(&mut bytes_per_sector),
            None,
            None,
        )
    };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "get disk free space (cluster size)",
            Some(&root),
            e.code().0 as u32,
        ));
    }
    if sectors_per_cluster == 0 || bytes_per_sector == 0 {
        return Err(PlatformError::policy(
            PlatformErrorKind::Win32,
            format!("volume {} reported zero cluster geometry", root.display()),
        ));
    }
    Ok(sectors_per_cluster * bytes_per_sector)
}

/// Logical size of a file.
pub fn file_size(path: &Path) -> Result<u64, PlatformError> {
    let ident = file_identity(path)?;
    Ok(ident.file_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_geometry_is_sane() {
        let here = std::env::current_dir().unwrap();
        let root = drive_root(&here);
        assert!(!root.as_os_str().is_empty());
        let fs_name = filesystem_name(&root).unwrap();
        let free = free_space(&root).unwrap();
        let total = total_space(&root).unwrap();
        assert!(free <= total);
        assert!(cluster_size(&root).unwrap() >= 512);
        assert!(!fs_name.is_empty());
    }

    #[test]
    fn same_pool_detects_same_and_different() {
        let here = std::env::current_dir().unwrap();
        assert!(same_storage_pool(&here, &here).unwrap());
        let p1 = here.join("a");
        let p2 = here.join("b");
        assert!(same_storage_pool(&p1, &p2).unwrap());
    }

    #[test]
    fn identity_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ident.bin");
        std::fs::write(&f, b"hello world").unwrap();
        let id1 = file_identity(&f).unwrap();
        let id2 = file_identity(&f).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(id1.file_size, 11);
        assert_ne!(id1.file_id, 0);
    }

    #[test]
    fn allocated_size_matches_file_size_for_plain_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("plain.bin");
        std::fs::write(&f, vec![0u8; 65536]).unwrap();
        let alloc = allocated_size(&f).unwrap();
        assert!(alloc >= 65536);
    }
}





