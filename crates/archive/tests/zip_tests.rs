//! Comprehensive integration, structural, and adversarial tests for ZIP / ZIP64.

use std::collections::HashMap;
use tempfile::tempdir;

use reclaimarc_archive::backend::{ArchiveBackend, ExtractOptions, OpenOptions};
use reclaimarc_archive::zip::backend::ZipBackend;
use reclaimarc_archive::zip::fixtures::{
    write_corrupt_crc_zip, write_data_descriptor_zip, write_invalid_data_descriptor_zip,
    write_overlapping_zip, write_real_unix_symlink_zip, write_sfx_zip, write_zip, ZipFixtureFile,
    ZipFixtureOptions,
};

#[test]
fn test_zip_stored_and_deflated_inspection() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("mixed.zip");

    let files = vec![
        ZipFixtureFile::stored("hello_stored.txt", b"Hello Stored World!"),
        ZipFixtureFile::deflated(
            "hello_deflated.txt",
            b"Hello Deflated World! Repeating repeating repeating repeating repeating!",
        ),
        ZipFixtureFile::stored("empty.bin", b""),
        ZipFixtureFile::dir("subdir"),
        ZipFixtureFile::stored("subdir/nested.txt", b"Nested content"),
    ];

    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let mut backend = ZipBackend::new(&zip_path);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    assert_eq!(info.format, "zip");
    assert_eq!(info.entries.len(), 5);
    assert!(!info.solid_archive);
    assert!(info.capability.supports_test_integrity);
    assert!(info.capability.restartable_units);
    assert!(info.capability.progressive_reclaim);

    // Verify recovery units (1 unit per file/entry)
    assert_eq!(info.recovery_units.len(), 5);
    for (i, u) in info.recovery_units.iter().enumerate() {
        assert_eq!(u.seq, i as u64);
        assert_eq!(u.first_entry, i as u64);
        assert_eq!(u.last_entry, i as u64);
    }

    // Verify retirement proofs exist for non-empty files
    let proofs = backend.retirement_proofs();
    assert!(!proofs.is_empty());
}

#[test]
fn test_zip_integrity_test_valid_and_corrupt() {
    let dir = tempdir().unwrap();

    // 1. Valid ZIP
    let valid_path = dir.path().join("valid.zip");
    let files = vec![
        ZipFixtureFile::stored("file1.txt", b"Content 1"),
        ZipFixtureFile::deflated(
            "file2.txt",
            b"Content 2 with some repetitive text repetitive text",
        ),
    ];
    write_zip(&valid_path, &files, &ZipFixtureOptions::default()).unwrap();

    let mut valid_backend = ZipBackend::new(&valid_path);
    let report = valid_backend.test_integrity(None, None, None).unwrap();
    assert!(report.ok);
    assert!(report.bytes_tested > 0);
    assert_eq!(report.first_failure, None);

    // 2. Corrupt ZIP
    let corrupt_path = dir.path().join("corrupt.zip");
    write_corrupt_crc_zip(&corrupt_path).unwrap();

    let mut corrupt_backend = ZipBackend::new(&corrupt_path);
    let corrupt_report = corrupt_backend.test_integrity(None, None, None).unwrap();
    assert!(!corrupt_report.ok);
    assert_eq!(corrupt_report.first_failure, Some(0));
}

