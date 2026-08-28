//! Domain records persisted by the journal.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Durable identity snapshot of a source volume/archive file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume_serial: u32,
    pub file_id: u64,
    pub file_size: u64,
    pub last_write_time: u64,
}

/// State of a recovery unit (the master-plan lifecycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitState {
    Pending,
    Extracting,
    OutputWritten,
    OutputVerified,
    OutputDurable,
    Committed,
    ReclaimIntent,
    Reclaimed,
}

impl UnitState {
    pub const ALL: [UnitState; 8] = [
        UnitState::Pending,
        UnitState::Extracting,
        UnitState::OutputWritten,
        UnitState::OutputVerified,
        UnitState::OutputDurable,
        UnitState::Committed,
        UnitState::ReclaimIntent,
        UnitState::Reclaimed,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            UnitState::Pending => "PENDING",
            UnitState::Extracting => "EXTRACTING",
            UnitState::OutputWritten => "OUTPUT_WRITTEN",
            UnitState::OutputVerified => "OUTPUT_VERIFIED",
            UnitState::OutputDurable => "OUTPUT_DURABLE",
            UnitState::Committed => "COMMITTED",
            UnitState::ReclaimIntent => "RECLAIM_INTENT",
            UnitState::Reclaimed => "RECLAIMED",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<UnitState> {
        match s {
            "PENDING" => Ok(UnitState::Pending),
            "EXTRACTING" => Ok(UnitState::Extracting),
            "OUTPUT_WRITTEN" => Ok(UnitState::OutputWritten),
            "OUTPUT_VERIFIED" => Ok(UnitState::OutputVerified),
            "OUTPUT_DURABLE" => Ok(UnitState::OutputDurable),
            "COMMITTED" => Ok(UnitState::Committed),
            "RECLAIM_INTENT" => Ok(UnitState::ReclaimIntent),
            "RECLAIMED" => Ok(UnitState::Reclaimed),
            other => Err(crate::error::JournalError::state(format!(
                "unknown unit state '{other}' in journal"
            ))),
        }
    }
}

/// Status of an entry (output file) within its unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryStatus {
    Pending,
    Written,
    Verified,
    Durable,
    Committed,
    Skipped,
    Failed,
}

impl EntryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryStatus::Pending => "PENDING",
            EntryStatus::Written => "WRITTEN",
            EntryStatus::Verified => "VERIFIED",
            EntryStatus::Durable => "DURABLE",
            EntryStatus::Committed => "COMMITTED",
            EntryStatus::Skipped => "SKIPPED",
            EntryStatus::Failed => "FAILED",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<EntryStatus> {
        Ok(match s {
            "PENDING" => EntryStatus::Pending,
            "WRITTEN" => EntryStatus::Written,
            "VERIFIED" => EntryStatus::Verified,
            "DURABLE" => EntryStatus::Durable,
            "COMMITTED" => EntryStatus::Committed,
            "SKIPPED" => EntryStatus::Skipped,
            "FAILED" => EntryStatus::Failed,
            other => {
                return Err(crate::error::JournalError::state(format!(
                    "unknown entry status '{other}' in journal"
                )))
            }
        })
    }
}

/// State of a packed source range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RangeState {
    Active,
    ReclaimIntent,
    Partial,
    Reclaimed,
}

impl RangeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RangeState::Active => "ACTIVE",
            RangeState::ReclaimIntent => "RECLAIM_INTENT",
            RangeState::Partial => "PARTIAL",
            RangeState::Reclaimed => "RECLAIMED",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<RangeState> {
        Ok(match s {
            "ACTIVE" => RangeState::Active,
            "RECLAIM_INTENT" => RangeState::ReclaimIntent,
            "PARTIAL" => RangeState::Partial,
            "RECLAIMED" => RangeState::Reclaimed,
            other => {
                return Err(crate::error::JournalError::state(format!(
                    "unknown range state '{other}' in journal"
                )))
            }
        })
    }
}

/// A source volume (archive part file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeRecord {
    pub path: PathBuf,
    pub identity: Option<FileIdentity>,
    pub allocated_before: u64,
    pub logical_size: u64,
    pub is_first: bool,
    pub structural_digest: Option<String>,
}

/// A recovery unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryUnitRecord {
    pub seq: u64,
    pub state: UnitState,
    pub first_entry: u64,
    pub last_entry: u64,
    pub error: Option<String>,
    pub updated_at: String,
}

/// An archive entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryRecord {
    pub index_in_archive: u64,
    pub name: String,
    pub packed_size: u64,
    pub unpacked_size: u64,
    pub crc32: Option<u32>,
    pub is_directory: bool,
    pub is_solid: bool,
    pub split_before: bool,
    pub split_after: bool,
    pub encrypted: bool,
    pub recovery_unit: u64,
    pub final_path: Option<PathBuf>,
    pub partial_path: Option<PathBuf>,
    pub blake3: Option<String>,
    pub status: EntryStatus,
    pub actual_committed_path: Option<PathBuf>,
    pub existed_before_job: bool,
    pub expected_digest: Option<String>,
    pub is_redirection: bool,
    pub redirection_kind: Option<String>,
}

/// A packed source range (tied to a volume).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackedRangeRecord {
    pub volume_index: u64,
    pub start: u64,
    pub len: u64,
    pub state: RangeState,
    pub recovery_unit: Option<u64>,
    pub physically_released_bytes: u64,
    pub blake3_digest: Option<String>,
}

/// Job-level metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMeta {
    pub job_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub archive_path: PathBuf,
    pub destination: PathBuf,
    pub archive_fingerprint: Option<String>,
    pub safety_mode: String,
    pub settings_json: String,
    pub current_unit: u64,
    pub job_state: JobState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Active,
    Paused,
    Finalizing,
    Completed,
    Failed,
    Abandoned,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Active => "ACTIVE",
            JobState::Paused => "PAUSED",
            JobState::Finalizing => "FINALIZING",
            JobState::Completed => "COMPLETED",
            JobState::Failed => "FAILED",
            JobState::Abandoned => "ABANDONED",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<JobState> {
        Ok(match s {
            "ACTIVE" => JobState::Active,
            "PAUSED" => JobState::Paused,
            "FINALIZING" => JobState::Finalizing,
            "COMPLETED" => JobState::Completed,
            "FAILED" => JobState::Failed,
            "ABANDONED" => JobState::Abandoned,
            other => {
                return Err(crate::error::JournalError::state(format!(
                    "unknown job state '{other}' in journal"
                )))
            }
        })
    }
}

/// A recorded error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRecord {
    pub id: i64,
    pub at: String,
    pub operation: String,
    pub message: String,
    pub os_error: Option<u32>,
    pub recovery_state: String,
    pub recommended_action: String,
}

/// A state transition event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRecord {
    pub unit_seq: u64,
    pub from_state: String,
    pub to_state: String,
    pub at: String,
}
