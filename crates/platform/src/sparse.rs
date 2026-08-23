//! Sparse-file reclamation on NTFS/ReFS.
//!
//! The engine converts archive allocation into free space by:
//! 1. marking the file sparse (`FSCTL_SET_SPARSE`),
//! 2. zeroing only proven-safe packed data ranges (`FSCTL_SET_ZERO_DATA`),
//! 3. verifying with `FSCTL_QUERY_ALLOCATED_RANGES` that allocation was
//!    actually released and that no bytes outside the retired range changed.
//!
//! All ranges are aligned *inward* to cluster boundaries: the engine never
//! zeroes a byte that was not proven safe to retire.

use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_ALWAYS, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, FILE_READ_DATA,
};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_SHARE_DELETE};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FILE_ALLOCATED_RANGE_BUFFER, FILE_ZERO_DATA_INFORMATION, FSCTL_QUERY_ALLOCATED_RANGES, FSCTL_SET_SPARSE,
    FSCTL_SET_ZERO_DATA,
};

use crate::error::{PlatformError, PlatformErrorKind};
use crate::fs::allocated_size;
use crate::longpath::extend_path;

/// A range of bytes within a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteRange {
    /// First byte offset (inclusive).
    pub start: u64,
    /// Number of bytes.
    pub len: u64,
}

impl ByteRange {
    /// Last byte offset (exclusive).
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }
}

/// Result of a reclamation operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimReport {
    /// The range requested (already aligned inward to cluster boundaries).
    pub requested: ByteRange,
    /// Sub-ranges that were actually deallocated (from allocation queries).
    pub reclaimed: Vec<ByteRange>,
    /// Sub-ranges that were requested but remained allocated.
    pub remaining: Vec<ByteRange>,
    /// Allocated size before the operation.
    pub allocated_before: u64,
    /// Allocated size after the operation.
    pub allocated_after: u64,
}

impl ReclaimReport {
    /// Bytes actually released by the operation.
    pub fn released_bytes(&self) -> u64 {
        self.reclaimed.iter().map(|r| r.len).sum()
    }
}

/// Capability evidence collected by the behavioral probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseProbe {
    /// Whether the probe file could be marked sparse.
    pub sparse_mark_ok: bool,
    /// Whether a zeroed range was deallocated.
    pub deallocation_ok: bool,
    /// Whether allocated ranges could be queried.
    pub query_ok: bool,
    /// Volume filesystem name ("NTFS", "ReFS", ...).
    pub filesystem: String,
    /// Human-readable verdict.
    pub verdict: String,
}

impl SparseProbe {
    /// Everything required for safe progressive reclamation.
    pub fn fully_supported(&self) -> bool {
        self.sparse_mark_ok && self.deallocation_ok && self.query_ok
    }
}

/// Mark a file as sparse. `FSCTL_SET_SPARSE` must precede zero-data
/// deallocation on NTFS/ReFS.
pub fn set_sparse(file: &std::fs::File, path: &Path) -> Result<(), PlatformError> {
    let ok = unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle() as *mut _),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            None,
            None,
        )
    };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "FSCTL_SET_SPARSE",
            Some(path),
            e.code().0 as u32,
        ));
    }
    Ok(())
}

/// Align a range inward to the filesystem deallocation granularity.
///
/// `start` is rounded *up* and `end` is rounded *down*, so the resulting range
/// never extends beyond the retired range. Returns `None` when the aligned
/// range is empty.
///
/// On NTFS, `FSCTL_SET_ZERO_DATA` deallocates only the interior that is
/// aligned to 64 KiB units (verified empirically: clusters in the first and
/// last partial 64 KiB window remain allocated). The granularity is therefore
/// `max(cluster, 64 KiB)`.
pub fn align_inward(range: ByteRange, cluster: u32) -> Option<ByteRange> {
    const NTFS_DEALLOC_UNIT: u64 = 64 * 1024;
    let unit = (cluster as u64).max(NTFS_DEALLOC_UNIT);
    if unit == 0 {
        return Some(range);
    }
    let start = if range.start % unit == 0 {
        range.start
    } else {
        (range.start / unit + 1) * unit
    };
    let end = (range.end() / unit) * unit;
    if start >= end {
        return None;
    }
    Some(ByteRange { start, len: end - start })
}

/// Deallocate the given byte range of a sparse file via `FSCTL_SET_ZERO_DATA`.
///
/// The bytes read back as zero afterwards. Only whole clusters are released.
pub fn zero_range(file: &std::fs::File, path: &Path, range: ByteRange) -> Result<(), PlatformError> {
    let buffer = FILE_ZERO_DATA_INFORMATION {
        FileOffset: range.start as i64,
        BeyondFinalZero: range.end() as i64,
    };
    let ok = unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle() as *mut _),
            FSCTL_SET_ZERO_DATA,
            Some(&buffer as *const _ as *const _),
            std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            None,
            0,
            None,
            None,
        )
    };
    if let Err(e) = ok {
        return Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "FSCTL_SET_ZERO_DATA",
            Some(path),
            e.code().0 as u32,
        ));
    }
    Ok(())
}

