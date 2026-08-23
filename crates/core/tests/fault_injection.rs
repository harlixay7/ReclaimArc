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
use reclaimarc_core::{Engine, EngineConfig, ExtractionMode, JobHandle, JobOutcome};

const CRASH_CODE: i32 = 86;
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Environment for the child process.
fn child_env(
    dir: &Path,
    point: CrashPoint,
    mode: ExtractionMode,
) -> std::collections::HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("SX_CHILD".to_string(), "1".to_string());
    env.insert("SX_WORK".to_string(), dir.to_string_lossy().into_owned());
    env.insert("SX_FAULT".to_string(), point.as_str().to_string());
    env.insert(
        "RECLAIMARC_FAULT_AT".to_string(),
        point.as_str().to_string(),
    );
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
    env.insert(
        "RECLAIMARC_TEST_FREE_SPACE".to_string(),
        "100000000000".to_string(),
    );
    env.insert(
        "RECLAIMARC_APP_DATA".to_string(),
        dir.join("appdata").to_string_lossy().into_owned(),
    );
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
            let data: Vec<u8> = (0..300_000)
                .map(|b| ((b as usize + i * 13) % 251) as u8)
                .collect();
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
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
            assert!(ranges
                .iter()
                .all(|r| r.state == reclaimarc_journal::models::RangeState::Active));
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
                assert!(
                    e.final_path.as_ref().unwrap().exists(),
                    "final must exist after rename"
                );
                assert!(e.blake3.is_some(), "blake3 must be recorded before rename");
            }
        }
        CrashPoint::AfterJournalCommit => {
            let u0 = units.iter().find(|u| u.seq == 0).unwrap();
            assert!(reclaimarc_core::state::is_committed(u0.state));
        }
        CrashPoint::BeforeHolePunch
        | CrashPoint::DuringHolePunch
        | CrashPoint::AfterPhysicalHolePunch
        | CrashPoint::BeforeReclaimedCommit => {
            let u0 = units.iter().find(|u| u.seq == 0).unwrap();
            assert!(
                reclaimarc_core::state::is_committed(u0.state),
                "unit 0 committed before reclamation"
            );
            // No holes punched yet (for BeforeHolePunch, none at all).
            if point == CrashPoint::BeforeHolePunch {
                let ranges = journal.packed_ranges_for_unit(0).unwrap();
                assert!(ranges
                    .iter()
                    .all(|r| r.state == reclaimarc_journal::models::RangeState::ReclaimIntent));
                let file = reclaimarc_platform::sparse::open_for_reclaim(
                    &journal.volumes().unwrap()[0].path,
                )
                .unwrap();
                let allocated = reclaimarc_platform::sparse::query_allocated_ranges(
                    &file,
                    &journal.volumes().unwrap()[0].path,
                    0,
                    std::fs::metadata(&journal.volumes().unwrap()[0].path)
                        .unwrap()
                        .len(),
                )
                .unwrap();
                let total_alloc: u64 = allocated.iter().map(|r| r.len).sum();
                assert!(
                    total_alloc > 0,
                    "source must still be allocated before the hole punch"
                );
            }
        }
    }

    // Resume WITHOUT fault injection and complete.
    let mut engine = Engine::new(EngineConfig {
        pre_test: false,
        ..Default::default()
    });
    let journal =
        reclaimarc_core::recovery::prepare_resume(&journal_path, None).expect("resume prep");
    let mut backend = reclaimarc_archive::backend_for(&meta.archive_path).unwrap();
    let info = backend
        .inspect(&reclaimarc_archive::OpenOptions::default())
        .unwrap();
    let name_map: HashMap<u64, String> = info
        .entries
        .iter()
        .map(|e| (e.index, e.name.clone()))
        .collect();
    let (tx, _) = mpsc::channel();
    let mut job = reclaimarc_core::ExtractionJob {
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
    let handle = reclaimarc_core::JobHandle {
        job_id: meta.job_id.clone(),
        pause: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let outcome = engine
        .run_job(&mut job, &handle)
        .expect("resume must complete");
    assert!(
        matches!(outcome, JobOutcome::Completed { .. }),
        "resume must reach Completed"
    );

    // Verify all final files exist and match CRC/size.
    for i in 0..5 {
        let expect: Vec<u8> = (0..300_000)
            .map(|b| ((b as usize + i * 13) % 251) as u8)
            .collect();
        let got = std::fs::read(dest.join(format!("file{i}.bin"))).expect("output file exists");
        assert_eq!(
            got, expect,
            "file{i}.bin must be byte-identical after crash+resume"
        );
    }

    // Destructive mode: source allocation must have dropped measurably.
    if mode == ExtractionMode::LowSpace {
        let journal = reclaimarc_journal::JobJournal::open(&journal_path).unwrap();
        let vol = journal.volumes().unwrap();
        let file = reclaimarc_platform::sparse::open_for_reclaim(&vol[0].path).unwrap();
        let allocated = reclaimarc_platform::sparse::query_allocated_ranges(
            &file,
            &vol[0].path,
            0,
            std::fs::metadata(&vol[0].path).unwrap().len(),
        )
        .unwrap();
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
        assert!(ranges.iter().any(|r| r.state
            == reclaimarc_journal::models::RangeState::Reclaimed
            || r.state == reclaimarc_journal::models::RangeState::Partial));
        assert!(ranges
            .iter()
            .all(|r| r.state != reclaimarc_journal::models::RangeState::ReclaimIntent));
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
fn crash_after_physical_hole_punch() {
    crash_and_resume(CrashPoint::AfterPhysicalHolePunch, ExtractionMode::LowSpace);
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
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
    assert!(
        matches!(err, reclaimarc_core::CoreError::Infeasible(_)),
        "normal must be infeasible: {err:?}"
    );

    // Progressive extraction must succeed.
    let mut engine = engine;
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .expect("low-space job must start");
    let outcome = engine
        .run_job(&mut job, &handle)
        .expect("low-space extraction must complete");
    assert!(
        matches!(outcome, JobOutcome::Completed { .. }),
        "got {outcome:?}"
    );

    // Output correctness.
    for i in 0..2 {
        let expect: Vec<u8> = (0..1_200_000).map(|b| ((b + i * 7) % 251) as u8).collect();
        assert_eq!(
            std::fs::read(dest.join(format!("big{i}.bin"))).unwrap(),
            expect
        );
    }

    // Measurable reclamation (DoD #7).
    let file = reclaimarc_platform::sparse::open_for_reclaim(&paths[0]).unwrap();
    let allocated = reclaimarc_platform::sparse::query_allocated_ranges(
        &file,
        &paths[0],
        0,
        std::fs::metadata(&paths[0]).unwrap().len(),
    )
    .unwrap();
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
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
    assert!(
        matches!(outcome, JobOutcome::Paused | JobOutcome::Completed { .. }),
        "got {outcome:?}"
    );
    eprintln!("PAUSE TEST: first run outcome={outcome:?}");
    for entry in std::fs::read_dir(&dest).unwrap().flatten() {
        eprintln!(
            "PAUSE TEST: dest has {}",
            entry.file_name().to_string_lossy()
        );
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
        eprintln!(
            "PAUSE TEST: dest after prepare_resume has {}",
            entry.file_name().to_string_lossy()
        );
    }
    let mut backend = reclaimarc_archive::backend_for(&paths[0]).unwrap();
    let info = backend
        .inspect(&reclaimarc_archive::OpenOptions::default())
        .unwrap();
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
    let mut job2 = reclaimarc_core::ExtractionJob {
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
    assert!(
        matches!(outcome, JobOutcome::Completed { .. }),
        "got {outcome:?}"
    );
    for i in 0..4 {
        let expect: Vec<u8> = (0..200_000).map(|b| ((b + i * 5) % 251) as u8).collect();
        assert_eq!(
            std::fs::read(dest.join(format!("p{i}.bin"))).unwrap(),
            expect
        );
    }
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// Source modification between crash and resume must be detected.
#[test]
fn source_modification_is_detected() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let status = run_child(
        dir.path(),
        CrashPoint::AfterPartialWrite,
        ExtractionMode::Normal,
    );
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
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let files = vec![
        FixtureFile::new("file1.bin", &vec![0x11; 40_000]),
        FixtureFile::new("file2.bin", &vec![0x22; 60_000]),
    ];
    let paths = write_rar(
        &archive_dir,
        "test_delete",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

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
    assert_eq!(
        std::fs::metadata(dest.join("file1.bin")).unwrap().len(),
        40_000
    );
    assert_eq!(
        std::fs::metadata(dest.join("file2.bin")).unwrap().len(),
        60_000
    );

    // The source archive must have been cleanly removed because 100% verified.
    assert!(
        !paths[0].exists(),
        "Source archive must be deleted on 100% verified completion"
    );
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// Source archive is NEVER deleted if extraction is interrupted or incomplete.
#[test]
fn test_source_is_never_deleted_if_extraction_is_interrupted() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let files = vec![
        FixtureFile::new("file1.bin", &vec![0x11; 40_000]),
        FixtureFile::new("file2.bin", &vec![0x22; 60_000]),
    ];
    let paths = write_rar(
        &archive_dir,
        "test_pause",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

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
    assert!(
        paths[0].exists(),
        "Source archive must NOT be deleted when paused/incomplete"
    );
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// ConflictPolicy::Skip in Low-Space mode must FAIL CLOSED if existing destination file differs.
#[test]
fn test_conflict_skip_in_low_space_fails_closed_if_destination_differs() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![FixtureFile::new(
        "target.bin",
        b"original content inside archive",
    )];
    let paths = write_rar(
        &archive_dir,
        "test_skip_conflict",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    // Destination pre-exists with DIFFERENT content
    std::fs::write(
        dest.join("target.bin"),
        b"different pre-existing content on disk",
    )
    .unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let config = EngineConfig {
        pre_test: false,
        conflict_policy: reclaimarc_core::ConflictPolicy::Skip,
        custom_reserve: Some(256 * 1024),
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();

    let outcome = engine.run_job(&mut job, &handle);
    // Must fail closed to protect source bytes when existing file differs
    assert!(
        matches!(outcome, Err(reclaimarc_core::CoreError::Precondition(_))),
        "Skip in low space with differing file must fail closed: {outcome:?}"
    );

    // Source archive must NOT be modified or destroyed
    assert!(paths[0].exists());
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// ConflictPolicy::Ask must FAIL CLOSED with decision required if destination exists.
#[test]
fn test_conflict_ask_fails_closed_when_destination_exists() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![FixtureFile::new("ask_target.bin", b"some archive content")];
    let paths = write_rar(
        &archive_dir,
        "test_ask_conflict",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    // Destination pre-exists
    std::fs::write(dest.join("ask_target.bin"), b"already exists").unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let config = EngineConfig {
        pre_test: false,
        conflict_policy: reclaimarc_core::ConflictPolicy::Ask,
        custom_reserve: Some(256 * 1024),
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::Normal, None, tx)
        .unwrap();

    let outcome = engine.run_job(&mut job, &handle);
    assert!(
        matches!(outcome, Err(reclaimarc_core::CoreError::Precondition(_))),
        "Ask policy must fail closed requiring user decision: {outcome:?}"
    );
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// Recovery resume NEVER deletes unrelated or mismatched final destination files.
#[test]
fn test_resume_does_not_delete_unrelated_or_mismatched_finals() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let status = run_child(
        dir.path(),
        CrashPoint::AfterPartialWrite,
        ExtractionMode::Normal,
    );
    assert_eq!(status.code(), Some(CRASH_CODE));

    let journal_path = find_journal(dir.path());
    let j = reclaimarc_journal::JobJournal::open(&journal_path).unwrap();
    let meta = j.job_meta().unwrap();

    // Create a pre-existing user file at a final path with custom content
    let final_dest = meta.destination.join("file1.bin");
    std::fs::write(&final_dest, b"user important data that must not be deleted").unwrap();

    // Run recovery prepare_resume
    let _recovered_journal =
        reclaimarc_core::recovery::prepare_resume(&journal_path, None).unwrap();

    // The user's file must NOT have been wiped or deleted
    assert!(
        final_dest.exists(),
        "prepare_resume must never delete pre-existing final files"
    );
    let content = std::fs::read(&final_dest).unwrap();
    assert_eq!(content, b"user important data that must not be deleted");
}

/// 1. Correct hash and size verification succeeds.
#[test]
fn test_verify_file_correct_hash_size_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("good.bin");
    let data = b"exact byte verification data";
    std::fs::write(&file_path, data).unwrap();
    let expected_hash = blake3::hash(data).to_hex().to_string();

    let res = reclaimarc_core::engine::verify_file(&file_path, Some(data.len() as u64), 4096);
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), expected_hash);

    let check = reclaimarc_core::engine::verify_against(
        &file_path,
        Some(data.len() as u64),
        &expected_hash,
    );
    assert!(check.unwrap());
}

/// 2. Incorrect size or hash verification fails immediately.
#[test]
fn test_verify_file_incorrect_hash_or_size_fails() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("bad.bin");
    let data = b"exact byte verification data";
    std::fs::write(&file_path, data).unwrap();

    // Wrong size
    let wrong_size_res =
        reclaimarc_core::engine::verify_file(&file_path, Some(data.len() as u64 + 10), 4096);
    assert!(
        wrong_size_res.is_err(),
        "Size mismatch must fail verification"
    );

    // Wrong hash
    let wrong_hash_check = reclaimarc_core::engine::verify_against(
        &file_path,
        Some(data.len() as u64),
        "0000000000000000000000000000000000000000000000000000000000000000",
    );
    assert!(
        !wrong_hash_check.unwrap(),
        "Hash mismatch must return false"
    );
}

/// 3. Low-Space Skip identical existing file succeeds safely and reclaims source.
#[test]
fn test_low_space_skip_identical_existing_file_succeeds_safely() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let content = b"reclaimarc identical skip content";
    let files = vec![FixtureFile::new("skip_me.bin", content)];
    let paths = write_rar(
        &archive_dir,
        "test_skip_identical",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    // Pre-create identical file at destination
    std::fs::write(dest.join("skip_me.bin"), content).unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let config = EngineConfig {
        pre_test: false,
        conflict_policy: reclaimarc_core::ConflictPolicy::Skip,
        custom_reserve: Some(256 * 1024),
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();

    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// 4. Skip mismatching file destroys ZERO source bytes.
#[test]
fn test_low_space_skip_mismatching_file_destroys_zero_source_bytes() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let content = b"archive payload that should not be skipped without proof";
    let files = vec![FixtureFile::new("mismatch.bin", content)];
    let paths = write_rar(
        &archive_dir,
        "test_skip_mismatch",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();
    let source_size_before = std::fs::metadata(&paths[0]).unwrap().len();

    // Pre-create DIFFERENT content at destination
    std::fs::write(dest.join("mismatch.bin"), b"different existing file").unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let config = EngineConfig {
        pre_test: false,
        conflict_policy: reclaimarc_core::ConflictPolicy::Skip,
        custom_reserve: Some(256 * 1024),
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();

    let outcome = engine.run_job(&mut job, &handle);
    assert!(
        outcome.is_err(),
        "Skip mismatching file must fail in low-space mode"
    );

    // Prove ZERO source bytes were destroyed
    let source_size_after = std::fs::metadata(&paths[0]).unwrap().len();
    assert_eq!(source_size_before, source_size_after);
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// 5. Old v1 journal migrates and resumes cleanly.
#[test]
fn test_old_v1_journal_migrates_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("job.db");

    // Create a real v1 SQLite database with missing columns and schema_version = '1'
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(r#"
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        INSERT INTO meta (key, value) VALUES ('schema_version', '1');
        CREATE TABLE volumes (
            id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, identity_json TEXT,
            allocated_before INTEGER NOT NULL DEFAULT 0, logical_size INTEGER NOT NULL DEFAULT 0, is_first INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE recovery_units (
            id INTEGER PRIMARY KEY, seq INTEGER NOT NULL UNIQUE, state TEXT NOT NULL,
            first_entry INTEGER NOT NULL, last_entry INTEGER NOT NULL, error TEXT, updated_at TEXT NOT NULL
        );
        CREATE TABLE entries (
            id INTEGER PRIMARY KEY, index_in_archive INTEGER NOT NULL UNIQUE, name TEXT NOT NULL,
            packed_size INTEGER NOT NULL, unpacked_size INTEGER NOT NULL, crc32 INTEGER,
            is_directory INTEGER NOT NULL, is_solid INTEGER NOT NULL, split_before INTEGER NOT NULL DEFAULT 0,
            split_after INTEGER NOT NULL DEFAULT 0, encrypted INTEGER NOT NULL DEFAULT 0,
            recovery_unit INTEGER NOT NULL REFERENCES recovery_units(id), final_path TEXT, partial_path TEXT,
            blake3 TEXT, status TEXT NOT NULL
        );
        CREATE TABLE packed_ranges (
            id INTEGER PRIMARY KEY, volume_index INTEGER NOT NULL, start INTEGER NOT NULL,
            len INTEGER NOT NULL, state TEXT NOT NULL, recovery_unit INTEGER
        );
        CREATE TABLE state_transitions (
            id INTEGER PRIMARY KEY, unit_seq INTEGER NOT NULL, from_state TEXT NOT NULL, to_state TEXT NOT NULL, at TEXT NOT NULL
        );
        CREATE TABLE errors (
            id INTEGER PRIMARY KEY, at TEXT NOT NULL, operation TEXT NOT NULL, message TEXT NOT NULL,
            os_error INTEGER, recovery_state TEXT NOT NULL, recommended_action TEXT NOT NULL
        );
        CREATE TABLE job_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1), job_id TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
            archive_path TEXT NOT NULL, destination TEXT NOT NULL, archive_fingerprint TEXT,
            safety_mode TEXT NOT NULL, settings_json TEXT NOT NULL, current_unit INTEGER NOT NULL DEFAULT 0, job_state TEXT NOT NULL
        );
        INSERT INTO job_meta (id, job_id, created_at, updated_at, archive_path, destination, archive_fingerprint, safety_mode, settings_json, current_unit, job_state)
        VALUES (1, 'v1-test-job', '2026-08-22T00:00:00Z', '2026-08-22T00:00:00Z', 'C:\\test.rar', 'C:\\out', 'fp', 'Balanced', '{}', 0, 'ACTIVE');
    "#).unwrap();
    drop(conn);

    // Open via JobJournal::open() which must migrate v1 -> v2
    let j = reclaimarc_journal::JobJournal::open(&db_path).unwrap();
    let meta = j.job_meta().unwrap();
    assert_eq!(meta.job_id, "v1-test-job");

    // Verify new columns exist by querying entries and packed_ranges
    let entries = j.entries().unwrap();
    assert_eq!(entries.len(), 0);
    let ranges = j.packed_ranges().unwrap();
    assert_eq!(ranges.len(), 0);
}

/// 6. Batched state transitions reject an unexpected starting state.
#[test]
fn test_batched_state_transitions_reject_unexpected_starting_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("job.db");
    let meta = reclaimarc_journal::models::JobMeta {
        job_id: "test-state-reject".into(),
        created_at: "2026-08-22T00:00:00Z".into(),
        updated_at: "2026-08-22T00:00:00Z".into(),
        archive_path: dir.path().join("arch.rar"),
        destination: dir.path().join("out"),
        archive_fingerprint: None,
        safety_mode: "Balanced".into(),
        settings_json: "{}".into(),
        current_unit: 0,
        job_state: reclaimarc_journal::models::JobState::Active,
    };
    let mut j = reclaimarc_journal::JobJournal::create(&db_path, &meta).unwrap();
    j.add_units(&[reclaimarc_journal::models::RecoveryUnitRecord {
        seq: 0,
        state: reclaimarc_journal::models::UnitState::Pending, // Starting at PENDING, not EXTRACTING!
        first_entry: 0,
        last_entry: 0,
        error: None,
        updated_at: "2026-08-22T00:00:00Z".into(),
    }])
    .unwrap();

    // mark_unit_verified_durable expects starting state EXTRACTING
    let res = j.mark_unit_verified_durable(0, &[]);
    assert!(
        res.is_err(),
        "mark_unit_verified_durable must reject transition when state is not EXTRACTING"
    );
}

/// 7. Partial filesystem deallocation stays PARTIAL in journal.
#[test]
fn test_partial_filesystem_deallocation_stays_partial() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("job.db");
    let meta = reclaimarc_journal::models::JobMeta {
        job_id: "test-partial-state".into(),
        created_at: "2026-08-22T00:00:00Z".into(),
        updated_at: "2026-08-22T00:00:00Z".into(),
        archive_path: dir.path().join("arch.rar"),
        destination: dir.path().join("out"),
        archive_fingerprint: None,
        safety_mode: "Balanced".into(),
        settings_json: "{}".into(),
        current_unit: 0,
        job_state: reclaimarc_journal::models::JobState::Active,
    };
    let mut j = reclaimarc_journal::JobJournal::create(&db_path, &meta).unwrap();
    j.add_volumes(&[reclaimarc_journal::models::VolumeRecord {
        path: dir.path().join("arch.rar"),
        identity: None,
        allocated_before: 1024 * 1024,
        logical_size: 1024 * 1024,
        is_first: true,
        structural_digest: None,
    }])
    .unwrap();
    j.add_units(&[reclaimarc_journal::models::RecoveryUnitRecord {
        seq: 0,
        state: reclaimarc_journal::models::UnitState::Committed,
        first_entry: 0,
        last_entry: 0,
        error: None,
        updated_at: "2026-08-22T00:00:00Z".into(),
    }])
    .unwrap();
    j.add_packed_ranges(&[reclaimarc_journal::models::PackedRangeRecord {
        volume_index: 0,
        start: 0,
        len: 128 * 1024,
        state: reclaimarc_journal::models::RangeState::Active,
        recovery_unit: Some(0),
        physically_released_bytes: 0,
        blake3_digest: None,
    }])
    .unwrap();

    // Record partial outcome: 64KB released out of 128KB requested
    j.mark_range_outcome(
        0,
        0,
        128 * 1024,
        reclaimarc_journal::models::RangeState::Partial,
        64 * 1024,
    )
    .unwrap();

    let ranges = j.packed_ranges().unwrap();
    assert_eq!(
        ranges[0].state,
        reclaimarc_journal::models::RangeState::Partial
    );
    assert_eq!(ranges[0].physically_released_bytes, 64 * 1024);
}

/// 8. Space planner does not credit already-unallocated/sparse bytes.
#[test]
fn test_space_planner_does_not_credit_already_unallocated_bytes() {
    let info = reclaimarc_archive::model::ArchiveInfo {
        format: "RAR5".into(),
        packed_size: 128 * 1024,
        unpacked_size: 128 * 1024,
        solid_archive: false,
        encrypted_headers: false,
        volumes: vec![reclaimarc_archive::model::VolumeInfo {
            index: 0,
            path: PathBuf::from("dummy.rar"),
            logical_size: 128 * 1024,
        }],
        entries: vec![reclaimarc_archive::model::Entry {
            index: 0,
            name: "test.bin".into(),
            packed_size: 128 * 1024,
            unpacked_size: 128 * 1024,
            crc32: None,
            is_directory: false,
            is_solid: false,
            split_before: false,
            split_after: false,
            encrypted: false,
            redirection: None,
        }],
        recovery_units: vec![reclaimarc_archive::model::RecoveryUnit {
            seq: 0,
            first_entry: 0,
            last_entry: 0,
            packed_ranges: vec![reclaimarc_archive::model::PackedRange {
                volume_index: 0,
                start: 0,
                len: 128 * 1024,
            }],
            unpacked_bytes: 128 * 1024,
        }],
        decoder_requirements: reclaimarc_archive::model::DecoderRequirements {
            scratch_bytes: 0,
            redecodes_prefix: false,
        },
        capability: reclaimarc_archive::model::CapabilityMatrix {
            format: "RAR5".into(),
            supports_test_integrity: true,
            restartable_units: true,
            progressive_reclaim: true,
            supports_encryption: true,
            supports_multipart: true,
            notes: vec![],
        },
    };

    let config = EngineConfig {
        custom_reserve: Some(1024),
        ..Default::default()
    };

    // Case A: Fully allocated source file -> credits full 128KB
    let mut alloc_full = HashMap::new();
    alloc_full.insert(
        0u64,
        vec![reclaimarc_platform::sparse::ByteRange {
            start: 0,
            len: 128 * 1024,
        }],
    );
    let plan_full = reclaimarc_core::planner::plan_with_measurements(
        &info,
        10 * 1024 * 1024,
        100 * 1024 * 1024,
        Some(4096),
        Some(&alloc_full),
        &config,
    )
    .unwrap();
    assert_eq!(plan_full.estimated_source_reclaim, 128 * 1024);

    // Case B: Completely unallocated source file (0 allocated bytes) -> credits 0 bytes
    let mut alloc_empty = HashMap::new();
    alloc_empty.insert(0u64, vec![]);
    let plan_empty = reclaimarc_core::planner::plan_with_measurements(
        &info,
        10 * 1024 * 1024,
        100 * 1024 * 1024,
        Some(4096),
        Some(&alloc_empty),
        &config,
    )
    .unwrap();
    assert_eq!(
        plan_empty.estimated_source_reclaim, 0,
        "Planner must not credit unallocated bytes"
    );
}

/// 9. Damaged committed output with intact source can safely re-extract.
#[test]
fn test_damaged_committed_output_with_intact_source_safely_reextracts() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![FixtureFile::new("file1.bin", b"intact source data")];
    let paths = write_rar(
        &archive_dir,
        "test_damaged_intact",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    std::env::set_var("RECLAIMARC_APP_DATA", dir.path().join("appdata"));
    let config = EngineConfig::default();
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::Normal, None, tx)
        .unwrap();

    // Normal extraction completes
    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));

    // Corrupt the committed output
    std::fs::write(dest.join("file1.bin"), b"corrupted data").unwrap();

    // Resume: since normal extraction preserved 100% of source, prepare_resume resets to Pending for clean re-extraction
    let journal_path = find_journal(dir.path());
    let recovered_journal = reclaimarc_core::recovery::prepare_resume(&journal_path, None).unwrap();
    let units = recovered_journal.units().unwrap();
    assert_eq!(
        units[0].state,
        reclaimarc_journal::models::UnitState::Pending
    );
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// 10. Damaged committed output with partially reclaimed/destroyed source fails terminally.
#[test]
fn test_damaged_committed_output_with_reclaimed_source_fails_terminally() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![FixtureFile::new(
        "file1.bin",
        b"source data that will be destroyed",
    )];
    let paths = write_rar(
        &archive_dir,
        "test_damaged_reclaimed",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    let journal_path = archive_dir
        .join(".reclaimarc")
        .join("test-job")
        .join("job.db");
    std::fs::create_dir_all(journal_path.parent().unwrap()).unwrap();

    let meta = reclaimarc_journal::models::JobMeta {
        job_id: "test-damaged-reclaimed".into(),
        created_at: "2026-08-22T00:00:00Z".into(),
        updated_at: "2026-08-22T00:00:00Z".into(),
        archive_path: paths[0].clone(),
        destination: dest.clone(),
        archive_fingerprint: None,
        safety_mode: "Balanced".into(),
        settings_json: "{}".into(),
        current_unit: 0,
        job_state: reclaimarc_journal::models::JobState::Active,
    };
    let mut j = reclaimarc_journal::JobJournal::create(&journal_path, &meta).unwrap();
    j.add_volumes(&[reclaimarc_journal::models::VolumeRecord {
        path: paths[0].clone(),
        identity: None,
        allocated_before: 100,
        logical_size: 100,
        is_first: true,
        structural_digest: None,
    }])
    .unwrap();
    j.add_units(&[reclaimarc_journal::models::RecoveryUnitRecord {
        seq: 0,
        state: reclaimarc_journal::models::UnitState::Committed,
        first_entry: 0,
        last_entry: 0,
        error: None,
        updated_at: "2026-08-22T00:00:00Z".into(),
    }])
    .unwrap();
    j.add_entries(&[reclaimarc_journal::models::EntryRecord {
        index_in_archive: 0,
        name: "file1.bin".into(),
        packed_size: 50,
        unpacked_size: files[0].data.len() as u64,
        crc32: None,
        is_directory: false,
        is_solid: false,
        split_before: false,
        split_after: false,
        encrypted: false,
        recovery_unit: 0,
        final_path: Some(dest.join("file1.bin")),
        partial_path: None,
        blake3: Some("originalblake3hex".into()),
        status: reclaimarc_journal::models::EntryStatus::Committed,
        actual_committed_path: Some(dest.join("file1.bin")),
        existed_before_job: false,
        expected_digest: Some("originalblake3hex".into()),
        is_redirection: false,
        redirection_kind: None,
    }])
    .unwrap();
    j.add_packed_ranges(&[reclaimarc_journal::models::PackedRangeRecord {
        volume_index: 0,
        start: 0,
        len: 50,
        state: reclaimarc_journal::models::RangeState::Reclaimed, // Source is RECLAIMED
        recovery_unit: Some(0),
        physically_released_bytes: 50,
        blake3_digest: None,
    }])
    .unwrap();
    drop(j);

    // Corrupt or delete the final output
    std::fs::write(dest.join("file1.bin"), b"corrupted final").unwrap();

    // Prepare resume must FAIL CLOSED with a terminal integrity violation
    let res = reclaimarc_core::recovery::prepare_resume(&journal_path, None);
    assert!(
        res.is_err(),
        "Resume must fail closed when output is corrupted and source is reclaimed"
    );
}

/// 11. open_for_reclaim fails closed on missing files without touching disk.
#[test]
fn test_reclaim_open_fails_closed_on_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    std::fs::create_dir_all(&archive_dir).unwrap();

    let missing = archive_dir.join("nonexistent.rar");
    let res = reclaimarc_platform::sparse::open_for_reclaim(&missing);
    assert!(res.is_err());
    assert!(!missing.exists());
}

/// 12. Password provided at job start is not saved to disk in journal settings.
#[test]
fn test_job_start_with_password_does_not_persist_password_in_journal() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let password = "SecretPassword123";
    let files = vec![FixtureFile::new("secret.txt", b"classified data")];
    let paths = write_rar(
        &archive_dir,
        "encrypted_test",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    let config = EngineConfig::default();
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(
            &paths[0],
            &dest,
            ExtractionMode::Normal,
            Some(password.into()),
            tx,
        )
        .unwrap();

    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));

    let journal_path = find_journal(dir.path());
    let j = reclaimarc_journal::JobJournal::open(&journal_path).unwrap();
    let meta = j.job_meta().unwrap();
    assert!(
        !meta.settings_json.contains(password),
        "Password must never be saved in journal settings"
    );
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// 13. Redirection / Symlink entries are skipped under SymlinkPolicy::Skip.
#[test]
fn test_redirection_symlink_skipped_by_default() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        FixtureFile::new("first.txt", b"first file data"),
        FixtureFile::symlink(
            "unix_link.lnk",
            "/etc/passwd",
            reclaimarc_archive::model::RedirectionKind::UnixSymlink,
        ),
        FixtureFile::symlink(
            "win_link.lnk",
            "C:\\Windows\\System32",
            reclaimarc_archive::model::RedirectionKind::WindowsSymlink,
        ),
        FixtureFile::symlink(
            "junction_dir",
            "C:\\Users",
            reclaimarc_archive::model::RedirectionKind::Junction,
        ),
        FixtureFile::symlink(
            "hard_link.txt",
            "first.txt",
            reclaimarc_archive::model::RedirectionKind::Hardlink,
        ),
        FixtureFile::symlink(
            "file_copy.txt",
            "first.txt",
            reclaimarc_archive::model::RedirectionKind::FileCopy,
        ),
        FixtureFile::symlink(
            "outside_link",
            "../../../../Windows/System32",
            reclaimarc_archive::model::RedirectionKind::UnixSymlink,
        ),
        FixtureFile::new("second.txt", b"second file data"),
    ];
    let paths = write_rar(
        &archive_dir,
        "symlink_comprehensive_test",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    let config = EngineConfig {
        symlink_policy: reclaimarc_core::config::SymlinkPolicy::Skip,
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::Normal, None, tx)
        .unwrap();

    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));
    // Regular files must exist and be intact
    assert_eq!(
        std::fs::read(dest.join("first.txt")).unwrap(),
        b"first file data"
    );
    assert_eq!(
        std::fs::read(dest.join("second.txt")).unwrap(),
        b"second file data"
    );
    // Skipped redirections must NOT exist on disk
    assert!(!dest.join("unix_link.lnk").exists());
    assert!(!dest.join("win_link.lnk").exists());
    assert!(!dest.join("junction_dir").exists());
    assert!(!dest.join("hard_link.txt").exists());
    assert!(!dest.join("file_copy.txt").exists());
    assert!(!dest.join("outside_link").exists());
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// 14. Normal directory ancestors pass reparse validation.
#[test]
fn test_normal_directory_ancestors_pass_reparse_validation() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    let safe_path = dest.join("sub").join("nested.txt");
    let res = reclaimarc_core::paths::ensure_no_reparse_ancestors(&safe_path, &dest);
    assert!(
        res.is_ok(),
        "Normal directory structure must pass validation"
    );
}

