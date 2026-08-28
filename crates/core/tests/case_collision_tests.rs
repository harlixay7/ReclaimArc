use std::sync::mpsc;
use std::sync::Mutex;
use tempfile::tempdir;

use reclaimarc_archive::zip::fixtures::{write_zip, ZipFixtureFile, ZipFixtureOptions};
use reclaimarc_core::config::{ConflictPolicy, EngineConfig};
use reclaimarc_core::engine::{Engine, ExtractionMode, JobOutcome};
use reclaimarc_core::error::CoreError;

static ENV_MUTEX: Mutex<()> = Mutex::new(());

#[test]
fn test_case_collision_fails_closed_by_default() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("case_collision.zip");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        ZipFixtureFile::stored("file.txt", b"lowercase content"),
        ZipFixtureFile::stored("FILE.TXT", b"UPPERCASE CONTENT"),
    ];
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    // 1. Default policy (Ask) -> fails closed in start_job
    let config = EngineConfig {
        conflict_policy: ConflictPolicy::Ask,
        ..Default::default()
    };
    let engine = Engine::new(config);
    let (tx, _rx) = mpsc::channel();

    let err = engine
        .start_job(&zip_path, &dest, ExtractionMode::Normal, None, tx.clone())
        .unwrap_err();
    assert!(
        matches!(err, CoreError::Precondition(_)),
        "Default policy must fail closed on case collision: {err:?}"
    );

    // 2. Overwrite policy -> also fails closed in start_job to prevent silent data loss
    let config_ow = EngineConfig {
        conflict_policy: ConflictPolicy::Overwrite,
        ..Default::default()
    };
    let engine_ow = Engine::new(config_ow);
    let err_ow = engine_ow
        .start_job(&zip_path, &dest, ExtractionMode::Normal, None, tx.clone())
        .unwrap_err();
    assert!(
        matches!(err_ow, CoreError::Precondition(_)),
        "Overwrite policy must fail closed on archive case collision: {err_ow:?}"
    );

    // 3. Skip policy -> also fails closed in start_job
    let config_skip = EngineConfig {
        conflict_policy: ConflictPolicy::Skip,
        ..Default::default()
    };
    let engine_skip = Engine::new(config_skip);
    let err_skip = engine_skip
        .start_job(&zip_path, &dest, ExtractionMode::Normal, None, tx)
        .unwrap_err();
    assert!(
        matches!(err_skip, CoreError::Precondition(_)),
        "Skip policy must fail closed on archive case collision: {err_skip:?}"
    );
}

#[test]
fn test_case_collision_disambiguates_under_rename_new() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("case_collision_rename.zip");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        ZipFixtureFile::stored("test.doc", b"first file"),
        ZipFixtureFile::stored("TEST.DOC", b"second file colliding"),
        ZipFixtureFile::stored("Test.Doc", b"third file colliding"),
    ];
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let config = EngineConfig {
        conflict_policy: ConflictPolicy::RenameNew,
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _rx) = mpsc::channel();

    let (handle, mut job) = engine
        .start_job(&zip_path, &dest, ExtractionMode::Normal, None, tx)
        .expect("start_job must succeed under RenameNew");

    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));

    // Verify all 3 files exist on disk with independent contents
    let file1 = dest.join("test.doc");
    let file2 = dest.join("TEST (case-collision-1).DOC");
    let file3 = dest.join("Test (case-collision-2).Doc");

    assert!(file1.exists(), "file1 must exist: {}", file1.display());
    assert!(file2.exists(), "file2 must exist: {}", file2.display());
    assert!(file3.exists(), "file3 must exist: {}", file3.display());

    assert_eq!(std::fs::read(&file1).unwrap(), b"first file");
    assert_eq!(std::fs::read(&file2).unwrap(), b"second file colliding");
    assert_eq!(std::fs::read(&file3).unwrap(), b"third file colliding");
}

#[test]
fn test_case_collision_nested_directories_under_rename_new() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("case_collision_nested.zip");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    let files = vec![
        ZipFixtureFile::stored("docs/README.md", b"# Upper README"),
        ZipFixtureFile::stored("docs/readme.md", b"# Lower readme"),
    ];
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let config = EngineConfig {
        conflict_policy: ConflictPolicy::RenameNew,
        ..Default::default()
    };
    let mut engine = Engine::new(config);
    let (tx, _rx) = mpsc::channel();

    let (handle, mut job) = engine
        .start_job(&zip_path, &dest, ExtractionMode::LowSpace, None, tx)
        .expect("start_job must succeed in LowSpace under RenameNew");

    let outcome = engine.run_job(&mut job, &handle).unwrap();
    assert!(matches!(outcome, JobOutcome::Completed { .. }));

    let file1 = dest.join("docs").join("README.md");
    let file2 = dest.join("docs").join("readme (case-collision-1).md");

    assert!(file1.exists(), "file1 must exist: {}", file1.display());
    assert!(file2.exists(), "file2 must exist: {}", file2.display());

    assert_eq!(std::fs::read(&file1).unwrap(), b"# Upper README");
    assert_eq!(std::fs::read(&file2).unwrap(), b"# Lower readme");
}
