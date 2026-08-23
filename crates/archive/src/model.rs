//! Archive model shared by all backends: entries, recovery units, packed
//! ranges, capability matrix and retirement proofs.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One volume (archive part file) of a multi-part archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// Index in the ordered volume list (0 = first).
    pub index: u64,
    /// Absolute path of the volume file.
    pub path: PathBuf,
    /// Logical size in bytes.
    pub logical_size: u64,
}

/// Kind of redirection recorded for an entry (symlink / hardlink).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedirectionKind {
    UnixSymlink,
    WindowsSymlink,
    Junction,
    Hardlink,
    FileCopy,
}

/// A redirection (symlink/hardlink) entry in the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Redirection {
    pub kind: RedirectionKind,
    pub target: String,
}

/// One archive entry (file or directory).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Position in archive order (0-based). Drives journal references.
    pub index: u64,
    /// Name as stored in the archive (hostile input; validated by core).
    pub name: String,
    /// Packed (on-disk) size of this entry's data.
    pub packed_size: u64,
    /// Unpacked (logical) size.
    pub unpacked_size: u64,
    /// CRC32 of the unpacked data (None when not recorded).
    pub crc32: Option<u32>,
    /// Whether this is a directory entry.
    pub is_directory: bool,
    /// Whether this file continues the solid dictionary.
    pub is_solid: bool,
    /// Whether the data starts in the previous volume.
    pub split_before: bool,
    /// Whether the data continues in the next volume.
    pub split_after: bool,
    /// Whether the file data is encrypted.
    pub encrypted: bool,
    /// Redirection, when the entry is a link.
    pub redirection: Option<Redirection>,
}

/// A byte range inside one volume file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackedRange {
    pub volume_index: u64,
    pub start: u64,
    pub len: u64,
}

impl PackedRange {
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }
}

/// A recovery unit: a set of entries that must be fully decoded, verified,
/// flushed and committed before any of their source bytes may be reclaimed.
///
/// For RAR: a non-solid file is one unit; a solid chain (all entries with the
/// solid flag between two non-solid boundaries) is one unit; an archive with
/// the archive-level solid flag is one unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryUnit {
    /// 0-based sequence number.
    pub seq: u64,
    /// First entry index (inclusive).
    pub first_entry: u64,
    /// Last entry index (inclusive).
    pub last_entry: u64,
    /// Exact packed source ranges that become reclaimable once this unit is
    /// durably committed.
    pub packed_ranges: Vec<PackedRange>,
    /// Total unpacked bytes written by this unit.
    pub unpacked_bytes: u64,
}

/// What the backend can do (capability matrix). The engine never assumes a
/// capability — it checks this matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityMatrix {
    /// Archive format name ("rar4", "rar5").
    pub format: String,
    /// Full archive integrity testing is available.
    pub supports_test_integrity: bool,
    /// Recovery units are true decoder restart boundaries.
    pub restartable_units: bool,
    /// Packed ranges can be mapped to exact source byte ranges, enabling
    /// progressive reclamation.
    pub progressive_reclaim: bool,
    /// Password-protected archives are supported.
    pub supports_encryption: bool,
    /// Multi-volume archives are supported.
    pub supports_multipart: bool,
    /// Human-readable notes about limitations.
    pub notes: Vec<String>,
}

/// Proof that a source byte range can never again be needed once its unit is
/// durably committed. Produced by the backend, consumed by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetirementProof {
    pub volume_index: u64,
    pub start: u64,
    pub len: u64,
    /// The unit whose durable commit makes this range safe to reclaim.
    pub unit_seq: u64,
    /// Why it is safe.
    pub reason: String,
}

/// Requirements of the decoder (for the planner / UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecoderRequirements {
    /// Scratch bytes needed beyond unit output (dictionary, temp files).
    pub scratch_bytes: u64,
    /// Whether decoding a unit requires re-decoding earlier entries
    /// (solid dictionary chains).
    pub redecodes_prefix: bool,
}

/// Result of a full-archive integrity test.
#[derive(Debug, Clone)]
pub struct IntegrityReport {
    /// All entries passed their checksum verification.
    pub ok: bool,
    /// Total bytes tested.
    pub bytes_tested: u64,
    /// Index of the first failing entry, if any.
    pub first_failure: Option<u64>,
    /// Details of the failure, if any.
    pub failure: Option<String>,
}

/// Result of extracting one recovery unit.
#[derive(Debug, Clone)]
pub struct UnitExtractReport {
    /// Entry indexes that were written.
    pub extracted: Vec<u64>,
    /// Total unpacked bytes written.
    pub bytes_written: u64,
    /// Whether every written file passed its archive checksum.
    pub verified: bool,
}

/// Progress event during integrity test or extraction.
#[derive(Debug, Clone, Copy)]
pub enum ProgressEvent {
    /// `current` bytes of `total` processed in the current entry.
    EntryProgress { entry_index: u64, current: u64, total: u64 },
}

/// Full archive inspection result.
#[derive(Debug, Clone)]
pub struct ArchiveInfo {
    pub format: String,
    /// Total logical packed bytes across all volumes.
    pub packed_size: u64,
    /// Total unpacked bytes.
    pub unpacked_size: u64,
    /// Archive-level solidity flag (main header).
    pub solid_archive: bool,
    pub encrypted_headers: bool,
    pub volumes: Vec<VolumeInfo>,
    pub entries: Vec<Entry>,
    pub recovery_units: Vec<RecoveryUnit>,
    pub capability: CapabilityMatrix,
    pub decoder_requirements: DecoderRequirements,
}