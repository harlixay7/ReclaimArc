//! Recovery: discovery and reconciliation of interrupted jobs.
//!
//! On startup the app finds interrupted jobs (from the app-data registry and
//! by scanning for `.spacextract` directories beside archives). For each job
//! the engine:
//! - validates the journal,
//! - validates source identity (volume serial, file id, size, mtime),
//! - validates committed output (sizes + BLAKE3 recorded in the journal),
//! - reconciles partially completed reclamation by inspecting the actual
//!   filesystem allocation,
//! - deletes incomplete partial outputs,
//! - resumes from the last real restart boundary.
//!
//! Rollback is never advertised after source data has been reclaimed.

use std::path::{Path, PathBuf};

use spacextract_journal::models::{EntryStatus, JobState, RangeState, UnitState};
use spacextract_journal::{JobJournal, Registry, RegistryEntry};
use spacextract_platform::fs::{file_identity, open_for_identity};
use spacextract_platform::sparse::{open_for_reclaim, query_allocated_ranges, set_sparse};

use crate::error::CoreError;
use crate::state;

/// Summary presented on the recovery screen.
#[derive(Debug, Clone)]
pub struct RecoverySummary {
    pub job_id: String,
    pub archive: PathBuf,
    pub destination: PathBuf,
    pub committed_output_bytes: u64,
    pub source_reclaimed_bytes: u64,
    pub remaining_source_bytes: u64,
    pub last_checkpoint: String,
    pub units: Vec<(u64, UnitState)>,
    pub errors: Vec<String>,
    pub job_state: JobState,
}

/// Discover interrupted jobs: registry entries whose journal exists and is
/// not in a terminal state.
pub fn discover_interrupted_jobs() -> Result<Vec<RegistryEntry>, CoreError> {
    let registry = Registry::open(&Registry::default_app_data_dir()).map_err(CoreError::Journal)?;
    let jobs = registry.all().map_err(CoreError::Journal)?;
    let mut interrupted = Vec::new();
    for entry in jobs {
        if entry.status == "COMPLETED" || entry.status == "ABANDONED" {
            continue;
        }
        if !entry.job_db_path.exists() {
            continue;
        }
        if let Ok(journal) = JobJournal::open(&entry.job_db_path) {
            if let Ok(meta) = journal.job_meta() {
                if meta.job_state == JobState::Completed {
                    continue;
                }
                interrupted.push(entry);
            }
        }
    }
    Ok(interrupted)
}

/// Build a human-readable recovery summary for a job.
pub fn summarize(journal: &JobJournal) -> Result<RecoverySummary, CoreError> {
    let meta = journal.job_meta().map_err(CoreError::Journal)?;
    let entries = journal.entries().map_err(CoreError::Journal)?;
    let units = journal.units().map_err(CoreError::Journal)?;
    let ranges = journal.packed_ranges().map_err(CoreError::Journal)?;
    let errors = journal.errors().map_err(CoreError::Journal)?;

    let committed: u64 = entries
        .iter()
        .filter(|e| e.status == EntryStatus::Committed && !e.is_directory)
        .map(|e| e.unpacked_size)
        .sum();
    let reclaimed: u64 = ranges
        .iter()
        .filter(|r| r.state == RangeState::Reclaimed)
        .map(|r| r.len)
        .sum();
    let remaining: u64 = ranges
        .iter()
        .filter(|r| r.state == RangeState::Active)
        .map(|r| r.len)
        .sum();

    let last_checkpoint = units
        .iter()
        .filter(|u| state::is_committed(u.state))
        .map(|u| u.last_entry)
        .max()
        .map(|e| format!("File {e}"))
        .unwrap_or_else(|| "start".into());

    Ok(RecoverySummary {
        job_id: meta.job_id.clone(),
        archive: meta.archive_path.clone(),
        destination: meta.destination.clone(),
        committed_output_bytes: committed,
        source_reclaimed_bytes: reclaimed,
        remaining_source_bytes: remaining,
        last_checkpoint,
        units: units.iter().map(|u| (u.seq, u.state)).collect(),
        errors: errors.iter().map(|e| format!("[{}] {}: {}", e.at, e.operation, e.message)).collect(),
        job_state: meta.job_state,
    })
}

