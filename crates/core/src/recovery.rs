//! Recovery: discovery and reconciliation of interrupted jobs.
//!
//! On startup the app finds interrupted jobs (from the app-data registry and
//! by scanning for `.reclaimarc` directories beside archives). For each job
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

use reclaimarc_journal::models::{EntryStatus, JobState, RangeState, UnitState};
use reclaimarc_journal::{JobJournal, Registry, RegistryEntry};
use reclaimarc_platform::fs::{file_identity, open_for_identity};
use reclaimarc_platform::sparse::{open_for_reclaim, query_allocated_ranges, set_sparse};

use crate::error::CoreError;
use crate::state;

/// Summary presented on the recovery screen.
#[derive(Debug, Clone, serde::Serialize)]
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
    let reclaimed: u64 = ranges.iter().map(|r| r.physically_released_bytes).sum();
    let remaining: u64 = ranges
        .iter()
        .map(|r| r.len.saturating_sub(r.physically_released_bytes))
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
        errors: errors
            .iter()
            .map(|e| format!("[{}] {}: {}", e.at, e.operation, e.message))
            .collect(),
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
    let meta = journal.job_meta().map_err(CoreError::Journal)?;

    if meta.job_state == JobState::Finalizing {
        // Extraction and output verification already finished completely before the crash.
        // Finish deleting remaining source shells idempotently and advance to Completed.
        // SAFETY GATE: Verify identity of all existing source volumes before removing any files.
        let volumes = journal.volumes().map_err(CoreError::Journal)?;
        for v in &volumes {
            if v.path.exists() {
                let ident = file_identity(&v.path).map_err(|e| {
                    CoreError::failed(
                        "validate source identity before finalizing cleanup",
                        Some(v.path.clone()),
                        e.os,
                        "recovery",
                        format!(
                            "cannot verify source '{}' before finalizing cleanup: {}",
                            v.path.display(),
                            e.message
                        ),
                        "If the archive was replaced or moved, the job cannot finish safely.",
                    )
                })?;
                if let Some(recorded) = &v.identity {
                    if ident.volume_serial != recorded.volume_serial
                        || ident.file_id != recorded.file_id
                        || ident.file_size != recorded.file_size
                    {
                        return Err(CoreError::failed(
                            "validate source identity before finalizing cleanup",
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
                                recorded.file_size,
                            ),
                            "The source file was modified or replaced after extraction completed. Aborting cleanup to prevent data loss.",
                        ));
                    }
                }
            }
        }

        // Only after all existing source files pass identity validation may deletion proceed.
        for v in &volumes {
            if v.path.exists() {
                std::fs::remove_file(&v.path).map_err(|e| {
                    CoreError::failed(
                        "delete source shell during finalizing recovery",
                        Some(v.path.clone()),
                        e.raw_os_error().map(|code| code as u32),
                        "recovery",
                        format!(
                            "cannot delete source '{}' during finalizing recovery: {e}",
                            v.path.display()
                        ),
                        "Ensure the source file is not locked or read-only, then retry resume.",
                    )
                })?;
            }
        }
        journal
            .set_job_state(JobState::Completed)
            .map_err(CoreError::Journal)?;
        return Ok(journal);
    }

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
                        recorded.file_size,
                    ),
                    "The archive was modified since the job started. The job cannot resume safely.",
                ));
            }
        }
    }

    // 1.5 Validate source content manifest (structural digests and intact active ranges).
    let ranges = journal.packed_ranges().map_err(CoreError::Journal)?;
    for (v_idx, v) in volumes.iter().enumerate() {
        if let Some(expected_struct) = &v.structural_digest {
            let mut vol_ranges: Vec<(u64, u64)> = ranges
                .iter()
                .filter(|r| r.volume_index as usize == v_idx)
                .map(|r| (r.start, r.len))
                .collect();
            vol_ranges.sort_by_key(|&(start, _)| start);
            let actual_struct = crate::engine::compute_volume_structural_digest(
                &v.path,
                v.logical_size,
                &vol_ranges,
            )?;
            if actual_struct != *expected_struct {
                return Err(CoreError::failed(
                    "verify volume structural digest on resume",
                    Some(v.path.clone()),
                    None,
                    "recovery",
                    format!(
                        "source volume '{}' structural headers/metadata have been modified (expected BLAKE3 {}, computed {})",
                        v.path.display(),
                        expected_struct,
                        actual_struct
                    ),
                    "The archive structure was altered. Cannot resume safely.",
                ));
            }
        }
    }

    let units = journal.units().map_err(CoreError::Journal)?;

    for r in &ranges {
        let is_committed_unit = r.recovery_unit.is_some_and(|u_seq| {
            units.iter().any(|u| {
                u.seq == u_seq
                    && (state::is_committed(u.state)
                        || u.state == UnitState::ReclaimIntent
                        || u.state == UnitState::Reclaimed)
            })
        });

        if r.state == RangeState::Active && !is_committed_unit {
            if let Some(expected_blake3) = &r.blake3_digest {
                let vol = volumes.get(r.volume_index as usize).ok_or_else(|| {
                    CoreError::failed(
                        "lookup volume for range digest verification",
                        None,
                        None,
                        "recovery",
                        format!("volume index {} not found", r.volume_index),
                        "Corrupted journal database.",
                    )
                })?;
                crate::engine::verify_range_digest(&vol.path, r.start, r.len, expected_blake3)?;
            }
        } else if (r.state == RangeState::Reclaimed || r.state == RangeState::Partial)
            && r.blake3_digest.is_none()
        {
            // Legacy journal with no manifest that already had reclaimed ranges: fail closed!
            return Err(CoreError::failed(
                "verify legacy journal manifest",
                None,
                None,
                "recovery",
                "Cannot resume legacy journal without source manifest after source data was already modified or reclaimed.",
                "Legacy journals without cryptographic proof cannot resume destructive mode.",
            ));
        }
    }

    // 2. Reconcile uncommitted entries: adopt renamed-but-uncommitted finals
    //    whose BLAKE3 matches; clean up only partial attempts (.sx-partial*).
    //    NEVER delete pre-existing or user-owned files at final_path.
    for unit in &units {
        // Pending units were never started; committed/reclaimed units are handled below.
        if unit.state == UnitState::Pending || state::is_committed(unit.state) {
            continue;
        }
        let entries = journal
            .entries_for_unit(unit.seq)
            .map_err(CoreError::Journal)?;
        for entry in &entries {
            if entry.is_directory || entry.status == EntryStatus::Skipped {
                continue;
            }
            let final_path = entry.final_path.clone().unwrap_or_default();
            let partial = entry.partial_path.clone().unwrap_or_default();
            if entry.status == EntryStatus::Verified || entry.status == EntryStatus::Durable {
                // Possibly already renamed into place. Verify the final file.
                if final_path.exists() && entry.blake3.is_some() {
                    let blake3_expected = entry.blake3.as_deref().unwrap_or("");
                    if let Ok(true) = crate::engine::verify_against(
                        &final_path,
                        Some(entry.unpacked_size),
                        blake3_expected,
                    ) {
                        // Adopt: the rename happened; the commit is now recorded.
                        journal
                            .set_entry_committed(
                                entry.index_in_archive,
                                &final_path,
                                blake3_expected,
                            )
                            .map_err(CoreError::Journal)?;
                        delete_partial_attempts(&partial);
                        continue;
                    }
                }
            }
            // Incomplete/unmatched: clean up only job-owned partial attempts.
            delete_partial_attempts(&partial);
            journal
                .set_entry_status(entry.index_in_archive, EntryStatus::Pending)
                .map_err(CoreError::Journal)?;
        }
        // Reset uncommitted unit to Pending for clean re-extraction
        journal
            .reconcile_unit_state_on_resume(unit.seq, unit.state, UnitState::Pending)
            .map_err(CoreError::Journal)?;
    }

    // 2.5 Verify already-COMMITTED output before permitting any further reclamation.
    // Invariant: If a committed file is missing or corrupted and its source was already
    // deallocated, the engine must FAIL CLOSED immediately with a terminal error.
    for unit in &units {
        if state::is_committed(unit.state) {
            let entries = journal
                .entries_for_unit(unit.seq)
                .map_err(CoreError::Journal)?;
            let ranges = journal
                .packed_ranges_for_unit(unit.seq)
                .map_err(CoreError::Journal)?;

            for entry in &entries {
                if entry.is_directory || entry.status != EntryStatus::Committed {
                    continue;
                }
                let final_p = entry
                    .actual_committed_path
                    .as_ref()
                    .or(entry.final_path.as_ref());
                let Some(path) = final_p else { continue };
                let blake3_expected = entry
                    .expected_digest
                    .as_deref()
                    .or(entry.blake3.as_deref())
                    .unwrap_or("");
                if !blake3_expected.is_empty() {
                    match crate::engine::verify_against(
                        path,
                        Some(entry.unpacked_size),
                        blake3_expected,
                    ) {
                        Ok(true) => {}
                        _ => {
                            let source_already_reclaimed = ranges.iter().any(|r| {
                                r.state == RangeState::Reclaimed || r.state == RangeState::Partial
                            });

                            if source_already_reclaimed {
                                return Err(CoreError::failed(
                                    "verify committed output on resume",
                                    Some(path.clone()),
                                    None,
                                    "recovery",
                                    format!(
                                        "Committed output '{}' is corrupted or missing after source allocation was destroyed/reclaimed",
                                        path.display()
                                    ),
                                    "Terminal integrity violation: source data was reclaimed and output is unrecoverable.",
                                ));
                            }

                            // Positively verify that 100% of required source bytes physically exist on disk
                            for r in &ranges {
                                let vol = volumes.get(r.volume_index as usize).ok_or_else(|| {
                                    CoreError::failed(
                                        "verify committed output on resume",
                                        Some(path.clone()),
                                        None,
                                        "recovery",
                                        format!("Source volume {} could not be found to prove source intact", r.volume_index),
                                        "Source integrity could not be proven.",
                                    )
                                })?;
                                let query_handle = reclaimarc_platform::sparse::open_for_query(&vol.path).map_err(|e| {
                                    CoreError::failed(
                                        "verify committed output on resume",
                                        Some(vol.path.clone()),
                                        e.os,
                                        "recovery",
                                        format!("Cannot open source volume '{}' to verify physical allocation: {}", vol.path.display(), e.message),
                                        "Source integrity could not be proven.",
                                    )
                                })?;
                                let allocated = reclaimarc_platform::sparse::query_allocated_ranges(
                                    &query_handle,
                                    &vol.path,
                                    r.start,
                                    r.len,
                                ).map_err(|e| {
                                    CoreError::failed(
                                        "verify committed output on resume",
                                        Some(vol.path.clone()),
                                        e.os,
                                        "recovery",
                                        format!("Cannot query physical allocation of source volume '{}': {}", vol.path.display(), e.message),
                                        "Source integrity could not be proven.",
                                    )
                                })?;
                                let total_allocated: u64 = allocated.iter().map(|a| a.len).sum();
                                if total_allocated < r.len {
                                    return Err(CoreError::failed(
                                        "verify committed output on resume",
                                        Some(path.clone()),
                                        None,
                                        "recovery",
                                        format!(
                                            "Committed output '{}' is corrupted or missing and source allocation is incomplete (allocated {} < required {})",
                                            path.display(),
                                            total_allocated,
                                            r.len
                                        ),
                                        "Terminal integrity violation: source data is missing and output is unrecoverable.",
                                    ));
                                }
                            }

                            // 100% of source bytes are positively proven physically intact on disk: reset unit to Pending for clean re-extraction
                            journal
                                .reconcile_unit_state_on_resume(
                                    unit.seq,
                                    unit.state,
                                    UnitState::Pending,
                                )
                                .map_err(CoreError::Journal)?;
                            for e in &entries {
                                if !e.is_directory {
                                    journal
                                        .set_entry_status(e.index_in_archive, EntryStatus::Pending)
                                        .map_err(CoreError::Journal)?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. Reconcile reclamation: RECLAIM_INTENT units must be completed by
    //    inspecting the actual filesystem state.
    for unit in &units {
        if unit.state != UnitState::ReclaimIntent {
            continue;
        }
        let ranges = journal
            .packed_ranges_for_unit(unit.seq)
            .map_err(CoreError::Journal)?;
        let volumes = journal.volumes().map_err(CoreError::Journal)?;
        for r in &ranges {
            let vol = volumes.get(r.volume_index as usize).ok_or_else(|| {
                CoreError::Precondition(format!("volume {} not found", r.volume_index))
            })?;
            let file = open_for_reclaim(&vol.path).map_err(CoreError::Platform)?;
            set_sparse(&file, &vol.path).map_err(CoreError::Platform)?;
            // What is still allocated in this range?
            let allocated = query_allocated_ranges(&file, &vol.path, r.start, r.len)
                .map_err(CoreError::Platform)?;
            if allocated.is_empty() {
                // Already fully reclaimed.
                journal
                    .mark_range_outcome(
                        r.volume_index,
                        r.start,
                        r.len,
                        RangeState::Reclaimed,
                        r.len,
                    )
                    .map_err(CoreError::Journal)?;
            } else {
                // Punch the remainder now (the unit is committed; its source
                // is provably safe to reclaim).
                let mut total_released = 0u64;
                for chunk in &allocated {
                    let report =
                        reclaimarc_platform::sparse::reclaim_range(&file, &vol.path, *chunk)
                            .map_err(CoreError::Platform)?;
                    total_released = total_released.saturating_add(report.released_bytes());
                }
                let rem_alloc = query_allocated_ranges(&file, &vol.path, r.start, r.len)
                    .map_err(CoreError::Platform)?;
                let rem_alloc_bytes: u64 = rem_alloc.iter().map(|c| c.len).sum();
                let verified_released = r.len.saturating_sub(rem_alloc_bytes).min(r.len);
                let state = if rem_alloc.is_empty() {
                    RangeState::Reclaimed
                } else if verified_released > 0 {
                    RangeState::Partial
                } else {
                    RangeState::Active
                };
                journal
                    .mark_range_outcome(r.volume_index, r.start, r.len, state, verified_released)
                    .map_err(CoreError::Journal)?;
            }
        }
        let updated_ranges = journal
            .packed_ranges_for_unit(unit.seq)
            .map_err(CoreError::Journal)?;
        if updated_ranges
            .iter()
            .all(|r| r.state == RangeState::Reclaimed)
        {
            journal
                .reconcile_unit_state_on_resume(
                    unit.seq,
                    UnitState::ReclaimIntent,
                    UnitState::Reclaimed,
                )
                .map_err(CoreError::Journal)?;
        }
    }

    // 5. Fingerprint sanity (only advisory — identity already validated).
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
    let Some(prefix) = recorded_partial
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    else {
        return;
    };
    if let Some(parent) = recorded_partial.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) {
                    let _ = reclaimarc_platform::longpath::remove_file_existing(&entry.path());
                }
            }
        }
    }
}

/// Abandon a job: remove the registry entry and the journal directory.
/// The caller confirms that reclaimed source data cannot be restored.
pub fn abandon_job(journal_path: &Path, job_id: &str) -> Result<(), CoreError> {
    let mut registry =
        Registry::open(&Registry::default_app_data_dir()).map_err(CoreError::Journal)?;
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