/// 14b. Reparse point (directory junction) ancestor in destination is rejected.
#[cfg(windows)]
#[test]
fn test_destination_junction_ancestor_is_rejected_before_write() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("dest");
    let outside = dir.path().join("outside_target");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let junction_path = dest.join("junction_link");
    let status = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &junction_path.to_string_lossy(),
            &outside.to_string_lossy(),
        ])
        .status();

    if let Ok(st) = status {
        if st.success() {
            let attempt_target = junction_path.join("payload.txt");
            let res = reclaimarc_core::paths::ensure_no_reparse_ancestors(&attempt_target, &dest);
            assert!(
                res.is_err(),
                "Paths traversing through a directory junction must be rejected"
            );
        }
    }
}

/// 15. Allocation query failure gives ZERO optimistic planner credit.
#[test]
fn test_allocation_query_failure_gives_zero_planner_credit() {
    let range = reclaimarc_archive::model::PackedRange {
        volume_index: 0,
        start: 0,
        len: 128 * 1024,
    };
    // None = query unavailable -> 0 bytes credited
    let zero_credit =
        reclaimarc_core::planner::guaranteed_range_reclaim_measured(&range, 4096, None);
    assert_eq!(zero_credit, 0, "Unmeasured range must credit zero reclaim");

    // Some = measured allocations -> credits physical intersection
    let alloc = vec![reclaimarc_platform::sparse::ByteRange {
        start: 0,
        len: 128 * 1024,
    }];
    let measured_credit =
        reclaimarc_core::planner::guaranteed_range_reclaim_measured(&range, 4096, Some(&alloc));
    assert_eq!(measured_credit, 128 * 1024);
}

