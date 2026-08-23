//! `ArchiveBackend` trait — the engine talks to archives only through this
//! interface. Backends provide exact packed-data ranges and explicit
//! `RetirementProof` objects describing when a source range can never again
//! be needed for a successful restart.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::error::ArchiveError;
use crate::model::*;

/// Options for a unit extraction pass.
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    /// Destination directory (absolute, validated by the engine).
    pub dest_dir: PathBuf,
    /// Job id used in partial-file suffixes.
    pub job_id: String,
    /// Partial-name suffix appended to each validated entry path. The engine
    /// makes this unique per attempt so that a file left locked by an aborted
    /// decoder never blocks a retry.
    pub partial_suffix: String,
    /// Optional password (never persisted anywhere).
    pub password: Option<String>,
    /// Cancellation flag (checked between files and mid-file via callback).
    pub cancel: Option<Arc<AtomicBool>>,
    /// Validated relative output paths for each entry (index → relative
    /// path). The backend MUST write through these names; raw archive names
    /// are hostile and never used for filesystem paths.
    pub name_map: HashMap<u64, String>,
}

/// Options for opening/listing an archive.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Optional password for encrypted archives.
    pub password: Option<String>,
}

/// Progress callback signature used by the engine to surface events.
pub type ProgressFn<'a> = &'a mut dyn FnMut(ProgressEvent) -> bool;

/// One file extracted by a streaming pass.
#[derive(Debug, Clone)]
pub struct ExtractedFile {
    /// Entry index in the archive.
    pub index: u64,
    /// Absolute path of the written partial file.
    pub partial_path: PathBuf,
}

/// The archive backend interface.
pub trait ArchiveBackend: Send {
    /// Inspect the archive: entries, volumes, recovery units, capabilities.
    fn inspect(&mut self, options: &OpenOptions) -> Result<ArchiveInfo, ArchiveError>;

    /// Run a full integrity test over every entry (verifies archive
    /// checksums). Used before destructive extraction.
    fn test_integrity<'p, 'c>(
        &mut self,
        password: Option<&str>,
        cancel: Option<Arc<AtomicBool>>,
        progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
    ) -> Result<IntegrityReport, ArchiveError>;

    /// Extract exactly one recovery unit.
    ///
    /// The backend re-opens the archive from the first volume, seeks past
    /// (never reads) entries that belong to already-committed units, decodes
    /// this unit's entries and writes each file to
    /// `<dest_dir>/<entry>.sx-partial-<job_id>` (absolute path, exactly as
    /// given). Directory entries are skipped (the engine creates them).
    fn extract_unit<'p, 'c>(
        &mut self,
        unit_seq: u64,
        options: &ExtractOptions,
        progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
    ) -> Result<UnitExtractReport, ArchiveError>;

    /// Request cancellation of the current operation.
    fn cancel(&mut self);

    /// Begin a single-pass streaming extraction: the decoder is opened once
    /// and walks the archive forward, extracting every file at or after
    /// `stop_at` as the caller calls `extract_next`.
    ///
    /// This avoids the O(n²) re-walks of per-unit passes and is the fast path
    /// for archives whose recovery units are single files.
    fn begin_extraction(&mut self, options: &ExtractOptions, stop_at: u64) -> Result<(), ArchiveError>;

    /// Extract the next file of the streaming pass. Returns `None` when the
    /// archive is exhausted. Entries before `stop_at` are skipped (seek, or
    /// verified in TEST mode for split files).
    fn extract_next<'p, 'c>(
        &mut self,
        options: &ExtractOptions,
        progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
    ) -> Result<Option<ExtractedFile>, ArchiveError>;

    /// Decoder requirements for the planner.
    fn decoder_requirements(&self) -> DecoderRequirements;

    /// Explicit proofs of when each packed range may be reclaimed.
    fn retirement_proofs(&self) -> Vec<RetirementProof>;

    /// The entries of the currently inspected archive.
    fn entries(&self) -> &[Entry];

    /// The recovery units of the currently inspected archive.
    fn recovery_units(&self) -> &[RecoveryUnit];

    /// Close any open decoder handles to release locks on archive files.
    fn close(&mut self) {}
}