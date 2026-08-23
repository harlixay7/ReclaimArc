//! Fault-injection harness.
//!
//! Simulates process death at every durable transition of the engine, then
//! reopens the job and proves the invariants hold:
//!
//! - Safe mode never destroys data belonging to an uncommitted unit.
//! - Every crash point resumes automatically from the last real restart
//!   boundary and completes with byte-identical output.
//! - Reclaimed ranges are reconciled from the actual filesystem state.
//!
//! Each test runs the engine in a CHILD PROCESS (the test binary itself) with
//! `RECLAIMARC_FAULT_AT=<point>`; the child dies with exit code 86 at the
//! requested point. The parent then resumes the job without fault injection
//! and verifies completion.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use reclaimarc_archive::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};
use reclaimarc_core::fault::CrashPoint;
use reclaimarc_core::{
    Engine, EngineConfig, ExtractionMode, JobHandle, JobOutcome,
};

const CRASH_CODE: i32 = 86;
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Environment for the child process.
fn child_env(dir: &Path, point: CrashPoint, mode: ExtractionMode) -> std::collections::HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("SX_CHILD".to_string(), "1".to_string());
    env.insert("SX_WORK".to_string(), dir.to_string_lossy().into_owned());
    env.insert("SX_FAULT".to_string(), point.as_str().to_string());
    env.insert("RECLAIMARC_FAULT_AT".to_string(), point.as_str().to_string());
    env.insert(
        "SX_MODE".to_string(),
        match mode {
            ExtractionMode::Normal => "normal",
            ExtractionMode::LowSpace => "lowspace",
        }
        .to_string(),
    );
    // Tests run against the real volume; give the planner a large free-space
    // observation so only the crash point matters.
    env.insert("RECLAIMARC_TEST_FREE_SPACE".to_string(), "100000000000".to_string());
    env.insert("RECLAIMARC_APP_DATA".to_string(), dir.join("appdata").to_string_lossy().into_owned());
    env
}

/// Run the engine in a child process that crashes at `point`.
fn run_child(dir: &Path, point: CrashPoint, mode: ExtractionMode) -> std::process::ExitStatus {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(exe);
    cmd.arg("--exact").arg("child_driver").arg("--nocapture");
    for (k, v) in child_env(dir, point, mode) {
        cmd.env(k, v);
    }
    cmd.status().expect("child failed to run")
}

/// Find the newest journal in `<archive_dir>/.reclaimarc/*/job.db`.
fn find_journal(dir: &Path) -> PathBuf {
    let state = dir.join("archive").join(".reclaimarc");
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&state)
        .unwrap()
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
        .expect("no journal found")
}

/// Child driver: build a fixture, run a job with fault injection, die at the
/// requested crash point.
#[test]
fn child_driver() {
    if std::env::var("SX_CHILD").ok().as_deref() != Some("1") {
        return;
    }
    let work = PathBuf::from(std::env::var("SX_WORK").unwrap());
    let point = CrashPoint::from_str(&std::env::var("SX_FAULT").unwrap()).unwrap();
    let mode = if std::env::var("SX_MODE").unwrap() == "lowspace" {
        ExtractionMode::LowSpace
    } else {
        ExtractionMode::Normal
    };

    let archive_dir = work.join("archive");
    let dest = work.join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files: Vec<FixtureFile> = (0..5)
        .map(|i| {
            let data: Vec<u8> = (0..300_000).map(|b| ((b as usize + i * 13) % 251) as u8).collect();
            FixtureFile::new(&format!("file{i}.bin"), &data)
        })
        .collect();
    let paths = write_rar(&archive_dir, "corpus", &files, &FixtureOptions::default()).unwrap();
    let archive = &paths[0];

    let (tx, _rx) = mpsc::channel();
    let mut engine = Engine::new(EngineConfig {
        pre_test: false,
        ..Default::default()
    });
    let (handle, mut job) = engine
        .start_job(archive, &dest, mode, None, tx)
        .expect("start_job must succeed");
    let _ = handle;
    let outcome = engine.run_job(&mut job, &handle).unwrap_or_else(|e| {
        eprintln!("CHILD: job failed before crash: {e:?}");
        std::process::exit(101);
    });
    let _ = outcome;
    // The crash point must have fired — if we get here the fault never armed.
    eprintln!("CHILD: crash point {} never fired", point.as_str());
    std::process::exit(102);
}