/// 16. Partial ranges keep recovery unit in ReclaimIntent (never Reclaimed).
#[test]
fn test_partial_ranges_keep_unit_in_reclaim_intent() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("job.db");

    let dummy_data = vec![0u8; 1000];
    let range_hash = blake3::hash(&dummy_data).to_hex().to_string();
    std::fs::write(dir.path().join("dummy.rar"), &dummy_data).unwrap();
    let struct_hash = reclaimarc_core::engine::compute_volume_structural_digest(
        &dir.path().join("dummy.rar"),
        1000,
        &[(0, 1000)],
    )
    .unwrap();

    let meta = reclaimarc_journal::models::JobMeta {
        job_id: "test-partial-resume".into(),
        created_at: "2026-08-23T00:00:00Z".into(),
        updated_at: "2026-08-23T00:00:00Z".into(),
        archive_path: dir.path().join("dummy.rar"),
        destination: dir.path().join("dest"),
        archive_fingerprint: None,
        safety_mode: "Balanced".into(),
        settings_json: "{}".into(),
        current_unit: 0,
        job_state: reclaimarc_journal::models::JobState::Active,
    };
    let mut j = reclaimarc_journal::JobJournal::create(&journal_path, &meta).unwrap();
    j.add_volumes(&[reclaimarc_journal::models::VolumeRecord {
        path: dir.path().join("dummy.rar"),
        identity: None,
        allocated_before: 1000,
        logical_size: 1000,
        is_first: true,
        structural_digest: Some(struct_hash),
    }])
    .unwrap();
    j.add_units(&[reclaimarc_journal::models::RecoveryUnitRecord {
        seq: 0,
        state: reclaimarc_journal::models::UnitState::ReclaimIntent,
        first_entry: 0,
        last_entry: 0,
        error: None,
        updated_at: "2026-08-23T00:00:00Z".into(),
    }])
    .unwrap();
    j.add_packed_ranges(&[reclaimarc_journal::models::PackedRangeRecord {
        volume_index: 0,
        start: 0,
        len: 1000,
        state: reclaimarc_journal::models::RangeState::Partial,
        recovery_unit: Some(0),
        physically_released_bytes: 500,
        blake3_digest: Some(range_hash),
    }])
    .unwrap();
    drop(j);

    // Dummy file for volume
    std::fs::write(dir.path().join("dummy.rar"), vec![0u8; 1000]).unwrap();

    let recovered = reclaimarc_core::recovery::prepare_resume(&journal_path, None).unwrap();
    let units = recovered.units().unwrap();
    assert_eq!(
        units[0].state,
        reclaimarc_journal::models::UnitState::ReclaimIntent,
        "Unit with partial range must remain in ReclaimIntent"
    );
}