#[test]
fn test_zip_extract_unit_and_streaming() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("extract_test.zip");
    let dest_dir = dir.path().join("output");
    std::fs::create_dir_all(&dest_dir).unwrap();

    let files = vec![
        ZipFixtureFile::stored("file_a.txt", b"Alpha content"),
        ZipFixtureFile::deflated(
            "file_b.txt",
            b"Beta content repeating for compression testing",
        ),
        ZipFixtureFile::dir("sub"),
        ZipFixtureFile::stored("sub/file_c.txt", b"Gamma content inside subfolder"),
    ];
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let mut backend = ZipBackend::new(&zip_path);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    let mut name_map = HashMap::new();
    for e in &info.entries {
        if !e.is_directory {
            name_map.insert(e.index, e.name.clone());
        }
    }

    let options = ExtractOptions {
        dest_dir: dest_dir.clone(),
        job_id: "testjob".into(),
        partial_suffix: ".test-partial".into(),
        password: None,
        cancel: None,
        name_map,
        max_compression_ratio: None,
    };

    // Test extracting recovery unit 0 (file_a.txt)
    let report = backend.extract_unit(0, &options, None).unwrap();
    assert_eq!(report.extracted, vec![0]);
    assert_eq!(report.bytes_written, 13);

    let partial_a = dest_dir.join("file_a.txt.test-partial");
    assert!(partial_a.exists());
    assert_eq!(std::fs::read(&partial_a).unwrap(), b"Alpha content");

    // Test streaming extraction for remaining entries with 1-to-1 stepping:
    // Entry 1: file_b.txt (extracted)
    // Entry 2: sub/ (directory, advanced 1 entry, partial_path is None)
    // Entry 3: sub/file_c.txt (extracted)
    backend.begin_extraction(&options, 1).unwrap();

    let next1 = backend.extract_next(&options, None).unwrap();
    assert!(next1.is_some());
    let f1 = next1.unwrap();
    assert_eq!(f1.index, 1);
    assert!(f1.partial_path.is_some());
    let partial_b = f1.partial_path.unwrap();
    assert!(partial_b.exists());
    assert_eq!(
        std::fs::read(&partial_b).unwrap(),
        b"Beta content repeating for compression testing"
    );

    // Entry 2 is directory -> MUST advance exactly 1 entry and return partial_path: None
    let next2 = backend.extract_next(&options, None).unwrap();
    assert!(next2.is_some());
    let f2 = next2.unwrap();
    assert_eq!(f2.index, 2);
    assert!(f2.partial_path.is_none());

    // Entry 3 is file_c.txt
    let next3 = backend.extract_next(&options, None).unwrap();
    assert!(next3.is_some());
    let f3 = next3.unwrap();
    assert_eq!(f3.index, 3);
    assert!(f3.partial_path.is_some());
    let partial_c = f3.partial_path.unwrap();
    assert!(partial_c.exists());
    assert_eq!(
        std::fs::read(&partial_c).unwrap(),
        b"Gamma content inside subfolder"
    );

    let next4 = backend.extract_next(&options, None).unwrap();
    assert!(next4.is_none());
}

#[test]
fn test_zip_data_descriptor_validation_valid_and_invalid() {
    let dir = tempdir().unwrap();

    // 1. Valid descriptor with signature
    let p_sig = dir.path().join("desc_sig.zip");
    write_data_descriptor_zip(&p_sig, true, false).unwrap();
    let mut b_sig = ZipBackend::new(&p_sig);
    let info_sig = b_sig.inspect(&OpenOptions::default()).unwrap();
    assert_eq!(info_sig.entries.len(), 1);
    assert!(info_sig.capability.progressive_reclaim);

    // 2. Valid descriptor without signature
    let p_nosig = dir.path().join("desc_nosig.zip");
    write_data_descriptor_zip(&p_nosig, false, false).unwrap();
    let mut b_nosig = ZipBackend::new(&p_nosig);
    let info_nosig = b_nosig.inspect(&OpenOptions::default()).unwrap();
    assert_eq!(info_nosig.entries.len(), 1);
    assert!(info_nosig.capability.progressive_reclaim);

    // 3. Corrupt data descriptor -> fails progressive reclaim / fails closed
    let p_bad = dir.path().join("desc_bad.zip");
    write_invalid_data_descriptor_zip(&p_bad).unwrap();
    let mut b_bad = ZipBackend::new(&p_bad);
    match b_bad.inspect(&OpenOptions::default()) {
        Ok(info_bad) => {
            assert!(
                !info_bad.capability.progressive_reclaim,
                "corrupt descriptor must disable progressive reclaim"
            );
            assert!(
                b_bad.retirement_proofs().is_empty(),
                "defense in depth: no retirement proofs exposed on failed progressive checks"
            );
        }
        Err(_) => {
            // Structural parse failure is also safe fail-closed behavior
        }
    }
}

