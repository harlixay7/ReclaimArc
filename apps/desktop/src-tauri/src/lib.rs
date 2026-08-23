//! ReclaimArc desktop backend: Tauri commands over the shared engine.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use reclaimarc_core::{Engine, EngineConfig, Event, ExtractionMode, JobOutcome, SafetyMode};
use serde::{Deserialize, Serialize};
use tauri::Emitter;

/// A running or paused job in this process.
struct ActiveJob {
    job_id: String,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    #[allow(dead_code)]
    outcome: Arc<Mutex<Option<JobOutcome>>>,
    #[allow(dead_code)]
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ActiveJob {
    fn event_json(e: &Event) -> serde_json::Value {
        use Event::*;
        match e {
            JobStarted { job_id } => serde_json::json!({
                "type": "job-started", "job_id": job_id,
            }),
            Analyzed {
                archive,
                plan_bytes,
            } => serde_json::json!({
                "type": "analyzed", "archive": archive, "plan": plan_bytes,
            }),
            PreTestStarted { bytes_total } => serde_json::json!({
                "type": "pre-test-started", "total": bytes_total,
            }),
            PreTestProgress { current, total } => serde_json::json!({
                "type": "pre-test-progress", "current": current, "total": total,
            }),
            PreTestFinished { ok, bytes_tested } => serde_json::json!({
                "type": "pre-test-finished", "ok": ok, "bytes": bytes_tested,
            }),
            UnitStarted {
                seq,
                first_entry,
                last_entry,
            } => serde_json::json!({
                "type": "unit-started", "seq": seq, "first": first_entry, "last": last_entry,
            }),
            EntryStarted { index, name } => serde_json::json!({
                "type": "entry-started", "index": index, "name": name,
            }),
            EntryProgress {
                index,
                current,
                total,
            } => serde_json::json!({
                "type": "entry-progress", "index": index, "current": current, "total": total,
            }),
            EntryVerified { index, blake3 } => serde_json::json!({
                "type": "entry-verified", "index": index, "blake3": blake3,
            }),
            EntryCommitted { index, path } => serde_json::json!({
                "type": "entry-committed", "index": index, "path": path,
            }),
            UnitCommitted { seq, bytes } => serde_json::json!({
                "type": "unit-committed", "seq": seq, "bytes": bytes,
            }),
            RangeReclaimed {
                volume_index,
                bytes,
            } => serde_json::json!({
                "type": "range-reclaimed", "volume": volume_index, "bytes": bytes,
            }),
            UnitReclaimed { seq, bytes } => serde_json::json!({
                "type": "unit-reclaimed", "seq": seq, "bytes": bytes,
            }),
            FreeSpace { bytes } => serde_json::json!({
                "type": "free-space", "bytes": bytes,
            }),
            JobPaused { .. } => serde_json::json!({ "type": "job-paused" }),
            JobCancelled { .. } => serde_json::json!({ "type": "job-cancelled" }),
            JobFinished {
                job_id,
                committed_bytes,
                reclaimed_bytes,
            } => serde_json::json!({
                "type": "job-finished", "job": job_id,
                "committed": committed_bytes, "reclaimed": reclaimed_bytes,
            }),
            JobFailed {
                operation,
                path,
                os_error,
                message,
                recommended_action,
            } => serde_json::json!({
                "type": "job-failed", "operation": operation, "path": path,
                "os_error": os_error, "message": message, "recommended": recommended_action,
            }),
            EntrySkipped {
                index,
                name,
                reason,
            } => serde_json::json!({
                "type": "entry-skipped", "index": index, "name": name, "reason": reason,
            }),
            LowSpace { free, reserve } => serde_json::json!({
                "type": "low-space", "free": free, "reserve": reserve,
            }),
        }
    }
}

/// Shared application state.
pub struct AppState {
    engine: Mutex<Engine>,
    config: Mutex<EngineConfig>,
    active: Mutex<Option<ActiveJob>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SettingsDto {
    pub safety_mode: String,
    pub conflict_policy: String,
    pub pre_test: bool,
    pub write_manifest: bool,
    pub retain_previous_unit: bool,
    pub delete_shells_on_completion: bool,
    pub log_level: String,
}

impl From<&EngineConfig> for SettingsDto {
    fn from(c: &EngineConfig) -> Self {
        SettingsDto {
            safety_mode: c.safety_mode.as_str().into(),
            conflict_policy: c.conflict_policy.as_str().into(),
            pre_test: c.pre_test,
            write_manifest: c.write_manifest,
            retain_previous_unit: c.retain_previous_unit,
            delete_shells_on_completion: c.delete_shells_on_completion,
            log_level: c.log_level.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct AnalyzeResult {
    pub info: serde_json::Value,
    pub plan: serde_json::Value,
}

#[derive(Serialize)]
pub struct JobListEntry {
    pub job_id: String,
    pub archive: String,
    pub destination: String,
    pub status: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct RecoveryView {
    pub job_id: String,
    pub archive: String,
    pub destination: String,
    pub committed_output_bytes: u64,
    pub source_reclaimed_bytes: u64,
    pub remaining_source_bytes: u64,
    pub last_checkpoint: String,
    pub units: Vec<serde_json::Value>,
    pub errors: Vec<String>,
}

fn err_msg(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Command: analyze an archive for a destination.
///
/// Runs off the main thread: inspect() walks every header twice and would
/// otherwise block the UI event loop (sync Tauri commands run on the main
/// thread), freezing the webview until it finishes.
#[tauri::command(async)]
fn analyze(
    state: tauri::State<'_, AppState>,
    archive: String,
    destination: String,
    password: Option<String>,
) -> Result<AnalyzeResult, String> {
    let archive_path = PathBuf::from(&archive);
    let dest_path = if destination.trim().is_empty() {
        archive_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(&destination)
    };
    let config = state.config.lock().unwrap().clone();
    let engine = Engine::new(config);
    let (info, plan) = engine
        .analyze(&archive_path, &dest_path, password)
        .map_err(err_msg)?;
    let info_json = serde_json::to_value(&info).map_err(err_msg)?;
    let plan_json = serde_json::to_value(&plan).map_err(err_msg)?;
    Ok(AnalyzeResult {
        info: info_json,
        plan: plan_json,
    })
}

/// Command: start a fresh extraction job.
///
/// `start_job` re-inspects the archive (full header walk) and sets up the
/// journal, so this must not run on the main thread either.
#[tauri::command(async)]
fn start_extraction(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    archive: String,
    destination: String,
    low_space: bool,
    password: Option<String>,
) -> Result<String, String> {
    if state.active.lock().unwrap().is_some() {
        return Err("an extraction is already running".into());
    }
    let archive_path = PathBuf::from(&archive);
    let dest_path = if destination.trim().is_empty() {
        archive_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(&destination)
    };
    let config = state.config.lock().unwrap().clone();
    let engine = Engine::new(config);
    let (tx, rx) = mpsc::channel::<Event>();

    // Event forwarder to the webview.
    let app2 = app.clone();
    let forwarder = std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            let _ = app2.emit("sx://event", ActiveJob::event_json(&event));
        }
    });
    let _ = forwarder;

    let mut active_lock = state.active.lock().unwrap();
    if let Some(j) = active_lock.as_ref() {
        if j.outcome.lock().unwrap().is_some() {
            *active_lock = None;
        } else {
            return Err("an extraction is already running".into());
        }
    }
    drop(active_lock);

    let mode = if low_space {
        ExtractionMode::LowSpace
    } else {
        ExtractionMode::Normal
    };
    let (handle, mut job) = engine
        .start_job(&archive_path, &dest_path, mode, password, tx.clone())
        .map_err(err_msg)?;

    let outcome = Arc::new(Mutex::new(None::<JobOutcome>));
    let outcome2 = outcome.clone();
    let pause = handle.pause.clone();
    let cancel = handle.cancel.clone();
    let job_id = handle.job_id.clone();
    let tx2 = tx.clone();

    let worker = std::thread::spawn(move || {
        let result = {
            let mut eng = engine;
            eng.run_job(&mut job, &handle)
        };
        let o = match result {
            Ok(o) => o,
            Err(e) => JobOutcome::Failed {
                failure: reclaimarc_core::FailureInfo::from(&e),
            },
        };
        *outcome2.lock().unwrap() = Some(o);
        let _ = tx2.send(Event::FreeSpace { bytes: 0 });
        drop(tx2);
    });

    *state.active.lock().unwrap() = Some(ActiveJob {
        job_id: job_id.clone(),
        pause,
        cancel,
        outcome,
        worker: Some(worker),
    });
    Ok(job_id)
}

/// Command: resume the interrupted job belonging to an archive.
#[tauri::command(async)]
fn resume_extraction(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    archive: String,
    password: Option<String>,
) -> Result<String, String> {
    if state.active.lock().unwrap().is_some() {
        return Err("an extraction is already running".into());
    }
    // Locate the journal beside the archive.
    let journal_path = find_journal(&PathBuf::from(&archive))?;
    let config = state.config.lock().unwrap().clone();
    let engine = Engine::new(config);
    let (tx, rx) = mpsc::channel::<Event>();
    let app2 = app.clone();
    let _forwarder = std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            let _ = app2.emit("sx://event", ActiveJob::event_json(&event));
        }
    });
    let (handle, mut job) = engine
        .resume_job(&journal_path, password, tx.clone())
        .map_err(err_msg)?;
    let outcome = Arc::new(Mutex::new(None::<JobOutcome>));
    let outcome2 = outcome.clone();
    let job_id = handle.job_id.clone();
    let pause = handle.pause.clone();
    let cancel = handle.cancel.clone();
    let tx2 = tx.clone();
    let worker = std::thread::spawn(move || {
        let result = {
            let mut eng = engine;
            eng.run_job(&mut job, &handle)
        };
        let o = match result {
            Ok(o) => o,
            Err(e) => JobOutcome::Failed {
                failure: reclaimarc_core::FailureInfo::from(&e),
            },
        };
        *outcome2.lock().unwrap() = Some(o);
        drop(tx2);
    });
    *state.active.lock().unwrap() = Some(ActiveJob {
        job_id: job_id.clone(),
        pause,
        cancel,
        outcome,
        worker: Some(worker),
    });
    Ok(job_id)
}

/// Command: pause the running job (safely abort the current unit).
#[tauri::command]
fn pause_job(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let active = state.active.lock().unwrap();
    if let Some(j) = active.as_ref() {
        j.pause.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// Command: stop the running job safely.
#[tauri::command]
fn stop_job(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let active = state.active.lock().unwrap();
    if let Some(j) = active.as_ref() {
        j.pause.store(true, Ordering::SeqCst);
        j.cancel.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// Command: cancel the running job (resumable; reclaimed data is gone).
#[tauri::command]
fn cancel_job(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let active = state.active.lock().unwrap();
    if let Some(j) = active.as_ref() {
        j.cancel.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// Command: the current job's id (empty when idle).
#[tauri::command]
fn current_job(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let mut active = state.active.lock().unwrap();
    if let Some(j) = active.as_ref() {
        if j.outcome.lock().unwrap().is_some() {
            *active = None;
            return Ok(None);
        }
    }
    Ok(active.as_ref().map(|j| j.job_id.clone()))
}

/// Command: list interrupted jobs from the registry.
#[tauri::command(async)]
fn list_jobs() -> Result<Vec<JobListEntry>, String> {
    let jobs = reclaimarc_core::discover_interrupted_jobs().map_err(err_msg)?;
    Ok(jobs
        .into_iter()
        .map(|j| JobListEntry {
            job_id: j.job_id,
            archive: j.archive.to_string_lossy().into_owned(),
            destination: j.destination.to_string_lossy().into_owned(),
            status: j.status,
            updated_at: j.updated_at,
        })
        .collect())
}

/// Command: recovery view for an archive.
#[tauri::command(async)]
fn recovery_view(archive: String) -> Result<RecoveryView, String> {
    let journal_path = find_journal(&PathBuf::from(&archive))?;
    let journal = reclaimarc_journal::JobJournal::open(&journal_path).map_err(err_msg)?;
    let s = reclaimarc_core::summarize(&journal).map_err(err_msg)?;
    Ok(RecoveryView {
        job_id: s.job_id,
        archive: s.archive.to_string_lossy().into_owned(),
        destination: s.destination.to_string_lossy().into_owned(),
        committed_output_bytes: s.committed_output_bytes,
        source_reclaimed_bytes: s.source_reclaimed_bytes,
        remaining_source_bytes: s.remaining_source_bytes,
        last_checkpoint: s.last_checkpoint,
        units: s
            .units
            .iter()
            .map(|(seq, st)| serde_json::json!({ "seq": seq, "state": format!("{st:?}") }))
            .collect(),
        errors: s.errors,
    })
}

/// Command: abandon a job.
#[tauri::command(async)]
fn abandon_job(archive: String) -> Result<(), String> {
    let journal_path = find_journal(&PathBuf::from(&archive))?;
    let journal = reclaimarc_journal::JobJournal::open(&journal_path).map_err(err_msg)?;
    let job_id = journal.job_meta().map_err(err_msg)?.job_id;
    reclaimarc_core::abandon_job(&journal_path, &job_id).map_err(err_msg)
}

fn settings_file_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ReclaimArc").join("settings.json")
}

fn load_persisted_config() -> EngineConfig {
    let p = settings_file_path();
    if let Ok(bytes) = std::fs::read(&p) {
        if let Ok(config) = serde_json::from_slice::<EngineConfig>(&bytes) {
            return config;
        }
    }
    EngineConfig::default()
}

fn persist_config(config: &EngineConfig) -> Result<(), String> {
    let p = settings_file_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(err_msg)?;
    }
    let json = serde_json::to_string_pretty(config).map_err(err_msg)?;
    std::fs::write(&p, json).map_err(err_msg)?;
    Ok(())
}

/// Command: get settings.
#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Result<SettingsDto, String> {
    let config = state.config.lock().unwrap();
    Ok(SettingsDto::from(&*config))
}

/// Command: save settings.
#[tauri::command]
fn set_settings(state: tauri::State<'_, AppState>, settings: SettingsDto) -> Result<(), String> {
    let mut config = state.config.lock().unwrap();
    config.safety_mode =
        SafetyMode::from_str(&settings.safety_mode).unwrap_or(SafetyMode::Balanced);
    config.conflict_policy = match settings.conflict_policy.as_str() {
        "skip" => reclaimarc_core::ConflictPolicy::Skip,
        "rename-new" => reclaimarc_core::ConflictPolicy::RenameNew,
        "overwrite" => reclaimarc_core::ConflictPolicy::Overwrite,
        _ => reclaimarc_core::ConflictPolicy::Ask,
    };
    config.pre_test = settings.pre_test;
    config.write_manifest = settings.write_manifest;
    config.retain_previous_unit = settings.retain_previous_unit;
    config.delete_shells_on_completion = settings.delete_shells_on_completion;
    config.log_level = settings.log_level;
    *state.engine.lock().unwrap() = Engine::new(config.clone());
    persist_config(&config)?;
    Ok(())
}

/// Command: open any folder in Explorer.
#[tauri::command(async)]
fn open_folder(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if p.exists() {
        open_in_explorer(&p)
    } else {
        Err(format!("Folder does not exist: {}", path))
    }
}

/// Command: open the logs directory.
#[tauri::command]
fn open_logs_dir() -> Result<(), String> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).map_err(err_msg)?;
    open_in_explorer(&dir)
}

/// Command: read the last N log lines (redacted).
#[tauri::command]
fn read_logs(last: usize) -> Result<String, String> {
    let dir = log_dir();
    let path = dir.join("reclaimarc.log");
    if !path.exists() {
        return Ok("(no log file yet)".into());
    }
    let content = std::fs::read_to_string(&path).map_err(err_msg)?;
    let lines: Vec<&str> = content.lines().collect();
    let tail: Vec<&str> = lines.iter().rev().take(last.max(50)).copied().collect();
    let mut out = tail.clone();
    out.reverse();
    Ok(out.join("\n"))
}

fn log_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ReclaimArc")
        .join("logs")
}

fn open_in_explorer(dir: &PathBuf) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open explorer: {e}"))
}