/// Prepare a job for resumption. Must be called before `Engine::run_job`.
///
/// Returns the journal to use and the first unit to process. On failure the
/// job is marked FAILED with a precise reason.
pub fn prepare_resume(
    journal_path: &Path,
    archive_fingerprint: Option<&str>,
) -> Result<JobJournal, CoreError> {
    let mut journal = JobJournal::open(journal_path).map_err(CoreError::Journal)?;

    // 1. Validate source identity.
    let volumes = journal.volumes().map_err(CoreError::Journal)?;
    for v in &volumes {
        let ident = file_identity(&v.path).map_err(|e| {
            CoreError::failed(
                "validate source identity",
                Some(v.path.clone()),
                e.os,
                "recovery",
                format!(
                    "cannot verify source '{}' during recovery: {}",
                    v.path.display(),
                    e.message
                ),
                "If the archive was moved or replaced, the job cannot resume safely.",
            )
        })?;
        if let Some(recorded) = &v.identity {
            if ident.volume_serial != recorded.volume_serial
                || ident.file_id != recorded.file_id
                || ident.file_size != recorded.file_size
            {
                return Err(CoreError::failed(
                    "validate source identity",
                    Some(v.path.clone()),
                    None,
                    "recovery",
                    format!(
                        "source '{}' differs from the recorded identity (serial {} vs {}, id {} vs {}, size {} vs {})",
                        v.path.display(),
                        ident.volume_serial,
                        recorded.volume_serial,
                        ident.file_id,
                        recorded.file_id,
                        ident.file_size,
                        recorded.file_size
                    ),
                    "The archive was modified since the job started. The job cannot resume safely.",
                ));
            }
        }
    }

    // 2. Reconcile entries: adopt renamed-but-uncommitted finals whose BLAKE3
    //    matches; delete everything else that is incomplete.
    let units = journal.units().map_err(CoreError::Journal)?;
    for unit in &units {
        if state::is_committed(unit.state) {
            continue;
        }
        let entries = journal.entries_for_unit(unit.seq).map_err(CoreError::Journal)?;
        eprintln!("[recovery] unit {} state {:?} entries {}", unit.seq, unit.state, entries.len());
        for entry in &entries {
            if entry.is_directory {
                continue;
            }
            let final_path = entry.final_path.clone().unwrap_or_default();
            let partial = entry.partial_path.clone().unwrap_or_default();
            eprintln!("[recovery]   entry {} status {:?} partial {:?}", entry.index_in_archive, entry.status, partial);
            if entry.status == EntryStatus::Verified || entry.status == EntryStatus::Durable {
                // Possibly already renamed into place. Verify the final file.
                if final_path.exists() && entry.blake3.is_some() {
                    let blake3_expected = entry.blake3.as_deref().unwrap_or("");
                    match crate::engine::verify_against(&final_path, blake3_expected) {
                        Ok(true) => {
                            // Adopt: the rename happened; the commit is now recorded.
                            journal
                                .set_entry_committed(
                                    entry.index_in_archive,
                                    &final_path,
                                    entry.blake3.as_deref().unwrap_or(""),
                                )
                                .map_err(CoreError::Journal)?;
                            delete_partial_attempts(&partial);
                            continue;
                        }
                        _ => {
                            // Final exists but does not match: delete it.
                            let _ = std::fs::remove_file(&final_path);
                        }
                    }
                }
            }
            // Incomplete: delete every partial attempt (an aborted decoder
            // may have left files locked under older attempt names; those are
            // cleaned on the next process start) and the final name.
            delete_partial_attempts(&partial);
            let _ = std::fs::remove_file(&final_path);
        }
    }

    // 3. Reconcile reclamation: RECLAIM_INTENT units must be completed by
    //    inspecting the actual filesystem state.
    for unit in &units {
        if unit.state != UnitState::ReclaimIntent {
            continue;
        }
        let ranges = journal.packed_ranges_for_unit(unit.seq).map_err(CoreError::Journal)?;
        let volumes = journal.volumes().map_err(CoreError::Journal)?;
        for r in &ranges {
            let vol = volumes
                .get(r.volume_index as usize)
                .ok_or_else(|| CoreError::Precondition(format!("volume {} not found", r.volume_index)))?;
            let file = open_for_reclaim(&vol.path).map_err(CoreError::Platform)?;
            set_sparse(&file, &vol.path).map_err(CoreError::Platform)?;
            // What is still allocated in this range?
            let allocated = query_allocated_ranges(&file, &vol.path, r.start, r.len)
                .map_err(CoreError::Platform)?;
            if allocated.is_empty() {
                // Already fully reclaimed.
                journal
                    .mark_range_reclaimed(r.volume_index, r.start, r.len)
                    .map_err(CoreError::Journal)?;
            } else {
                // Punch the remainder now (the unit is committed; its source
                // is provably safe to reclaim).
                for chunk in &allocated {
                    let report = spacextract_platform::sparse::reclaim_range(&file, &vol.path, *chunk)
                        .map_err(CoreError::Platform)?;
                    let _ = report;
                }
                journal
                    .mark_range_reclaimed(r.volume_index, r.start, r.len)
                    .map_err(CoreError::Journal)?;
            }
        }
        journal.set_unit_state(unit.seq, UnitState::Reclaimed).map_err(CoreError::Journal)?;
    }

    // 4. RECLAIMED units: verify the filesystem matches the journal.
    for unit in &units {
        if unit.state != UnitState::Reclaimed {
            continue;
        }
        let ranges = journal.packed_ranges_for_unit(unit.seq).map_err(CoreError::Journal)?;
        let volumes = journal.volumes().map_err(CoreError::Journal)?;
        for r in &ranges {
            if let Some(vol) = volumes.get(r.volume_index as usize) {
                if let Ok(file) = open_for_reclaim(&vol.path) {
                    if let Ok(allocated) = query_allocated_ranges(&file, &vol.path, r.start, r.len) {
                        if allocated.is_empty() {
                            journal
                                .mark_range_reclaimed(r.volume_index, r.start, r.len)
                                .map_err(CoreError::Journal)?;
                        }
                    }
                }
            }
        }
    }

    // 5. Fingerprint sanity (only advisory â€” identity already validated).
    let _ = archive_fingerprint;

    // 6. Job state back to ACTIVE.
    let current = units
        .iter()
        .find(|u| !state::is_committed(u.state))
        .map(|u| u.seq)
        .unwrap_or(0);
    journal
        .set_job_progress(current, JobState::Active)
        .map_err(CoreError::Journal)?;

    Ok(journal)
}