#[test]
fn test_zip_real_unix_symlinks() {
    let dir = tempdir().unwrap();
    let symlink_path = dir.path().join("symlink.zip");
    write_real_unix_symlink_zip(&symlink_path, "link_to_target", "target.txt").unwrap();

    let mut backend = ZipBackend::new(&symlink_path);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    assert_eq!(info.entries.len(), 1);
    assert!(info.entries[0].redirection.is_some());
}

#[test]
fn test_zip_overlapping_intervals_fail_closed() {
    let dir = tempdir().unwrap();
    let overlap_path = dir.path().join("overlap.zip");
    write_overlapping_zip(&overlap_path).unwrap();

    let mut backend = ZipBackend::new(&overlap_path);
    match backend.inspect(&OpenOptions::default()) {
        Ok(info) => {
            assert!(
                !info.capability.progressive_reclaim,
                "overlapping ZIP must have progressive_reclaim disabled"
            );
            assert!(backend.retirement_proofs().is_empty());
        }
        Err(_) => {
            // Structural rejection is also a safe fail-closed outcome
        }
    }
}

#[test]
fn test_zip_sfx_disables_low_space() {
    let dir = tempdir().unwrap();
    let sfx_path = dir.path().join("sfx.zip");
    write_sfx_zip(&sfx_path).unwrap();

    let mut backend = ZipBackend::new(&sfx_path);
    match backend.inspect(&OpenOptions::default()) {
        Ok(info) => {
            assert!(
                !info.capability.progressive_reclaim,
                "SFX ZIP must have progressive_reclaim disabled"
            );
        }
        Err(_) => {
            // Rejection of non-standard prelude is also safe
        }
    }
}

#[test]
fn test_zip_utf8_filenames() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("utf8.zip");

    let files = vec![
        ZipFixtureFile::stored("日本語_ファイル.txt", "こんにちは世界".as_bytes()),
        ZipFixtureFile::deflated(
            "données_françaises_éàü.txt",
            "Contenu accentué pour tester l'encodage UTF-8.".as_bytes(),
        ),
    ];
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let mut backend = ZipBackend::new(&zip_path);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    assert_eq!(info.entries.len(), 2);
    assert_eq!(info.entries[0].name, "日本語_ファイル.txt");
    assert_eq!(info.entries[1].name, "données_françaises_éàü.txt");
    assert!(info.capability.progressive_reclaim);
}

#[test]
fn test_cp437_exhaustive_table_mapping() {
    use reclaimarc_archive::zip::parser::{decode_cp437, CP437_EXTENDED};

    assert_eq!(CP437_EXTENDED.len(), 128);

    // Verify all 128 extended characters from 0x80 to 0xFF match table
    for (idx, &expected_char) in CP437_EXTENDED.iter().enumerate() {
        let byte = 0x80u8 + (idx as u8);
        let decoded = decode_cp437(&[byte]);
        assert_eq!(
            decoded,
            expected_char.to_string(),
            "Byte 0x{:02X} must decode to expected CP437 character '{expected_char}'",
            byte
        );
    }

    // Critical spec characters
    assert_eq!(decode_cp437(&[0xF4]), "⌠"); // U+2320
    assert_eq!(decode_cp437(&[0xF5]), "⌡"); // U+2321
    assert_eq!(decode_cp437(&[0xFF]), "\u{00A0}"); // U+00A0 (Non-breaking space)
    assert_eq!(decode_cp437(&[0x80]), "Ç"); // U+00C7
    assert_eq!(decode_cp437(&[0x81]), "ü"); // U+00FC
    assert_eq!(decode_cp437(&[0x82]), "é"); // U+00E9
    assert_eq!(decode_cp437(&[0x90]), "É"); // U+00C9
    assert_eq!(decode_cp437(&[0xA0]), "á"); // U+00E1
    assert_eq!(decode_cp437(&[0xE0]), "α"); // U+03B1
}

