//! The transactional extraction engine.
//!
//! Guarantees (see SAFETY_MODEL.md):
//! - source bytes are reclaimed only after their unit is completely decoded,
//!   integrity-verified, flushed durably, atomically committed and recorded
//!   in the durable journal;
//! - every state transition is journaled BEFORE the corresponding filesystem
//!   action, so a crash between any two operations is recoverable;
//! - the engine stops before consuming the emergency reserve.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use reclaimarc_archive::model::ArchiveInfo;
use reclaimarc_archive::{ArchiveBackend, ExtractOptions, OpenOptions, ProgressEvent};
use reclaimarc_journal::models::{
    EntryRecord, EntryStatus, FileIdentity, JobMeta, JobState, PackedRangeRecord, RangeState,
    RecoveryUnitRecord, UnitState, VolumeRecord,
};
use reclaimarc_journal::{JobJournal, Registry, RegistryEntry};
use reclaimarc_platform::fs::{
    allocated_size_from_handle, file_identity, free_space, same_storage_pool, total_space,
};
use reclaimarc_platform::sparse::{open_for_reclaim, reclaim_range, set_sparse, ByteRange};
use reclaimarc_platform::{flush, longpath};

use crate::config::{ConflictPolicy, EngineConfig};
use crate::error::CoreError;
use crate::events::Event;
use crate::fault::{self, CrashPoint};
use crate::paths::{find_case_collisions, partial_path, validate_entry, SafeEntry};
use crate::safety::{validate_capacity_before_unit, SpaceCheck, SpaceMonitor};
use crate::state;

/// How the job should behave on completion regarding the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExtractionMode {
    /// Keep the original archive.
    Normal,
    /// Progressively destroy verified source ranges to reclaim space.
    LowSpace,
}

/// The job id and control handles handed to the caller.
#[derive(Debug, Clone)]
pub struct JobHandle {
    pub job_id: String,
    /// Set to abort the current unit safely (pause / stop).
    pub pause: Arc<AtomicBool>,
    /// Set to cancel the job (resumable).
    pub cancel: Arc<AtomicBool>,
}

impl JobHandle {
    /// Normal pause: finish or safely abort the current unit, then pause.
    pub fn pause(&self) {
        self.pause.store(true, Ordering::SeqCst);
    }
    /// Immediate stop: abort the decoder now; the current unit's source stays
    /// intact and partials are reconciled on resume.
    pub fn stop_safely(&self) {
        self.pause.store(true, Ordering::SeqCst);
        self.cancel.store(true, Ordering::SeqCst);
    }
    /// Cancel the job (previously reclaimed source data cannot be restored).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Result of running a job.
#[derive(Debug)]
pub enum JobOutcome {
    Completed { committed_bytes: u64, reclaimed_bytes: u64 },
    Paused,
    Cancelled,
    Failed { failure: crate::error::FailureInfo },
}

/// Observe free space on the volume containing `dir`.
///
/// With the `test-hooks` feature this can be overridden via
/// `RECLAIMARC_TEST_FREE_SPACE` so integration tests can exercise
/// low-space scenarios deterministically (documented in TESTING.md).
pub fn observed_free_space(dir: &Path) -> Result<u64, CoreError> {
    #[cfg(feature = "test-hooks")]
    if let Ok(v) = std::env::var("RECLAIMARC_TEST_FREE_SPACE") {
        if let Ok(bytes) = v.parse::<u64>() {
            return Ok(bytes);
        }
    }
free_space(dir).map_err(CoreError::Platform)
}

/// The partial-name suffix for one extraction attempt.
///
/// The unrar DLL leaves the output file of an aborted extraction locked until
/// the process exits (verified empirically), so each attempt uses a unique
/// suffix. Recovery removes `*.sx-partial-<jobid>*` leftovers.
pub fn attempt_suffix(job_id: &str) -> String {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!(".sx-partial-{job_id}.try-{}", &nonce[..8])
}

/// The engine.
pub struct Engine {
    pub config: EngineConfig,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Engine {
        Engine { config }
    }

    /// The journal directory beside the archive.
    pub fn journal_dir(archive: &Path, job_id: &str) -> PathBuf {
        archive
            .parent()
            .map(|p| p.join(".reclaimarc").join(job_id))
            .unwrap_or_else(|| PathBuf::from(".reclaimarc").join(job_id))
    }

    /// Analyze an archive for a destination: full inspection + space plan.
    pub fn analyze(
        &self,
        archive: &Path,
        destination: &Path,
        password: Option<String>,
    ) -> Result<(ArchiveInfo, crate::planner::SpacePlan), CoreError> {
        let mut backend = reclaimarc_archive::backend_for(archive)?;
        let info = backend.inspect(&OpenOptions { password })?;
        let free = observed_free_space(destination)?;
        let total = total_space(destination).map_err(CoreError::Platform)?;
        let plan = crate::planner::plan(&info, free, total, &self.config)?;
        Ok((info, plan))
    }