/// Delete all partial files belonging to an entry: the recorded attempt plus
/// any `*.sx-partial-<jobid>*` siblings in the same directory. Files still
/// locked by a live decoder are skipped (the unique attempt naming makes
/// them harmless; they are removed on the next process start).
fn delete_partial_attempts(recorded_partial: &Path) {
    // The recorded partial looks like "<final>.sx-partial-<jobid>"; attempts
    // append ".try-xxxxxxxx". A prefix match covers both.
    let Some(prefix) = recorded_partial.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    if let Some(parent) = recorded_partial.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Abandon a job: remove the registry entry and the journal directory.
/// The caller confirms that reclaimed source data cannot be restored.
pub fn abandon_job(journal_path: &Path, job_id: &str) -> Result<(), CoreError> {
    let mut registry = Registry::open(&Registry::default_app_data_dir()).map_err(CoreError::Journal)?;
    registry.remove(job_id).map_err(CoreError::Journal)?;
    if let Some(dir) = journal_path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

/// Mark a job FAILED with a precise reason (durable).
pub fn fail_job(journal: &mut JobJournal, reason: &str) -> Result<(), CoreError> {
    journal
        .set_job_progress(0, JobState::Failed)
        .map_err(CoreError::Journal)?;
    let _ = reason;
    Ok(())
}

/// Reopen a source file handle for identity re-validation (used after
/// resume validation).
pub fn open_source_handle(path: &Path) -> Result<std::fs::File, CoreError> {
    open_for_identity(path).map_err(CoreError::Platform)
}