#[test]
fn test_zip_with_fake_zip64_comment_is_classified_as_zip() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("fake_zip64_comment.zip");
    reclaimarc_archive::zip::fixtures::write_zip_with_fake_zip64_comment(&zip_path).unwrap();

    let mut backend = ZipBackend::new(&zip_path);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    // Must be classified as standard "zip", NOT "zip64", because the signatures only appear in the comment
    assert_eq!(
        info.format, "zip",
        "ZIP with fake ZIP64 signatures in comment must NOT be classified as zip64"
    );
    assert_eq!(info.entries.len(), 1);
    assert_eq!(info.entries[0].name, "file.txt");
}

#[test]
fn test_real_zip64_compact_matrix() {
    let dir = tempdir().unwrap();

    // 1. Genuine ZIP64 with Stored and Deflate entries
    let zip64_stored = dir.path().join("real_zip64.zip");
    let files = vec![
        ZipFixtureFile::stored("file1.bin", b"ZIP64 stored payload 1"),
        ZipFixtureFile::deflated(
            "file2.bin",
            b"ZIP64 deflated payload 2 with repeating text repeating text repeating text",
        ),
        ZipFixtureFile::stored("empty.bin", b""),
    ];
    write_zip(
        &zip64_stored,
        &files,
        &ZipFixtureOptions {
            force_zip64: true,
            comment: None,
        },
    )
    .unwrap();

    let mut backend = ZipBackend::new(&zip64_stored);
    let info = backend.inspect(&OpenOptions::default()).unwrap();
    assert_eq!(info.format, "zip64");
    assert_eq!(info.entries.len(), 3);
    assert!(info.capability.progressive_reclaim);
    let proofs = backend.retirement_proofs();
    assert_eq!(proofs.len(), 2); // 2 non-empty files

    // 2. Test integrity of real ZIP64
    let report = backend.test_integrity(None, None, None).unwrap();
    assert!(report.ok);
    assert_eq!(report.first_failure, None);

    // 3. Test extraction of real ZIP64
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let mut name_map = HashMap::new();
    name_map.insert(0, "file1.bin".to_string());
    name_map.insert(1, "file2.bin".to_string());
    name_map.insert(2, "empty.bin".to_string());

    let opts = ExtractOptions {
        dest_dir: dest.clone(),
        job_id: "zip64_test_job".into(),
        partial_suffix: ".part".into(),
        password: None,
        cancel: None,
        name_map,
        max_compression_ratio: None,
    };

    backend.begin_extraction(&opts, 0).unwrap();
    let res0 = backend.extract_unit(0, &opts, None).unwrap();
    assert!(res0.extracted.contains(&0));
    let res1 = backend.extract_unit(1, &opts, None).unwrap();
    assert!(res1.extracted.contains(&1));
    let res2 = backend.extract_unit(2, &opts, None).unwrap();
    assert!(res2.extracted.contains(&2));

    assert_eq!(
        std::fs::read(dest.join("file1.bin.part")).unwrap(),
        b"ZIP64 stored payload 1"
    );
    assert_eq!(
        std::fs::read(dest.join("file2.bin.part")).unwrap(),
        b"ZIP64 deflated payload 2 with repeating text repeating text repeating text"
    );
    assert_eq!(std::fs::read(dest.join("empty.bin.part")).unwrap(), b"");
}

