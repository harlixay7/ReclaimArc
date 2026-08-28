//! ZIP backend: implements `ArchiveBackend` for standard ZIP and ZIP64 archives
//! using independent structural verification (rawzip + zip-rs) and bounded streaming decompression.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::backend::{ArchiveBackend, ExtractOptions, ExtractedFile, OpenOptions};
use crate::error::ArchiveError;
use crate::model::{
    ArchiveInfo, DecoderRequirements, Entry, IntegrityReport, ProgressEvent, RecoveryUnit,
    RetirementProof, UnitExtractReport,
};
use crate::zip::decoder::ZipDecoder;
use crate::zip::parser::parse_and_validate;

/// The ZIP archive backend.
pub struct ZipBackend {
    path: PathBuf,
    info: Option<ArchiveInfo>,
    retirement_proofs: Vec<RetirementProof>,
    decoder: Option<ZipDecoder>,
    next_index: u64,
    pass_done: bool,
}

impl ZipBackend {
    /// Create a backend instance for the archive at `path`.
    pub fn new(path: &Path) -> Self {
        ZipBackend {
            path: path.to_path_buf(),
            info: None,
            retirement_proofs: Vec::new(),
            decoder: None,
            next_index: 0,
            pass_done: false,
        }
    }
}

/// RAII guard that automatically removes an in-progress partial file if dropped
/// before ownership is transferred to the caller via `disarm()`.
pub struct PartialFileGuard {
    path: Option<PathBuf>,
}

impl PartialFileGuard {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref().expect("guard active")
    }

    pub fn disarm(mut self) -> PathBuf {
        self.path.take().expect("guard active")
    }
}

impl Drop for PartialFileGuard {
    fn drop(&mut self) {
        if let Some(ref p) = self.path {
            if p.exists() {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// The partial output path for an entry, derived strictly from the engine-validated
/// `name_map` and the per-attempt partial suffix.
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

impl ArchiveBackend for ZipBackend {
    fn inspect(&mut self, options: &OpenOptions) -> Result<ArchiveInfo, ArchiveError> {
        let analysis = parse_and_validate(&self.path, options.password.as_deref())?;
        self.info = Some(analysis.info.clone());
        self.retirement_proofs = analysis.retirement_proofs;
        Ok(analysis.info)
    }

    fn test_integrity<'p, 'c>(
        &mut self,
        password: Option<&str>,
        cancel: Option<Arc<AtomicBool>>,
        progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
    ) -> Result<IntegrityReport, ArchiveError> {
        let mut decoder = ZipDecoder::open(&self.path, password)?;
        decoder.test_integrity(cancel, progress)
    }

    fn extract_unit<'p, 'c>(
        &mut self,
        unit_seq: u64,
        options: &ExtractOptions,
        mut progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
    ) -> Result<UnitExtractReport, ArchiveError> {
        let info = match self.info {
            Some(ref i) => i.clone(),
            None => self.inspect(&OpenOptions {
                password: options.password.clone(),
            })?,
        };

        let unit = info
            .recovery_units
            .iter()
            .find(|u| u.seq == unit_seq)
            .ok_or_else(|| ArchiveError::NotFound(format!("recovery unit {unit_seq} not found")))?;

        let mut extracted = Vec::new();
        let mut bytes_written = 0u64;
        let mut guards = Vec::new();

        // Open decoder for extraction
        let mut decoder = ZipDecoder::open(&self.path, options.password.as_deref())?;

        for entry_idx in unit.first_entry..=unit.last_entry {
            let entry = &info.entries[entry_idx as usize];

            if entry.is_directory {
                continue;
            }

            if entry.redirection.is_some() {
                // Symlink/redirection handled by engine policy
                continue;
            }

            let partial_path = partial_output_path(options, entry_idx)?;
            let guard = PartialFileGuard::new(partial_path);
            let written = decoder.extract_file(
                entry_idx as usize,
                guard.path(),
                entry.unpacked_size,
                options.max_compression_ratio,
                options.cancel.clone(),
                progress.as_deref_mut(),
            )?;

            extracted.push(entry_idx);
            bytes_written = bytes_written.saturating_add(written);
            guards.push(guard);
        }

        // Close decoder handle to release file locks on Windows
        decoder.close();

        // All entries in unit succeeded without error: disarm guards to transfer ownership to engine
        for g in guards {
            g.disarm();
        }

        Ok(UnitExtractReport {
            extracted,
            bytes_written,
            verified: true,
        })
    }

    fn cancel(&mut self) {
        if let Some(ref mut d) = self.decoder {
            d.close();
        }
    }

    fn begin_extraction(
        &mut self,
        options: &ExtractOptions,
        stop_at: u64,
    ) -> Result<(), ArchiveError> {
        if self.info.is_none() {
            self.inspect(&OpenOptions {
                password: options.password.clone(),
            })?;
        }
        self.decoder = Some(ZipDecoder::open(&self.path, options.password.as_deref())?);
        self.next_index = stop_at;
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

        let num_entries = match self.info {
            Some(ref i) => i.entries.len(),
            None => {
                let info = self.inspect(&OpenOptions {
                    password: options.password.clone(),
                })?;
                info.entries.len()
            }
        };

        if (self.next_index as usize) >= num_entries {
            self.pass_done = true;
            self.close();
            return Ok(None);
        }

        let idx = self.next_index;
        self.next_index += 1;

        let entry = self.info.as_ref().unwrap().entries[idx as usize].clone();

        if entry.is_directory || entry.redirection.is_some() {
            return Ok(Some(ExtractedFile {
                index: idx,
                partial_path: None,
            }));
        }

        let partial_path = partial_output_path(options, idx)?;
        let guard = PartialFileGuard::new(partial_path);

        let decoder = self.decoder.as_mut().ok_or_else(|| {
            ArchiveError::Decoder("ZIP decoder not initialized; call begin_extraction first".into())
        })?;

        decoder.extract_file(
            idx as usize,
            guard.path(),
            entry.unpacked_size,
            options.max_compression_ratio,
            options.cancel.clone(),
            progress,
        )?;

        let final_path = guard.disarm();
        Ok(Some(ExtractedFile {
            index: idx,
            partial_path: Some(final_path),
        }))
    }

    fn decoder_requirements(&self) -> DecoderRequirements {
        DecoderRequirements {
            scratch_bytes: 0,
            redecodes_prefix: false,
        }
    }

    fn retirement_proofs(&self) -> Vec<RetirementProof> {
        self.retirement_proofs.clone()
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
        if let Some(ref mut d) = self.decoder {
            d.close();
        }
        self.decoder = None;
    }
}