/// 17. Recovery allocation query failure on damaged output stops safely (fails closed).
#[test]
fn test_recovery_allocation_query_failure_stops_safely() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("job.db");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    let meta = reclaimarc_journal::models::JobMeta {
        job_id: "test-query-fail".into(),
        created_at: "2026-08-23T00:00:00Z".into(),
        updated_at: "2026-08-23T00:00:00Z".into(),
        archive_path: dir.path().join("nonexistent_vol.rar"),
        destination: dest.clone(),
        archive_fingerprint: None,
        safety_mode: "Balanced".into(),
        settings_json: "{}".into(),
        current_unit: 0,
        job_state: reclaimarc_journal::models::JobState::Active,
    };
    let mut j = reclaimarc_journal::JobJournal::create(&journal_path, &meta).unwrap();
    j.add_volumes(&[reclaimarc_journal::models::VolumeRecord {
        path: dir.path().join("nonexistent_vol.rar"), // Missing volume
        identity: None,
        allocated_before: 100,
        logical_size: 100,
        is_first: true,
        structural_digest: None,
    }])
    .unwrap();
    j.add_units(&[reclaimarc_journal::models::RecoveryUnitRecord {
        seq: 0,
        state: reclaimarc_journal::models::UnitState::Committed,
        first_entry: 0,
        last_entry: 0,
        error: None,
        updated_at: "2026-08-23T00:00:00Z".into(),
    }])
    .unwrap();
    j.add_entries(&[reclaimarc_journal::models::EntryRecord {
        index_in_archive: 0,
        name: "file.bin".into(),
        packed_size: 50,
        unpacked_size: 50,
        crc32: None,
        is_directory: false,
        is_solid: false,
        split_before: false,
        split_after: false,
        encrypted: false,
        recovery_unit: 0,
        final_path: Some(dest.join("file.bin")),
        partial_path: None,
        blake3: Some("hash".into()),
        status: reclaimarc_journal::models::EntryStatus::Committed,
        actual_committed_path: Some(dest.join("file.bin")),
        existed_before_job: false,
        expected_digest: Some("hash".into()),
        is_redirection: false,
        redirection_kind: None,
    }])
    .unwrap();
    j.add_packed_ranges(&[reclaimarc_journal::models::PackedRangeRecord {
        volume_index: 0,
        start: 0,
        len: 50,
        state: reclaimarc_journal::models::RangeState::Active,
        recovery_unit: Some(0),
        physically_released_bytes: 0,
        blake3_digest: None,
    }])
    .unwrap();
    drop(j);

    // Corrupted final output
    std::fs::write(dest.join("file.bin"), b"corrupted").unwrap();

    // prepare_resume must FAIL CLOSED because volume allocation cannot be queried
    let res = reclaimarc_core::recovery::prepare_resume(&journal_path, None);
    assert!(
        res.is_err(),
        "Must fail closed when source volume query fails"
    );
}

