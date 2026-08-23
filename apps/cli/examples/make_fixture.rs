//! Generate a test RAR archive.
//! Usage: cargo run -p reclaimarc-cli --example make_fixture <dir> <name>
use reclaimarc_archive::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("dir"));
    let name = std::env::args().nth(2).unwrap_or_else(|| "demo".into());
    let files: Vec<FixtureFile> = (0..6)
        .map(|i| {
            let data: Vec<u8> = (0..500_000).map(|b| ((b as usize + i * 31) % 251) as u8).collect();
            if i == 2 {
                FixtureFile::new(&format!("docs/readme-{i}.txt"), &data)
            } else {
                FixtureFile::new(&format!("file-{i}.bin"), &data)
            }
        })
        .collect();
    let paths = write_rar(&dir, &name, &files, &FixtureOptions::default()).unwrap();
    println!("{}", paths[0].display());
}