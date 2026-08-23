use reclaimarc_archive::backend::{ArchiveBackend, OpenOptions};
use reclaimarc_archive::rar::backend::RarBackend;
use reclaimarc_archive::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};

#[test]
fn test_corpus_rar4_single_and_multi_entry() {
    let dir = tempfile::tempdir().unwrap();
    let files = vec![
        FixtureFile::new("alpha.txt", b"Hello World from RAR4 alpha!"),
        FixtureFile::new("beta.bin", &[0xAA; 10000]),
        FixtureFile::new("gamma/delta.txt", b"Nested file in RAR4 gamma directory"),
    ];
    let opts = FixtureOptions {
        rar5: false,
        ..Default::default()
    };
    let paths = write_rar(dir.path(), "corpus_rar4", &files, &opts).unwrap();
    assert_eq!(paths.len(), 1);

    let mut backend = RarBackend::new(&paths[0]);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    assert_eq!(info.format, "rar4");
    assert_eq!(info.entries.len(), 3);
    assert_eq!(info.recovery_units.len(), 3);

    let report = backend.test_integrity(None, None, None).unwrap();
    assert!(report.ok);
    assert!(report.first_failure.is_none());
}

#[test]
fn test_corpus_rar5_solid_and_service_headers() {
    let dir = tempfile::tempdir().unwrap();
    let files = vec![
        FixtureFile::new("file1.dat", &[0x11; 4096]),
        FixtureFile::new("file2.dat", &[0x22; 8192]),
        FixtureFile::new("file3.dat", &[0x33; 16384]),
    ];
    let opts = FixtureOptions {
        solid_archive: true,
        ..Default::default()
    };
    let paths = write_rar(dir.path(), "corpus_rar5_solid", &files, &opts).unwrap();

    let mut backend = RarBackend::new(&paths[0]);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    assert_eq!(info.format, "rar5");
    assert_eq!(info.entries.len(), 3);
    assert_eq!(info.recovery_units.len(), 1); // Solid archive must be exactly 1 unit
    assert_eq!(info.recovery_units[0].first_entry, 0);
    assert_eq!(info.recovery_units[0].last_entry, 2);

    let report = backend.test_integrity(None, None, None).unwrap();
    assert!(report.ok);
}

#[test]
fn test_corpus_rar5_multipart_volume_spanning() {
    let dir = tempfile::tempdir().unwrap();
    let files = vec![
        FixtureFile::new("small.txt", b"Initial small file"),
        FixtureFile::new("spanning.bin", &[0x77; 60000]),
        FixtureFile::new("tail.txt", b"Final tail file in multipart archive"),
    ];
    let opts = FixtureOptions {
        volume_size: Some(20000),
        ..Default::default()
    };
    let paths = write_rar(dir.path(), "corpus_multipart", &files, &opts).unwrap();
    assert!(paths.len() > 1, "Must produce multiple volumes");

    let mut backend = RarBackend::new(&paths[0]);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    assert_eq!(info.volumes.len(), paths.len());
    assert_eq!(info.entries.len(), 3);

    // Verify all ranges are within valid volume bounds
    for u in &info.recovery_units {
        for r in &u.packed_ranges {
            let vol = &info.volumes[r.volume_index as usize];
            assert!(
                r.start + r.len <= vol.logical_size,
                "Packed range [{}..{}] exceeds volume size {}",
                r.start,
                r.start + r.len,
                vol.logical_size
            );
        }
    }

    let report = backend.test_integrity(None, None, None).unwrap();
    assert!(report.ok);
}
