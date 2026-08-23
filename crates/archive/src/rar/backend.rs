//! RAR backend: implements `ArchiveBackend` for RAR 4.x / 5.x using the
//! official UnRAR library for decoding and our own header parser for exact
//! packed ranges, solid chains and recovery units.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::backend::{ArchiveBackend, ExtractOptions, ExtractedFile, OpenOptions};
use crate::error::ArchiveError;
use crate::model::{
    ArchiveInfo, CapabilityMatrix, DecoderRequirements, Entry, IntegrityReport, ProgressEvent,
    RecoveryUnit, RetirementProof, UnitExtractReport, VolumeInfo,
};
use crate::rar::decoder::{OpenMode, Operation, Unrar};
use crate::rar::parser::{build_recovery_units, parse, VolumeMeta};
use crate::rar::volumes::{describe, discover_volumes};

/// The RAR archive backend.
pub struct RarBackend {
    first_volume: PathBuf,
    info: Option<ArchiveInfo>,
    /// Streaming pass state.
    decoder: Option<Unrar>,
    next_index: u64,
    stop_at: u64,
    pass_done: bool,
}

impl RarBackend {
    /// Create a backend for the archive at `path` (any volume works).
    pub fn new(path: &Path) -> RarBackend {
        RarBackend {
            first_volume: path.to_path_buf(),
            info: None,
            decoder: None,
            next_index: 0,
            stop_at: 0,
            pass_done: false,
        }
    }
}

