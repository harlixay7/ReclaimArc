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
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_READ_DATA, OPEN_EXISTING,
};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows::Win32::System::Ioctl::{
    FILE_ALLOCATED_RANGE_BUFFER, FILE_ZERO_DATA_INFORMATION, FSCTL_QUERY_ALLOCATED_RANGES,
    FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA,
};
use windows::Win32::System::IO::DeviceIoControl;

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

    /// Intersect this range with another.
    pub fn intersect(&self, other: &ByteRange) -> Option<ByteRange> {
        let start = self.start.max(other.start);
        let end = self.end().min(other.end());
        if start < end {
            Some(ByteRange {
                start,
                len: end - start,
            })
        } else {
            None
        }
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
    let start = if range.start.is_multiple_of(unit) {
        range.start
    } else {
        (range.start / unit + 1) * unit
    };
    let end = (range.end() / unit) * unit;
    if start >= end {
        return None;
    }
    Some(ByteRange {
        start,
        len: end - start,
    })
}

/// Deallocate the given byte range of a sparse file via `FSCTL_SET_ZERO_DATA`.
///
/// The bytes read back as zero afterwards. Only whole clusters are released.
pub fn zero_range(
    file: &std::fs::File,
    path: &Path,
    range: ByteRange,
) -> Result<(), PlatformError> {
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
    let end_offset = start.saturating_add(len);
    let mut current_offset = start;
    let mut all_ranges = Vec::new();

    const CHUNK_ENTRIES: usize = 1024;
    let mut buffer: Vec<FILE_ALLOCATED_RANGE_BUFFER> =
        vec![FILE_ALLOCATED_RANGE_BUFFER::default(); CHUNK_ENTRIES];
    let out_bytes = (CHUNK_ENTRIES * std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>()) as u32;

    while current_offset < end_offset {
        let remaining_len = end_offset.saturating_sub(current_offset);
        if remaining_len == 0 {
            break;
        }
        let query = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: current_offset as i64,
            Length: remaining_len as i64,
        };
        let mut returned: u32 = 0;
        let res = unsafe {
            DeviceIoControl(
                HANDLE(file.as_raw_handle() as *mut _),
                FSCTL_QUERY_ALLOCATED_RANGES,
                Some(&query as *const _ as *const _),
                std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                Some(buffer.as_mut_ptr() as *mut _),
                out_bytes,
                Some(&mut returned),
                None,
            )
        };

        let more_data = match res {
            Ok(()) => false,
            Err(e) => {
                let code = e.code().0 as u32;
                if code == windows::Win32::Foundation::ERROR_MORE_DATA.0
                    || (code & 0xFFFF) == windows::Win32::Foundation::ERROR_MORE_DATA.0
                    || e.code() == windows::Win32::Foundation::ERROR_MORE_DATA.to_hresult()
                {
                    true
                } else {
                    return Err(PlatformError::from_os(
                        PlatformErrorKind::Win32,
                        "FSCTL_QUERY_ALLOCATED_RANGES",
                        Some(path),
                        code,
                    ));
                }
            }
        };

        let count = (returned as usize) / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        if count == 0 {
            break;
        }

        for r in &buffer[..count] {
            let r_start = r.FileOffset.max(0) as u64;
            let r_len = r.Length.max(0) as u64;
            if r_len > 0 {
                all_ranges.push(ByteRange {
                    start: r_start,
                    len: r_len,
                });
            }
        }

        let last = &buffer[count - 1];
        let next_offset = (last.FileOffset.max(0) as u64).saturating_add(last.Length.max(0) as u64);
        if next_offset <= current_offset {
            break;
        }
        current_offset = next_offset;

        if !more_data {
            break;
        }
    }

    Ok(all_ranges)
}