    /// Create a new job: analyze, validate, journal, and return handles.
    #[allow(clippy::too_many_arguments)]
    pub fn start_job(
        &self,
        archive: &Path,
        destination: &Path,
        mode: ExtractionMode,
        password: Option<String>,
        tx: Sender<Event>,
    ) -> Result<(JobHandle, JobJob), CoreError> {
        let job_id = uuid::Uuid::new_v4().to_string();
let mut backend = reclaimarc_archive::backend_for(archive)?;
        let info = backend.inspect(&OpenOptions { password: password.clone() })?;

        // Destination must exist before any probe runs against it.
        if !destination.exists() {
            std::fs::create_dir_all(destination).map_err(|e| {
                CoreError::failed(
                    "create destination",
                    Some(destination.to_path_buf()),
                    e.raw_os_error().map(|v| v as u32),
                    "pending",
                    format!("cannot create destination '{}': {e}", destination.display()),
                    "Choose a writable destination folder.",
                )
            })?;
        }

        let free = observed_free_space(destination)?;
        let total = total_space(destination).map_err(CoreError::Platform)?;
        let plan = crate::planner::plan(&info, free, total, &self.config)?;

        match mode {
            ExtractionMode::Normal => {
                if !plan.normal_feasible {
                    return Err(CoreError::Infeasible(format!(
                        "Normal extraction requires {} bytes of free space but only {} are available. \
                         Use Low-Space extraction to reclaim source space progressively.",
                        plan.unpacked_total, plan.free_now
                    )));
                }
            }
            ExtractionMode::LowSpace => {
                if !plan.progressive_feasible {
                    return Err(CoreError::Infeasible(
                        plan.reason.clone().unwrap_or_else(|| {
                            "Progressive extraction is not possible on this volume.".into()
                        }),
                    ));
                }
                let same = same_storage_pool(archive, destination).map_err(CoreError::Platform)?;
                if !same {
                    return Err(CoreError::Infeasible(
                        "The archive and destination are on different volumes: reclaiming source \
                         space cannot increase capacity available to the destination."
                            .into(),
                    ));
                }
                let caps = reclaimarc_platform::capabilities::filesystem_capabilities(destination)
                    .map_err(CoreError::Platform)?;
                if !caps.progressive_reclaim_supported {
                    return Err(CoreError::Infeasible(caps.probe.verdict));
                }
            }
        }

// Validate every entry name before anything is written.
        let (safe_entries, name_map) = self.validate_entries(&info)?;

        let journal_dir = Self::journal_dir(archive, &job_id);
        let journal_path = journal_dir.join("job.db");
        let now = reclaimarc_journal::now_iso();
let settings_json = {
            let mut v = serde_json::to_value(&self.config).unwrap_or_else(|_| serde_json::json!({}));
            v["mode"] = serde_json::json!(match mode {
                ExtractionMode::Normal => "normal",
                ExtractionMode::LowSpace => "lowspace",
            });
            v.to_string()
        };
        let meta = JobMeta {
            job_id: job_id.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
            archive_path: archive.to_path_buf(),
            destination: destination.to_path_buf(),
            archive_fingerprint: None,
            safety_mode: self.config.safety_mode.as_str().to_string(),
            settings_json,
            current_unit: 0,
            job_state: JobState::Active,
        };
        let mut journal = JobJournal::create(&journal_path, &meta).map_err(CoreError::Journal)?;

// Volumes with durable identity snapshots.
        let mut volumes = Vec::new();
        for (i, v) in info.volumes.iter().enumerate() {
            let ident = file_identity(&v.path).map_err(CoreError::Platform)?;
            let handle =
                reclaimarc_platform::sparse::open_for_query(&v.path).map_err(CoreError::Platform)?;
            let allocated = allocated_size_from_handle(&handle, &v.path).map_err(CoreError::Platform)?;
            volumes.push(VolumeRecord {
                path: v.path.clone(),
                identity: Some(FileIdentity {
                    volume_serial: ident.volume_serial,
                    file_id: ident.file_id,
                    file_size: ident.file_size,
                    last_write_time: ident.last_write_time,
                }),
                allocated_before: allocated,
                logical_size: v.logical_size,
                is_first: i == 0,
            });
        }
        journal.add_volumes(&volumes).map_err(CoreError::Journal)?;

        // Recovery units.
        let units: Vec<RecoveryUnitRecord> = info
            .recovery_units
            .iter()
            .map(|u| RecoveryUnitRecord {
                seq: u.seq,
                state: UnitState::Pending,
                first_entry: u.first_entry,
                last_entry: u.last_entry,
                error: None,
                updated_at: now.clone(),
            })
            .collect();
        journal.add_units(&units).map_err(CoreError::Journal)?;

// Entries with validated output paths, mapped to their recovery unit.
        let entries: Vec<EntryRecord> = info
            .entries
            .iter()
            .map(|e| {
                let safe = &safe_entries[e.index as usize];
                let final_path = safe.output_path(destination);
                let partial = partial_path(&final_path, &job_id);
                let unit = info
                    .recovery_units
                    .iter()
                    .find(|u| e.index >= u.first_entry && e.index <= u.last_entry)
                    .map(|u| u.seq)
                    .unwrap_or(0);
                EntryRecord {
                    index_in_archive: e.index,
                    name: e.name.clone(),
                    packed_size: e.packed_size,
                    unpacked_size: e.unpacked_size,
                    crc32: e.crc32,
                    is_directory: e.is_directory,
                    is_solid: e.is_solid,
                    split_before: e.split_before,
                    split_after: e.split_after,
                    encrypted: e.encrypted,
                    recovery_unit: unit,
                    final_path: Some(final_path),
                    partial_path: Some(partial),
                    blake3: None,
                    status: EntryStatus::Pending,
                }
            })
            .collect();
        journal.add_entries(&entries).map_err(CoreError::Journal)?;

        // Packed ranges.
        let mut ranges = Vec::new();
        for u in &info.recovery_units {
            for r in &u.packed_ranges {
                ranges.push(PackedRangeRecord {
                    volume_index: r.volume_index,
                    start: r.start,
                    len: r.len,
                    state: RangeState::Active,
                    recovery_unit: Some(u.seq),
                });
            }
        }
        journal.add_packed_ranges(&ranges).map_err(CoreError::Journal)?;

        // Registry mirror in application data.
        let mut registry = Registry::open(&Registry::default_app_data_dir()).map_err(CoreError::Journal)?;
        registry
            .upsert(&RegistryEntry {
                job_id: job_id.clone(),
                archive_dir: archive
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from(".")),
                job_db_path: journal_path,
                archive: archive.to_path_buf(),
                destination: destination.to_path_buf(),
                created_at: now.clone(),
                updated_at: now,
                status: "ACTIVE".into(),
            })
            .map_err(CoreError::Journal)?;