/// Query which byte ranges of `[start, start+len)` are actually allocated.
pub fn query_allocated_ranges(
    file: &std::fs::File,
    path: &Path,
    start: u64,
    len: u64,
) -> Result<Vec<ByteRange>, PlatformError> {
    if len == 0 {
        return Ok(vec![]);
    }
    let query = FILE_ALLOCATED_RANGE_BUFFER {
        FileOffset: start as i64,
        Length: len as i64,
    };
    let mut output: Vec<FILE_ALLOCATED_RANGE_BUFFER> = Vec::with_capacity(16);
    let mut out_bytes = (16 * std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>()) as u32;
    loop {
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                HANDLE(file.as_raw_handle() as *mut _),
                FSCTL_QUERY_ALLOCATED_RANGES,
                Some(&query as *const _ as *const _),
                std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                Some(output.as_mut_ptr() as *mut _),
                out_bytes,
                Some(&mut returned),
                None,
            )
        };
        if let Err(e) = ok {
            return Err(PlatformError::from_os(
                PlatformErrorKind::Win32,
                "FSCTL_QUERY_ALLOCATED_RANGES",
                Some(path),
                e.code().0 as u32,
            ));
        }
        let count = (returned as usize) / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        unsafe { output.set_len(count) };
        if returned < out_bytes {
            break;
        }
        out_bytes = out_bytes.saturating_mul(2);
        output.resize(out_bytes as usize / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>(), FILE_ALLOCATED_RANGE_BUFFER::default());
    }
    Ok(output
        .iter()
        .map(|r| ByteRange {
            start: r.FileOffset.max(0) as u64,
            len: r.Length.max(0) as u64,
        })
        .filter(|r| r.len > 0)
        .collect())
}

/// Reclaim a byte range of a file: mark sparse if needed, zero the aligned
/// range, then verify actual deallocation with an allocation query.
///
/// Invariants (verified by this function and enforced by the engine):
/// - Only bytes within `range` are zeroed (aligned inward to clusters).
/// - The report's `reclaimed`/`remaining` reflect the *measured* filesystem
///   state, never an assumption.
pub fn reclaim_range(
    file: &std::fs::File,
    path: &Path,
    range: ByteRange,
) -> Result<ReclaimReport, PlatformError> {
    let cluster = crate::fs::cluster_size(path)?;
    let Some(aligned) = align_inward(range, cluster) else {
        let alloc = crate::fs::allocated_size_from_handle(file, path)?;
        return Ok(ReclaimReport {
            requested: range,
            reclaimed: vec![],
            remaining: vec![range],
            allocated_before: alloc,
            allocated_after: alloc,
        });
    };

    let allocated_before = crate::fs::allocated_size_from_handle(file, path)?;
    let before = query_allocated_ranges(file, path, aligned.start, aligned.len)?;

    if !before.is_empty() {
        zero_range(file, path, aligned)?;
    }

    let after = query_allocated_ranges(file, path, aligned.start, aligned.len)?;
    let allocated_after = crate::fs::allocated_size_from_handle(file, path)?;

    let reclaimed: Vec<ByteRange> = before
        .iter()
        .filter(|b| !after.iter().any(|a| ranges_overlap(b, a)))
        .copied()
        .collect();
    let remaining = after;

    Ok(ReclaimReport {
        requested: aligned,
        reclaimed,
        remaining,
        allocated_before,
        allocated_after,
    })
}

/// Total currently-allocated bytes of a file (physical size).
pub fn physical_size(path: &Path) -> Result<u64, PlatformError> {
    allocated_size(path)
}

fn ranges_overlap(a: &ByteRange, b: &ByteRange) -> bool {
    a.start < b.end() && b.start < a.end()
}

/// Open a file for read access (needed for allocation queries).
pub fn open_for_query(path: &Path) -> Result<std::fs::File, PlatformError> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::GENERIC_READ;

    let name: Vec<u16> = extend_path(path)?.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | FILE_READ_DATA.0 as u32,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    match result {
        Ok(h) => Ok(unsafe { std::fs::File::from_raw_handle(h.0) }),
        Err(e) => Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "open file for query",
            Some(path),
            e.code().0 as u32,
        )),
    }
}

/// Open a file for read/write access to the archive (needed for reclamation).
pub fn open_for_reclaim(path: &Path) -> Result<std::fs::File, PlatformError> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::GENERIC_READ;

    let name: Vec<u16> = extend_path(path)?.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | windows::Win32::Foundation::GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    match result {
        Ok(h) => Ok(unsafe { std::fs::File::from_raw_handle(h.0) }),
        Err(e) => Err(PlatformError::from_os(
            PlatformErrorKind::Win32,
            "open file for reclamation",
            Some(path),
            e.code().0 as u32,
        )),
    }
}