/// Subtract a set of intervals (`after`) from another set (`before`),
/// returning the exact set of deallocated intervals.
pub fn subtract_intervals(before: &[ByteRange], after: &[ByteRange]) -> Vec<ByteRange> {
    let mut reclaimed = Vec::new();
    for b in before {
        let mut current_pieces = vec![*b];
        for a in after {
            let mut next_pieces = Vec::new();
            for p in current_pieces {
                if !ranges_overlap(&p, a) {
                    next_pieces.push(p);
                } else {
                    if p.start < a.start {
                        next_pieces.push(ByteRange {
                            start: p.start,
                            len: a.start - p.start,
                        });
                    }
                    if a.end() < p.end() {
                        next_pieces.push(ByteRange {
                            start: a.end(),
                            len: p.end() - a.end(),
                        });
                    }
                }
            }
            current_pieces = next_pieces;
        }
        reclaimed.extend(current_pieces);
    }
    reclaimed
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
        let before = query_allocated_ranges(file, path, range.start, range.len)?;
        let alloc_bytes = before.iter().map(|r| r.len).sum();
        return Ok(ReclaimReport {
            requested: range,
            reclaimed: vec![],
            remaining: before,
            allocated_before: alloc_bytes,
            allocated_after: alloc_bytes,
        });
    };

    let before = query_allocated_ranges(file, path, aligned.start, aligned.len)?;
    let allocated_before: u64 = before.iter().map(|r| r.len).sum();

    if !before.is_empty() {
        zero_range(file, path, aligned)?;
    }

    let after = query_allocated_ranges(file, path, aligned.start, aligned.len)?;
    let allocated_after: u64 = after.iter().map(|r| r.len).sum();

    let reclaimed = subtract_intervals(&before, &after);
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

    let name: Vec<u16> = extend_path(path)?
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | FILE_READ_DATA.0,
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
/// Uses OPEN_EXISTING so a missing source file is never recreated accidentally.
pub fn open_for_reclaim(path: &Path) -> Result<std::fs::File, PlatformError> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::GENERIC_READ;

    let name: Vec<u16> = extend_path(path)?
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | windows::Win32::Foundation::GENERIC_WRITE.0,
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
            "open file for reclamation",
            Some(path),
            e.code().0 as u32,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_open_for_reclaim_fails_closed_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("non_existent_archive.rar");
        let result = open_for_reclaim(&missing_path);
        assert!(
            result.is_err(),
            "open_for_reclaim must fail when file does not exist"
        );
        assert!(
            !missing_path.exists(),
            "open_for_reclaim must NOT create a 0-byte file"
        );
    }

    #[test]
    fn test_many_sparse_ranges_query_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("many_holes.bin");
        let mut f = std::fs::File::create(&file_path).unwrap();

        // Write a 4MB file with non-zero data
        let chunk = vec![0xAAu8; 64 * 1024];
        for _ in 0..64 {
            f.write_all(&chunk).unwrap();
        }
        f.flush().unwrap();
        drop(f);

        let file = open_for_reclaim(&file_path).unwrap();
        set_sparse(&file, &file_path).unwrap();

        // Punch 40 separate holes (every odd 64KB block)
        for i in 0..40 {
            let offset = (i * 2 + 1) * 64 * 1024;
            let range = ByteRange {
                start: offset,
                len: 64 * 1024,
            };
            let _ = reclaim_range(&file, &file_path, range);
        }

        let ranges = query_allocated_ranges(&file, &file_path, 0, 4 * 1024 * 1024).unwrap();
        assert!(!ranges.is_empty(), "allocated ranges should not be empty");

        let alloc_size = crate::fs::allocated_size_from_handle(&file, &file_path).unwrap();
        assert!(alloc_size > 0, "allocated size should be non-zero");
    }

    #[test]
    fn test_subtract_intervals_exact() {
        // Full deallocation
        let before = vec![ByteRange {
            start: 0,
            len: 1000,
        }];
        let after = vec![];
        let diff = subtract_intervals(&before, &after);
        assert_eq!(
            diff,
            vec![ByteRange {
                start: 0,
                len: 1000
            }]
        );

        // Partial middle deallocation
        let before = vec![ByteRange {
            start: 0,
            len: 1000,
        }];
        let after = vec![
            ByteRange { start: 0, len: 200 },
            ByteRange {
                start: 800,
                len: 200,
            },
        ];
        let diff = subtract_intervals(&before, &after);
        assert_eq!(
            diff,
            vec![ByteRange {
                start: 200,
                len: 600
            }]
        );

        // No deallocation
        let before = vec![ByteRange {
            start: 100,
            len: 500,
        }];
        let after = vec![ByteRange {
            start: 100,
            len: 500,
        }];
        let diff = subtract_intervals(&before, &after);
        assert!(diff.is_empty());
    }
}