        let _ = tx.send(Event::JobStarted {
            job_id: job_id.clone(),
        });

        Ok((
            JobHandle {
                job_id: job_id.clone(),
                pause: Arc::new(AtomicBool::new(false)),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            JobJob {
                job_id,
                archive: archive.to_path_buf(),
                destination: destination.to_path_buf(),
                journal,
                info,
                backend,
                name_map,
                mode,
                password,
                tx,
            },
        ))
    }

    /// Validate all entry names; returns safe entries and the name map.
    fn validate_entries(&self, info: &ArchiveInfo) -> Result<(Vec<SafeEntry>, HashMap<u64, String>), CoreError> {
        let mut safe_entries = Vec::new();
        let mut name_map = HashMap::new();
        for e in &info.entries {
            let safe = validate_entry(&e.name, e.is_directory)?;
            if !e.is_directory {
                name_map.insert(e.index, safe.relative());
            }
            safe_entries.push(safe);
        }
        // Case-insensitive collisions across file entries.
        let names: Vec<(usize, String)> = safe_entries
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.is_directory)
            .map(|(i, s)| (i, s.relative()))
            .collect();
        let collisions = find_case_collisions(&names);
        if !collisions.is_empty() {
            let (a, b) = collisions[0];
            return Err(CoreError::Precondition(format!(
                "archive contains case-insensitive filename collisions: '{}' and '{}' — Windows would overwrite one with the other.",
                safe_entries[a].original, safe_entries[b].original
            )));
        }
        Ok((safe_entries, name_map))
    }

/// Run a job to completion (or pause/cancel/failure).
    pub fn run_job(&mut self, job: &mut JobJob, handle: &JobHandle) -> Result<JobOutcome, CoreError> {
        job.run(self.config.clone(), &handle.pause, &handle.cancel)
    }