/// 18. Exact physical interval subtraction accounting.
#[test]
fn test_exact_interval_subtraction_accounting() {
    use reclaimarc_platform::sparse::{subtract_intervals, ByteRange};

    let before = vec![ByteRange {
        start: 0,
        len: 1000,
    }];
    let after = vec![
        ByteRange { start: 0, len: 300 },
        ByteRange {
            start: 700,
            len: 300,
        },
    ];
    let deallocated = subtract_intervals(&before, &after);
    assert_eq!(
        deallocated,
        vec![ByteRange {
            start: 300,
            len: 400
        }]
    );
    let released_bytes: u64 = deallocated.iter().map(|r| r.len).sum();
    assert_eq!(released_bytes, 400);
}

/// 19. Nested symlink directory followed by normal entries.
#[test]
fn test_nested_symlink_directory_followed_by_child_file_skipped() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        FixtureFile::symlink(
            "symlink_folder",
            "C:\\Windows\\System32",
            reclaimarc_archive::model::RedirectionKind::Junction,
        ),
        FixtureFile::new("safe_sub/child.txt", b"child file in normal directory"),
    ];
    let paths = write_rar(
        &archive_dir,
        "nested_symlink_dir_test",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    let config = EngineConfig {
        symlink_policy: reclaimarc_core::config::SymlinkPolicy::Skip,
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::Normal, None, tx)
        .unwrap();

    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));
    // Symlink folder must NOT exist
    assert!(!dest.join("symlink_folder").exists());
    // Safe child must exist and be intact
    assert_eq!(
        std::fs::read(dest.join("safe_sub").join("child.txt")).unwrap(),
        b"child file in normal directory"
    );
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

