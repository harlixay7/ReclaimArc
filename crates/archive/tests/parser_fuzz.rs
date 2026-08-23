use proptest::prelude::*;
use reclaimarc_archive::rar::parser::{parse, validate_structural_invariants, VolumeMeta};
use std::io::Write;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn test_arbitrary_bytes_never_panic(data in prop::collection::vec(any::<u8>(), 0..8192)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fuzz.rar");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&data).unwrap();
        drop(file);

        let vol = VolumeMeta {
            path: path.clone(),
            len: data.len() as u64,
        };

        let res = parse(vec![vol]);
        if let Ok(parsed) = res {
            // If the parser succeeded, structural invariants must hold strictly
            let inv = validate_structural_invariants(&parsed);
            prop_assert!(inv.is_ok(), "Parsed archive failed invariants: {:?}", inv);
        }
    }

    #[test]
    fn test_mutated_rar5_headers_never_panic(
        header_bytes in prop::collection::vec(any::<u8>(), 0..4096)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fuzz_r5.rar");
        let mut full_data = vec![0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];
        full_data.extend_from_slice(&header_bytes);

        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&full_data).unwrap();
        drop(file);

        let vol = VolumeMeta {
            path: path.clone(),
            len: full_data.len() as u64,
        };

        let res = parse(vec![vol]);
        if let Ok(parsed) = res {
            let inv = validate_structural_invariants(&parsed);
            prop_assert!(inv.is_ok(), "Parsed archive failed invariants: {:?}", inv);
        }
    }

    #[test]
    fn test_mutated_rar4_headers_never_panic(
        header_bytes in prop::collection::vec(any::<u8>(), 0..4096)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fuzz_r4.rar");
        let mut full_data = vec![0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];
        full_data.extend_from_slice(&header_bytes);

        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&full_data).unwrap();
        drop(file);

        let vol = VolumeMeta {
            path: path.clone(),
            len: full_data.len() as u64,
        };

        let res = parse(vec![vol]);
        if let Ok(parsed) = res {
            let inv = validate_structural_invariants(&parsed);
            prop_assert!(inv.is_ok(), "Parsed archive failed invariants: {:?}", inv);
        }
    }
}
