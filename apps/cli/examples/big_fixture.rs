//! Generate a large test RAR with service headers (stress the parser↔decoder
//! cross-validation at scale).
//! Usage: cargo run -p reclaimarc-cli --example big_fixture <dir> <name> <count>
use reclaimarc_archive::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("dir"));
    let name = std::env::args().nth(2).unwrap_or_else(|| "big".into());
    let count: usize = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let files: Vec<FixtureFile> = (0..count)
        .map(|i| {
            let data: Vec<u8> = (0..2048).map(|b| ((b + i * 7) % 251) as u8).collect();
            if i % 10 == 0 {
                FixtureFile::new(&format!("docs/dir{i}/file-{i}.txt"), &data)
            } else {
                FixtureFile::new(&format!("file-{i}.bin"), &data)
            }
        })
        .collect();
    let opts = FixtureOptions {
        service_headers: vec!["NTFS".into(), "ACL".into(), "CMT".into()],
        ..Default::default()
    };
    let paths = write_rar(&dir, &name, &files, &opts).unwrap();
    println!("{}", paths[0].display());
}