/// Find the newest journal beside an archive.
fn find_journal(archive: &std::path::Path) -> Result<PathBuf, String> {
    let state = archive
        .parent()
        .map(|p| p.join(".reclaimarc"))
        .ok_or_else(|| "archive has no parent directory".to_string())?;
    if !state.exists() {
        return Err(format!(
            "no ReclaimArc state found beside '{}'",
            archive.display()
        ));
    }
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&state)
        .map_err(err_msg)?
        .flatten()
        .map(|e| e.path().join("job.db"))
        .filter(|p| p.exists())
        .filter_map(|p| {
            std::fs::metadata(&p)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, p))
        })
        .collect();
    candidates.sort_by_key(|(t, _)| *t);
    candidates
        .last()
        .map(|(_, p)| p.clone())
        .ok_or_else(|| format!("no journal found beside '{}'", archive.display()))
}

/// Initialise logging (structured, redacted).
pub fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("reclaimarc.log"));
    if let Ok(file) = file {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_ansi(false)
            .with_writer(file)
            .try_init();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_logging();
    let initial_config = load_persisted_config();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            engine: Mutex::new(Engine::new(initial_config.clone())),
            config: Mutex::new(initial_config),
            active: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            analyze,
            start_extraction,
            resume_extraction,
            pause_job,
            stop_job,
            cancel_job,
            current_job,
            list_jobs,
            recovery_view,
            abandon_job,
            get_settings,
            set_settings,
            open_folder,
            open_logs_dir,
            read_logs,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ReclaimArc");
}

#[cfg(test)]
mod tests {
    use super::*;
    use reclaimarc_archive::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};

    #[test]
    fn test_analyze_with_empty_destination_defaults_to_parent() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![FixtureFile::new("sample.txt", b"hello analyze world")];
        let paths = write_rar(
            dir.path(),
            "test_analyze",
            &files,
            &FixtureOptions::default(),
        )
        .unwrap();
        let archive_path = paths[0].to_str().unwrap().to_string();

        let state = AppState {
            engine: Mutex::new(Engine::new(EngineConfig::default())),
            config: Mutex::new(EngineConfig::default()),
            active: Mutex::new(None),
        };

        // Test fallback resolution logic
        let archive = PathBuf::from(&archive_path);
        let dest = archive
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let engine = state.engine.lock().unwrap();
        let (info, plan) = engine.analyze(&archive, &dest, None).unwrap();

        assert_eq!(info.entries.len(), 1);
        assert_eq!(info.entries[0].name, "sample.txt");
        assert!(plan.free_now > 0);
    }
}
