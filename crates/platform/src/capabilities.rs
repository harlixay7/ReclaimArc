//! Filesystem capability detection.
//!
//! Rather than trusting a name lookup, `filesystem_capabilities` performs a
//! *behavioral probe*: it creates a temporary file on the target volume,
//! marks it sparse, zeroes a range and verifies that allocation actually
//! dropped. The evidence is returned so callers can show exactly why
//! progressive extraction is or is not possible here.

use std::path::Path;

use crate::error::{PlatformError, PlatformErrorKind};
use crate::fs::{filesystem_name, same_storage_pool};
use crate::sparse::{align_inward, query_allocated_ranges, reclaim_range, set_sparse, ByteRange, SparseProbe};

/// Everything the engine needs to know about a destination volume.
#[derive(Debug, Clone)]
pub struct FilesystemCapabilities {
    /// Filesystem name from the volume ("NTFS", "ReFS", "exFAT", ...).
    pub name: String,
    /// Evidence from the behavioral probe.
    pub probe: SparseProbe,
    /// The engine refuses destructive mode when this is false.
    pub progressive_reclaim_supported: bool,
    /// Volume supports directory flushes (NTFS/ReFS).
    pub directory_flush_supported: bool,
}

/// Whether a path names a filesystem SpaceExtract trusts for destructive work.
fn trusted_filesystem(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.as_str(), "ntfs" | "refs")
}

/// Probe a volume by creating a temporary probe file inside `dir`.
///
/// Returns `(capabilities, cleaned_up_ok)`.
pub fn filesystem_capabilities(dir: &Path) -> Result<FilesystemCapabilities, PlatformError> {
    let name = filesystem_name(dir).unwrap_or_else(|_| "unknown".into());
    let trusted = trusted_filesystem(&name);

    let probe_path = dir.join(format!(".spacextract-probe-{}.bin", std::process::id()));
    let mut probe = SparseProbe {
        sparse_mark_ok: false,
        deallocation_ok: false,
        query_ok: false,
        filesystem: name.clone(),
        verdict: String::new(),
    };
    let probe_result = probe_inner(&probe_path, &mut probe);
    let _ = std::fs::remove_file(&probe_path);

    let supported = trusted && probe.fully_supported();
    probe.verdict = match (trusted, probe.fully_supported()) {
        (true, true) => format!("Sparse reclamation verified on {name}"),
        (true, false) => format!(
            "Filesystem {name} is trusted, but the behavioral probe failed (sparse={}, dealloc={}, query={}). Destructive extraction is DISABLED.",
            probe.sparse_mark_ok, probe.deallocation_ok, probe.query_ok
        ),
        (false, _) => format!(
            "Filesystem {name} is not trusted for destructive extraction. Only normal extraction is allowed."
        ),
    };

    if let Err(e) = probe_result {
        probe.verdict = format!("{} Probe error: {}", probe.verdict, e.message);
    }

    let dir_flush = crate::flush::directory_flush_supported(&name);
    Ok(FilesystemCapabilities {
        name,
        probe,
        progressive_reclaim_supported: supported,
        directory_flush_supported: dir_flush,
    })
}

fn probe_inner(probe_path: &Path, probe: &mut SparseProbe) -> Result<(), PlatformError> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut file = crate::sparse::open_for_reclaim(probe_path)?;
    // Write a patterned buffer (non-zero) to make allocation measurable.
    let mut buf = vec![0u8; 1 << 16];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    for _ in 0..16 {
        file.write_all(&buf).map_err(|e| {
            PlatformError::from_io(PlatformErrorKind::Io, "probe write", Some(probe_path), &e)
        })?;
    }
    file.sync_all().map_err(|e| {
        PlatformError::from_io(PlatformErrorKind::Io, "probe sync", Some(probe_path), &e)
    })?;

    match set_sparse(&file, probe_path) {
        Ok(()) => probe.sparse_mark_ok = true,
        Err(e) => {
            return Err(PlatformError::policy(
                PlatformErrorKind::UnsupportedFilesystem,
                format!("cannot mark probe file sparse: {}", e.message),
            ));
        }
    }

    // Verify the file is physically sparse-capable: query ranges.
    let range = ByteRange { start: 0, len: buf.len() as u64 * 16 };
    let before = query_allocated_ranges(&file, probe_path, 0, range.len);
    match before {
        Ok(_) => probe.query_ok = true,
        Err(_) => {
            return Err(PlatformError::policy(
                PlatformErrorKind::UnsupportedFilesystem,
                "FSCTL_QUERY_ALLOCATED_RANGES not supported on this volume",
            ));
        }
    }

    // Zero the middle half and verify deallocation.
    let cluster = crate::fs::cluster_size(probe_path)?;
    let middle = ByteRange { start: range.len / 4, len: range.len / 2 };
    let aligned = align_inward(middle, cluster);
    if let Some(aligned) = aligned {
        let report = reclaim_range(&file, probe_path, aligned)?;
        let total_reclaimed: u64 = report.reclaimed.iter().map(|r| r.len).sum();
        probe.deallocation_ok = total_reclaimed > 0;
    } else {
        probe.deallocation_ok = false;
    }

    // Byte integrity: unzeroed bytes must be unchanged. Read the whole file.
    file.seek(SeekFrom::Start(0)).map_err(|e| {
        PlatformError::from_io(PlatformErrorKind::Io, "probe seek", Some(probe_path), &e)
    })?;
    let mut all = Vec::new();
    file.read_to_end(&mut all).map_err(|e| {
        PlatformError::from_io(PlatformErrorKind::Io, "probe read-back", Some(probe_path), &e)
    })?;
    if all.len() != range.len as usize {
        return Err(PlatformError::policy(
            PlatformErrorKind::Precondition,
            format!("probe read-back size mismatch: {} != {}", all.len(), range.len),
        ));
    }
    for (i, b) in all.iter().enumerate() {
        let i64 = i as u64;
        let expected = if aligned.map(|a| a.start <= i64 && i64 < a.end()).unwrap_or(false) {
            0
        } else {
            (i % 251) as u8
        };
        if *b != expected {
            return Err(PlatformError::policy(
                PlatformErrorKind::Precondition,
                format!("probe byte {i} corrupted: expected {expected}, got {b}"),
            ));
        }
    }
    Ok(())
}

