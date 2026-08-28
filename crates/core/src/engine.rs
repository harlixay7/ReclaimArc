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
use reclaimarc_platform::sparse::{
    open_for_reclaim, query_allocated_ranges, reclaim_range, set_sparse, ByteRange,
};
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
    Completed {
        committed_bytes: u64,
        reclaimed_bytes: u64,
    },
    Paused,
    Cancelled,
    Failed {
        failure: crate::error::FailureInfo,
    },
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

    /// Analyze an archive for a destination: full inspection + space plan with real volume measurements.
    pub fn analyze(
        &self,
        archive: &Path,
        destination: &Path,
        password: Option<String>,
    ) -> Result<(ArchiveInfo, crate::planner::SpacePlan), CoreError> {
        let dest_str = destination.to_string_lossy();
        let destination_norm = if dest_str.len() == 2 && dest_str.ends_with(':') {
            PathBuf::from(format!("{}\\", dest_str))
        } else {
            destination.to_path_buf()
        };
        let destination = destination_norm.as_path();

        let mut backend = reclaimarc_archive::backend_for(archive)?;
        let info = backend.inspect(&OpenOptions { password })?;
        let free = observed_free_space(destination)?;
        let total = total_space(destination).map_err(CoreError::Platform)?;
        let cluster = reclaimarc_platform::fs::cluster_size(destination).ok();
        let mut allocated_by_vol = std::collections::HashMap::new();
        for v in &info.volumes {
            if let Ok(handle) = reclaimarc_platform::sparse::open_for_query(&v.path) {
                if let Ok(ranges) = reclaimarc_platform::sparse::query_allocated_ranges(
                    &handle,
                    &v.path,
                    0,
                    v.logical_size,
                ) {
                    allocated_by_vol.insert(v.index, ranges);
                }
            }
        }
        let plan = crate::planner::plan_with_measurements(
            &info,
            free,
            total,
            cluster,
            Some(&allocated_by_vol),
            &self.config,
        )?;
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
    ) -> Result<(JobHandle, ExtractionJob), CoreError> {
        let dest_str = destination.to_string_lossy();
        let destination_norm = if dest_str.len() == 2 && dest_str.ends_with(':') {
            PathBuf::from(format!("{}\\", dest_str))
        } else {
            destination.to_path_buf()
        };
        let destination = destination_norm.as_path();

        let job_id = uuid::Uuid::new_v4().to_string();
        let mut backend = reclaimarc_archive::backend_for(archive)?;
        let info = backend.inspect(&OpenOptions {
            password: password.clone(),
        })?;

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
        let cluster = reclaimarc_platform::fs::cluster_size(destination).ok();
        let mut allocated_by_vol = std::collections::HashMap::new();
        for v in &info.volumes {
            if let Ok(handle) = reclaimarc_platform::sparse::open_for_query(&v.path) {
                if let Ok(ranges) = reclaimarc_platform::sparse::query_allocated_ranges(
                    &handle,
                    &v.path,
                    0,
                    v.logical_size,
                ) {
                    allocated_by_vol.insert(v.index, ranges);
                }
            }
        }
        let plan = crate::planner::plan_with_measurements(
            &info,
            free,
            total,
            cluster,
            Some(&allocated_by_vol),
            &self.config,
        )?;

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
                    return Err(CoreError::Infeasible(plan.reason.clone().unwrap_or_else(
                        || "Progressive extraction is not possible on this volume.".into(),
                    )));
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
            let mut v =
                serde_json::to_value(&self.config).unwrap_or_else(|_| serde_json::json!({}));
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

        let (struct_digests, range_digests) =
            compute_source_manifest(&info.volumes, &info.recovery_units)?;

        // Volumes with durable identity snapshots and structural digests.
        let mut volumes = Vec::new();
        for (i, v) in info.volumes.iter().enumerate() {
            let ident = file_identity(&v.path).map_err(CoreError::Platform)?;
            let handle = reclaimarc_platform::sparse::open_for_query(&v.path)
                .map_err(CoreError::Platform)?;
            let allocated =
                allocated_size_from_handle(&handle, &v.path).map_err(CoreError::Platform)?;
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
                structural_digest: struct_digests.get(i).cloned(),
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
                let existed_before_job = final_path.exists();
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
                    actual_committed_path: None,
                    existed_before_job,
                    expected_digest: None,
                    is_redirection: e.redirection.is_some(),
                    redirection_kind: e.redirection.as_ref().map(|r| format!("{:?}", r.kind)),
                }
            })
            .collect();
        journal.add_entries(&entries).map_err(CoreError::Journal)?;

        // Packed ranges with cryptographic BLAKE3 hashes.
        let mut ranges = Vec::new();
        for u in &info.recovery_units {
            for r in &u.packed_ranges {
                let digest = range_digests
                    .get(&(r.volume_index, r.start, r.len))
                    .cloned();
                ranges.push(PackedRangeRecord {
                    volume_index: r.volume_index,
                    start: r.start,
                    len: r.len,
                    state: RangeState::Active,
                    recovery_unit: Some(u.seq),
                    physically_released_bytes: 0,
                    blake3_digest: digest,
                });
            }
        }
        journal
            .add_packed_ranges(&ranges)
            .map_err(CoreError::Journal)?;

        // Registry mirror in application data.
        let mut registry =
            Registry::open(&Registry::default_app_data_dir()).map_err(CoreError::Journal)?;
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
            ExtractionJob {
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

    /// Run a job to completion (or pause/cancel/failure).
    pub fn run_job(
        &mut self,
        job: &mut ExtractionJob,
        handle: &JobHandle,
    ) -> Result<JobOutcome, CoreError> {
        job.run(self.config.clone(), &handle.pause, &handle.cancel)
    }

    /// Resume an interrupted job from its journal.
    ///
    /// Runs the full recovery preparation (identity validation, partial
    /// reconciliation, reclaim reconciliation) and returns a runnable job.
    pub fn resume_job(
        &self,
        journal_path: &Path,
        password: Option<String>,
        tx: Sender<Event>,
    ) -> Result<(JobHandle, ExtractionJob), CoreError> {
        let journal = crate::recovery::prepare_resume(journal_path, None)?;
        let meta = journal.job_meta().map_err(CoreError::Journal)?;

        if meta.job_state == JobState::Completed {
            let job_id = meta.job_id.clone();
            let _ = tx.send(Event::JobFinished {
                job_id: job_id.clone(),
                committed_bytes: 0,
                reclaimed_bytes: 0,
            });
            let info = ArchiveInfo {
                format: "completed".into(),
                packed_size: 0,
                unpacked_size: 0,
                solid_archive: false,
                encrypted_headers: false,
                volumes: vec![],
                entries: vec![],
                recovery_units: vec![],
                capability: reclaimarc_archive::CapabilityMatrix {
                    format: "completed".into(),
                    supports_test_integrity: false,
                    restartable_units: false,
                    progressive_reclaim: false,
                    supports_encryption: false,
                    supports_multipart: false,
                    notes: vec![],
                },
                decoder_requirements: reclaimarc_archive::DecoderRequirements {
                    scratch_bytes: 0,
                    redecodes_prefix: false,
                },
            };
            let backend = Box::new(reclaimarc_archive::ZipBackend::new(&meta.archive_path));
            return Ok((
                JobHandle {
                    job_id: job_id.clone(),
                    pause: Arc::new(AtomicBool::new(false)),
                    cancel: Arc::new(AtomicBool::new(false)),
                },
                ExtractionJob {
                    job_id,
                    archive: meta.archive_path.clone(),
                    destination: meta.destination.clone(),
                    journal,
                    info,
                    backend,
                    name_map: HashMap::new(),
                    mode: ExtractionMode::Normal,
                    password,
                    tx,
                },
            ));
        }

        let mut backend = reclaimarc_archive::backend_for(&meta.archive_path)?;
        let info = backend
            .inspect(&OpenOptions {
                password: password.clone(),
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
            .and_then(|v| {
                v.get("mode")
                    .and_then(|m| m.as_str())
                    .and_then(|m| match m {
                        "normal" => Some(ExtractionMode::Normal),
                        "lowspace" => Some(ExtractionMode::LowSpace),
                        _ => None,
                    })
            })
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
            ExtractionJob {
                job_id,
                archive: meta.archive_path.clone(),
                destination: meta.destination.clone(),
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
    fn validate_entries(
        &self,
        info: &ArchiveInfo,
    ) -> Result<(Vec<SafeEntry>, HashMap<u64, String>), CoreError> {
        if (info.entries.len() as u64) > self.config.max_entry_count {
            return Err(CoreError::Precondition(format!(
                "Archive contains {} entries, exceeding the configured safety limit of {}.",
                info.entries.len(),
                self.config.max_entry_count
            )));
        }

        if info.unpacked_size > self.config.max_total_unpacked_bytes {
            return Err(CoreError::Precondition(format!(
                "Archive declared total unpacked size of {} bytes exceeds safety limit of {} bytes.",
                info.unpacked_size,
                self.config.max_total_unpacked_bytes
            )));
        }

        for e in &info.entries {
            if e.unpacked_size > self.config.max_single_file_bytes {
                return Err(CoreError::Precondition(format!(
                    "Entry '{}' declared unpacked size of {} bytes exceeds single-file safety limit of {} bytes.",
                    e.name,
                    e.unpacked_size,
                    self.config.max_single_file_bytes
                )));
            }
        }

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
            if self.config.conflict_policy == ConflictPolicy::RenameNew {
                let mut seen_lower: HashMap<String, usize> = HashMap::new();
                for (idx, entry) in safe_entries.iter_mut().enumerate() {
                    if entry.is_directory {
                        continue;
                    }
                    let key = entry.relative().to_lowercase();
                    if let std::collections::hash_map::Entry::Vacant(e) = seen_lower.entry(key) {
                        e.insert(idx);
                    } else {
                        let last_comp = entry.components.last().cloned().unwrap_or_default();
                        let (stem, ext) = match last_comp.rfind('.') {
                            Some(dot) if dot > 0 => (
                                last_comp[..dot].to_string(),
                                Some(last_comp[dot..].to_string()),
                            ),
                            _ => (last_comp.clone(), None),
                        };
                        let mut counter = 1u64;
                        loop {
                            let candidate_name = match &ext {
                                Some(e) => format!("{stem} (case-collision-{counter}){e}"),
                                None => format!("{stem} (case-collision-{counter})"),
                            };
                            let mut candidate_comps = entry.components.clone();
                            if let Some(last) = candidate_comps.last_mut() {
                                *last = candidate_name;
                            }
                            let candidate_rel = candidate_comps.join("\\");
                            let candidate_key = candidate_rel.to_lowercase();
                            if let std::collections::hash_map::Entry::Vacant(e) =
                                seen_lower.entry(candidate_key)
                            {
                                entry.components = candidate_comps;
                                e.insert(idx);
                                name_map.insert(idx as u64, entry.relative());
                                break;
                            }
                            counter += 1;
                        }
                    }
                }
            } else {
                let (a, b) = collisions[0];
                return Err(CoreError::Precondition(format!(
                    "archive contains case-insensitive filename collisions: '{}' and '{}' — Windows would overwrite one with the other. Use RenameNew conflict policy to auto-rename colliding entries.",
                    safe_entries[a].original, safe_entries[b].original
                )));
            }
        }
        Ok((safe_entries, name_map))
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
pub struct ExtractionJob {
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

impl std::fmt::Debug for ExtractionJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractionJob")
            .field("job_id", &self.job_id)
            .field("archive", &self.archive)
            .field("destination", &self.destination)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl ExtractionJob {
    /// Run the job. Blocking; call from a worker thread.
    pub fn run(
        &mut self,
        config: EngineConfig,
        pause_flag: &AtomicBool,
        cancel_flag: &AtomicBool,
    ) -> Result<JobOutcome, CoreError> {
        let meta = self.journal.job_meta().map_err(CoreError::Journal)?;
        if meta.job_state == JobState::Completed {
            return Ok(JobOutcome::Completed {
                committed_bytes: 0,
                reclaimed_bytes: 0,
            });
        }

        let tx = self.tx.clone();

        // Determine units state and whether this is a resume of an in-progress job.
        let units = self.journal.units().map_err(CoreError::Journal)?;
        let is_resuming = units
            .iter()
            .any(|u| state::is_committed(u.state) || state::is_reclaimed(u.state));

        // Pre-test for destructive extraction (mandatory for Low-Space mode on initial start, before any source holes are punched).
        #[cfg(feature = "test-hooks")]
        let skip_pre_test_test_hook = std::env::var("RECLAIMARC_TEST_SKIP_PRE_TEST")
            .ok()
            .as_deref()
            == Some("1");
        #[cfg(not(feature = "test-hooks"))]
        let skip_pre_test_test_hook = false;
        if self.mode == ExtractionMode::LowSpace && !is_resuming && !skip_pre_test_test_hook {
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

        // Retirement Proof Validation Gate: Enforce that every destructive PackedRange has a matching backend RetirementProof.
        if self.mode == ExtractionMode::LowSpace {
            let proofs = self.backend.retirement_proofs();
            let proof_set: std::collections::HashSet<(u64, u64, u64, u64)> = proofs
                .iter()
                .map(|p| (p.volume_index, p.start, p.len, p.unit_seq))
                .collect();
            for u in &self.info.recovery_units {
                for r in &u.packed_ranges {
                    if !proof_set.contains(&(r.volume_index, r.start, r.len, u.seq)) {
                        return Err(CoreError::Infeasible(format!(
                            "Recovery unit {} packed range [{}, +{}) has no matching backend retirement proof. Destructive extraction aborted.",
                            u.seq, r.start, r.len
                        )));
                    }
                }
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
                    .filter(|e| {
                        e.status == reclaimarc_journal::models::EntryStatus::Committed
                            && !e.is_directory
                    })
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
                    .map(|r| r.physically_released_bytes)
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
        let fast_path = self
            .info
            .recovery_units
            .iter()
            .all(|u| u.first_entry == u.last_entry);

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

        let mut volume_handles: HashMap<u64, std::fs::File> = HashMap::new();
        let volumes = self.journal.volumes().map_err(CoreError::Journal)?;
        let journal_ranges = self.journal.packed_ranges().map_err(CoreError::Journal)?;

        if destructive {
            // Reconcile entries between live backend info and journaled plan
            let journal_entries = self.journal.entries().map_err(CoreError::Journal)?;
            if self.info.entries.len() != journal_entries.len() {
                return Err(CoreError::failed(
                    "source structural reconciliation",
                    Some(self.archive.clone()),
                    None,
                    "setup",
                    format!(
                        "Archive entry count ({}) does not match journaled entry count ({})",
                        self.info.entries.len(),
                        journal_entries.len()
                    ),
                    "Aborting destructive extraction immediately: archive structure has changed.",
                ));
            }
            for (e_info, e_jour) in self.info.entries.iter().zip(journal_entries.iter()) {
                if e_info.name != e_jour.name
                    || e_info.unpacked_size != e_jour.unpacked_size
                    || e_info.is_directory != e_jour.is_directory
                    || e_info.crc32 != e_jour.crc32
                {
                    return Err(CoreError::failed(
                        "source structural reconciliation",
                        Some(self.archive.clone()),
                        None,
                        "setup",
                        format!(
                            "Archive entry '{}' does not match journaled plan for entry '{}'",
                            e_info.name, e_jour.name
                        ),
                        "Aborting destructive extraction immediately: archive structure has changed.",
                    ));
                }
            }

            for (idx, vol) in volumes.iter().enumerate() {
                let f = open_for_reclaim(&vol.path).map_err(CoreError::Platform)?;
                let ident = file_identity(&vol.path).map_err(CoreError::Platform)?;
                if let Some(expected_id) = &vol.identity {
                    if ident.volume_serial != expected_id.volume_serial
                        || ident.file_id != expected_id.file_id
                        || ident.file_size != expected_id.file_size
                    {
                        return Err(CoreError::failed(
                            "source volume identity verification",
                            Some(vol.path.clone()),
                            None,
                            "setup",
                            format!(
                                "Source volume '{}' identity changed before extraction (serial {} vs {}, id {} vs {}, size {} vs {})",
                                vol.path.display(),
                                ident.volume_serial, expected_id.volume_serial,
                                ident.file_id, expected_id.file_id,
                                ident.file_size, expected_id.file_size,
                            ),
                            "Aborting destructive extraction immediately to prevent data loss.",
                        ));
                    }
                }

                if let Some(expected_struct_digest) = &vol.structural_digest {
                    let mut vol_ranges: Vec<(u64, u64)> = journal_ranges
                        .iter()
                        .filter(|r| r.volume_index as usize == idx)
                        .map(|r| (r.start, r.len))
                        .collect();
                    vol_ranges.sort_by_key(|&(start, _)| start);
                    let live_struct_hash =
                        compute_volume_structural_digest(&vol.path, vol.logical_size, &vol_ranges)?;
                    if &live_struct_hash != expected_struct_digest {
                        return Err(CoreError::failed(
                            "source volume structural verification",
                            Some(vol.path.clone()),
                            None,
                            "setup",
                            format!(
                                "Source volume '{}' structural metadata changed after planning (expected BLAKE3 {}, live {})",
                                vol.path.display(),
                                expected_struct_digest,
                                live_struct_hash
                            ),
                            "Aborting destructive extraction immediately: archive structure does not match journaled plan.",
                        ));
                    }
                }

                set_sparse(&f, &vol.path).map_err(CoreError::Platform)?;
                volume_handles.insert(idx as u64, f);
            }
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
                max_compression_ratio: Some(config.max_compression_ratio),
            };
            self.backend
                .begin_extraction(&opts, stop_at)
                .map_err(CoreError::Archive)?;
        }

        let mut cumulative_extracted_bytes = 0u64;
        let mut last_space_check_bytes = 0u64;
        let mut last_space_check_time = std::time::Instant::now();

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

            // If the unit is already fully reclaimed, skip it immediately.
            if state::is_reclaimed(unit_state) {
                seq += 1;
                continue;
            }

            let reclaim_only =
                destructive && state::is_committed(unit_state) && !state::is_reclaimed(unit_state);

            let _ = tx.send(Event::UnitStarted {
                seq,
                first_entry: unit.first_entry,
                last_entry: unit.last_entry,
            });
            if let Some(entry) = self.info.entries.get(unit.first_entry as usize) {
                if !entry.is_directory {
                    let _ = tx.send(Event::EntryStarted {
                        index: entry.index,
                        name: entry.name.clone(),
                    });
                }
            }

            // Safety gate: enough capacity for output + scratch + reserve.
            validate_capacity_before_unit(&self.destination, unit, scratch, reserve)?;
            if seq.is_multiple_of(32) || seq + 1 == unit_count as u64 {
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
                    max_compression_ratio: Some(config.max_compression_ratio),
                };
                let mut last_entry_index: Option<u64> = None;
                let mut last_current = 0u64;
                let mut space_check_failed = false;
                let mut space_check_error: Option<String> = None;
                let mut progress_cb = |e: ProgressEvent| {
                    match e {
                        ProgressEvent::EntryProgress {
                            entry_index,
                            current,
                            total,
                        } => {
                            let is_new_entry = last_entry_index != Some(entry_index);
                            let delta = match last_entry_index {
                                Some(last_idx) if last_idx == entry_index => {
                                    let d = current.saturating_sub(last_current);
                                    last_current = current;
                                    d
                                }
                                _ => {
                                    last_entry_index = Some(entry_index);
                                    last_current = current;
                                    current
                                }
                            };
                            if is_new_entry {
                                let name = self
                                    .info
                                    .entries
                                    .get(entry_index as usize)
                                    .map(|e| e.name.clone())
                                    .unwrap_or_default();
                                let _ = tx.send(Event::EntryStarted {
                                    index: entry_index,
                                    name,
                                });
                            }
                            cumulative_extracted_bytes =
                                cumulative_extracted_bytes.saturating_add(delta);

                            if destructive {
                                let bytes_since_check = cumulative_extracted_bytes
                                    .saturating_sub(last_space_check_bytes);
                                let time_since_check = last_space_check_time.elapsed();
                                if bytes_since_check >= 10 * 1024 * 1024
                                    || time_since_check >= std::time::Duration::from_millis(500)
                                {
                                    last_space_check_bytes = cumulative_extracted_bytes;
                                    last_space_check_time = std::time::Instant::now();
                                    match monitor.check(&self.destination) {
                                        Ok(SpaceCheck::BelowReserve) => {
                                            space_check_failed = true;
                                            return false;
                                        }
                                        Ok(SpaceCheck::Ok | SpaceCheck::ApproachingReserve) => {}
                                        Err(e) => {
                                            space_check_error =
                                                Some(format!("Free space probe failed: {e}"));
                                            space_check_failed = true;
                                            return false;
                                        }
                                    }
                                }
                            }
                            let _ = tx.send(Event::EntryProgress {
                                index: entry_index,
                                current,
                                total,
                            });
                        }
                    }
                    if space_check_failed {
                        return false;
                    }
                    // Pause = safe abort at the next file boundary.
                    !cancel_flag.load(Ordering::SeqCst) && !pause_flag.load(Ordering::SeqCst)
                };

                let entries = self
                    .journal
                    .entries_for_unit(seq)
                    .map_err(CoreError::Journal)?;

                // Perform ancestry and reparse validation BEFORE allowing the decoder to write partial outputs
                for entry in &entries {
                    let entry_info = self
                        .info
                        .entries
                        .iter()
                        .find(|e| e.index == entry.index_in_archive)
                        .ok_or_else(|| {
                            CoreError::Precondition(format!(
                                "entry {} not found in archive info",
                                entry.index_in_archive
                            ))
                        })?;
                    if !entry.is_directory && entry_info.redirection.is_none() {
                        if let Some(final_path) = &entry.final_path {
                            crate::paths::ensure_no_reparse_ancestors(
                                final_path,
                                &self.destination,
                            )?;
                        }
                    } else if entry_info.redirection.is_some() {
                        self.journal
                            .set_entry_status(entry.index_in_archive, EntryStatus::Skipped)
                            .map_err(CoreError::Journal)?;
                    }
                }

                // Pre-unit cryptographic verification: verify BLAKE3 of all active source packed ranges for this unit
                let unit_ranges = self
                    .journal
                    .packed_ranges_for_unit(seq)
                    .map_err(CoreError::Journal)?;
                for r in &unit_ranges {
                    if r.state == RangeState::Active {
                        if let Some(expected_blake3) = &r.blake3_digest {
                            let vol = volumes.get(r.volume_index as usize).ok_or_else(|| {
                                CoreError::Precondition(format!(
                                    "volume {} not found",
                                    r.volume_index
                                ))
                            })?;
                            verify_range_digest(&vol.path, r.start, r.len, expected_blake3)?;
                        }
                    }
                }

                if fast_path {
                    // The unit holds a single entry.
                    let single = unit.first_entry;
                    let entry_info = self
                        .info
                        .entries
                        .iter()
                        .find(|e| e.index == single)
                        .ok_or_else(|| {
                            CoreError::Precondition(format!("entry {single} not found"))
                        })?;
                    if !entry_info.is_directory && entry_info.redirection.is_none() {
                        match self.backend.extract_next(&opts, Some(&mut progress_cb)) {
                            Err(reclaimarc_archive::ArchiveError::Cancelled)
                                if space_check_failed =>
                            {
                                if let Some(err_msg) = space_check_error {
                                    return Err(CoreError::Infeasible(err_msg));
                                }
                                return Err(CoreError::Infeasible(
                                    "Free space dropped below emergency reserve during extraction. Aborting unit safely before commitment."
                                        .into(),
                                ));
                            }
                            Err(reclaimarc_archive::ArchiveError::Cancelled)
                                if pause_flag.load(Ordering::SeqCst) =>
                            {
                                self.journal
                                    .set_job_progress(seq, JobState::Paused)
                                    .map_err(CoreError::Journal)?;
                                let _ = tx.send(Event::JobPaused {
                                    job_id: self.job_id.clone(),
                                });
                                return Ok(JobOutcome::Paused);
                            }
                            Err(reclaimarc_archive::ArchiveError::Cancelled) => {
                                let _ = tx.send(Event::JobCancelled {
                                    job_id: self.job_id.clone(),
                                });
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
                    } else {
                        // Directory or redirection: advance backend decoder by 1 entry
                        let _ = self.backend.extract_next(&opts, Some(&mut progress_cb))?;
                    }
                } else {
                    let extract = self
                        .backend
                        .extract_unit(seq, &opts, Some(&mut progress_cb));
                    if let Err(e) = extract {
                        if space_check_failed {
                            if let Some(err_msg) = space_check_error {
                                return Err(CoreError::Infeasible(err_msg));
                            }
                            return Err(CoreError::Infeasible(
                                "Free space dropped below emergency reserve during extraction. Aborting unit safely before commitment."
                                    .into(),
                            ));
                        }
                        return match e {
                            reclaimarc_archive::ArchiveError::Cancelled
                                if pause_flag.load(Ordering::SeqCst) =>
                            {
                                self.journal
                                    .set_job_progress(seq, JobState::Paused)
                                    .map_err(CoreError::Journal)?;
                                let _ = tx.send(Event::JobPaused {
                                    job_id: self.job_id.clone(),
                                });
                                Ok(JobOutcome::Paused)
                            }
                            reclaimarc_archive::ArchiveError::Cancelled => {
                                let _ = tx.send(Event::JobCancelled {
                                    job_id: self.job_id.clone(),
                                });
                                Ok(JobOutcome::Cancelled)
                            }
                            other => Err(CoreError::Archive(other)),
                        };
                    }
                }
                if space_check_failed {
                    if let Some(err_msg) = space_check_error {
                        return Err(CoreError::Infeasible(err_msg));
                    }
                    return Err(CoreError::Infeasible(
                        "Free space dropped below emergency reserve during extraction. Aborting unit safely before commitment."
                            .into(),
                    ));
                }
                fault::fire(CrashPoint::AfterPartialWrite, &self.job_id);

                // ---- OUTPUT_WRITTEN → OUTPUT_DURABLE (one durable transaction) ----
                // Verify each partial (size + BLAKE3), journal the verified
                // hashes, then flush every partial durably. The intermediate
                // states are journaled inside a single transaction; crash
                // recovery treats any non-committed unit identically.
                let entries = self
                    .journal
                    .entries_for_unit(seq)
                    .map_err(CoreError::Journal)?;
                let mut written_bytes: u64 = 0;
                if !reclaim_only {
                    let mut verified: Vec<(u64, String)> = Vec::new();
                    for entry in &entries {
                        if entry.is_directory || entry.status == EntryStatus::Skipped {
                            continue;
                        }
                        let partial = entry.partial_path.clone().ok_or_else(|| {
                            CoreError::Precondition(format!(
                                "entry {} has no partial path",
                                entry.index_in_archive
                            ))
                        })?;
                        let blake3 =
                        verify_file(&partial, Some(entry.unpacked_size), config.io_buffer_size).map_err(
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
                        if entry.is_directory || entry.status == EntryStatus::Skipped {
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
                let entries = self
                    .journal
                    .entries_for_unit(seq)
                    .map_err(CoreError::Journal)?;
                for entry in &entries {
                    if entry.is_directory || entry.status == EntryStatus::Skipped {
                        continue;
                    }
                    let partial = entry.partial_path.clone().unwrap();
                    let final_path = entry.final_path.clone().unwrap();
                    let blake3_hex = entry.blake3.as_deref().unwrap_or("");
                    let outcome = rename_into_place(
                        &self.destination,
                        &partial,
                        &final_path,
                        entry.unpacked_size,
                        blake3_hex,
                        config.conflict_policy,
                        destructive,
                    )?;
                    let committed_path = match outcome {
                        CommitOutcome::Committed(p) | CommitOutcome::ReusedExisting(p) => p,
                    };
                    self.journal
                        .set_entry_committed(entry.index_in_archive, &committed_path, blake3_hex)
                        .map_err(CoreError::Journal)?;
                    committed_bytes = committed_bytes.saturating_add(entry.unpacked_size);
                    let _ = tx.send(Event::EntryCommitted {
                        index: entry.index_in_archive,
                        path: committed_path,
                    });
                }
                // Create directories that were only entries.
                create_directory_entries(&self.destination, &entries, &mut self.journal)?;
                // Flush the destination directory periodically so renames are durable without MFT bottlenecks.
                if seq.is_multiple_of(64) || seq + 1 == unit_count as u64 {
                    let _ = flush::flush_directory(&self.destination);
                }
                fault::fire(CrashPoint::AfterRename, &self.job_id);

                // ---- COMMITTED (durable) ----
                self.journal
                    .transition_unit_state(seq, UnitState::OutputDurable, UnitState::Committed)
                    .map_err(CoreError::Journal)?;
                let _ = tx.send(Event::UnitCommitted {
                    seq,
                    bytes: written_bytes,
                });
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
                    let current_u = self.journal.unit(seq).map_err(CoreError::Journal)?;
                    if current_u.state == UnitState::Committed {
                        self.journal
                            .transition_unit_state(
                                seq,
                                UnitState::Committed,
                                UnitState::ReclaimIntent,
                            )
                            .map_err(CoreError::Journal)?;
                    }
                    fault::fire(CrashPoint::BeforeHolePunch, &self.job_id);

                    let mut reclaimed_for_unit: u64 = 0;
                    let mut by_volume: HashMap<u64, Vec<ByteRange>> = HashMap::new();
                    for r in &ranges {
                        by_volume
                            .entry(r.volume_index)
                            .or_default()
                            .push(ByteRange {
                                start: r.start,
                                len: r.len,
                            });
                    }
                    let mut processed_ranges = 0usize;

                    for (v_idx, ranges) in by_volume {
                        let vol = volumes.get(v_idx as usize).ok_or_else(|| {
                            CoreError::Precondition(format!("volume {v_idx} not found"))
                        })?;
                        let ident = file_identity(&vol.path).map_err(CoreError::Platform)?;
                        if let Some(expected_id) = &vol.identity {
                            if ident.volume_serial != expected_id.volume_serial
                                || ident.file_id != expected_id.file_id
                                || ident.file_size != expected_id.file_size
                            {
                                return Err(CoreError::failed(
                                    "reclaim source verification",
                                    Some(vol.path.clone()),
                                    None,
                                    "reclaim",
                                    format!("Source volume '{}' identity changed before reclamation", vol.path.display()),
                                    "Aborting destructive extraction immediately to prevent data corruption.",
                                ));
                            }
                        }
                        let file = match volume_handles.get(&v_idx) {
                            Some(f) => f,
                            None => {
                                let f = open_for_reclaim(&vol.path).map_err(CoreError::Platform)?;
                                set_sparse(&f, &vol.path).map_err(CoreError::Platform)?;
                                volume_handles.entry(v_idx).or_insert(f)
                            }
                        };
                        for range in ranges {
                            // Verify intact packed range digest immediately before initial physical reclamation
                            if !reclaim_only {
                                if let Some(packed_record) = self
                                    .journal
                                    .packed_ranges_for_unit(seq)
                                    .map_err(CoreError::Journal)?
                                    .into_iter()
                                    .find(|pr| {
                                        pr.volume_index == v_idx
                                            && pr.start == range.start
                                            && pr.len == range.len
                                    })
                                {
                                    if let Some(expected_blake3) = &packed_record.blake3_digest {
                                        verify_range_digest(
                                            &vol.path,
                                            range.start,
                                            range.len,
                                            expected_blake3,
                                        )?;
                                    }
                                }
                            }

                            let report = reclaim_range(file, &vol.path, range)
                                .map_err(CoreError::Platform)?;
                            let released = report.released_bytes();
                            reclaimed_for_unit = reclaimed_for_unit.saturating_add(released);
                            reclaimed_bytes = reclaimed_bytes.saturating_add(released);
                            fault::fire(CrashPoint::AfterPhysicalHolePunch, &self.job_id);

                            let current_alloc =
                                query_allocated_ranges(file, &vol.path, range.start, range.len)
                                    .map_err(CoreError::Platform)?;
                            let currently_allocated: u64 =
                                current_alloc.iter().map(|r| r.len).sum();
                            let verified_released =
                                range.len.saturating_sub(currently_allocated).min(range.len);

                            let outcome_state = if current_alloc.is_empty() {
                                RangeState::Reclaimed
                            } else if verified_released > 0 {
                                RangeState::Partial
                            } else {
                                RangeState::Active
                            };
                            self.journal
                                .mark_range_outcome(
                                    v_idx,
                                    range.start,
                                    range.len,
                                    outcome_state,
                                    verified_released,
                                )
                                .map_err(CoreError::Journal)?;

                            processed_ranges += 1;
                            if processed_ranges == 1 {
                                fault::fire(CrashPoint::DuringHolePunch, &self.job_id);
                            }
                        }
                    }
                    fault::fire(CrashPoint::BeforeReclaimedCommit, &self.job_id);
                    let updated_ranges = self
                        .journal
                        .packed_ranges_for_unit(seq)
                        .map_err(CoreError::Journal)?;
                    if updated_ranges
                        .iter()
                        .all(|r| r.state == RangeState::Reclaimed)
                    {
                        self.journal
                            .transition_unit_state(
                                seq,
                                UnitState::ReclaimIntent,
                                UnitState::Reclaimed,
                            )
                            .map_err(CoreError::Journal)?;
                    }
                    let _ = tx.send(Event::UnitReclaimed {
                        seq,
                        bytes: reclaimed_for_unit,
                    });
                } else {
                    // Unit has no reclaimable source; transition through intent to reclaimed.
                    let current_u = self.journal.unit(seq).map_err(CoreError::Journal)?;
                    if current_u.state == UnitState::Committed {
                        self.journal
                            .transition_unit_state(
                                seq,
                                UnitState::Committed,
                                UnitState::ReclaimIntent,
                            )
                            .map_err(CoreError::Journal)?;
                    }
                    let current_u = self.journal.unit(seq).map_err(CoreError::Journal)?;
                    if current_u.state == UnitState::ReclaimIntent {
                        self.journal
                            .transition_unit_state(
                                seq,
                                UnitState::ReclaimIntent,
                                UnitState::Reclaimed,
                            )
                            .map_err(CoreError::Journal)?;
                    }
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
                    let _ = tx.send(Event::JobPaused {
                        job_id: self.job_id.clone(),
                    });
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
                let _ = tx.send(Event::JobPaused {
                    job_id: self.job_id.clone(),
                });
                return Ok(JobOutcome::Paused);
            }
            if cancel_flag.load(Ordering::SeqCst) {
                let _ = tx.send(Event::JobCancelled {
                    job_id: self.job_id.clone(),
                });
                return Ok(JobOutcome::Cancelled);
            }
            seq += 1;
        }

        // Drop cached volume write handles and close backend decoder before deleting archive shells.
        drop(volume_handles);
        self.backend.close();

        // Completion.
        if config.delete_shells_on_completion {
            self.journal
                .set_job_state(JobState::Finalizing)
                .map_err(CoreError::Journal)?;
            fault::fire(CrashPoint::BeforeShellDeletion, &self.job_id);
            delete_archive_shells(&self.journal, &self.job_id)?;
            fault::fire(CrashPoint::AfterShellDeletionBeforeCompleted, &self.job_id);
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
        let entry = self
            .journal
            .entry(entry_index)
            .map_err(CoreError::Journal)?;
        let partial = entry.partial_path.clone().unwrap();
        verify_file(&partial, Some(entry.unpacked_size), 1 << 20).map_err(|e| {
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

/// Verify a file against a stored BLAKE3 digest and optional expected size.
/// Used during recovery to adopt renamed-but-uncommitted outputs, and for Low-Space skip verification.
pub fn verify_against(
    path: &Path,
    expected_size: Option<u64>,
    expected_blake3: &str,
) -> Result<bool, CoreError> {
    let actual = verify_file(path, expected_size, 1 << 20).map_err(|e| {
        CoreError::failed(
            "verify output",
            Some(path.to_path_buf()),
            e.raw_os_error().map(|v| v as u32),
            "verification",
            format!("verification of '{}' failed: {e}", path.display()),
            "The output is discarded and the unit is re-extracted.",
        )
    })?;
    Ok(actual == expected_blake3)
}

/// Verify a file: exact size (if specified) + BLAKE3 digest.
pub fn verify_file(
    path: &Path,
    expected_size: Option<u64>,
    buffer_size: usize,
) -> std::io::Result<String> {
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
    if let Some(expected) = expected_size {
        if total != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("size mismatch: expected {expected}, got {total}"),
            ));
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Authoritative outcome of committing an entry into the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Newly written and atomically moved into place.
    Committed(PathBuf),
    /// Reused pre-existing destination file after proving exact size + BLAKE3 hash equivalence.
    ReusedExisting(PathBuf),
}

/// Atomically rename a partial file into its final place, honoring the conflict policy.
///
/// Invariants enforced:
/// - Overwrite: journals the exact committed destination.
/// - RenameNew: returns and journals the actual unique generated path.
/// - Skip: in Low-Space mode, satisfies an entry ONLY IF the existing destination is
///   independently proven byte-equivalent to the verified extraction (exact size + BLAKE3).
///   Otherwise returns an error without destroying source bytes.
/// - Ask: fails closed with an explicit decision-required error before commitment.
fn rename_into_place(
    dest_root: &Path,
    partial: &Path,
    final_path: &Path,
    expected_size: u64,
    expected_blake3: &str,
    policy: ConflictPolicy,
    is_low_space: bool,
) -> Result<CommitOutcome, CoreError> {
    crate::paths::ensure_no_reparse_ancestors(final_path, dest_root)?;
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
            })?;
            Ok(CommitOutcome::Committed(final_path.to_path_buf()))
        }
        ConflictPolicy::RenameNew => {
            if !final_path.exists() {
                return rename_into_place(
                    dest_root,
                    partial,
                    final_path,
                    expected_size,
                    expected_blake3,
                    ConflictPolicy::Overwrite,
                    is_low_space,
                );
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
                    return rename_into_place(
                        dest_root,
                        partial,
                        &candidate,
                        expected_size,
                        expected_blake3,
                        ConflictPolicy::Overwrite,
                        is_low_space,
                    );
                }
                n += 1;
            }
        }
        ConflictPolicy::Skip => {
            if final_path.exists() {
                if is_low_space {
                    // Low-Space mode: Skip is permitted ONLY IF the existing file is
                    // byte-identical to the verified extraction (exact size + BLAKE3).
                    match verify_against(final_path, Some(expected_size), expected_blake3) {
                        Ok(true) => {
                            // Proven byte-identical. Delete the partial and reuse existing.
                            let _ = longpath::remove_file_existing(partial);
                            Ok(CommitOutcome::ReusedExisting(final_path.to_path_buf()))
                        }
                        _ => {
                            Err(CoreError::Precondition(format!(
                                "Existing destination file '{}' differs from archive entry; refusing to destroy source in Low-Space mode",
                                final_path.display()
                            )))
                        }
                    }
                } else {
                    // Normal mode: safe to skip (source will be preserved).
                    let _ = longpath::remove_file_existing(partial);
                    Ok(CommitOutcome::ReusedExisting(final_path.to_path_buf()))
                }
            } else {
                rename_into_place(
                    dest_root,
                    partial,
                    final_path,
                    expected_size,
                    expected_blake3,
                    ConflictPolicy::Overwrite,
                    is_low_space,
                )
            }
        }
        ConflictPolicy::Ask => {
            if final_path.exists() {
                Err(CoreError::Precondition(format!(
                    "Destination file '{}' already exists: interactive conflict decision required before commitment",
                    final_path.display()
                )))
            } else {
                rename_into_place(
                    dest_root,
                    partial,
                    final_path,
                    expected_size,
                    expected_blake3,
                    ConflictPolicy::Overwrite,
                    is_low_space,
                )
            }
        }
    }
}

/// Create directory entries that no file created implicitly.
fn create_directory_entries(
    dest: &Path,
    entries: &[EntryRecord],
    journal: &mut JobJournal,
) -> Result<(), CoreError> {
    for e in entries {
        if !e.is_directory {
            continue;
        }
        let final_path = e.final_path.clone().unwrap();
        crate::paths::ensure_no_reparse_ancestors(&final_path, dest)?;
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
        journal
            .set_entry_status(e.index_in_archive, EntryStatus::Committed)
            .map_err(CoreError::Journal)?;
    }
    Ok(())
}

/// Delete archive shells after a successful destructive extraction.
///
/// Final source-shell deletion preconditions:
/// The source archive is ONLY deleted if:
/// 1. Every single unit in the journal is in `Reclaimed` or `Committed` state.
/// 2. Zero errors are recorded in the journal.
/// 3. Every single entry in the journal is either:
///    - `Committed` (physically exists on disk and its size matches `unpacked_size`, and digest matches);
///    - OR `Skipped` if and only if it is an authorized redirection policy skip (`is_redirection == true`),
///      and no rogue file or link was created on disk.
///
/// If even a single file is missing, size-mismatched, unreadable, or in an unexpected skip state,
/// this function returns an error and refuses to delete the source archive.
pub fn delete_archive_shells(journal: &JobJournal, job_id: &str) -> Result<(), CoreError> {
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

    // 2. Verify zero recorded errors in the journal (fails closed on journal read failure).
    let errors = journal.errors().map_err(CoreError::Journal)?;
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

    // 3. Verify entry dispositions and physical filesystem state.
    let entries = journal.entries().map_err(CoreError::Journal)?;
    if entries.is_empty() {
        return Err(CoreError::Precondition(
            "cannot delete source shells: no entries found in journal".into(),
        ));
    }

    for e in &entries {
        match e.status {
            EntryStatus::Committed => {
                if e.is_directory {
                    let Some(final_path) = &e.final_path else {
                        return Err(CoreError::failed(
                            "verify entry paths before shell deletion",
                            None,
                            None,
                            "COMPLETED",
                            format!("directory entry {} has no final path", e.index_in_archive),
                            "Source archive will NOT be deleted.",
                        ));
                    };
                    if !final_path.is_dir() {
                        return Err(CoreError::failed(
                            "verify physical directory before shell deletion",
                            Some(final_path.clone()),
                            None,
                            "COMPLETED",
                            format!(
                                "output directory '{}' does not exist on disk",
                                final_path.display()
                            ),
                            "Source archive will NOT be deleted.",
                        ));
                    }
                } else {
                    let target_path = e
                        .actual_committed_path
                        .as_ref()
                        .or(e.final_path.as_ref())
                        .ok_or_else(|| {
                            CoreError::failed(
                                "verify entry paths before shell deletion",
                                None,
                                None,
                                "COMPLETED",
                                format!("entry {} has no final/committed path", e.index_in_archive),
                                "Source archive will NOT be deleted.",
                            )
                        })?;

                    let live_blake3 = verify_file(target_path, Some(e.unpacked_size), 64 * 1024).map_err(|err| {
                        CoreError::failed(
                            "verify physical file before shell deletion",
                            Some(target_path.clone()),
                            err.raw_os_error().map(|c| c as u32),
                            "COMPLETED",
                            format!(
                                "output file '{}' verification failed (missing or size mismatch): {err}",
                                target_path.display()
                            ),
                            "Source archive will NOT be deleted.",
                        )
                    })?;

                    if let Some(expected_digest) =
                        e.expected_digest.as_deref().or(e.blake3.as_deref())
                    {
                        if live_blake3 != expected_digest {
                            return Err(CoreError::failed(
                                "verify physical file digest before shell deletion",
                                Some(target_path.clone()),
                                None,
                                "COMPLETED",
                                format!(
                                    "output file '{}' hash mismatch: expected BLAKE3 {}, computed {}",
                                    target_path.display(),
                                    expected_digest,
                                    live_blake3
                                ),
                                "Source archive will NOT be deleted.",
                            ));
                        }
                    }
                }
            }
            EntryStatus::Skipped => {
                // Authorized policy skip: must be a known redirection/link entry
                if !e.is_redirection {
                    return Err(CoreError::failed(
                        "verify entry disposition before shell deletion",
                        e.final_path.clone(),
                        None,
                        "COMPLETED",
                        format!(
                            "entry {} was skipped without an authorized redirection policy disposition",
                            e.index_in_archive
                        ),
                        "Source archive will NOT be deleted.",
                    ));
                }
                // Verify no rogue file or symlink was created on disk for the skipped link
                if let Some(final_path) = &e.final_path {
                    if final_path.exists() || final_path.is_symlink() {
                        return Err(CoreError::failed(
                            "verify skipped redirection on disk before shell deletion",
                            Some(final_path.clone()),
                            None,
                            "COMPLETED",
                            format!(
                                "skipped redirection '{}' unexpectedly exists on disk",
                                final_path.display()
                            ),
                            "Source archive will NOT be deleted.",
                        ));
                    }
                }
            }
            other => {
                return Err(CoreError::failed(
                    "verify entries before shell deletion",
                    e.final_path.clone(),
                    None,
                    "COMPLETED",
                    format!(
                        "entry {} is in uncommitted/incomplete status '{:?}'",
                        e.index_in_archive, other
                    ),
                    "Source archive will NOT be deleted.",
                ));
            }
        }
    }

    // 4. All checks passed with verified completion — delete the volume files idempotently.
    let volumes = journal.volumes().map_err(CoreError::Journal)?;
    for (v_idx, v) in volumes.iter().enumerate() {
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
            if v_idx == 0 && volumes.len() > 1 {
                fault::fire(CrashPoint::DuringMultipartShellDeletion, job_id);
            }
        }
    }
    Ok(())
}

/// Verification helpers for Source Content Manifest
pub fn verify_range_digest(
    path: &Path,
    start: u64,
    len: u64,
    expected_blake3: &str,
) -> Result<(), CoreError> {
    if len == 0 {
        return Ok(());
    }
    let mut file = std::fs::File::open(path).map_err(|e| {
        CoreError::failed(
            "open source file for digest verification",
            Some(path.to_path_buf()),
            None,
            "verification",
            format!("cannot open '{}': {e}", path.display()),
            "Ensure the source file exists and is accessible.",
        )
    })?;
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start)).map_err(|e| {
        CoreError::failed(
            "seek source file for digest verification",
            Some(path.to_path_buf()),
            None,
            "verification",
            format!("cannot seek to offset {start} in '{}': {e}", path.display()),
            "Ensure the source file is not truncated.",
        )
    })?;

    let mut hasher = blake3::Hasher::new();
    let mut remaining = len;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let to_read = (remaining as usize).min(buf.len());
        let n = file.read(&mut buf[..to_read]).map_err(|e| {
            CoreError::failed(
                "read source file for digest verification",
                Some(path.to_path_buf()),
                None,
                "verification",
                format!("read error in '{}': {e}", path.display()),
                "Ensure source storage is healthy.",
            )
        })?;
        if n == 0 {
            return Err(CoreError::failed(
                "read source file for digest verification",
                Some(path.to_path_buf()),
                None,
                "verification",
                format!(
                    "unexpected EOF reading source range [{}..{}] in '{}'",
                    start,
                    start + len,
                    path.display()
                ),
                "Source file is smaller than expected.",
            ));
        }
        hasher.update(&buf[..n]);
        remaining = remaining.saturating_sub(n as u64);
    }

    let actual = hasher.finalize().to_hex().to_string();
    if actual != expected_blake3 {
        return Err(CoreError::failed(
            "verify packed range BLAKE3 digest",
            Some(path.to_path_buf()),
            None,
            "verification",
            format!(
                "source packed range [{}..{}] in '{}' has been modified or corrupted (expected BLAKE3 {}, computed {})",
                start,
                start + len,
                path.display(),
                expected_blake3,
                actual
            ),
            "The archive source data was changed. Aborting extraction to prevent data loss.",
        ));
    }
    Ok(())
}

pub fn compute_volume_structural_digest(
    path: &Path,
    vol_len: u64,
    ranges: &[(u64, u64)],
) -> Result<String, CoreError> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        CoreError::failed(
            "open volume for structural digest",
            Some(path.to_path_buf()),
            None,
            "manifest",
            format!("cannot open volume '{}': {e}", path.display()),
            "Ensure the source volume is accessible.",
        )
    })?;
    use std::io::{Read, Seek, SeekFrom};

    let mut hasher = blake3::Hasher::new();
    let mut current_offset: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    for &(r_start, r_len) in ranges {
        if r_start > current_offset {
            let seg_start = current_offset;
            let seg_len = r_start - current_offset;
            hasher.update(&seg_start.to_le_bytes());
            hasher.update(&seg_len.to_le_bytes());

            file.seek(SeekFrom::Start(seg_start)).map_err(|e| {
                CoreError::failed(
                    "seek volume for structural digest",
                    Some(path.to_path_buf()),
                    None,
                    "manifest",
                    format!("seek error in '{}': {e}", path.display()),
                    "Ensure volume is not truncated.",
                )
            })?;
            let mut rem = seg_len;
            while rem > 0 {
                let to_read = (rem as usize).min(buf.len());
                let n = file.read(&mut buf[..to_read]).map_err(|e| {
                    CoreError::failed(
                        "read structural data from volume",
                        Some(path.to_path_buf()),
                        None,
                        "manifest",
                        format!("read error in '{}': {e}", path.display()),
                        "Ensure storage is healthy.",
                    )
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                rem = rem.saturating_sub(n as u64);
            }
        }
        current_offset = r_start.saturating_add(r_len);
    }

    if current_offset < vol_len {
        let seg_start = current_offset;
        let seg_len = vol_len - current_offset;
        hasher.update(&seg_start.to_le_bytes());
        hasher.update(&seg_len.to_le_bytes());

        file.seek(SeekFrom::Start(seg_start)).map_err(|e| {
            CoreError::failed(
                "seek volume tail for structural digest",
                Some(path.to_path_buf()),
                None,
                "manifest",
                format!("seek error in '{}': {e}", path.display()),
                "Ensure volume is not truncated.",
            )
        })?;
        let mut rem = seg_len;
        while rem > 0 {
            let to_read = (rem as usize).min(buf.len());
            let n = file.read(&mut buf[..to_read]).map_err(|e| {
                CoreError::failed(
                    "read structural tail from volume",
                    Some(path.to_path_buf()),
                    None,
                    "manifest",
                    format!("read error in '{}': {e}", path.display()),
                    "Ensure storage is healthy.",
                )
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            rem = rem.saturating_sub(n as u64);
        }
    }

    Ok(hasher.finalize().to_hex().to_string())
}

pub type SourceManifest = (Vec<String>, HashMap<(u64, u64, u64), String>);

pub fn compute_source_manifest(
    volumes: &[reclaimarc_archive::model::VolumeInfo],
    units: &[reclaimarc_archive::model::RecoveryUnit],
) -> Result<SourceManifest, CoreError> {
    let mut struct_digests = Vec::new();
    let mut range_digests = HashMap::new();

    for (v_idx, vol) in volumes.iter().enumerate() {
        let mut vol_ranges: Vec<(u64, u64)> = Vec::new();
        for u in units {
            for r in &u.packed_ranges {
                if r.volume_index as usize == v_idx {
                    vol_ranges.push((r.start, r.len));
                }
            }
        }
        vol_ranges.sort_by_key(|&(start, _)| start);

        let struct_hash =
            compute_volume_structural_digest(&vol.path, vol.logical_size, &vol_ranges)?;
        struct_digests.push(struct_hash);

        // Compute BLAKE3 for each packed range
        let mut file = std::fs::File::open(&vol.path).map_err(|e| {
            CoreError::failed(
                "open volume for range manifest",
                Some(vol.path.clone()),
                None,
                "manifest",
                format!("cannot open '{}': {e}", vol.path.display()),
                "Ensure volume is accessible.",
            )
        })?;
        use std::io::{Read, Seek, SeekFrom};
        let mut buf = vec![0u8; 64 * 1024];

        for &(r_start, r_len) in &vol_ranges {
            file.seek(SeekFrom::Start(r_start)).map_err(|e| {
                CoreError::failed(
                    "seek volume for range manifest",
                    Some(vol.path.clone()),
                    None,
                    "manifest",
                    format!("seek error in '{}': {e}", vol.path.display()),
                    "Ensure volume is accessible.",
                )
            })?;
            let mut hasher = blake3::Hasher::new();
            let mut rem = r_len;
            while rem > 0 {
                let to_read = (rem as usize).min(buf.len());
                let n = file.read(&mut buf[..to_read]).map_err(|e| {
                    CoreError::failed(
                        "read range from volume",
                        Some(vol.path.clone()),
                        None,
                        "manifest",
                        format!("read error in '{}': {e}", vol.path.display()),
                        "Ensure volume is healthy.",
                    )
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                rem = rem.saturating_sub(n as u64);
            }
            range_digests.insert(
                (v_idx as u64, r_start, r_len),
                hasher.finalize().to_hex().to_string(),
            );
        }
    }

    Ok((struct_digests, range_digests))
}