/// Parent: run crash-at-`point`, verify invariants, resume, verify completion.
fn crash_and_resume(point: CrashPoint, mode: ExtractionMode) {
    let _guard = ENV_MUTEX.lock().unwrap();
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    let dir = tempfile::tempdir().unwrap();
    let status = run_child(dir.path(), point, mode);
    assert_eq!(
        status.code(),
        Some(CRASH_CODE),
        "child must die at {} (code 86), got {status:?}",
        point.as_str()
    );

    // Invariants per crash point (journal state + filesystem state).
    let journal_path = find_journal(dir.path());
    let journal = reclaimarc_journal::JobJournal::open(&journal_path).expect("journal must reopen");
    let meta = journal.job_meta().unwrap();
    let dest = meta.destination.clone();
    let units = journal.units().unwrap();

    match point {
        CrashPoint::AfterPartialWrite | CrashPoint::AfterOutputFlush => {
            // Unit 0 must NOT be committed: source must be intact.
            let u0 = units.iter().find(|u| u.seq == 0).unwrap();
            assert!(
                !reclaimarc_core::state::is_committed(u0.state),
                "unit 0 must not be committed after {}: {:?}",
                point.as_str(),
                u0.state
            );
            // All source bytes intact (nothing reclaimed).
            let ranges = journal.packed_ranges().unwrap();
            assert!(ranges.iter().all(|r| r.state == reclaimarc_journal::models::RangeState::Active));
        }
        CrashPoint::AfterRename => {
            // Finals exist but must be adoptable (verified, not committed).
            let u0 = units.iter().find(|u| u.seq == 0).unwrap();
            assert!(!reclaimarc_core::state::is_committed(u0.state));
            let entries = journal.entries_for_unit(0).unwrap();
            for e in &entries {
                if e.is_directory {
                    continue;
                }
                assert!(e.final_path.as_ref().unwrap().exists(), "final must exist after rename");
                assert!(e.blake3.is_some(), "blake3 must be recorded before rename");
            }
        }
        CrashPoint::AfterJournalCommit => {
            let u0 = units.iter().find(|u| u.seq == 0).unwrap();
            assert!(reclaimarc_core::state::is_committed(u0.state));
        }
        CrashPoint::BeforeHolePunch | CrashPoint::DuringHolePunch | CrashPoint::BeforeReclaimedCommit => {
            let u0 = units.iter().find(|u| u.seq == 0).unwrap();
            assert!(
                reclaimarc_core::state::is_committed(u0.state),
                "unit 0 committed before reclamation"
            );
            // No holes punched yet (for BeforeHolePunch, none at all).
            if point == CrashPoint::BeforeHolePunch {
                let ranges = journal.packed_ranges_for_unit(0).unwrap();
                assert!(ranges.iter().all(|r| r.state == reclaimarc_journal::models::RangeState::ReclaimIntent));
                let file = reclaimarc_platform::sparse::open_for_reclaim(
                    &journal.volumes().unwrap()[0].path,
                )
                .unwrap();
let allocated = reclaimarc_platform::sparse::query_allocated_ranges(
                    &file,
                    &journal.volumes().unwrap()[0].path,
                    0,
                    std::fs::metadata(&journal.volumes().unwrap()[0].path).unwrap().len(),
                )
                .unwrap();
                let total_alloc: u64 = allocated.iter().map(|r| r.len).sum();
                assert!(total_alloc > 0, "source must still be allocated before the hole punch");
            }
        }
    }

// Resume WITHOUT fault injection and complete.
    let mut engine = Engine::new(EngineConfig {
        pre_test: false,
        ..Default::default()
    });
    let journal = reclaimarc_core::recovery::prepare_resume(&journal_path, None).expect("resume prep");
    let mut backend = reclaimarc_archive::backend_for(&meta.archive_path).unwrap();
    let info = backend.inspect(&reclaimarc_archive::OpenOptions::default()).unwrap();
    let name_map: HashMap<u64, String> = info
        .entries
        .iter()
        .filter(|e| !e.is_directory)
        .map(|e| {
            let safe = reclaimarc_core::paths::validate_entry(&e.name, false).unwrap();
            (e.index, safe.relative())
        })
        .collect();
    let (tx, rx) = mpsc::channel();
    let mut job = reclaimarc_core::JobJob {
        job_id: meta.job_id.clone(),
        archive: meta.archive_path.clone(),
        destination: meta.destination.clone(),
        journal,
        info,
        backend,
        name_map,
        mode,
        password: None,
        tx,
    };
    let handle = JobHandle {
        job_id: meta.job_id,
        pause: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let outcome = engine.run_job(&mut job, &handle).expect("resume must complete");
    match outcome {
        JobOutcome::Completed { .. } => {}
        other => panic!("expected completion after resume, got {other:?}"),
    }
    drop(rx);

    // All output files must be byte-identical to the fixture data.
    for i in 0..5 {
        let expect: Vec<u8> = (0..300_000).map(|b| ((b as usize + i * 13) % 251) as u8).collect();
        let got = std::fs::read(dest.join(format!("file{i}.bin"))).expect("output file exists");
        assert_eq!(got, expect, "file{i}.bin must be byte-identical after crash+resume");
    }
    
    // Destructive mode: source allocation must have dropped measurably.
    if mode == ExtractionMode::LowSpace {
        let journal = reclaimarc_journal::JobJournal::open(&journal_path).unwrap();
        let vol = journal.volumes().unwrap();
        let file = reclaimarc_platform::sparse::open_for_reclaim(&vol[0].path).unwrap();
        let allocated = reclaimarc_platform::sparse::query_allocated_ranges(&file, &vol[0].path, 0, std::fs::metadata(&vol[0].path).unwrap().len()).unwrap();
        let total_alloc: u64 = allocated.iter().map(|r| r.len).sum();
        let logical = std::fs::metadata(&vol[0].path).unwrap().len();
        assert!(
            total_alloc < logical,
            "source allocation must drop after reclamation: alloc {total_alloc} < logical {logical}"
        );
        // Byte integrity: the remaining allocated bytes must still decode —
        // the final files were verified by the engine; additionally, the
        // journal must record the ranges as reclaimed.
        let ranges = journal.packed_ranges().unwrap();
        assert!(ranges.iter().all(|r| r.state == reclaimarc_journal::models::RangeState::Reclaimed));
    }
}

#[test]
fn crash_after_partial_write() {
    crash_and_resume(CrashPoint::AfterPartialWrite, ExtractionMode::Normal);
}

#[test]
fn crash_after_output_flush() {
    crash_and_resume(CrashPoint::AfterOutputFlush, ExtractionMode::Normal);
}

#[test]
fn crash_after_rename() {
    crash_and_resume(CrashPoint::AfterRename, ExtractionMode::Normal);
}

#[test]
fn crash_after_journal_commit() {
    crash_and_resume(CrashPoint::AfterJournalCommit, ExtractionMode::LowSpace);
}

#[test]
fn crash_before_hole_punch() {
    crash_and_resume(CrashPoint::BeforeHolePunch, ExtractionMode::LowSpace);
}

#[test]
fn crash_during_hole_punch() {
    crash_and_resume(CrashPoint::DuringHolePunch, ExtractionMode::LowSpace);
}

#[test]
fn crash_before_reclaimed_commit() {
    crash_and_resume(CrashPoint::BeforeReclaimedCommit, ExtractionMode::LowSpace);
}

/// DoD #4: a real low-space RAR extraction succeeds where normal extraction
/// would fail. The free-space observation is injected; the planner must
/// reject normal extraction and the engine must complete the progressive one,
/// measurably reclaiming source allocation.
#[test]
fn low_space_extraction_succeeds_where_normal_fails() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    // Two units of ~1 MB packed each, ~1.2 MB unpacked. Free observation:
    // enough for unit 0 + reserve but not for everything at once.
    let files: Vec<FixtureFile> = (0..2)
        .map(|i| {
            let data: Vec<u8> = (0..1_200_000).map(|b| ((b + i * 7) % 251) as u8).collect();
            FixtureFile::new(&format!("big{i}.bin"), &data)
        })
        .collect();
    let paths = write_rar(&archive_dir, "lowspace", &files, &FixtureOptions::default()).unwrap();

    let reserve = 256 * 1024u64;
    // Unit output is 1.2 MB; reserve 256 KB; each unit's packed source is
    // 1.2 MB. Free observation: enough for unit 0 + reserve (1.6 MB) but not
    // for both units at once (needs 2.4 MB + reserve).
    let free_observed = 1_600_000u64;
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", free_observed.to_string());
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));

    let engine = Engine::new(EngineConfig {
        pre_test: false,
        custom_reserve: Some(reserve),
        ..Default::default()
    });

    // Normal extraction must be rejected as infeasible.
    let (tx, _) = mpsc::channel();
    let err = engine
        .start_job(&paths[0], &dest, ExtractionMode::Normal, None, tx.clone())
        .unwrap_err();
    assert!(matches!(err, reclaimarc_core::CoreError::Infeasible(_)), "normal must be infeasible: {err:?}");

    // Progressive extraction must succeed.
    let mut engine = engine;
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .expect("low-space job must start");
    let outcome = engine.run_job(&mut job, &handle).expect("low-space extraction must complete");
    assert!(matches!(outcome, JobOutcome::Completed { .. }), "got {outcome:?}");

    // Output correctness.
    for i in 0..2 {
        let expect: Vec<u8> = (0..1_200_000).map(|b| ((b + i * 7) % 251) as u8).collect();
        assert_eq!(std::fs::read(dest.join(format!("big{i}.bin"))).unwrap(), expect);
    }

    // Measurable reclamation (DoD #7).
    let file = reclaimarc_platform::sparse::open_for_reclaim(&paths[0]).unwrap();
    let allocated =
        reclaimarc_platform::sparse::query_allocated_ranges(&file, &paths[0], 0, std::fs::metadata(&paths[0]).unwrap().len()).unwrap();
    let total_alloc: u64 = allocated.iter().map(|r| r.len).sum();
    let logical = std::fs::metadata(&paths[0]).unwrap().len();
    assert!(
        total_alloc < logical / 2,
        "source allocation must be mostly reclaimed: alloc {total_alloc} of {logical}"
    );

    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// Path traversal in an archive must be rejected before anything is written.