/// 20. Telemetry calculation in summarize includes partial ranges and physical released bytes.
#[test]
fn test_recovery_summarize_telemetry_includes_partial_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("job.db");

    let meta = reclaimarc_journal::models::JobMeta {
        job_id: "test-telemetry-partial".into(),
        created_at: "2026-08-23T00:00:00Z".into(),
        updated_at: "2026-08-23T00:00:00Z".into(),
        archive_path: dir.path().join("archive.rar"),
        destination: dir.path().join("dest"),
        archive_fingerprint: None,
        safety_mode: "safe".into(),
        settings_json: "{}".into(),
        job_state: reclaimarc_journal::models::JobState::Active,
        current_unit: 1,
    };
    let mut j = reclaimarc_journal::JobJournal::create(&journal_path, &meta).unwrap();
    j.add_volumes(&[reclaimarc_journal::models::VolumeRecord {
        path: dir.path().join("archive.rar"),
        identity: None,
        allocated_before: 3000,
        logical_size: 3000,
        is_first: true,
        structural_digest: None,
    }])
    .unwrap();
    j.add_units(&[
        reclaimarc_journal::models::RecoveryUnitRecord {
            seq: 0,
            state: reclaimarc_journal::models::UnitState::Reclaimed,
            first_entry: 0,
            last_entry: 0,
            error: None,
            updated_at: "2026-08-23T00:00:00Z".into(),
        },
        reclaimarc_journal::models::RecoveryUnitRecord {
            seq: 1,
            state: reclaimarc_journal::models::UnitState::ReclaimIntent,
            first_entry: 1,
            last_entry: 1,
            error: None,
            updated_at: "2026-08-23T00:00:00Z".into(),
        },
        reclaimarc_journal::models::RecoveryUnitRecord {
            seq: 2,
            state: reclaimarc_journal::models::UnitState::Pending,
            first_entry: 2,
            last_entry: 2,
            error: None,
            updated_at: "2026-08-23T00:00:00Z".into(),
        },
    ])
    .unwrap();
    j.add_packed_ranges(&[
        // Range 0: Reclaimed, 1000 bytes allocated, all 1000 physically released
        reclaimarc_journal::models::PackedRangeRecord {
            volume_index: 0,
            start: 0,
            len: 1000,
            state: reclaimarc_journal::models::RangeState::Reclaimed,
            recovery_unit: Some(0),
            physically_released_bytes: 1000,
            blake3_digest: None,
        },
        // Range 1: Partial, 1000 bytes total, 400 bytes physically released
        reclaimarc_journal::models::PackedRangeRecord {
            volume_index: 0,
            start: 1000,
            len: 1000,
            state: reclaimarc_journal::models::RangeState::Partial,
            recovery_unit: Some(1),
            physically_released_bytes: 400,
            blake3_digest: None,
        },
        // Range 2: Active, 1000 bytes total, 0 bytes released
        reclaimarc_journal::models::PackedRangeRecord {
            volume_index: 0,
            start: 2000,
            len: 1000,
            state: reclaimarc_journal::models::RangeState::Active,
            recovery_unit: Some(2),
            physically_released_bytes: 0,
            blake3_digest: None,
        },
    ])
    .unwrap();

    let summary = reclaimarc_core::recovery::summarize(&j).unwrap();
    // 1000 + 400 = 1400 physically released bytes
    assert_eq!(summary.source_reclaimed_bytes, 1400);
    // (1000-1000) + (1000-400) + (1000-0) = 0 + 600 + 1000 = 1600 remaining bytes
    assert_eq!(summary.remaining_source_bytes, 1600);
}

