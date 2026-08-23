use std::process::Command;
use reclaimarc_archive::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};

#[test]
fn test_cli_inspect_and_plan_and_extract() {
    let dir = tempfile::tempdir().unwrap();
    let files = vec![
        FixtureFile::new("file1.txt", b"content for file 1"),
        FixtureFile::new("nested/file2.bin", &[42u8; 1000]),
    ];
    let paths = write_rar(dir.path(), "cli_test_arc", &files, &FixtureOptions::default()).unwrap();
    let archive = paths[0].to_str().unwrap();
    let dest_dir = dir.path().join("out");
    let dest = dest_dir.to_str().unwrap();

    let exe = env!("CARGO_BIN_EXE_reclaimarc");

    // 1. Test inspect
    let output = Command::new(exe)
        .arg("inspect")
        .arg(archive)
        .output()
        .expect("failed to run inspect");
    assert!(output.status.success(), "inspect failed: {:?}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("file1.txt"));
    assert!(stdout.contains("nested/file2.bin"));

    // 2. Test plan
    let output = Command::new(exe)
        .arg("plan")
        .arg(archive)
        .arg(dest)
        .output()
        .expect("failed to run plan");
    assert!(output.status.success(), "plan failed: {:?}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Space plan for"));
    assert!(stdout.contains("POSSIBLE"));

    // 3. Test extract
    let output = Command::new(exe)
        .arg("extract")
        .arg(archive)
        .arg(dest)
        .output()
        .expect("failed to run extract");
    assert!(output.status.success(), "extract failed: {:?}", String::from_utf8_lossy(&output.stderr));

    // Verify extracted files
    assert_eq!(std::fs::read(dest_dir.join("file1.txt")).unwrap(), b"content for file 1");
    assert_eq!(std::fs::read(dest_dir.join("nested").join("file2.bin")).unwrap(), vec![42u8; 1000]);
}
