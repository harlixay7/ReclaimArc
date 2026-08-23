//! Debug: write a fixture archive and inspect it with our parser AND the
//! official library.
//! Usage: cargo run -p spacextract-archive --example rar_debug <dir> [corrupt]
use spacextract_archive::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("dir arg"));
    let corrupt = std::env::args().nth(2).is_some();
    let files = vec![spacextract_archive::rar::fixtures::FixtureFile::new("a.txt", b"aaa")];
    let opts = if corrupt {
        FixtureOptions {
            corrupt: Some((44, 0x00)),
            ..Default::default()
        }
    } else {
        FixtureOptions::default()
    };
    let paths = write_rar(&dir, "dbg", &files, &opts).unwrap();
    println!("volumes: {paths:?}");

    // Decoder path — canonical read/process/read sequence:
    match spacextract_archive::rar::decoder::Unrar::open(
        &paths[0],
        spacextract_archive::rar::decoder::OpenMode::List,
        None,
        None,
    ) {
        Ok(mut u) => loop {
            match u.read_header() {
                Ok(Some(h)) => println!(
                    "DECODER: entry {:?} pack={} unp={} flags={:#x}",
                    h.file_name_w, h.pack_size, h.unp_size, h.flags
                ),
                Ok(None) => {
                    println!("DECODER: end of archive");
                    break;
                }
                Err(e) => {
                    println!("DECODER: error: {e:?}");
                    break;
                }
            }
            if let Err(e) =
                u.process_file(spacextract_archive::rar::decoder::Operation::Skip, None, None, None, 0, 0)
            {
                println!("DECODER process error: {e:?}");
                break;
            }
        },
        Err(e) => println!("DECODER open error: {e:?}"),
    }

    // Our parser:
    let mut vols = Vec::new();
    for p in &paths {
        let len = std::fs::metadata(p).unwrap().len();
        vols.push(spacextract_archive::rar::parser::VolumeMeta { path: p.clone(), len });
    }
    match spacextract_archive::rar::parser::parse(vols) {
        Ok(parsed) => {
            println!("PARSER: format={:?} entries={}", parsed.format, parsed.entries.len());
            for e in &parsed.entries {
                println!(
                    "  entry {} name={:?} pack={} unp={} crc={:x?} dir={} solid={}",
                    e.index, e.name, e.packed_size, e.unpacked_size, e.crc32, e.is_directory, e.is_solid
                );
            }
            println!("  parts: {:?}", parsed.parts);
        }
        Err(e) => println!("PARSER error: {e:?}"),
    }
}