#[test]
fn test_cp437_precedence_when_bit11_unset() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("cp437_precedence.zip");
    reclaimarc_archive::zip::fixtures::write_cp437_vs_utf8_zip(&zip_path).unwrap();

    let mut backend = ZipBackend::new(&zip_path);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    // When bit 11 is unset, [0xC3, 0xA9] MUST be decoded as CP437 "├⌐.txt", not UTF-8 "é.txt"
    assert_eq!(info.entries[0].name, "├⌐.txt");
}

#[test]
fn test_unicode_path_0x7075_valid_and_invalid() {
    let dir = tempdir().unwrap();

    // 1. Valid 0x7075 extra field
    let valid_path = dir.path().join("unicode_valid.zip");
    reclaimarc_archive::zip::fixtures::write_unicode_path_0x7075_zip(
        &valid_path,
        b"cp437_name.txt",
        "custom_unicode_name_🚀.txt",
        true,
    )
    .unwrap();

    let mut backend_valid = ZipBackend::new(&valid_path);
    let info_valid = backend_valid.inspect(&OpenOptions::default()).unwrap();
    assert_eq!(info_valid.entries[0].name, "custom_unicode_name_🚀.txt");

    // 2. Invalid CRC in 0x7075 extra field -> fails closed or falls back to CP437 name
    let invalid_path = dir.path().join("unicode_invalid.zip");
    reclaimarc_archive::zip::fixtures::write_unicode_path_0x7075_zip(
        &invalid_path,
        b"fallback_cp437.txt",
        "should_be_ignored.txt",
        false,
    )
    .unwrap();

    let mut backend_invalid = ZipBackend::new(&invalid_path);
    match backend_invalid.inspect(&OpenOptions::default()) {
        Ok(info_invalid) => {
            assert_eq!(info_invalid.entries[0].name, "fallback_cp437.txt");
        }
        Err(_) => {
            // Fails closed on CRC mismatch in extra field (zip-rs strict validation)
        }
    }
}

#[test]
fn test_envelope_overlap_disables_low_space() {
    let dir = tempdir().unwrap();
    let overlap_path = dir.path().join("envelope_overlap.zip");
    reclaimarc_archive::zip::fixtures::write_envelope_overlap_zip(&overlap_path).unwrap();

    let mut backend = ZipBackend::new(&overlap_path);
    match backend.inspect(&OpenOptions::default()) {
        Ok(info) => {
            assert!(
                !info.capability.progressive_reclaim,
                "Envelope overlap (A payload over B header) must disable progressive reclaim"
            );
            assert!(
                backend.retirement_proofs().is_empty(),
                "Zero retirement proofs must be exposed for overlapping envelopes"
            );
        }
        Err(_) => {
            // Structural parse rejection is also a safe fail-closed outcome
        }
    }
}

#[test]
fn test_checked_range_arithmetic_overflow() {
    use reclaimarc_archive::zip::parser::checked_range;

    assert!(checked_range(100, 50, 200).is_ok());
    assert_eq!(checked_range(100, 50, 200).unwrap(), (100, 150));

    // Exceeds limit
    assert!(checked_range(100, 50, 140).is_err());

    // Near u64::MAX overflow
    assert!(checked_range(u64::MAX - 10, 20, u64::MAX).is_err());
    assert!(checked_range(u64::MAX, 1, u64::MAX).is_err());
}

