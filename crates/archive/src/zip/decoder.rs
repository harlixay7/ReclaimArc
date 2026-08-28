//! Streaming zip-rs decoder wrapper with strict output bounds, hard size ceiling,
//! and archive CRC32 verification.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::ArchiveError;
use crate::model::{IntegrityReport, ProgressEvent};

/// Managed ZIP decoder instance.
pub struct ZipDecoder {
    path: PathBuf,
    password: Option<String>,
    archive: Option<zip::ZipArchive<File>>,
}

impl ZipDecoder {
    /// Open a decoder for the archive at `path`.
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self, ArchiveError> {
        let file = File::open(path)
            .map_err(|e| ArchiveError::open(format!("cannot open '{}': {e}", path.display())))?;
        let archive = zip::ZipArchive::new(file)
            .map_err(|e| ArchiveError::invalid(format!("cannot open ZIP archive: {e}")))?;

        Ok(ZipDecoder {
            path: path.to_path_buf(),
            password: password.map(|s| s.to_string()),
            archive: Some(archive),
        })
    }

    /// Re-open the archive reader if closed.
    fn ensure_archive(&mut self) -> Result<&mut zip::ZipArchive<File>, ArchiveError> {
        if self.archive.is_none() {
            let file = File::open(&self.path).map_err(|e| {
                ArchiveError::open(format!("cannot re-open '{}': {e}", self.path.display()))
            })?;
            let archive = zip::ZipArchive::new(file)
                .map_err(|e| ArchiveError::invalid(format!("cannot open ZIP archive: {e}")))?;
            self.archive = Some(archive);
        }
        Ok(self.archive.as_mut().unwrap())
    }

    /// Close the underlying file handle so that Windows can safely deallocate
    /// sparse zero ranges without file locking conflicts.
    pub fn close(&mut self) {
        self.archive = None;
    }

    /// Number of entries in the archive.
    pub fn len(&mut self) -> Result<usize, ArchiveError> {
        let archive = self.ensure_archive()?;
        Ok(archive.len())
    }

    /// Whether the archive has no entries.
    pub fn is_empty(&mut self) -> Result<bool, ArchiveError> {
        Ok(self.len()? == 0)
    }