/// The partial output path for an entry, derived ONLY from the engine's
/// validated name map and the engine-chosen per-attempt suffix.
fn partial_output_path(
    options: &ExtractOptions,
    entry_index: u64,
) -> Result<PathBuf, ArchiveError> {
    let validated = options.name_map.get(&entry_index).ok_or_else(|| {
        ArchiveError::invalid(format!(
            "entry index {entry_index} has no validated output mapping in name_map"
        ))
    })?;
    let suffix = if options.partial_suffix.is_empty() {
        format!(".sx-partial-{}", options.job_id)
    } else {
        options.partial_suffix.clone()
    };
    let path = options.dest_dir.join(format!("{validated}{suffix}"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    Ok(path)
}

impl ArchiveBackend for RarBackend {
    fn inspect(&mut self, options: &OpenOptions) -> Result<ArchiveInfo, ArchiveError> {
        let set = discover_volumes(&self.first_volume)?;
        let mut volumes_meta = Vec::new();
        for (i, p) in set.paths.iter().enumerate() {
            let len = std::fs::metadata(p)
                .map_err(|e| ArchiveError::open(format!("cannot stat '{}': {e}", p.display())))?
                .len();
            volumes_meta.push(VolumeMeta {
                path: p.clone(),
                len,
            });
            let _ = i;
        }

        let parsed = parse(volumes_meta)?;
        let units = build_recovery_units(&parsed);

        // Cross-validate our parser against the official library's header
        // stream (names, sizes, CRC, flags). This is the guarantee that the
        // packed ranges we compute match what the decoder sees.
        let mut decoder = Unrar::open(
            &parsed.volumes[0].path,
            OpenMode::List,
            options.password.clone(),
            None,
        )?;
        let mut lib_entries = 0usize;
        while let Some(h) = decoder.read_header()? {
            if lib_entries >= parsed.entries.len() {
                return Err(ArchiveError::invalid(format!(
                    "decoder reported more entries ({}) than the parser found ({})",
                    lib_entries + 1,
                    parsed.entries.len()
                )));
            }
            let p_entry = &parsed.entries[lib_entries];
            // Names may differ in separator normalization; compare with
            // forward slashes normalized.
            let lib_name = h.file_name_w.replace('\\', "/");
            let our_name = p_entry.name.replace('\\', "/");
            if !same_name(&lib_name, &our_name) {
                return Err(ArchiveError::invalid(format!(
                    "parser/decoder name mismatch at entry {lib_entries}: parser='{our_name}' decoder='{lib_name}'"
                )));
            }
            if h.unp_size != p_entry.unpacked_size {
                return Err(ArchiveError::invalid(format!(
                    "parser/decoder unpacked-size mismatch at entry {lib_entries}: parser={} decoder={}",
                    p_entry.unpacked_size, h.unp_size
                )));
            }
            // Directory flag check (UnRAR HeaderDataEx.Flags bit 0x20 = RHDF_DIRECTORY)
            let lib_is_dir = (h.flags & 0x20) != 0;
            if lib_is_dir != p_entry.is_directory {
                return Err(ArchiveError::invalid(format!(
                    "parser/decoder directory-flag mismatch at entry {lib_entries}: parser={} decoder={}",
                    p_entry.is_directory, lib_is_dir
                )));
            }
            // Solid flag check (UnRAR HeaderDataEx.Flags bit 0x10 = RHDF_SOLID)
            if (h.flags & 0x10 != 0) != p_entry.is_solid && p_entry.unpacked_size > 0 {
                return Err(ArchiveError::invalid(format!(
                    "parser/decoder solid-flag mismatch at entry {lib_entries}"
                )));
            }
            // Encryption check (UnRAR HeaderDataEx.Flags bit 0x04 = RHDF_ENCRYPTED)
            let lib_encrypted = (h.flags & 0x04) != 0;
            if lib_encrypted != p_entry.encrypted {
                return Err(ArchiveError::invalid(format!(
                    "parser/decoder encryption mismatch at entry {lib_entries}: parser={} decoder={}",
                    p_entry.encrypted, lib_encrypted
                )));
            }
            // Split flag checks: in single-volume archives, verify split flags directly.
            // In multipart archives, UnRAR's first-volume header only reflects the first volume's split flags.
            let lib_split_before = (h.flags & 0x01) != 0;
            let lib_split_after = (h.flags & 0x02) != 0;
            if parsed.volumes.len() == 1 {
                if lib_split_before != p_entry.split_before {
                    return Err(ArchiveError::invalid(format!(
                        "parser/decoder split-before mismatch at entry {lib_entries}: parser={} decoder={}",
                        p_entry.split_before, lib_split_before
                    )));
                }
                if lib_split_after != p_entry.split_after {
                    return Err(ArchiveError::invalid(format!(
                        "parser/decoder split-after mismatch at entry {lib_entries}: parser={} decoder={}",
                        p_entry.split_after, lib_split_after
                    )));
                }
            }
            // File CRC check when both have non-zero CRC32
            if let Some(p_crc) = p_entry.crc32 {
                if h.file_crc != 0 && p_crc != h.file_crc {
                    return Err(ArchiveError::invalid(format!(
                        "parser/decoder file CRC mismatch at entry {lib_entries}: parser=0x{p_crc:08x} decoder=0x{:08x}",
                        h.file_crc
                    )));
                }
            }
            // For non-split single volume files, verify packed size matches UnRAR pack_size
            if !p_entry.split_before
                && !p_entry.split_after
                && parsed.volumes.len() == 1
                && h.pack_size != p_entry.packed_size
            {
                return Err(ArchiveError::invalid(format!(
                    "parser/decoder packed-size mismatch at entry {lib_entries}: parser={} decoder={}",
                    p_entry.packed_size, h.pack_size
                )));
            }
            // File CRC check when both have non-zero CRC32
            if let Some(p_crc) = p_entry.crc32 {
                if h.file_crc != 0 && p_crc != h.file_crc {
                    return Err(ArchiveError::invalid(format!(
                        "parser/decoder file CRC mismatch at entry {lib_entries}: parser=0x{p_crc:08x} decoder=0x{:08x}",
                        h.file_crc
                    )));
                }
            }

            // Advance the decoder to the next header (DLL contract: every
            // ReadHeader must be followed by a ProcessFile).
            decoder.process_file(
                Operation::Skip,
                None,
                None,
                None,
                lib_entries as u64,
                h.pack_size,
            )?;
            lib_entries += 1;
        }
        drop(decoder);
        if lib_entries != parsed.entries.len() {
            return Err(ArchiveError::invalid(format!(
                "decoder reported fewer entries ({lib_entries}) than the parser found ({})",
                parsed.entries.len()
            )));
        }

        let packed_size: u64 = parsed.packed_size;
        let unpacked_size: u64 = parsed.unpacked_size;

        let mut notes = Vec::new();
        if parsed.solid_archive {
            notes.push(
                "Archive-level solid flag set: the whole archive is one recovery unit; \
                 progressive reclamation requires enough space for the full chain."
                    .to_string(),
            );
        }
        notes.push(format!(
            "Volumes: {} ({}).",
            parsed.volumes.len(),
            describe(&set)
        ));

        let capability = CapabilityMatrix {
            format: parsed.format.as_str().to_string(),
            supports_test_integrity: true,
            restartable_units: true,
            progressive_reclaim: true,
            supports_encryption: true,
            supports_multipart: parsed.volumes.len() > 1,
            notes,
        };

        let volumes: Vec<VolumeInfo> = parsed
            .volumes
            .iter()
            .enumerate()
            .map(|(i, v)| VolumeInfo {
                index: i as u64,
                path: v.path.clone(),
                logical_size: v.len,
            })
            .collect();

        let info = ArchiveInfo {
            format: parsed.format.as_str().to_string(),
            packed_size,
            unpacked_size,
            solid_archive: parsed.solid_archive,
            encrypted_headers: parsed.encrypted_headers,
            volumes,
            entries: parsed.entries.clone(),
            recovery_units: units,
            capability,
            decoder_requirements: DecoderRequirements {
                scratch_bytes: 0,
                redecodes_prefix: false,
            },
        };
        self.first_volume = info.volumes[0].path.clone();
        self.info = Some(info.clone());
        Ok(info)
    }

    fn test_integrity<'p, 'c>(
        &mut self,
        password: Option<&str>,
        cancel: Option<Arc<AtomicBool>>,
        progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
    ) -> Result<IntegrityReport, ArchiveError> {
        let info = self.info.clone().ok_or_else(|| {
            ArchiveError::open("inspect() must be called before test_integrity()")
        })?;
        let first = &info.volumes[0].path;

        let mut decoder = Unrar::open(
            first,
            OpenMode::Process,
            password.map(|s| s.to_string()),
            cancel,
        )?;
        let mut bytes_tested: u64 = 0;
        let mut index = 0u64;
        let mut progress = progress;
        let total_archive_packed = info.packed_size;
        while let Some(h) = decoder.read_header()? {
            let total = h.pack_size;
            let current_tested = bytes_tested;
            let mut sub_progress = progress.as_deref_mut().map(|cb| {
                move |e: ProgressEvent| match e {
                    ProgressEvent::EntryProgress {
                        current,
                        total: entry_total,
                        ..
                    } => {
                        let ratio = if entry_total > 0 {
                            (current as f64) / (entry_total as f64)
                        } else {
                            1.0
                        };
                        let entry_done = (total as f64 * ratio.min(1.0)) as u64;
                        let overall_done = current_tested.saturating_add(entry_done);
                        cb(ProgressEvent::EntryProgress {
                            entry_index: index,
                            current: overall_done,
                            total: total_archive_packed,
                        })
                    }
                }
            });
            let result = decoder.process_file(
                Operation::Test,
                None,
                None,
                sub_progress
                    .as_mut()
                    .map(|f| f as &mut dyn FnMut(ProgressEvent) -> bool),
                index,
                total,
            );
            match result {
                Ok(()) => {
                    bytes_tested = bytes_tested.saturating_add(total);
                    index += 1;
                }
                Err(ArchiveError::Corrupt(msg)) => {
                    return Ok(IntegrityReport {
                        ok: false,
                        bytes_tested,
                        first_failure: Some(index),
                        failure: Some(msg),
                    });
                }
                Err(ArchiveError::Cancelled) => return Err(ArchiveError::Cancelled),
                Err(e) => return Err(e),
            }
        }
        Ok(IntegrityReport {
            ok: true,
            bytes_tested,
            first_failure: None,
            failure: None,
        })
    }

    fn extract_unit<'p, 'c>(
        &mut self,
        unit_seq: u64,
        options: &ExtractOptions,
        progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
    ) -> Result<UnitExtractReport, ArchiveError> {
        let info = self
            .info
            .clone()
            .ok_or_else(|| ArchiveError::open("inspect() must be called before extract_unit()"))?;
        let unit = info
            .recovery_units
            .iter()
            .find(|u| u.seq == unit_seq)
            .ok_or_else(|| ArchiveError::NotFound(format!("recovery unit {unit_seq}")))?;

        let first = &info.volumes[0].path;
        let mut decoder = Unrar::open(
            first,
            OpenMode::Process,
            options.password.clone(),
            options.cancel.clone(),
        )?;

        let mut extracted: Vec<u64> = Vec::new();
        let mut bytes_written: u64 = 0;
        let mut index = 0u64;
        let mut progress = progress;
        let _ = info;

        while let Some(h) = decoder.read_header()? {
            if index < unit.first_entry {
                // Belongs to an already-committed unit.
                // Split files cannot be seek-skipped in PROCESS mode (the
                // DLL fails on the continuation with a checksum error —
                // verified empirically), so process them in TEST mode
                // instead: the whole split file is decoded and CRC-verified,
                // and the decoder lands past it.
                let is_split = h.flags & 1 != 0 || h.flags & 2 != 0;
                let op = if is_split {
                    Operation::Test
                } else {
                    Operation::Skip
                };
                decoder.process_file(op, None, None, None, index, h.pack_size)?;
            } else if index <= unit.last_entry {
                let entry = self
                    .info
                    .as_ref()
                    .ok_or_else(|| {
                        ArchiveError::open("inspect() must be called before extract_unit()")
                    })?
                    .entries
                    .iter()
                    .find(|e| e.index == index)
                    .ok_or_else(|| ArchiveError::NotFound(format!("entry {index}")))?;
                if entry.is_directory || entry.redirection.is_some() {
                    // Directories are created by the engine after commit; redirections/links
                    // are skipped under SymlinkPolicy::Skip to prevent hostile link creation.
                    decoder.process_file(Operation::Skip, None, None, None, index, h.pack_size)?;
                } else {
                    let partial = partial_output_path(options, index)?;
                    let partial_str = partial.to_string_lossy().into_owned();
                    decoder.process_file(
                        Operation::Extract,
                        None,
                        Some(&partial_str),
                        progress.as_deref_mut(),
                        index,
                        entry.unpacked_size,
                    )?;
                    extracted.push(index);
                    bytes_written = bytes_written.saturating_add(entry.unpacked_size);
                }
            } else {
                break;
            }
            index += 1;
        }

        Ok(UnitExtractReport {
            extracted,
            bytes_written,
            verified: true,
        })
    }

    fn cancel(&mut self) {
        // Cancellation is handled via the shared AtomicBool passed at open.
    }

    fn begin_extraction(
        &mut self,
        options: &ExtractOptions,
        stop_at: u64,
    ) -> Result<(), ArchiveError> {
        let info = self.info.clone().ok_or_else(|| {
            ArchiveError::open("inspect() must be called before begin_extraction()")
        })?;
        let first = &info.volumes[0].path;
        let decoder = Unrar::open(
            first,
            OpenMode::Process,
            options.password.clone(),
            options.cancel.clone(),
        )?;
        self.decoder = Some(decoder);
        self.next_index = 0;
        self.stop_at = stop_at;
        self.pass_done = false;
        Ok(())
    }

    fn extract_next<'p, 'c>(
        &mut self,
        options: &ExtractOptions,
        progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
    ) -> Result<Option<ExtractedFile>, ArchiveError> {
        if self.pass_done {
            return Ok(None);
        }
        let info = self
            .info
            .clone()
            .ok_or_else(|| ArchiveError::open("inspect() must be called before extract_next()"))?;
        let mut progress = progress;
        loop {
            let Some(decoder) = self.decoder.as_mut() else {
                self.pass_done = true;
                return Ok(None);
            };
            let Some(h) = decoder.read_header()? else {
                self.decoder = None;
                self.pass_done = true;
                return Ok(None);
            };
            let index = self.next_index;
            if index < self.stop_at {
                // Already-committed entries: seek, or verify split files in
                // TEST mode (the DLL cannot seek past split continuations).
                let is_split = h.flags & 1 != 0 || h.flags & 2 != 0;
                let op = if is_split {
                    Operation::Test
                } else {
                    Operation::Skip
                };
                decoder.process_file(op, None, None, None, index, h.pack_size)?;
                self.next_index += 1;
                continue;
            }
            let entry = info
                .entries
                .iter()
                .find(|e| e.index == index)
                .ok_or_else(|| ArchiveError::NotFound(format!("entry {index}")))?;
            self.next_index += 1;
            if entry.is_directory || entry.redirection.is_some() {
                // Directories are created by the engine; redirections/links are skipped
                // under SymlinkPolicy::Skip to prevent hostile link creation.
                decoder.process_file(Operation::Skip, None, None, None, index, h.pack_size)?;
                return Ok(Some(ExtractedFile {
                    index,
                    partial_path: None,
                }));
            }
            let partial = partial_output_path(options, index)?;
            let partial_str = partial.to_string_lossy().into_owned();
            decoder.process_file(
                Operation::Extract,
                None,
                Some(&partial_str),
                progress.as_deref_mut(),
                index,
                entry.unpacked_size,
            )?;
            return Ok(Some(ExtractedFile {
                index,
                partial_path: Some(partial),
            }));
        }
    }

    fn decoder_requirements(&self) -> DecoderRequirements {
        self.info
            .as_ref()
            .map(|i| i.decoder_requirements.clone())
            .unwrap_or(DecoderRequirements {
                scratch_bytes: 0,
                redecodes_prefix: false,
            })
    }

    fn retirement_proofs(&self) -> Vec<RetirementProof> {
        let info = self.info.as_ref();
        let mut proofs = Vec::new();
        if let Some(info) = info {
            for unit in &info.recovery_units {
                for range in &unit.packed_ranges {
                    let reason = if info.solid_archive {
                        "Archive-level solid: the entire chain is one recovery unit; bytes are \
                         safe once the chain is durably committed."
                            .to_string()
                    } else {
                        "The decoder can seek past this range after its unit is durably \
                         committed; it is never read again."
                            .to_string()
                    };
                    proofs.push(RetirementProof {
                        volume_index: range.volume_index,
                        start: range.start,
                        len: range.len,
                        unit_seq: unit.seq,
                        reason,
                    });
                }
            }
        }
        proofs
    }

    fn entries(&self) -> &[Entry] {
        self.info
            .as_ref()
            .map(|i| i.entries.as_slice())
            .unwrap_or(&[])
    }

    fn recovery_units(&self) -> &[RecoveryUnit] {
        self.info
            .as_ref()
            .map(|i| i.recovery_units.as_slice())
            .unwrap_or(&[])
    }

    fn close(&mut self) {
        self.decoder = None;
    }
}

/// Name comparison tolerant of trailing slashes on directories.
fn same_name(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn inspect_matches_decoder_for_simple_rar5() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("hello.txt", b"hello world"),
            FixtureFile::new("sub/dir/file.bin", &vec![7u8; 5000]),
            FixtureFile::dir("emptydir"),
        ];
        let paths = write_rar(dir.path(), "t", &files, &FixtureOptions::default()).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.format, "rar5");
        assert_eq!(info.entries.len(), 3);
        assert_eq!(info.entries[0].name, "hello.txt");
        assert_eq!(info.entries[0].unpacked_size, 11);
        assert_eq!(
            info.entries[0].crc32,
            Some(crate::rar::fixtures::crc32(b"hello world"))
        );
        assert!(info.entries[1].name.ends_with("file.bin"));
        assert_eq!(info.recovery_units.len(), 3);
        assert_eq!(info.packed_size, 5011);
        assert_eq!(info.unpacked_size, 5011);
        assert!(!info.capability.notes.is_empty());
    }

    #[test]
    fn inspect_rar4() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![FixtureFile::new("a.txt", b"rar4 data")];
        let opts = FixtureOptions {
            rar5: false,
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "t4", &files, &opts).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.format, "rar4");
        assert_eq!(info.entries.len(), 1);
        assert_eq!(info.recovery_units.len(), 1);
    }

    #[test]
    fn solid_archive_is_one_unit() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("a.txt", b"aaa"),
            FixtureFile::new("b.txt", b"bbb"),
            FixtureFile::new("c.txt", b"ccc"),
        ];
        let opts = FixtureOptions {
            solid_archive: true,
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "s", &files, &opts).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert!(info.solid_archive);
        assert_eq!(info.recovery_units.len(), 1);
        let unit = &info.recovery_units[0];
        assert_eq!(unit.first_entry, 0);
        assert_eq!(unit.last_entry, 2);
        assert_eq!(unit.packed_ranges.len(), 3);
    }

    #[test]
    fn non_solid_units_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("a.txt", b"aaa"),
            FixtureFile::new("b.txt", b"bbb"),
            FixtureFile::new("c.txt", b"ccc"),
        ];
        let paths = write_rar(dir.path(), "n", &files, &FixtureOptions::default()).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.recovery_units.len(), 3);
        for u in &info.recovery_units {
            assert_eq!(u.first_entry, u.last_entry);
            assert_eq!(u.packed_ranges.len(), 1);
        }
    }

    #[test]
    fn integrity_test_passes_clean_archive() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("a.txt", b"aaa"),
            FixtureFile::new("b.bin", &vec![9u8; 10000]),
        ];
        let paths = write_rar(dir.path(), "c", &files, &FixtureOptions::default()).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        backend.inspect(&OpenOptions::default()).unwrap();
        let report = backend.test_integrity(None, None, None).unwrap();
        eprintln!("INTEGRITY REPORT: {:?}", report);
        assert!(report.ok);
        assert_eq!(report.bytes_tested, 10003);
    }

    #[test]
    fn integrity_test_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("a.txt", b"aaa"),
            FixtureFile::new("b.bin", &vec![9u8; 10000]),
        ];
        let opts = FixtureOptions {
            corrupt: Some((200, 0x55)),
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "cc", &files, &opts).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        backend.inspect(&OpenOptions::default()).unwrap();
        let report = backend.test_integrity(None, None, None).unwrap();
        assert!(!report.ok, "corrupt archive must fail integrity test");
        assert!(report.first_failure.is_some());
    }

    #[test]
    fn extract_unit_writes_partial_names() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let files = vec![
            FixtureFile::new("one.txt", b"one"),
            FixtureFile::new("sub/two.txt", b"two"),
        ];
        let paths = write_rar(dir.path(), "e", &files, &FixtureOptions::default()).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        backend.inspect(&OpenOptions::default()).unwrap();

        // Extract unit 0.
        let cancel = Arc::new(AtomicBool::new(false));
        let mut opts = crate::backend::ExtractOptions {
            dest_dir: out.clone(),
            job_id: "job1".into(),
            password: None,
            cancel: Some(cancel),
            partial_suffix: String::new(),
            name_map: std::collections::HashMap::new(),
        };
        opts.name_map.insert(0, "one.txt".to_string());
        opts.name_map.insert(1, "sub\\two.txt".to_string());
        let report = backend.extract_unit(0, &opts, None).unwrap();
        assert_eq!(report.extracted, vec![0]);
        let partial = out.join("one.txt.sx-partial-job1");
        assert!(partial.exists(), "partial file must exist at {partial:?}");
        assert_eq!(std::fs::read(&partial).unwrap(), b"one");
        // Unit 1 (sub dir).
        let report = backend.extract_unit(1, &opts, None).unwrap();
        assert_eq!(report.extracted, vec![1]);
        let partial = out.join("sub").join("two.txt.sx-partial-job1");
        assert!(partial.exists());
        assert_eq!(std::fs::read(&partial).unwrap(), b"two");
    }

    #[test]
    fn extraction_verifies_crc_after_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let files = vec![FixtureFile::new("a.txt", b"aaa")];
        let opts = FixtureOptions {
            corrupt: Some((40, 0x00)),
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "x", &files, &opts).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        backend.inspect(&OpenOptions::default()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut opts = ExtractOptions {
            dest_dir: out,
            job_id: "j".into(),
            password: None,
            cancel: Some(cancel),
            partial_suffix: String::new(),
            name_map: std::collections::HashMap::new(),
        };
        opts.name_map.insert(0, "a.txt".to_string());
        let result = backend.extract_unit(0, &opts, None);
        assert!(result.is_err(), "corrupt file must fail extraction");
        match result.unwrap_err() {
            ArchiveError::Corrupt(_) => {}
            ArchiveError::Decoder(_) => {}
            other => panic!("expected corrupt/decoder error, got {other:?}"),
        }
    }

    #[test]
    fn multipart_inspection_ranges_span_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("a.bin", &vec![0x11; 4000]),
            FixtureFile::new("b.bin", &vec![0x22; 4000]),
            FixtureFile::new("c.bin", &vec![0x33; 4000]),
        ];
        let opts = FixtureOptions {
            volume_size: Some(2500),
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "mv", &files, &opts).unwrap();
        assert!(paths.len() >= 2);
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.entries.len(), 3);
        assert_eq!(info.volumes.len(), paths.len());
        // Extract all units across volumes.
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut opts = ExtractOptions {
            dest_dir: out.clone(),
            job_id: "mv".into(),
            password: None,
            cancel: Some(cancel),
            partial_suffix: String::new(),
            name_map: std::collections::HashMap::new(),
        };
        opts.name_map.insert(0, "a.bin".to_string());
        opts.name_map.insert(1, "b.bin".to_string());
        opts.name_map.insert(2, "c.bin".to_string());
        for u in &info.recovery_units {
            let report = backend.extract_unit(u.seq, &opts, None).unwrap();
            assert!(report.verified);
        }
        assert_eq!(
            std::fs::read(out.join("a.bin.sx-partial-mv")).unwrap(),
            vec![0x11; 4000]
        );
        assert_eq!(
            std::fs::read(out.join("b.bin.sx-partial-mv")).unwrap(),
            vec![0x22; 4000]
        );
        assert_eq!(
            std::fs::read(out.join("c.bin.sx-partial-mv")).unwrap(),
            vec![0x33; 4000]
        );
    }

    /// Real WinRAR archives contain service subheaders (NTFS streams, ACLs,
    /// comments). They must NOT be counted as entries and must not disturb
    /// packed-range positions. Regression for "decoder reported fewer
    /// entries than the parser found" on real archives.
    #[test]
    fn service_headers_are_not_entries_rar5() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("one.bin", &vec![0x11; 3000]),
            FixtureFile::new("two.bin", &vec![0x22; 3000]),
            FixtureFile::new("three.bin", &vec![0x33; 3000]),
        ];
        let opts = FixtureOptions {
            service_headers: vec!["NTFS".into(), "ACL".into(), "CMT".into()],
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "svc", &files, &opts).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        // inspect() cross-validates parser vs decoder; it fails if counts
        // disagree, so this single call covers the regression.
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.entries.len(), 3, "service headers must not be entries");
        assert_eq!(info.recovery_units.len(), 3);

        // Extraction of every unit must still produce byte-identical output.
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut opts = ExtractOptions {
            dest_dir: out.clone(),
            job_id: "svc".into(),
            password: None,
            cancel: Some(cancel),
            partial_suffix: String::new(),
            name_map: std::collections::HashMap::new(),
        };
        opts.name_map.insert(0, "one.bin".to_string());
        opts.name_map.insert(1, "two.bin".to_string());
        opts.name_map.insert(2, "three.bin".to_string());
        for u in &info.recovery_units {
            let report = backend.extract_unit(u.seq, &opts, None).unwrap();
            assert!(report.verified);
        }
        assert_eq!(
            std::fs::read(out.join("one.bin.sx-partial-svc")).unwrap(),
            vec![0x11; 3000]
        );
        assert_eq!(
            std::fs::read(out.join("two.bin.sx-partial-svc")).unwrap(),
            vec![0x22; 3000]
        );
        assert_eq!(
            std::fs::read(out.join("three.bin.sx-partial-svc")).unwrap(),
            vec![0x33; 3000]
        );
    }

    #[test]
    fn service_headers_are_not_entries_rar4() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("one.bin", &vec![0x44; 3000]),
            FixtureFile::new("two.bin", &vec![0x55; 3000]),
        ];
        let opts = FixtureOptions {
            rar5: false,
            service_headers: vec!["ACL".into()],
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "svc4", &files, &opts).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.entries.len(), 2);
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut opts = ExtractOptions {
            dest_dir: out.clone(),
            job_id: "svc4".into(),
            password: None,
            cancel: Some(cancel),
            partial_suffix: String::new(),
            name_map: std::collections::HashMap::new(),
        };
        opts.name_map.insert(0, "one.bin".to_string());
        opts.name_map.insert(1, "two.bin".to_string());
        for u in &info.recovery_units {
            let report = backend.extract_unit(u.seq, &opts, None).unwrap();
            assert!(report.verified);
        }
        assert_eq!(
            std::fs::read(out.join("one.bin.sx-partial-svc4")).unwrap(),
            vec![0x44; 3000]
        );
        assert_eq!(
            std::fs::read(out.join("two.bin.sx-partial-svc4")).unwrap(),
            vec![0x55; 3000]
        );
    }

    /// A file split in half across two volumes. The decoder must report ONE
    /// entry for it (LIST mode skips the continuation header) and extraction
    /// must reassemble it byte-exactly.
    #[test]
    fn split_file_across_volumes_rar5() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![0xABu8; 50_000];
        let files = vec![
            FixtureFile::new("a.bin", &vec![0x11; 1000]),
            FixtureFile::new("big.bin", &data),
            FixtureFile::new("c.bin", &vec![0x33; 1000]),
        ];
        let opts = FixtureOptions {
            force_split_file: Some(1),
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "sp", &files, &opts).unwrap();
        assert_eq!(paths.len(), 2, "split archive must have 2 volumes");
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.entries.len(), 3, "split file must be ONE entry");
        assert_eq!(info.volumes.len(), 2);
        let split = info.entries.iter().find(|e| e.name == "big.bin").unwrap();
        assert!(
            split.split_before && split.split_after,
            "split flags must be recorded"
        );
        // The split entry's recovery unit is the tail unit, spanning both volumes.
        let unit = info
            .recovery_units
            .iter()
            .find(|u| u.first_entry == split.index)
            .unwrap();
        assert!(
            unit.packed_ranges.iter().any(|r| r.volume_index == 0),
            "packed ranges must include volume 0"
        );
        assert!(
            unit.packed_ranges.iter().any(|r| r.volume_index == 1),
            "packed ranges must include volume 1"
        );

        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut opts = ExtractOptions {
            dest_dir: out.clone(),
            job_id: "sp".into(),
            password: None,
            cancel: Some(cancel),
            partial_suffix: String::new(),
            name_map: std::collections::HashMap::new(),
        };
        opts.name_map.insert(0, "a.bin".to_string());
        opts.name_map.insert(1, "big.bin".to_string());
        opts.name_map.insert(2, "c.bin".to_string());
        for u in &info.recovery_units {
            let report = backend.extract_unit(u.seq, &opts, None).unwrap();
            assert!(report.verified);
        }
        assert_eq!(
            std::fs::read(out.join("a.bin.sx-partial-sp")).unwrap(),
            vec![0x11; 1000]
        );
        assert_eq!(
            std::fs::read(out.join("big.bin.sx-partial-sp")).unwrap(),
            data
        );
        assert_eq!(
            std::fs::read(out.join("c.bin.sx-partial-sp")).unwrap(),
            vec![0x33; 1000]
        );
    }

    #[test]
    fn split_file_across_volumes_rar4() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![0xCDu8; 40_000];
        let files = vec![
            FixtureFile::new("a.bin", &vec![0x11; 1000]),
            FixtureFile::new("big.bin", &data),
            FixtureFile::new("c.bin", &vec![0x33; 1000]),
        ];
        let opts = FixtureOptions {
            rar5: false,
            force_split_file: Some(1),
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "sp4", &files, &opts).unwrap();
        assert_eq!(paths.len(), 2);
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.entries.len(), 3);
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut opts = ExtractOptions {
            dest_dir: out.clone(),
            job_id: "sp4".into(),
            password: None,
            cancel: Some(cancel),
            partial_suffix: String::new(),
            name_map: std::collections::HashMap::new(),
        };
        opts.name_map.insert(0, "a.bin".to_string());
        opts.name_map.insert(1, "big.bin".to_string());
        opts.name_map.insert(2, "c.bin".to_string());
        for u in &info.recovery_units {
            let report = backend.extract_unit(u.seq, &opts, None).unwrap();
            assert!(report.verified);
        }
        assert_eq!(
            std::fs::read(out.join("big.bin.sx-partial-sp4")).unwrap(),
            data
        );
    }

    /// Large archive: thousands of entries plus service subheaders must
    /// cross-validate and extract. Regression for real-world archives.
    #[test]
    fn large_archive_cross_validates() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<FixtureFile> = (0..3000)
            .map(|i| {
                let data: Vec<u8> = (0..2048).map(|b| ((b + i * 7) % 251) as u8).collect();
                FixtureFile::new(&format!("file-{i}.bin"), &data)
            })
            .collect();
        let opts = FixtureOptions {
            service_headers: vec!["NTFS".into(), "ACL".into()],
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "big", &files, &opts).unwrap();
        let mut backend = RarBackend::new(&paths[0]);
        let info = backend.inspect(&OpenOptions::default()).unwrap();
        assert_eq!(info.entries.len(), 3000);
        assert_eq!(info.recovery_units.len(), 3000);
        // Extract a handful of units across the archive to prove offsets are
        // exact despite the service headers.
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut name_map = std::collections::HashMap::new();
        for i in 0..3000u64 {
            name_map.insert(i, format!("file-{i}.bin"));
        }
        let opts = ExtractOptions {
            dest_dir: out.clone(),
            job_id: "big".into(),
            password: None,
            cancel: Some(cancel),
            partial_suffix: String::new(),
            name_map,
        };
        for seq in [0u64, 500, 1499, 2500, 2999] {
            let report = backend.extract_unit(seq, &opts, None).unwrap();
            assert!(report.verified);
        }
        let expect: Vec<u8> = (0..2048).map(|b| ((b + 2999 * 7) % 251) as u8).collect();
        assert_eq!(
            std::fs::read(out.join("file-2999.bin.sx-partial-big")).unwrap(),
            expect
        );
    }
}