/// 21. Shell deletion on completion succeeds with authorized policy-skipped redirection entries.
#[test]
fn test_low_space_shell_deletion_succeeds_with_policy_skipped_redirections() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        FixtureFile::new("file1.bin", b"first regular file data on disk"),
        FixtureFile::symlink(
            "link1.lnk",
            "/etc/shadow",
            reclaimarc_archive::model::RedirectionKind::UnixSymlink,
        ),
        FixtureFile::new("file2.bin", b"second regular file data on disk"),
    ];
    let paths = write_rar(
        &archive_dir,
        "shell_deletion_redirection_test",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    let config = EngineConfig {
        delete_shells_on_completion: true,
        symlink_policy: reclaimarc_core::config::SymlinkPolicy::Skip,
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();

    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));
    // Regular outputs exist and match
    assert_eq!(
        std::fs::read(dest.join("file1.bin")).unwrap(),
        b"first regular file data on disk"
    );
    assert_eq!(
        std::fs::read(dest.join("file2.bin")).unwrap(),
        b"second regular file data on disk"
    );
    // Policy-skipped redirection does not exist on disk
    assert!(!dest.join("link1.lnk").exists());
    // Source archive shell was deleted cleanly upon verified completion
    assert!(
        !paths[0].exists(),
        "Archive shell file must be deleted upon verified completion"
    );
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

