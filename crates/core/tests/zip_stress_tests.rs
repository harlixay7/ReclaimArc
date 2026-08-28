//! Real-world intensive ZIP stress tests covering large entry counts,
//! multi-megabyte deflated/stored payloads, ZIP64 scale, progressive physical
//! NTFS sparse hole punching, and verification.

use std::collections::HashMap;
use std::sync::mpsc;
use tempfile::tempdir;

use reclaimarc_archive::zip::fixtures::{write_zip, ZipFixtureFile, ZipFixtureOptions};
use reclaimarc_core::{Engine, EngineConfig, ExtractionMode, JobOutcome};

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Generate pseudo-random deterministic test data with controllable compressibility.
fn generate_test_data(seed: usize, len: usize, compressible: bool) -> Vec<u8> {
    if compressible {
        let pattern = format!("ReclaimArc-Compression-Block-{seed:08x}-");
        let pattern_bytes = pattern.as_bytes();
        let mut data = Vec::with_capacity(len);
        while data.len() < len {
            let chunk = (len - data.len()).min(pattern_bytes.len());
            data.extend_from_slice(&pattern_bytes[..chunk]);
        }
        data
    } else {
        let mut state = seed as u64 ^ 0x5DEECE66D;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }
}

#[test]
fn test_real_zip_massive_multi_entry_stress_low_space() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest_dir = dir.path().join("dest");
    let app_data = dir.path().join("appdata");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    std::fs::create_dir_all(&app_data).unwrap();

    std::env::set_var("RECLAIMARC_APP_DATA", &app_data);
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");

    let num_files = 150;
    let mut files = Vec::with_capacity(num_files);
    let mut expected_payloads = HashMap::with_capacity(num_files);

    // Create 150 entries across 20 directory trees with varied sizes
    for i in 0..num_files {
        let dir_idx = i % 20;
        let is_deflate = (i % 2) == 0;
        let is_compressible = (i % 3) != 0;
        let size = match i % 6 {
            0 => 64,
            1 => 1024,
            2 => 8192,
            3 => 131_072,
            4 => 262_144,
            _ => 524_288,
        };

        let data = generate_test_data(i, size, is_compressible);
        let path_name = format!("folder_{dir_idx:02}/sub_{i:04}/file_{i:04}.bin");

        expected_payloads.insert(path_name.clone(), data.clone());
        if is_deflate {
            files.push(ZipFixtureFile::deflated(&path_name, &data));
        } else {
            files.push(ZipFixtureFile::stored(&path_name, &data));
        }
    }

    let zip_path = archive_dir.join("massive_150.zip");
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let mut engine = Engine::new(EngineConfig {
        pre_test: false,
        ..Default::default()
    });

    let (tx, _rx) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&zip_path, &dest_dir, ExtractionMode::LowSpace, None, tx)
        .expect("job must start cleanly");

    let outcome = engine
        .run_job(&mut job, &handle)
        .expect("extraction must succeed");

    assert!(
        matches!(outcome, JobOutcome::Completed { .. }),
        "expected JobOutcome::Completed, got {outcome:?}"
    );

    // 1. Verify byte-exact fidelity of all 1,000 files
    for (rel_path, expected_bytes) in &expected_payloads {
        let extracted_path = dest_dir.join(rel_path);
        assert!(
            extracted_path.exists(),
            "extracted file '{}' must exist on disk",
            extracted_path.display()
        );
        let actual_bytes = std::fs::read(&extracted_path).expect("read extracted file");
        assert_eq!(
            actual_bytes, *expected_bytes,
            "byte mismatch on '{}'",
            rel_path
        );
    }

    // 2. Verify source physical deallocation occurred
    let file = reclaimarc_platform::sparse::open_for_reclaim(&zip_path).unwrap();
    let allocated = reclaimarc_platform::sparse::query_allocated_ranges(
        &file,
        &zip_path,
        0,
        std::fs::metadata(&zip_path).unwrap().len(),
    )
    .unwrap();
    let total_alloc: u64 = allocated.iter().map(|r| r.len).sum();
    let logical = std::fs::metadata(&zip_path).unwrap().len();
    assert!(
        total_alloc < logical,
        "source allocated size ({total_alloc}) must drop below logical size ({logical})"
    );

    std::env::remove_var("RECLAIMARC_APP_DATA");
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

