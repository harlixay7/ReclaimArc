//! Job events streamed to the UI/CLI.

use std::path::PathBuf;

/// Events emitted by the engine during a job.
#[derive(Debug, Clone)]
pub enum Event {
    /// The job was created and registered.
    JobStarted { job_id: String },
    /// Analysis finished (plan computed).
    Analyzed {
        archive: PathBuf,
        plan_bytes: String,
    },
    /// Full integrity pre-test started.
    PreTestStarted { bytes_total: u64 },
    /// Integrity pre-test progress.
    PreTestProgress { current: u64, total: u64 },
    /// Integrity pre-test result.
    PreTestFinished { ok: bool, bytes_tested: u64 },
    /// A recovery unit started.
    UnitStarted {
        seq: u64,
        first_entry: u64,
        last_entry: u64,
    },
    /// An entry started extracting.
    EntryStarted { index: u64, name: String },
    /// Extraction progress for an entry (unpacked bytes).
    EntryProgress {
        index: u64,
        current: u64,
        total: u64,
    },
    /// An entry passed verification (BLAKE3 + archive CRC).
    EntryVerified { index: u64, blake3: String },
    /// An entry was durably committed (renamed into place).
    EntryCommitted { index: u64, path: PathBuf },
    /// A unit was durably committed.
    UnitCommitted { seq: u64, bytes: u64 },
    /// A source range was reclaimed (bytes released).
    RangeReclaimed { volume_index: u64, bytes: u64 },
    /// A unit's source was fully reclaimed.
    UnitReclaimed { seq: u64, bytes: u64 },
    /// Current free space on the destination volume.
    FreeSpace { bytes: u64 },
    /// The job paused.
    JobPaused { job_id: String },
    /// The job was cancelled (resumable).
    JobCancelled { job_id: String },
    /// The job finished.
    JobFinished {
        job_id: String,
        committed_bytes: u64,
        reclaimed_bytes: u64,
    },
    /// The job failed with a structured error.
    JobFailed {
        operation: String,
        path: Option<PathBuf>,
        os_error: Option<u32>,
        message: String,
        recommended_action: String,
    },
    /// A conflict was resolved by policy (skipped entry).
    EntrySkipped {
        index: u64,
        name: String,
        reason: String,
    },
    /// Free space is dangerously low.
    LowSpace { free: u64, reserve: u64 },
}