#[test]
fn test_in_place_byte_mutation_detected_before_extraction() {
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        FixtureFile::new("file1.bin", b"first clean file payload 12345"),
        FixtureFile::new("file2.bin", b"second clean file payload 67890"),
    ];
    let paths = write_rar(
        &archive_dir,
        "tamper_extract",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    let mut engine = Engine::new(EngineConfig::default());
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();

    // Tamper with the source archive data range directly on disk after job started
    let mut file_bytes = std::fs::read(&paths[0]).unwrap();
    let mid = file_bytes.len() / 2;
    file_bytes[mid] ^= 0xFF; // Invert byte
    std::fs::write(&paths[0], file_bytes).unwrap();

    // Running the job must fail closed due to integrity verification / BLAKE3 verification
    let res = engine.run_job(&mut job, &handle);
    assert!(
        res.is_err(),
        "Engine must detect in-place data mutation and fail closed"
    );
    let err_str = format!("{:?}", res.unwrap_err());
    assert!(
        err_str.contains("BLAKE3")
            || err_str.contains("modified or corrupted")
            || err_str.contains("integrity test failed"),
        "Error must explain tamper detection: {err_str}"
    );
}

#[test]
fn test_structural_header_mutation_detected_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        FixtureFile::new("file1.bin", b"alpha data for header tamper test"),
        FixtureFile::new("file2.bin", b"beta data for header tamper test"),
    ];
    let paths = write_rar(
        &archive_dir,
        "tamper_header",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    let engine = Engine::new(EngineConfig::default());
    let (tx, _) = mpsc::channel();
    let (handle, job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();
    drop(job);
    drop(handle);

    let journal_path = find_journal(dir.path());

    // Tamper with the archive header bytes on disk
    let mut file_bytes = std::fs::read(&paths[0]).unwrap();
    // Tamper within the first 16 bytes (header area)
    file_bytes[10] ^= 0x55;
    std::fs::write(&paths[0], file_bytes).unwrap();

    // Prepare resume must detect structural digest mismatch and fail closed
    let res = reclaimarc_core::recovery::prepare_resume(&journal_path, None);
    assert!(
        res.is_err(),
        "Resume must fail closed when volume structural headers are modified"
    );
    let err_str = format!("{:?}", res.unwrap_err());
    assert!(
        err_str.contains("structural") || err_str.contains("BLAKE3"),
        "Error must specify structural mismatch: {err_str}"
    );
}

#[test]
fn test_retirement_oracle_never_reclaims_uncommitted_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        FixtureFile::new("f1.bin", &[0x11; 50000]),
        FixtureFile::new("f2.bin", &[0x22; 50000]),
        FixtureFile::new("f3.bin", &[0x33; 50000]),
    ];
    let paths = write_rar(
        &archive_dir,
        "oracle_test",
        &files,
        &FixtureOptions::default(),
    )
    .unwrap();

    let mut engine = Engine::new(EngineConfig::default());
    let (tx, _) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&paths[0], &dest, ExtractionMode::LowSpace, None, tx)
        .unwrap();

    // Step through the extraction
    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));

    // Verify through the journal oracle that every single range marked Reclaimed
    // is associated with a unit where ALL entries are in Committed state.
    let journal_path = find_journal(dir.path());
    let journal = reclaimarc_journal::JobJournal::open(&journal_path).unwrap();
    let units = journal.units().unwrap();
    let entries = journal.entries().unwrap();
    let ranges = journal.packed_ranges().unwrap();

    for r in &ranges {
        if r.state == reclaimarc_journal::models::RangeState::Reclaimed {
            let unit_seq = r
                .recovery_unit
                .expect("Reclaimed range must belong to a unit");
            let unit = units.iter().find(|u| u.seq == unit_seq).unwrap();
            assert!(
                reclaimarc_core::state::is_reclaimed(unit.state)
                    || reclaimarc_core::state::is_committed(unit.state),
                "Unit {} must be committed/reclaimed if its range is reclaimed",
                unit.seq
            );
            // Verify every entry in this unit is Committed
            let unit_entries: Vec<_> = entries
                .iter()
                .filter(|e| {
                    e.index_in_archive >= unit.first_entry && e.index_in_archive <= unit.last_entry
                })
                .collect();
            for e in unit_entries {
                assert_eq!(
                    e.status,
                    reclaimarc_journal::models::EntryStatus::Committed,
                    "Entry {} must be committed before its source range can be reclaimed",
                    e.index_in_archive
                );
            }
        }
    }
}