#[test]
fn test_real_zip_large_payload_reclaim_stress() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest_dir = dir.path().join("dest");
    let app_data = dir.path().join("appdata");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    std::fs::create_dir_all(&app_data).unwrap();

    std::env::set_var("RECLAIMARC_APP_DATA", &app_data);
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");

    let num_large_files = 6;
    let file_size = 2_000_000; // 2 MB each -> 12 MB payload
    let mut files = Vec::new();
    let mut expected_payloads = HashMap::new();

    for i in 0..num_large_files {
        let is_deflate = (i % 2) == 0;
        let data = generate_test_data(i * 100, file_size, true);
        let name = format!("large_payload_{i}.dat");
        expected_payloads.insert(name.clone(), data.clone());

        if is_deflate {
            files.push(ZipFixtureFile::deflated(&name, &data));
        } else {
            files.push(ZipFixtureFile::stored(&name, &data));
        }
    }

    let zip_path = archive_dir.join("large_payloads.zip");
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let mut engine = Engine::new(EngineConfig {
        pre_test: false,
        ..Default::default()
    });

    let (tx, _rx) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&zip_path, &dest_dir, ExtractionMode::LowSpace, None, tx)
        .expect("job must start cleanly");

    let outcome = engine
        .run_job(&mut job, &handle)
        .expect("extraction must succeed");

    assert!(
        matches!(outcome, JobOutcome::Completed { .. }),
        "expected JobOutcome::Completed, got {outcome:?}"
    );

    // Verify all large files
    for (name, expected_bytes) in &expected_payloads {
        let actual = std::fs::read(dest_dir.join(name)).expect("read large file");
        assert_eq!(actual.len(), expected_bytes.len());
        assert_eq!(blake3::hash(&actual), blake3::hash(expected_bytes));
    }

    // Verify physical sparse reclaim
    let file = reclaimarc_platform::sparse::open_for_reclaim(&zip_path).unwrap();
    let allocated = reclaimarc_platform::sparse::query_allocated_ranges(
        &file,
        &zip_path,
        0,
        std::fs::metadata(&zip_path).unwrap().len(),
    )
    .unwrap();
    let total_alloc: u64 = allocated.iter().map(|r| r.len).sum();
    let logical = std::fs::metadata(&zip_path).unwrap().len();
    assert!(
        total_alloc < logical,
        "source allocation ({total_alloc}) must be smaller than logical size ({logical})"
    );

    std::env::remove_var("RECLAIMARC_APP_DATA");
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

#[test]
fn test_real_zip64_large_scale_stress() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest_dir = dir.path().join("dest");
    let app_data = dir.path().join("appdata");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    std::fs::create_dir_all(&app_data).unwrap();

    std::env::set_var("RECLAIMARC_APP_DATA", &app_data);
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");

    let num_files = 100;
    let mut files = Vec::new();
    let mut expected_payloads = HashMap::new();

    for i in 0..num_files {
        let data = generate_test_data(i + 500, 30_000, true);
        let name = format!("zip64_tree/entry_{i:03}.bin");
        expected_payloads.insert(name.clone(), data.clone());
        if i % 2 == 0 {
            files.push(ZipFixtureFile::deflated(&name, &data));
        } else {
            files.push(ZipFixtureFile::stored(&name, &data));
        }
    }

    let zip_path = archive_dir.join("zip64_stress.zip");
    write_zip(
        &zip_path,
        &files,
        &ZipFixtureOptions {
            force_zip64: true,
            comment: Some("ZIP64 Stress Test Archive".to_string()),
        },
    )
    .unwrap();

    let mut engine = Engine::new(EngineConfig {
        pre_test: false,
        ..Default::default()
    });

    let (tx, _rx) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&zip_path, &dest_dir, ExtractionMode::LowSpace, None, tx)
        .expect("job must start cleanly");

    let outcome = engine
        .run_job(&mut job, &handle)
        .expect("extraction must succeed");

    assert!(
        matches!(outcome, JobOutcome::Completed { .. }),
        "expected JobOutcome::Completed, got {outcome:?}"
    );

    for (name, expected_bytes) in &expected_payloads {
        let actual = std::fs::read(dest_dir.join(name)).expect("read zip64 entry");
        assert_eq!(actual, *expected_bytes);
    }

    std::env::remove_var("RECLAIMARC_APP_DATA");
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}

