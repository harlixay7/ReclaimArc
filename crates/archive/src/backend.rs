//! `ArchiveBackend` trait — the engine talks to archives only through this
//! interface. Backends provide exact packed-data ranges and explicit
//! `RetirementProof` objects describing when a source range can never again
//! be needed for a successful restart.

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
    /// Optional password (never persisted anywhere).
    pub password: Option<String>,
    /// Cancellation flag (checked between files and mid-file via callback).
    pub cancel: Option<Arc<AtomicBool>>,
}

/// Options for opening/listing an archive.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Optional password for encrypted archives.
    pub password: Option<String>,
}

/// Progress callback signature used by the engine to surface events.
pub type ProgressFn<'a> = &'a mut dyn FnMut(ProgressEvent) -> bool;

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

    /// Decoder requirements for the planner.
    fn decoder_requirements(&self) -> DecoderRequirements;

    /// Explicit proofs of when each packed range may be reclaimed.
    fn retirement_proofs(&self) -> Vec<RetirementProof>;

    /// The entries of the currently inspected archive.
    fn entries(&self) -> &[Entry];

    /// The recovery units of the currently inspected archive.
    fn recovery_units(&self) -> &[RecoveryUnit];
}