/// Convenience for the engine: is progressive extraction *possible* for the
/// destination, and does the archive share its storage pool?
#[derive(Debug, Clone)]
pub struct ReclaimFeasibility {
    /// Whether progressive reclamation is possible and helpful here.
    pub supported: bool,
    /// Whether the archive and destination share a storage pool.
    pub same_volume: bool,
    /// Human-readable explanation of the verdict.
    pub reason: String,
}

/// Assess whether reclaiming source bytes can help a destination.
pub fn reclamation_feasible(
    archive_path: &Path,
    destination: &Path,
) -> Result<ReclaimFeasibility, PlatformError> {
    let caps = filesystem_capabilities(destination)?;
    let same = same_storage_pool(archive_path, destination)?;
    let supported = caps.progressive_reclaim_supported && same;
    let reason = if !caps.progressive_reclaim_supported {
        caps.probe.verdict.clone()
    } else if !same {
        "The archive and destination are on different volumes: reclaiming source space cannot increase capacity available to the destination.".into()
    } else {
        "Progressive extraction is possible on this volume.".into()
    };
    Ok(ReclaimFeasibility { supported, same_volume: same, reason })
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::fs::cluster_size;
    use crate::sparse::open_for_reclaim;

    /// Real Windows integration test: sparse-capable temp file, reclaim a
    /// middle range, verify allocated size drops and unrelated bytes are
    /// unchanged. Skips cleanly on volumes without sparse support.
    #[test]
    fn sparse_reclaim_releases_allocation_and_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reclaim.bin");
        let caps = filesystem_capabilities(dir.path()).unwrap();
        if !caps.progressive_reclaim_supported {
            eprintln!("SKIP: volume '{}' does not support sparse reclamation: {}", caps.name, caps.probe.verdict);
            return;
        }
        assert!(caps.directory_flush_supported);

        use std::io::{Seek, SeekFrom, Write};
        let mut file = open_for_reclaim(&path).unwrap();
        let mut buf = vec![0u8; 65536];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i * 7 % 251) as u8;
        }
        for _ in 0..64 {
            file.write_all(&buf).unwrap();
        }
        file.sync_all().unwrap();
        let total = 64u64 * 65536;

        // A file must be marked sparse before ranges can be deallocated.
        set_sparse(&file, &path).unwrap();

        let cluster = cluster_size(&path).unwrap();
        let middle = ByteRange { start: total / 4, len: total / 2 };
        let aligned = align_inward(middle, cluster).unwrap();
        assert!(aligned.start >= middle.start && aligned.end() <= middle.end());

        let before_alloc = crate::fs::allocated_size_from_handle(&file, &path).unwrap();
        let whole_before = query_allocated_ranges(&file, &path, 0, total).unwrap();
        let before_sum: u64 = whole_before.iter().map(|r| r.len).sum();
        let report = reclaim_range(&file, &path, aligned).unwrap();
        let after_alloc = crate::fs::allocated_size_from_handle(&file, &path).unwrap();
        let whole_after = query_allocated_ranges(&file, &path, 0, total).unwrap();
        let after_sum: u64 = whole_after.iter().map(|r| r.len).sum();

        assert_eq!(before_sum, total, "fully written file should be fully allocated");
        assert_eq!(after_sum, total - aligned.len, "only the aligned range is deallocated");
        assert_eq!(after_alloc, after_sum, "measured allocated size must match allocation query");
        assert!(!report.reclaimed.is_empty(), "middle range must be reclaimed");
        let reclaimed_sum: u64 = report.reclaimed.iter().map(|r| r.len).sum();
        assert_eq!(
            before_sum - after_sum,
            reclaimed_sum,
            "deallocation must be exactly the sum of reclaimed ranges"
        );
        assert_eq!(before_alloc, before_sum);
        assert!(report.remaining.iter().all(|r| r.len == 0 || r.start >= aligned.start && r.end() <= aligned.end()));
        assert!(report.remaining.iter().all(|r| r.len == 0 || r.start >= aligned.start && r.end() <= aligned.end()));

        // Byte integrity: everything outside the aligned range is unchanged,
        // everything inside reads back as zero.
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut all = Vec::new();
        file.read_to_end(&mut all).unwrap();
        assert_eq!(all.len() as u64, total);
        for (i, b) in all.iter().enumerate() {
            let i = i as u64;
            if i >= aligned.start && i < aligned.end() {
                assert_eq!(*b, 0, "zeroed byte at {i}");
            } else {
                // Pattern resets every 64 KiB buffer: byte = (idx_in_buffer * 7) % 251.
                assert_eq!(*b as u64, ((i % 65536) * 7) % 251, "byte outside range at {i} must be unchanged");
            }
        }
    }

    #[test]
    fn probe_file_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let _ = filesystem_capabilities(dir.path()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftovers.is_empty(), "probe file must be removed");
    }
}