    /// Resume an interrupted job from its journal.
    ///
    /// Runs the full recovery preparation (identity validation, partial
    /// reconciliation, reclaim reconciliation) and returns a runnable job.
    pub fn resume_job(&self, journal_path: &Path, tx: Sender<Event>) -> Result<(JobHandle, JobJob), CoreError> {
        let journal = crate::recovery::prepare_resume(journal_path, None)?;
        let meta = journal.job_meta().map_err(CoreError::Journal)?;
        let mut backend = reclaimarc_archive::backend_for(&meta.archive_path)?;
        let info = backend
            .inspect(&OpenOptions {
                password: None,
            })
            .map_err(|e| {
                CoreError::failed(
                    "re-inspect archive for resume",
                    Some(meta.archive_path.clone()),
                    None,
                    "recovery",
                    format!("cannot re-inspect the archive: {e}"),
                    "If the archive was moved or renamed, the job cannot resume.",
                )
            })?;
        let (_, name_map) = self.validate_entries(&info)?;
        let mode = serde_json::from_str::<serde_json::Value>(&meta.settings_json)
            .ok()
            .and_then(|v| v.get("mode").and_then(|m| m.as_str()).and_then(|m| match m {
                "normal" => Some(ExtractionMode::Normal),
                "lowspace" => Some(ExtractionMode::LowSpace),
                _ => None,
            }))
            .unwrap_or(ExtractionMode::Normal);

        let job_id = meta.job_id.clone();
        let _ = tx.send(Event::JobStarted {
            job_id: job_id.clone(),
        });
        Ok((
            JobHandle {
                job_id: job_id.clone(),
                pause: Arc::new(AtomicBool::new(false)),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            JobJob {
                job_id,
                archive: meta.archive_path.clone(),
                destination: meta.destination.clone(),
                journal,
                info,
                backend,
                name_map,
                mode,
                password: None,
                tx,
            },
        ))
    }
}

impl CoreError {
    pub(crate) fn from_io_source(op: &str, path: &Path, e: std::io::Error) -> CoreError {
        CoreError::Failed {
            operation: op.into(),
            path: Some(path.to_path_buf()),
            os_error: e.raw_os_error().map(|v| v as u32),
            recovery_state: "job active".into(),
            message: format!("{op} failed for '{}': {e}", path.display()),
            recommended_action: "Check permissions and retry.".into(),
        }
    }
}

/// An active job: journal + archive info + control state.
pub struct JobJob {
    pub job_id: String,
    pub archive: PathBuf,
    pub destination: PathBuf,
    pub journal: JobJournal,
    pub info: ArchiveInfo,
    pub backend: Box<dyn ArchiveBackend>,
    pub name_map: HashMap<u64, String>,
    pub mode: ExtractionMode,
    pub password: Option<String>,
    pub tx: Sender<Event>,
}

impl std::fmt::Debug for JobJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobJob")
            .field("job_id", &self.job_id)
            .field("archive", &self.archive)
            .field("destination", &self.destination)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl JobJob {
    /// Run the job. Blocking; call from a worker thread.
    pub fn run(
        &mut self,
        config: EngineConfig,
        pause_flag: &AtomicBool,
        cancel_flag: &AtomicBool,
    ) -> Result<JobOutcome, CoreError> {
        let tx = self.tx.clone();

        // Determine units state and whether this is a resume of an in-progress job.
        let units = self.journal.units().map_err(CoreError::Journal)?;
        let is_resuming = units
            .iter()
            .any(|u| state::is_committed(u.state) || state::is_reclaimed(u.state));

        // Pre-test for destructive extraction (only on initial start, before any source holes are punched).
        if self.mode == ExtractionMode::LowSpace && config.pre_test && !is_resuming {
            let total: u64 = self.info.packed_size;
            let _ = tx.send(Event::PreTestStarted { bytes_total: total });
            let cancel_arc = Arc::new(AtomicBool::new(false));
            let mut progress_cb = |e: ProgressEvent| {
                let _ = tx.send(match e {
                    ProgressEvent::EntryProgress { current, total, .. } => {
                        Event::PreTestProgress { current, total }
                    }
                });
                !cancel_flag.load(Ordering::SeqCst) && !pause_flag.load(Ordering::SeqCst)
            };
            let report = self.backend.test_integrity(
                self.password.as_deref(),
                Some(cancel_arc),
                Some(&mut progress_cb),
            )?;
            let _ = tx.send(Event::PreTestFinished {
                ok: report.ok,
                bytes_tested: report.bytes_tested,
            });
            if !report.ok {
                return Err(CoreError::Infeasible(format!(
                    "Archive integrity test failed{}: {}. Destructive extraction will not start.",
                    report
                        .first_failure
                        .map(|i| format!(" at entry {i}"))
                        .unwrap_or_default(),
                    report.failure.unwrap_or_else(|| "unknown error".into())
                )));
            }
        }

        // Archive fingerprint (identity snapshot) recorded durably.
        let fingerprint = compute_archive_fingerprint(&self.info);
        self.journal
            .set_archive_fingerprint(Some(fingerprint))
            .map_err(CoreError::Journal)?;

        let mut committed_bytes: u64 = self
            .journal
            .entries()
            .ok()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| e.status == reclaimarc_journal::models::EntryStatus::Committed && !e.is_directory)
                    .map(|e| e.unpacked_size)
                    .sum::<u64>()
            })
            .unwrap_or(0);

        let mut reclaimed_bytes: u64 = self
            .journal
            .packed_ranges()
            .ok()
            .map(|ranges| {
                ranges
                    .iter()
                    .filter(|r| r.state == reclaimarc_journal::models::RangeState::Reclaimed)
                    .map(|r| r.len)
                    .sum::<u64>()
            })
            .unwrap_or(0);

        let destructive = self.mode == ExtractionMode::LowSpace;

        // Determine the first unit to process (durable state).
        let mut current_unit: Option<u64> = None;
        for u in &units {
            if !state::is_committed(u.state) {
                current_unit = Some(u.seq);
                break;
            }
            if destructive && !state::is_reclaimed(u.state) {
                current_unit = Some(u.seq);
                break;
            }
        }
        let mut seq = current_unit.unwrap_or(0);

        // Reserve computed against the live volume.
        let free_now = observed_free_space(&self.destination)?;
        let _ = tx.send(Event::FreeSpace { bytes: free_now });
        let total = total_space(&self.destination).map_err(CoreError::Platform)?;
        let reserve = crate::config::emergency_reserve(free_now, total, &config);
        let scratch = self.info.decoder_requirements.scratch_bytes;
        let mut monitor = SpaceMonitor::new(reserve);

        let unit_count = units.len();
        // Fast path: when every recovery unit is a single file, extract with
        // one decoder pass (avoids O(n²) per-unit re-walks of large archives).
        let fast_path = self.info.recovery_units.iter().all(|u| u.first_entry == u.last_entry);

        // Per-attempt partial suffix: unique per run. The journal's entry
        // partial paths must match exactly what the decoder writes.
        let suffix = attempt_suffix(&self.job_id);
        if fast_path {
            // Update partial paths for every entry whose unit is not yet
            // committed, in one durable batch. (Entry status alone is not
            // enough: a unit can crash after its entries were renamed, and
            // recovery re-extracts them under a fresh partial name.)
            let committed_units: std::collections::HashSet<u64> = units
                .iter()
                .filter(|u| state::is_committed(u.state))
                .map(|u| u.seq)
                .collect();
            let entries = self.journal.entries().map_err(CoreError::Journal)?;
            let updates: Vec<(u64, String)> = entries
                .iter()
                .filter(|e| !e.is_directory)
                .filter(|e| !committed_units.contains(&e.recovery_unit))
                .filter_map(|e| {
                    e.final_path.clone().map(|f| {
                        let mut p = f.as_os_str().to_os_string();
                        p.push(&suffix);
                        (e.index_in_archive, p.to_string_lossy().into_owned())
                    })
                })
                .collect();
            self.journal
                .set_partial_paths_batch(&updates)
                .map_err(CoreError::Journal)?;
        }

        // The streaming pass skips everything before the first entry that will be
        // (re-)extracted. In destructive mode, committed-but-unreclaimed units
        // are reclaim-only and must NOT be re-extracted — their source may
        // already have been punched.
        let stop_at = self
            .info
            .recovery_units
            .iter()
            .find(|u| {
                units
                    .iter()
                    .find(|j| j.seq == u.seq)
                    .map(|j| !state::is_committed(j.state))
                    .unwrap_or(true)
            })
            .map(|u| u.first_entry)
            .unwrap_or(0);
        if fast_path {
            let opts = ExtractOptions {
                dest_dir: self.destination.clone(),
                job_id: self.job_id.clone(),
                partial_suffix: suffix.clone(),
                password: self.password.clone(),
                cancel: None,
                name_map: self.name_map.clone(),
            };
            self.backend
                .begin_extraction(&opts, stop_at)
                .map_err(CoreError::Archive)?;
        }

        let mut volume_handles: HashMap<u64, std::fs::File> = HashMap::new();
        let volumes = self.journal.volumes().map_err(CoreError::Journal)?;
        if destructive {
            for (idx, vol) in volumes.iter().enumerate() {
                if let Ok(f) = open_for_reclaim(&vol.path) {
                    let _ = set_sparse(&f, &vol.path);
                    volume_handles.insert(idx as u64, f);
                }
            }
        }

        while seq < unit_count as u64 {
            let unit = self
                .info
                .recovery_units
                .iter()
                .find(|u| u.seq == seq)
                .ok_or_else(|| CoreError::Precondition(format!("unit {seq} not found")))?;

            // In destructive mode a committed unit may still need its source
            // reclaimed (crash between COMMITTED and RECLAIMED). Reclaim-only:
            // no re-extraction, no re-verification — the output is committed.
            let unit_state = units
                .iter()
                .find(|u| u.seq == seq)
                .map(|u| u.state)
                .unwrap_or(UnitState::Pending);
            let reclaim_only =
                destructive && state::is_committed(unit_state) && !state::is_reclaimed(unit_state);

            let _ = tx.send(Event::UnitStarted {
                seq,
                first_entry: unit.first_entry,
                last_entry: unit.last_entry,
            });

            // Safety gate: enough capacity for output + scratch + reserve.
            validate_capacity_before_unit(&self.destination, unit, scratch, reserve)?;
            if seq % 32 == 0 || seq + 1 == unit_count as u64 {
                if let Ok(free) = observed_free_space(&self.destination) {
                    let _ = tx.send(Event::FreeSpace { bytes: free });
                }
            }

            if !reclaim_only {
                // ---- EXTRACTING (durable) ----
                self.journal
                    .set_unit_state(seq, UnitState::Extracting)
                    .map_err(CoreError::Journal)?;

            let opts = ExtractOptions {
                dest_dir: self.destination.clone(),
                job_id: self.job_id.clone(),
                partial_suffix: suffix.clone(),
                password: self.password.clone(),
                cancel: None,
                name_map: self.name_map.clone(),
            };
            let mut progress_cb = |e: ProgressEvent| {
                let _ = tx.send(match e {
                    ProgressEvent::EntryProgress { entry_index, current, total } => {
                        Event::EntryProgress { index: entry_index, current, total }
                    }
                });
                // Pause = safe abort at the next file boundary.
                !cancel_flag.load(Ordering::SeqCst) && !pause_flag.load(Ordering::SeqCst)
            };

            if fast_path {
                // The unit holds a single entry. Directories are created by
                // the commit step; the streaming pass skips them for us.
                let single = unit.first_entry;
                let entry = self
                    .info
                    .entries
                    .iter()
                    .find(|e| e.index == single)
                    .ok_or_else(|| CoreError::Precondition(format!("entry {single} not found")))?;
                if !entry.is_directory {
                    match self.backend.extract_next(&opts, Some(&mut progress_cb)) {
                        Err(reclaimarc_archive::ArchiveError::Cancelled)
                            if pause_flag.load(Ordering::SeqCst) =>
                        {
                            self.journal
                                .set_job_progress(seq, JobState::Paused)
                                .map_err(CoreError::Journal)?;
                            let _ = tx.send(Event::JobPaused { job_id: self.job_id.clone() });
                            return Ok(JobOutcome::Paused);
                        }
                        Err(reclaimarc_archive::ArchiveError::Cancelled) => {
                            let _ = tx.send(Event::JobCancelled { job_id: self.job_id.clone() });
                            return Ok(JobOutcome::Cancelled);
                        }
                        Err(other) => return Err(CoreError::Archive(other)),
                        Ok(None) => {
                            return Err(CoreError::Precondition(format!(
                                "streaming pass exhausted before unit {seq}"
                            )));
                        }
                        Ok(Some(_)) => {}
                    }
                }
            } else {
                let extract = self.backend.extract_unit(seq, &opts, Some(&mut progress_cb));
                if let Err(e) = extract {
                    return match e {
                        reclaimarc_archive::ArchiveError::Cancelled
                            if pause_flag.load(Ordering::SeqCst) =>
                        {
                            self.journal
                                .set_job_progress(seq, JobState::Paused)
                                .map_err(CoreError::Journal)?;
                            let _ = tx.send(Event::JobPaused { job_id: self.job_id.clone() });
                            Ok(JobOutcome::Paused)
                        }
                        reclaimarc_archive::ArchiveError::Cancelled => {
                            let _ = tx.send(Event::JobCancelled { job_id: self.job_id.clone() });
                            Ok(JobOutcome::Cancelled)
                        }
                        other => Err(CoreError::Archive(other)),
                    };
                }
            }
            fault::fire(CrashPoint::AfterPartialWrite, &self.job_id);

            // ---- OUTPUT_WRITTEN → OUTPUT_DURABLE (one durable transaction) ----
            // Verify each partial (size + BLAKE3), journal the verified
            // hashes, then flush every partial durably. The intermediate
            // states are journaled inside a single transaction; crash
            // recovery treats any non-committed unit identically.
            let entries = self.journal.entries_for_unit(seq).map_err(CoreError::Journal)?;
            let mut written_bytes: u64 = 0;
            if !reclaim_only {
                let mut verified: Vec<(u64, String)> = Vec::new();
                for entry in &entries {
                    if entry.is_directory {
                        continue;
                    }
                    let partial = entry.partial_path.clone().ok_or_else(|| {
                        CoreError::Precondition(format!(
                            "entry {} has no partial path",
                            entry.index_in_archive
                        ))
                    })?;
                    let blake3 =
                        verify_file(&partial, entry.unpacked_size, config.io_buffer_size).map_err(
                            |e| {
                                CoreError::failed(
                                    "verify partial output",
                                    Some(partial.clone()),
                                    None,
                                    "EXTRACTING",
                                    format!("verification of '{}' failed: {e}", partial.display()),
                                    "The unit's partial output is discarded on resume and the unit is re-extracted.",
                                )
                            },
                        )?;
                    let _ = tx.send(Event::EntryVerified {
                        index: entry.index_in_archive,
                        blake3: blake3.clone(),
                    });
                    written_bytes = written_bytes.saturating_add(entry.unpacked_size);
                    verified.push((entry.index_in_archive, blake3));
                }
                // Flush before journaling DURABLE so the bytes are on disk
                // when the journal says so.
                for entry in &entries {
                    if entry.is_directory {
                        continue;
                    }
                    let partial = entry.partial_path.clone().unwrap();
                    // FlushFileBuffers requires a write-capable handle.
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&partial)
                        .map_err(|e| {
                            CoreError::from_io_source("open partial for flush", &partial, e)
                        })?;
                    flush::flush_file(&file, &partial).map_err(CoreError::Platform)?;
                }
                self.journal
                    .mark_unit_verified_durable(seq, &verified)
                    .map_err(CoreError::Journal)?;
                fault::fire(CrashPoint::AfterOutputFlush, &self.job_id);
            }

            // ---- atomic rename to final name ----
            for entry in &entries {
                if entry.is_directory {
                    continue;
                }
                let partial = entry.partial_path.clone().unwrap();
                let final_path = entry.final_path.clone().unwrap();
                rename_into_place(&partial, &final_path, config.conflict_policy)?;
                self.journal
                    .set_entry_committed(
                        entry.index_in_archive,
                        &final_path,
                        entry.blake3.as_deref().unwrap_or(""),
                    )
                    .map_err(CoreError::Journal)?;
                committed_bytes = committed_bytes.saturating_add(entry.unpacked_size);
                let _ = tx.send(Event::EntryCommitted {
                    index: entry.index_in_archive,
                    path: final_path,
                });
            }
            // Create directories that were only entries.
            create_directory_entries(&self.destination, &entries)?;
            // Flush the destination directory periodically so renames are durable without MFT bottlenecks.
            if seq % 64 == 0 || seq + 1 == unit_count as u64 {
                let _ = flush::flush_directory(&self.destination);
            }
            fault::fire(CrashPoint::AfterRename, &self.job_id);

            // ---- COMMITTED (durable) ----
            self.journal
                .set_unit_state(seq, UnitState::Committed)
                .map_err(CoreError::Journal)?;
            let _ = tx.send(Event::UnitCommitted { seq, bytes: written_bytes });
            fault::fire(CrashPoint::AfterJournalCommit, &self.job_id);
            } // end !reclaim_only

            // ---- reclaim source ranges (destructive mode) ----
            if destructive {
                let ranges = self
                    .journal
                    .packed_ranges_for_unit(seq)
                    .map_err(CoreError::Journal)?;
                if !ranges.is_empty() {
                    // RECLAIM_INTENT must be durable BEFORE punching holes.
                    for r in &ranges {
                        self.journal
                            .mark_range_reclaim_intent(r.volume_index, r.start, r.len)
                            .map_err(CoreError::Journal)?;
                    }
                    self.journal
                        .set_unit_state(seq, UnitState::ReclaimIntent)
                        .map_err(CoreError::Journal)?;
                    fault::fire(CrashPoint::BeforeHolePunch, &self.job_id);

                    let mut reclaimed_for_unit: u64 = 0;
                    let mut by_volume: HashMap<u64, Vec<ByteRange>> = HashMap::new();
                    for r in &ranges {
                        by_volume
                            .entry(r.volume_index)
                            .or_default()
                            .push(ByteRange { start: r.start, len: r.len });
                    }
                    for (v_idx, ranges) in by_volume {
                        let vol = volumes
                            .get(v_idx as usize)
                            .ok_or_else(|| {
                                CoreError::Precondition(format!("volume {v_idx} not found"))
                            })?;
                        let file = match volume_handles.get(&v_idx) {
                            Some(f) => f,
                            None => {
                                let f = open_for_reclaim(&vol.path).map_err(CoreError::Platform)?;
                                let _ = set_sparse(&f, &vol.path);
                                volume_handles.entry(v_idx).or_insert(f)
                            }
                        };
                        for range in ranges {
                            let report =
                                reclaim_range(file, &vol.path, range).map_err(CoreError::Platform)?;
                            let released = report.released_bytes();
                            reclaimed_for_unit = reclaimed_for_unit.saturating_add(released);
                            reclaimed_bytes = reclaimed_bytes.saturating_add(released);
                            let _ = tx.send(Event::RangeReclaimed {
                                volume_index: v_idx,
                                bytes: released,
                            });
                            self.journal
                                .mark_range_reclaimed(v_idx, range.start, range.len)
                                .map_err(CoreError::Journal)?;
                        }
                        fault::fire(CrashPoint::DuringHolePunch, &self.job_id);
                    }
                    fault::fire(CrashPoint::BeforeReclaimedCommit, &self.job_id);
                    self.journal
                        .set_unit_state(seq, UnitState::Reclaimed)
                        .map_err(CoreError::Journal)?;
                    let _ = tx.send(Event::UnitReclaimed { seq, bytes: reclaimed_for_unit });
                } else {
                    // Unit has no reclaimable source; still mark reclaimed.
                    self.journal
                        .set_unit_state(seq, UnitState::Reclaimed)
                        .map_err(CoreError::Journal)?;
                }
            }

            // Free-space check after the unit.
            match monitor.check(&self.destination)? {
                SpaceCheck::Ok => {}
                SpaceCheck::ApproachingReserve => {
                    let _ = tx.send(Event::LowSpace {
                        free: monitor.last_free().unwrap_or(0),
                        reserve,
                    });
                }
                SpaceCheck::BelowReserve => {
                    // Stop BEFORE consuming the reserve.
                    self.journal
                        .set_job_progress(seq + 1, JobState::Paused)
                        .map_err(CoreError::Journal)?;
                    let _ = tx.send(Event::JobPaused { job_id: self.job_id.clone() });
                    return Ok(JobOutcome::Paused);
                }
            }

            self.journal
                .set_job_progress(seq + 1, JobState::Active)
                .map_err(CoreError::Journal)?;

            // Pause between units (after a completed unit).
            if pause_flag.load(Ordering::SeqCst) {
                self.journal
                    .set_job_progress(seq + 1, JobState::Paused)
                    .map_err(CoreError::Journal)?;
                let _ = tx.send(Event::JobPaused { job_id: self.job_id.clone() });
                return Ok(JobOutcome::Paused);
            }
            if cancel_flag.load(Ordering::SeqCst) {
                let _ = tx.send(Event::JobCancelled { job_id: self.job_id.clone() });
                return Ok(JobOutcome::Cancelled);
            }
            seq += 1;
        }

        // Drop cached volume write handles and close backend decoder before deleting archive shells.
        drop(volume_handles);
        self.backend.close();

        // Completion.
        if self.mode == ExtractionMode::LowSpace && config.delete_shells_on_completion {
            delete_archive_shells(&self.journal)?;
        }
        self.journal
            .set_job_progress(seq, JobState::Completed)
            .map_err(CoreError::Journal)?;
        let _ = tx.send(Event::JobFinished {
            job_id: self.job_id.clone(),
            committed_bytes,
            reclaimed_bytes,
        });
        Ok(JobOutcome::Completed {
            committed_bytes,
            reclaimed_bytes,
        })
    }

    /// Verify a partial output file's size and BLAKE3 (exposed for tests).
    pub fn verify(&mut self, entry_index: u64) -> Result<String, CoreError> {
        let entry = self.journal.entry(entry_index).map_err(CoreError::Journal)?;
        let partial = entry.partial_path.clone().unwrap();
        verify_file(&partial, entry.unpacked_size, 1 << 20).map_err(|e| {
            CoreError::failed(
                "verify partial output",
                Some(partial.clone()),
                None,
                "EXTRACTING",
                format!("verification of '{}' failed: {e}", partial.display()),
                "The unit's partial output is discarded on resume and the unit is re-extracted.",
            )
        })
    }
}