#[test]
fn test_data_descriptor_signed_and_unsigned_variations() {
    let dir = tempdir().unwrap();

    // 1. Signed 32-bit (16 bytes)
    let p1 = dir.path().join("desc_signed_32.zip");
    write_data_descriptor_zip(&p1, true, false).unwrap();
    let mut b1 = ZipBackend::new(&p1);
    let i1 = b1.inspect(&OpenOptions::default()).unwrap();
    assert!(i1.capability.progressive_reclaim);
    let r1 = b1.test_integrity(None, None, None).unwrap();
    assert!(r1.ok);

    // 2. Unsigned 32-bit (12 bytes)
    let p2 = dir.path().join("desc_unsigned_32.zip");
    write_data_descriptor_zip(&p2, false, false).unwrap();
    let mut b2 = ZipBackend::new(&p2);
    let i2 = b2.inspect(&OpenOptions::default()).unwrap();
    assert!(i2.capability.progressive_reclaim);
    let r2 = b2.test_integrity(None, None, None).unwrap();
    assert!(r2.ok);

    // 3. Signed ZIP64 (24 bytes)
    let p3 = dir.path().join("desc_signed_64.zip");
    write_data_descriptor_zip(&p3, true, true).unwrap();
    let mut b3 = ZipBackend::new(&p3);
    let i3 = b3.inspect(&OpenOptions::default()).unwrap();
    assert!(i3.capability.progressive_reclaim);
    let r3 = b3.test_integrity(None, None, None).unwrap();
    assert!(r3.ok);
}

#[test]
fn test_data_descriptor_crc_equals_signature_edge_case() {
    use reclaimarc_archive::zip::fixtures::write_data_descriptor_crc_edge_case_zip;
    let dir = tempdir().unwrap();

    // 1. Unsigned descriptor where CRC equals signature 0x08074B50
    let p_unsigned = dir.path().join("crc_edge_unsigned.zip");
    write_data_descriptor_crc_edge_case_zip(&p_unsigned, false).unwrap();
    let mut b_unsigned = ZipBackend::new(&p_unsigned);
    let info_unsigned = b_unsigned.inspect(&OpenOptions::default()).unwrap();
    assert!(info_unsigned.capability.progressive_reclaim);
    let rep_unsigned = b_unsigned.test_integrity(None, None, None).unwrap();
    assert!(rep_unsigned.ok);

    // 2. Signed descriptor where CRC equals signature 0x08074B50
    let p_signed = dir.path().join("crc_edge_signed.zip");
    write_data_descriptor_crc_edge_case_zip(&p_signed, true).unwrap();
    let mut b_signed = ZipBackend::new(&p_signed);
    let info_signed = b_signed.inspect(&OpenOptions::default()).unwrap();
    assert!(info_signed.capability.progressive_reclaim);
    let rep_signed = b_signed.test_integrity(None, None, None).unwrap();
    assert!(rep_signed.ok);
}

#[test]
fn test_zip_partial_file_raii_cleanup_on_corrupt_extraction() {
    let dir = tempdir().unwrap();
    let corrupt_path = dir.path().join("corrupt_for_partial.zip");
    write_corrupt_crc_zip(&corrupt_path).unwrap();

    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    let mut backend = ZipBackend::new(&corrupt_path);
    let info = backend.inspect(&OpenOptions::default()).unwrap();

    let mut name_map = HashMap::new();
    for e in &info.entries {
        name_map.insert(e.index, e.name.clone());
    }

    let extract_opts = ExtractOptions {
        dest_dir: dest.clone(),
        name_map,
        partial_suffix: ".sx-partial-testjob".to_string(),
        password: None,
        job_id: "testjob".to_string(),
        cancel: None,
        max_compression_ratio: None,
    };

    // Extraction of corrupt unit must fail with Corrupt error
    let res = backend.extract_unit(0, &extract_opts, None);
    assert!(res.is_err(), "extracting corrupt unit must fail");

    // RAII guard must have cleaned up the partial file
    let partial_file = dest.join("corrupt_file.txt.sx-partial-testjob");
    assert!(
        !partial_file.exists(),
        "RAII PartialFileGuard must remove partial file on extraction failure"
    );
}

#[test]
fn test_zip_unsupported_compression_fails_closed() {
    use reclaimarc_archive::zip::fixtures::write_unsupported_compression_zip;
    let dir = tempdir().unwrap();
    let p = dir.path().join("unsupported_bzip2.zip");
    write_unsupported_compression_zip(&p).unwrap();

    let mut b = ZipBackend::new(&p);
    // Inspection or test must fail closed
    let inspect_res = b.inspect(&OpenOptions::default());
    if let Ok(info) = inspect_res {
        assert!(
            !info.capability.progressive_reclaim,
            "Unsupported compression must not allow progressive reclaim"
        );
        let test_res = b.test_integrity(None, None, None);
        assert!(test_res.is_err() || !test_res.unwrap().ok);
    }
}