    /// Run full integrity testing over every entry, streaming decompressed data to EOF
    /// to trigger zip-rs and independent crc32fast verification and ensure exact uncompressed size match.
    pub fn test_integrity(
        &mut self,
        cancel: Option<Arc<AtomicBool>>,
        mut progress: Option<&mut (dyn FnMut(ProgressEvent) -> bool + '_)>,
    ) -> Result<IntegrityReport, ArchiveError> {
        let password = self.password.clone();
        let archive = self.ensure_archive()?;
        let num_entries = archive.len();
        let mut total_bytes_tested: u64 = 0;
        let mut buf = vec![0u8; 65536];

        for i in 0..num_entries {
            if let Some(ref c) = cancel {
                if c.load(Ordering::SeqCst) {
                    return Err(ArchiveError::Cancelled);
                }
            }

            let mut zip_file = if let Some(ref pw) = password {
                archive.by_index_decrypt(i, pw.as_bytes()).map_err(|e| {
                    ArchiveError::invalid(format!("entry {i} decryption/access failed: {e}"))
                })?
            } else {
                archive
                    .by_index(i)
                    .map_err(|e| ArchiveError::invalid(format!("entry {i} access failed: {e}")))?
            };

            if zip_file.is_dir() {
                continue;
            }

            let expected_size = zip_file.size();
            let expected_crc = zip_file.crc32();
            let mut hasher = crc32fast::Hasher::new();
            let mut read_bytes: u64 = 0;

            loop {
                if let Some(ref c) = cancel {
                    if c.load(Ordering::SeqCst) {
                        return Err(ArchiveError::Cancelled);
                    }
                }

                let remaining = expected_size.saturating_sub(read_bytes);
                let to_read = if remaining == 0 {
                    1 // 1-byte read at expected EOF to trigger CRC check and verify no trailing data
                } else {
                    buf.len().min(remaining as usize)
                };

                match zip_file.read(&mut buf[..to_read]) {
                    Ok(0) => {
                        // EOF reached
                        if read_bytes != expected_size {
                            return Ok(IntegrityReport {
                                ok: false,
                                bytes_tested: total_bytes_tested,
                                first_failure: Some(i as u64),
                                failure: Some(format!(
                                    "entry {i} premature EOF: expected {expected_size} bytes, got {read_bytes}"
                                )),
                            });
                        }
                        let computed_crc = hasher.finalize();
                        if computed_crc != expected_crc {
                            return Ok(IntegrityReport {
                                ok: false,
                                bytes_tested: total_bytes_tested,
                                first_failure: Some(i as u64),
                                failure: Some(format!(
                                    "entry {i} CRC32 mismatch: expected 0x{expected_crc:08x}, computed 0x{computed_crc:08x}"
                                )),
                            });
                        }
                        break;
                    }
                    Ok(n) => {
                        if read_bytes >= expected_size {
                            return Ok(IntegrityReport {
                                ok: false,
                                bytes_tested: total_bytes_tested,
                                first_failure: Some(i as u64),
                                failure: Some(format!(
                                    "entry {i} decompressed data exceeded declared uncompressed size ({expected_size})"
                                )),
                            });
                        }
                        hasher.update(&buf[..n]);
                        read_bytes = read_bytes.saturating_add(n as u64);
                        total_bytes_tested = total_bytes_tested.saturating_add(n as u64);

                        if let Some(ref mut cb) = progress {
                            let keep_going = cb(ProgressEvent::EntryProgress {
                                entry_index: i as u64,
                                current: read_bytes,
                                total: expected_size,
                            });
                            if !keep_going {
                                return Err(ArchiveError::Cancelled);
                            }
                        }
                    }
                    Err(e) => {
                        return Ok(IntegrityReport {
                            ok: false,
                            bytes_tested: total_bytes_tested,
                            first_failure: Some(i as u64),
                            failure: Some(format!("entry {i} decompression or CRC error: {e}")),
                        });
                    }
                }
            }
        }

        Ok(IntegrityReport {
            ok: true,
            bytes_tested: total_bytes_tested,
            first_failure: None,
            failure: None,
        })
    }

    /// Extract a single entry to its partial path, enforcing an exact hard ceiling on output size.
    pub fn extract_file(
        &mut self,
        index: usize,
        out_path: &Path,
        expected_size: u64,
        max_compression_ratio: Option<u64>,
        cancel: Option<Arc<AtomicBool>>,
        mut progress: Option<&mut (dyn FnMut(ProgressEvent) -> bool + '_)>,
    ) -> Result<u64, ArchiveError> {
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                ArchiveError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "cannot create destination directory '{}': {e}",
                        parent.display()
                    ),
                ))
            })?;
        }

        let mut out_file = File::create(out_path).map_err(|e| {
            ArchiveError::Io(std::io::Error::new(
                e.kind(),
                format!("cannot create partial file '{}': {e}", out_path.display()),
            ))
        })?;

        let password = self.password.clone();
        let archive = self.ensure_archive()?;
        let mut zip_file = if let Some(ref pw) = password {
            archive
                .by_index_decrypt(index, pw.as_bytes())
                .map_err(|e| {
                    ArchiveError::invalid(format!("entry {index} decryption failed: {e}"))
                })?
        } else {
            archive
                .by_index(index)
                .map_err(|e| ArchiveError::invalid(format!("entry {index} access failed: {e}")))?
        };

        if zip_file.is_dir() {
            return Ok(0);
        }

        let expected_crc = zip_file.crc32();
        let mut hasher = crc32fast::Hasher::new();
        let mut written: u64 = 0;
        let mut buf = vec![0u8; 65536];

        loop {
            if let Some(ref c) = cancel {
                if c.load(Ordering::SeqCst) {
                    let _ = fs::remove_file(out_path);
                    return Err(ArchiveError::Cancelled);
                }
            }

            let remaining = expected_size.saturating_sub(written);
            let to_read = if remaining == 0 {
                1 // Boundary probe
            } else {
                buf.len().min(remaining as usize)
            };

            match zip_file.read(&mut buf[..to_read]) {
                Ok(0) => {
                    // EOF reached cleanly
                    if written != expected_size {
                        let _ = fs::remove_file(out_path);
                        return Err(ArchiveError::invalid(format!(
                            "entry {index} truncated: expected {expected_size} bytes, decompressed {written}"
                        )));
                    }
                    let computed_crc = hasher.finalize();
                    if computed_crc != expected_crc {
                        let _ = fs::remove_file(out_path);
                        return Err(ArchiveError::Corrupt(format!(
                            "entry {index} CRC32 mismatch: expected 0x{expected_crc:08x}, computed 0x{computed_crc:08x}"
                        )));
                    }
                    break;
                }
                Ok(n) => {
                    if written >= expected_size {
                        // Decoder produced data beyond planned size -> fail before writing excess byte
                        let _ = fs::remove_file(out_path);
                        return Err(ArchiveError::invalid(format!(
                            "entry {index} decompressed data exceeded planned size ({expected_size})"
                        )));
                    }

                    hasher.update(&buf[..n]);
                    out_file.write_all(&buf[..n]).map_err(|e| {
                        let _ = fs::remove_file(out_path);
                        ArchiveError::Io(std::io::Error::new(
                            e.kind(),
                            format!("write error on partial file '{}': {e}", out_path.display()),
                        ))
                    })?;

                    written = written.saturating_add(n as u64);

                    let max_ratio = match max_compression_ratio {
                        Some(0) | None => 1000,
                        Some(r) => r,
                    };

                    let comp_size = zip_file.compressed_size();
                    if comp_size > 0 && written > 10_000_000 && (written / comp_size) > max_ratio {
                        let _ = fs::remove_file(out_path);
                        return Err(ArchiveError::invalid(format!(
                            "entry {index} observed compression ratio ({}x) exceeds safety limit of {max_ratio}:1 (possible zip bomb)",
                            written / comp_size
                        )));
                    }

                    if let Some(ref mut cb) = progress {
                        let keep_going = cb(ProgressEvent::EntryProgress {
                            entry_index: index as u64,
                            current: written,
                            total: expected_size,
                        });
                        if !keep_going {
                            let _ = fs::remove_file(out_path);
                            return Err(ArchiveError::Cancelled);
                        }
                    }
                }
                Err(e) => {
                    let _ = fs::remove_file(out_path);
                    return Err(ArchiveError::Corrupt(format!(
                        "entry {index} decompression or CRC verification error: {e}"
                    )));
                }
            }
        }

        out_file.sync_all().map_err(|e| {
            ArchiveError::Io(std::io::Error::new(
                e.kind(),
                format!("flush error on partial file '{}': {e}", out_path.display()),
            ))
        })?;

        Ok(written)
    }
}