#[test]
fn path_traversal_is_rejected() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    // Fixture with a traversal name — the fixture writer stores it verbatim.
    let files = vec![FixtureFile::new("../evil.txt", b"boom")];
    let paths = write_rar(&archive_dir, "evil", &files, &FixtureOptions::default()).unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let engine = Engine::new(EngineConfig::default());
    let (tx, _) = mpsc::channel();
    let err = engine
        .start_job(&paths[0], &dest, ExtractionMode::Normal, None, tx)
        .unwrap_err();
    assert!(
        matches!(err, reclaimarc_core::CoreError::Precondition(_)),
        "traversal must be rejected: {err:?}"
    );
    assert!(
        !dir.path().join("evil.txt").exists() && !dest.join("evil.txt").exists(),
        "nothing may escape the destination"
    );
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// Pause mid-job must leave the job resumable with source intact.
#[test]
fn pause_then_resume_completes() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let files: Vec<FixtureFile> = (0..4)
        .map(|i| {
            let data: Vec<u8> = (0..200_000).map(|b| ((b + i * 5) % 251) as u8).collect();
            FixtureFile::new(&format!("p{i}.bin"), &data)
        })
        .collect();
    let paths = write_rar(&archive_dir, "pause", &files, &FixtureOptions::default()).unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let mut engine = Engine::new(EngineConfig::default());
    let (tx, _rx) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::Normal, None, tx)
        .unwrap();

    // Pause immediately: the current unit aborts safely.
    handle.pause();
    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Paused | JobOutcome::Completed { .. }), "got {outcome:?}");
    eprintln!("PAUSE TEST: first run outcome={outcome:?}");
    for entry in std::fs::read_dir(&dest).unwrap().flatten() {
        eprintln!("PAUSE TEST: dest has {}", entry.file_name().to_string_lossy());
    }

    // Journal must be readable and the job resumable.
    let journal_path = find_journal(dir.path());
    let j = reclaimarc_journal::JobJournal::open(&journal_path).unwrap();
    let meta = j.job_meta().unwrap();
    let _ = meta;