#[test]
fn test_zip_truncated_eocd_fails_closed() {
    use reclaimarc_archive::zip::fixtures::write_truncated_zip;
    let dir = tempdir().unwrap();
    let p = dir.path().join("truncated.zip");
    write_truncated_zip(&p).unwrap();

    let mut b = ZipBackend::new(&p);
    assert!(
        b.inspect(&OpenOptions::default()).is_err(),
        "Truncated ZIP must fail inspection"
    );
}

#[test]
fn test_zip_large_entry_count_inspect_and_extract() {
    use reclaimarc_archive::zip::fixtures::write_large_entry_count_zip;
    let dir = tempdir().unwrap();
    let p = dir.path().join("large_count.zip");
    write_large_entry_count_zip(&p, 500).unwrap();

    let mut b = ZipBackend::new(&p);
    let info = b.inspect(&OpenOptions::default()).unwrap();
    assert_eq!(info.entries.len(), 500);
    assert_eq!(info.recovery_units.len(), 500);
    assert!(info.capability.progressive_reclaim);
}

#[test]
fn test_zip_configured_compression_ratio_enforcement() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("high_ratio.zip");
    let dest = dir.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    // 12 MB payload with ~400:1 compression ratio
    let payload: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 25) as u8).collect();
    let files = vec![ZipFixtureFile::deflated("huge.bin", &payload)];
    write_zip(&zip_path, &files, &ZipFixtureOptions::default()).unwrap();

    let mut backend = ZipBackend::new(&zip_path);
    let _ = backend.inspect(&OpenOptions::default()).unwrap();

    let mut name_map = HashMap::new();
    name_map.insert(0, "huge.bin".to_string());

    // 1. Configured ratio of 50 -> must reject the ~800:1 payload and clean up partial file
    let opts_reject = ExtractOptions {
        dest_dir: dest.clone(),
        job_id: "ratio-test-reject".into(),
        partial_suffix: ".sx-partial-test".into(),
        password: None,
        cancel: None,
        name_map: name_map.clone(),
        max_compression_ratio: Some(50),
    };
    let err = backend.extract_unit(0, &opts_reject, None).unwrap_err();
    assert!(
        matches!(
            err,
            reclaimarc_archive::error::ArchiveError::InvalidMetadata(_)
        ),
        "Must fail with ArchiveError::InvalidMetadata on ratio violation: {err:?}"
    );
    let partial_file = dest.join("huge.bin.sx-partial-test");
    assert!(
        !partial_file.exists(),
        "Partial file must be cleaned up after ratio violation"
    );

    // 2. Configured ratio of 1000 -> succeeds
    let opts_accept = ExtractOptions {
        dest_dir: dest.clone(),
        job_id: "ratio-test-accept".into(),
        partial_suffix: ".sx-partial-test2".into(),
        password: None,
        cancel: None,
        name_map: name_map.clone(),
        max_compression_ratio: Some(1000),
    };
    let res = backend.extract_unit(0, &opts_accept, None).unwrap();
    assert_eq!(res.bytes_written, 12 * 1024 * 1024);

    // 3. 0 -> safe fallback to default 1000:1
    let opts_default = ExtractOptions {
        dest_dir: dest.clone(),
        job_id: "ratio-test-default".into(),
        partial_suffix: ".sx-partial-test3".into(),
        password: None,
        cancel: None,
        name_map,
        max_compression_ratio: Some(0),
    };
    let res_default = backend.extract_unit(0, &opts_default, None).unwrap();
    assert_eq!(res_default.bytes_written, 12 * 1024 * 1024);
}