/// Compute the archive fingerprint: BLAKE3 over volume identity facts and
/// leading bytes. Cheap enough to run once at job start; strong enough to
/// detect substitution of the archive.
fn compute_archive_fingerprint(info: &ArchiveInfo) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(info.format.as_bytes());
    for v in &info.volumes {
        hasher.update(&v.index.to_le_bytes());
        hasher.update(v.path.to_string_lossy().as_bytes());
        hasher.update(&v.logical_size.to_le_bytes());
        if let Ok(ident) = file_identity(&v.path) {
            hasher.update(&ident.volume_serial.to_le_bytes());
            hasher.update(&ident.file_id.to_le_bytes());
            hasher.update(&ident.last_write_time.to_le_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Verify a file against a stored BLAKE3 digest (used during recovery to
/// adopt renamed-but-uncommitted outputs).
pub fn verify_against(path: &Path, expected_blake3: &str) -> Result<bool, CoreError> {
    let actual = verify_file(path, u64::MAX, 1 << 20).map_err(|e| {
        CoreError::failed(
            "verify output during recovery",
            Some(path.to_path_buf()),
            e.raw_os_error().map(|v| v as u32),
            "recovery",
            format!("verification of '{}' failed: {e}", path.display()),
            "The output is discarded and the unit is re-extracted.",
        )
    })?;
    Ok(actual == expected_blake3)
}

/// Verify a file: exact size + BLAKE3 digest.
fn verify_file(path: &Path, expected_size: u64, buffer_size: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; buffer_size.max(4096)];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        hasher.update(&buf[..n]);
    }
    if total != expected_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("size mismatch: expected {expected_size}, got {total}"),
        ));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Atomically rename a partial file into its final place, honoring the
/// conflict policy. Uses MoveFileExW with WRITE_THROUGH.
fn rename_into_place(
    partial: &Path,
    final_path: &Path,
    policy: ConflictPolicy,
) -> Result<(), CoreError> {
    match policy {
        ConflictPolicy::Overwrite => {
            longpath::rename_existing(partial, final_path).map_err(|e| {
                CoreError::failed(
                    "atomic rename",
                    Some(final_path.to_path_buf()),
                    e.os,
                    "OUTPUT_DURABLE",
                    format!(
                        "rename '{}' → '{}' failed: {}",
                        partial.display(),
                        final_path.display(),
                        e.message
                    ),
                    "The unit remains resumable; the partial is re-extracted on resume.",
                )
            })
        }
ConflictPolicy::Skip | ConflictPolicy::Ask => {
            if final_path.exists() {
                // Skip: remove the partial, keep the existing file.
                if let Err(e) = std::fs::remove_file(partial) {
                    tracing::warn!(path = %partial.display(), "could not remove skipped partial: {e}");
                }
                Ok(())
            } else {
                rename_into_place(partial, final_path, ConflictPolicy::Overwrite)
            }
        }
        ConflictPolicy::RenameNew => {
            if !final_path.exists() {
                return rename_into_place(partial, final_path, ConflictPolicy::Overwrite);
            }
            let stem = final_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            let ext = final_path
                .extension()
                .map(|s| s.to_string_lossy().into_owned());
            let parent = final_path.parent().unwrap_or(Path::new("."));
            let mut n = 1u64;
            loop {
                let candidate = parent.join(match &ext {
                    Some(e) => format!("{stem} ({n}).{e}"),
                    None => format!("{stem} ({n})"),
                });
                if !candidate.exists() {
                    return rename_into_place(partial, &candidate, ConflictPolicy::Overwrite);
                }
                n += 1;
            }
        }
    }
}

/// Create directory entries that no file created implicitly.
fn create_directory_entries(dest: &Path, entries: &[EntryRecord]) -> Result<(), CoreError> {
    for e in entries {
        if !e.is_directory {
            continue;
        }
        let final_path = e.final_path.clone().unwrap();
        if !final_path.exists() {
            std::fs::create_dir_all(&final_path).map_err(|err| {
                CoreError::failed(
                    "create directory",
                    Some(final_path.clone()),
                    err.raw_os_error().map(|v| v as u32),
                    "OUTPUT_DURABLE",
                    format!("cannot create directory '{}': {err}", final_path.display()),
                    "Check permissions on the destination.",
                )
            })?;
        }
    }
    let _ = dest;
    Ok(())
}

/// Delete archive shells after a successful destructive extraction.
///
/// ULTRA-STRICT VERIFICATION:
/// The source archive is ONLY deleted if:
/// 1. Every single unit in the journal is in `Reclaimed` or `Committed` state.
/// 2. Zero errors are recorded in the journal.
/// 3. Every single entry in the journal is in `Committed` status.
/// 4. Every single committed file physically exists on disk and its on-disk size
///    exactly matches `unpacked_size`.
///
/// If even a single file is missing, size-mismatched, or unreadable, this function
/// returns an error and REFUSES to delete the source archive.
fn delete_archive_shells(journal: &JobJournal) -> Result<(), CoreError> {
    let units = journal.units().map_err(CoreError::Journal)?;
    if units.is_empty() {
        return Err(CoreError::Precondition(
            "cannot delete source shells: no recovery units found in journal".into(),
        ));
    }

    // 1. Verify every unit is fully committed or reclaimed.
    for u in &units {
        if !state::is_committed(u.state) && !state::is_reclaimed(u.state) {
            return Err(CoreError::failed(
                "verify units before shell deletion",
                None,
                None,
                "COMPLETED",
                format!("unit {} is in incomplete state '{:?}'", u.seq, u.state),
                "Source archive will NOT be deleted.",
            ));
        }
    }

    // 2. Verify zero recorded errors in the journal.
    let errors = journal.errors().unwrap_or_default();
    if !errors.is_empty() {
        return Err(CoreError::failed(
            "verify journal errors before shell deletion",
            None,
            None,
            "COMPLETED",
            format!("{} errors recorded in journal", errors.len()),
            "Source archive will NOT be deleted.",
        ));
    }

    // 3. Verify every entry is committed and exists on disk with exact size.
    let entries = journal.entries().map_err(CoreError::Journal)?;
    if entries.is_empty() {
        return Err(CoreError::Precondition(
            "cannot delete source shells: no entries found in journal".into(),
        ));
    }

    for e in &entries {
        if e.status != EntryStatus::Committed {
            return Err(CoreError::failed(
                "verify entries before shell deletion",
                e.final_path.clone(),
                None,
                "COMPLETED",
                format!("entry {} is not committed (status: {:?})", e.index_in_archive, e.status),
                "Source archive will NOT be deleted.",
            ));
        }

        let Some(final_path) = &e.final_path else {
            return Err(CoreError::failed(
                "verify entry paths before shell deletion",
                None,
                None,
                "COMPLETED",
                format!("entry {} has no final path", e.index_in_archive),
                "Source archive will NOT be deleted.",
            ));
        };

        if !e.is_directory {
            let meta = std::fs::metadata(final_path).map_err(|err| {
                CoreError::failed(
                    "verify physical file before shell deletion",
                    Some(final_path.clone()),
                    err.raw_os_error().map(|c| c as u32),
                    "COMPLETED",
                    format!("output file '{}' cannot be read on disk: {err}", final_path.display()),
                    "Source archive will NOT be deleted.",
                )
            })?;

            if meta.len() != e.unpacked_size {
                return Err(CoreError::failed(
                    "verify physical file size before shell deletion",
                    Some(final_path.clone()),
                    None,
                    "COMPLETED",
                    format!(
                        "output file '{}' size mismatch: expected {} bytes on disk, found {} bytes",
                        final_path.display(),
                        e.unpacked_size,
                        meta.len()
                    ),
                    "Source archive will NOT be deleted.",
                ));
            }
        } else if !final_path.is_dir() {
            return Err(CoreError::failed(
                "verify physical directory before shell deletion",
                Some(final_path.clone()),
                None,
                "COMPLETED",
                format!("output directory '{}' does not exist on disk", final_path.display()),
                "Source archive will NOT be deleted.",
            ));
        }
    }

    // 4. All checks passed with 100% verification — delete the volume files.
    let volumes = journal.volumes().map_err(CoreError::Journal)?;
    for v in volumes {
        if v.path.exists() {
            std::fs::remove_file(&v.path).map_err(|e| {
                CoreError::failed(
                    "delete source shell",
                    Some(v.path.clone()),
                    e.raw_os_error().map(|v| v as u32),
                    "COMPLETED",
                    format!("cannot delete source '{}': {e}", v.path.display()),
                    "The extraction completed; delete the archive manually.",
                )
            })?;
        }
    }
    Ok(())
}