#[test]
fn test_real_zip_stress_mid_flight_interruption_and_resume() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempdir().unwrap();
    let archive_dir = dir.path().join("archive");
    let dest_dir = dir.path().join("dest");
    let app_data = dir.path().join("appdata");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    std::fs::create_dir_all(&app_data).unwrap();

    std::env::set_var("RECLAIMARC_APP_DATA", &app_data);
    std::env::set_var("RECLAIMARC_TEST_FREE_SPACE", "100000000000");

    let num_files = 80;
    let mut files = Vec::new();
    let mut expected_payloads = HashMap::new();

    for i in 0..num_files {
        let size = 100_000; // 100 KB each -> 8 MB archive
        let data = generate_test_data(i + 1000, size, true);
        let name = format!("resume_stress/entry_{i:03}.dat");
        expected_payloads.insert(name.clone(), data.clone());
        if i % 2 == 0 {
            files.push(ZipFixtureFile::deflated(&name, &data));
        } else {
            files.push(ZipFixtureFile::stored(&name, &data));
        }
    }

    let zip_path = archive_dir.join("resume_stress.zip");
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let mut engine = Engine::new(EngineConfig {
        pre_test: false,
        ..Default::default()
    });

    let (tx, rx) = mpsc::channel();
    let (handle, mut job) = engine
        .start_job(&zip_path, &dest_dir, ExtractionMode::LowSpace, None, tx)
        .expect("job must start cleanly");

    let handle_clone = handle.clone();
    std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            if let reclaimarc_core::Event::UnitCommitted { seq, .. } = event {
                if seq == 35 {
                    handle_clone.pause();
                    break;
                }
            }
        }
    });

    let outcome_1 = engine
        .run_job(&mut job, &handle)
        .expect("first run must handle pause");

    assert!(
        matches!(outcome_1, JobOutcome::Paused),
        "expected JobOutcome::Paused, got {outcome_1:?}"
    );

    let job_id = job.job_id.clone();
    drop(job);

    let journal_path = archive_dir.join(".reclaimarc").join(&job_id).join("job.db");

    // Re-open and resume the job
    let (tx2, _rx2) = mpsc::channel();
    let (handle2, mut job2) = engine
        .resume_job(&journal_path, None, tx2)
        .expect("resume_job must succeed");

    let outcome_2 = engine
        .run_job(&mut job2, &handle2)
        .expect("resumed run must succeed");

    assert!(
        matches!(outcome_2, JobOutcome::Completed { .. }),
        "expected JobOutcome::Completed, got {outcome_2:?}"
    );

    // Verify all 80 files match exact source bytes
    for (name, expected_bytes) in &expected_payloads {
        let actual = std::fs::read(dest_dir.join(name)).expect("read resumed entry");
        assert_eq!(blake3::hash(&actual), blake3::hash(expected_bytes));
    }

    // Verify physical sparse reclaim on the resumed archive
    let file = reclaimarc_platform::sparse::open_for_reclaim(&zip_path).unwrap();
    let allocated = reclaimarc_platform::sparse::query_allocated_ranges(
        &file,
        &zip_path,
        0,
        std::fs::metadata(&zip_path).unwrap().len(),
    )
    .unwrap();
    let total_alloc: u64 = allocated.iter().map(|r| r.len).sum();
    let logical = std::fs::metadata(&zip_path).unwrap().len();
    assert!(
        total_alloc < logical,
        "source allocation ({total_alloc}) must be smaller than logical size ({logical})"
    );

    std::env::remove_var("RECLAIMARC_APP_DATA");
    std::env::remove_var("RECLAIMARC_TEST_FREE_SPACE");
}
