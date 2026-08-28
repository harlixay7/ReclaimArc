//! Engine-level extraction and streaming synchronization tests for ZIP archives.
//! Verifies mixed directory/file topologies in both Normal and Low-Space modes.

use std::sync::mpsc;
use tempfile::tempdir;

use reclaimarc_archive::zip::fixtures::{write_zip, ZipFixtureFile, ZipFixtureOptions};
use reclaimarc_core::{Engine, EngineConfig, ExtractionMode, JobOutcome};

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_zip_extraction(
    files: &[ZipFixtureFile],
    mode: ExtractionMode,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest_dir = dir.path().join("dest");
    let app_data = dir.path().join("appdata");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    std::fs::create_dir_all(&app_data).unwrap();

    let zip_path = archive_dir.join("test.zip");
    write_zip(&zip_path, files, &ZipFixtureOptions::default()).unwrap();

    std::env::set_var("RECLAIMARC_APP_DATA", &app_data);

    let mut engine = Engine::new(EngineConfig {
        pre_test: false,
        ..Default::default()
    });

    let (tx, _rx) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&zip_path, &dest_dir, mode, None, tx)
        .expect("job must start cleanly");

    let outcome = engine
        .run_job(&mut job, &handle)
        .expect("extraction must succeed");

    std::env::remove_var("RECLAIMARC_APP_DATA");

    assert!(
        matches!(outcome, JobOutcome::Completed { .. }),
        "expected JobOutcome::Completed, got {outcome:?}"
    );

    (dir, dest_dir)
}

#[test]
fn test_topology_file_dir_file_file() {
    let files = vec![
        ZipFixtureFile::stored("file1.txt", b"First file data"),
        ZipFixtureFile::dir("middle_dir"),
        ZipFixtureFile::deflated(
            "middle_dir/file2.txt",
            b"Second file inside directory with deflate data",
        ),
        ZipFixtureFile::stored("file3.txt", b"Third file after directory"),
    ];

    for mode in [ExtractionMode::Normal, ExtractionMode::LowSpace] {
        let (_dir, dest) = run_zip_extraction(&files, mode);
        assert_eq!(
            std::fs::read(dest.join("file1.txt")).unwrap(),
            b"First file data"
        );
        assert!(dest.join("middle_dir").is_dir());
        assert_eq!(
            std::fs::read(dest.join("middle_dir/file2.txt")).unwrap(),
            b"Second file inside directory with deflate data"
        );
        assert_eq!(
            std::fs::read(dest.join("file3.txt")).unwrap(),
            b"Third file after directory"
        );
    }
}

#[test]
fn test_topology_dir_file() {
    let files = vec![
        ZipFixtureFile::dir("root_dir"),
        ZipFixtureFile::stored("root_dir/item.txt", b"Item inside root directory"),
    ];

    for mode in [ExtractionMode::Normal, ExtractionMode::LowSpace] {
        let (_dir, dest) = run_zip_extraction(&files, mode);
        assert!(dest.join("root_dir").is_dir());
        assert_eq!(
            std::fs::read(dest.join("root_dir/item.txt")).unwrap(),
            b"Item inside root directory"
        );
    }
}

#[test]
fn test_topology_multiple_consecutive_directories() {
    let files = vec![
        ZipFixtureFile::dir("dir_a"),
        ZipFixtureFile::dir("dir_b"),
        ZipFixtureFile::dir("dir_b/sub_c"),
        ZipFixtureFile::deflated(
            "dir_b/sub_c/payload.bin",
            b"Payload nested after multiple consecutive directories",
        ),
    ];

    for mode in [ExtractionMode::Normal, ExtractionMode::LowSpace] {
        let (_dir, dest) = run_zip_extraction(&files, mode);
        assert!(dest.join("dir_a").is_dir());
        assert!(dest.join("dir_b").is_dir());
        assert!(dest.join("dir_b/sub_c").is_dir());
        assert_eq!(
            std::fs::read(dest.join("dir_b/sub_c/payload.bin")).unwrap(),
            b"Payload nested after multiple consecutive directories"
        );
    }
}

#[test]
fn test_topology_file_dir_at_end() {
    let files = vec![
        ZipFixtureFile::stored("first.txt", b"First data"),
        ZipFixtureFile::deflated("second.txt", b"Second data with some extra compressibility"),
        ZipFixtureFile::dir("trailing_dir"),
    ];

    for mode in [ExtractionMode::Normal, ExtractionMode::LowSpace] {
        let (_dir, dest) = run_zip_extraction(&files, mode);
        assert_eq!(
            std::fs::read(dest.join("first.txt")).unwrap(),
            b"First data"
        );
        assert_eq!(
            std::fs::read(dest.join("second.txt")).unwrap(),
            b"Second data with some extra compressibility"
        );
        assert!(dest.join("trailing_dir").is_dir());
    }
}

#[test]
fn test_topology_zero_byte_files_mixed_with_directories() {
    let files = vec![
        ZipFixtureFile::stored("zero1.txt", b""),
        ZipFixtureFile::dir("d1"),
        ZipFixtureFile::stored("d1/zero2.bin", b""),
        ZipFixtureFile::dir("d2"),
        ZipFixtureFile::deflated("d2/nonzero.txt", b"Non-zero payload content"),
        ZipFixtureFile::stored("zero3.dat", b""),
    ];

    for mode in [ExtractionMode::Normal, ExtractionMode::LowSpace] {
        let (_dir, dest) = run_zip_extraction(&files, mode);
        assert_eq!(std::fs::read(dest.join("zero1.txt")).unwrap(), b"");
        assert!(dest.join("d1").is_dir());
        assert_eq!(std::fs::read(dest.join("d1/zero2.bin")).unwrap(), b"");
        assert!(dest.join("d2").is_dir());
        assert_eq!(
            std::fs::read(dest.join("d2/nonzero.txt")).unwrap(),
            b"Non-zero payload content"
        );
        assert_eq!(std::fs::read(dest.join("zero3.dat")).unwrap(), b"");
    }
}