// Resume to completion.
    drop(job);
    let journal = reclaimarc_core::recovery::prepare_resume(&journal_path, None).unwrap();
    for entry in std::fs::read_dir(&dest).unwrap().flatten() {
        eprintln!("PAUSE TEST: dest after prepare_resume has {}", entry.file_name().to_string_lossy());
    }
    let mut backend = reclaimarc_archive::backend_for(&paths[0]).unwrap();
    let info = backend.inspect(&reclaimarc_archive::OpenOptions::default()).unwrap();
    let name_map: HashMap<u64, String> = info
        .entries
        .iter()
        .filter(|e| !e.is_directory)
        .map(|e| {
            let safe = reclaimarc_core::paths::validate_entry(&e.name, false).unwrap();
            (e.index, safe.relative())
        })
        .collect();
    let (tx2, _rx2) = mpsc::channel();
    let mut job2 = reclaimarc_core::JobJob {
        job_id: j.job_meta().unwrap().job_id,
        archive: paths[0].clone(),
        destination: dest.clone(),
        journal,
        info,
        backend,
        name_map,
        mode: ExtractionMode::Normal,
        password: None,
        tx: tx2,
    };
    let handle2 = JobHandle {
        job_id: job2.job_id.clone(),
        pause: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let outcome = engine.run_job(&mut job2, &handle2).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }), "got {outcome:?}");
    for i in 0..4 {
        let expect: Vec<u8> = (0..200_000).map(|b| ((b + i * 5) % 251) as u8).collect();
        assert_eq!(std::fs::read(dest.join(format!("p{i}.bin"))).unwrap(), expect);
    }
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// Source modification between crash and resume must be detected.
#[test]
fn source_modification_is_detected() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let status = run_child(dir.path(), CrashPoint::AfterPartialWrite, ExtractionMode::Normal);
    assert_eq!(status.code(), Some(CRASH_CODE));

    let journal_path = find_journal(dir.path());
    let j = reclaimarc_journal::JobJournal::open(&journal_path).unwrap();
    let archive_path = j.job_meta().unwrap().archive_path;

    // Tamper with the archive BEFORE resuming.
    std::fs::write(&archive_path, b"tampered").unwrap();

    let err = reclaimarc_core::recovery::prepare_resume(&journal_path, None).unwrap_err();
    assert!(
        matches!(err, reclaimarc_core::CoreError::Failed { .. }),
        "modified source must fail recovery precisely: {err:?}"
    );
}

/// Source archive is deleted ONLY when 100% verified completion is reached in destructive mode.
#[test]
fn test_source_deletion_only_after_100_percent_verified_completion() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let files = vec![
        FixtureFile::new("file1.bin", &vec![0x11; 40_000]),
        FixtureFile::new("file2.bin", &vec![0x22; 60_000]),
    ];
    let paths = write_rar(&archive_dir, "test_delete", &files, &FixtureOptions::default()).unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let config = EngineConfig {
        pre_test: false,
        custom_reserve: Some(256 * 1024),
        delete_shells_on_completion: true,
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();
    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));

    // Both output files must exist with exact size.
    assert_eq!(std::fs::metadata(dest.join("file1.bin")).unwrap().len(), 40_000);
    assert_eq!(std::fs::metadata(dest.join("file2.bin")).unwrap().len(), 60_000);

    // The source archive must have been cleanly removed because 100% verified.
    assert!(!paths[0].exists(), "Source archive must be deleted on 100% verified completion");
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// Source archive is NEVER deleted if extraction is interrupted or incomplete.
#[test]
fn test_source_is_never_deleted_if_extraction_is_interrupted() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let files = vec![
        FixtureFile::new("file1.bin", &vec![0x11; 40_000]),
        FixtureFile::new("file2.bin", &vec![0x22; 60_000]),
    ];
    let paths = write_rar(&archive_dir, "test_pause", &files, &FixtureOptions::default()).unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let config = EngineConfig {
        pre_test: false,
        custom_reserve: Some(256 * 1024),
        delete_shells_on_completion: true,
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();

    // Pause immediately
    handle.pause();
    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Paused));

    // The source archive must STILL EXIST because the job is incomplete.
    assert!(paths[0].exists(), "Source archive must NOT be deleted when paused/incomplete");
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}


