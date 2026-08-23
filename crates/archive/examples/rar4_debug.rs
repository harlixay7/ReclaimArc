//! Debug RAR4 fixture structure.
//! Usage: cargo run -p spacextract-archive --example rar4_debug <dir>
use spacextract_archive::rar::fixtures::{write_rar, FixtureFile, FixtureOptions};

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("dir arg"));
    let files = vec![
        spacextract_archive::rar::fixtures::FixtureFile::new("a.txt", b"rar4 data"),
        spacextract_archive::rar::fixtures::FixtureFile::new("b.txt", b"second"),
    ];
    let opts = FixtureOptions { rar5: false, ..Default::default() };
    let paths = write_rar(&dir, "dbg4", &files, &opts).unwrap();
    println!("volumes: {paths:?}");
    let bytes = std::fs::read(&paths[0]).unwrap();
    println!("len: {}", bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        print!("{b:02x} ");
        if (i + 1) % 16 == 0 {
            println!();
        }
    }
    println!();
    // Walk RAR4 headers: [crc16 2][type 1][flags 2][size 2] + fields.
    // For file headers: size covers everything; data follows.
    let mut pos = 7usize;
    loop {
        if pos + 7 > bytes.len() {
            println!("(end at {pos})");
            break;
        }
        let crc = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
        let htype = bytes[pos + 2];
        let flags = u16::from_le_bytes([bytes[pos + 3], bytes[pos + 4]]);
        let head_size = u16::from_le_bytes([bytes[pos + 5], bytes[pos + 6]]) as usize;
        let block = &bytes[pos + 2..(pos + head_size).min(bytes.len())];
        let computed = spacextract_archive::rar::fixtures::crc16(block);
        let pack = if htype == 0x74 || htype == 0x7a {
            u32::from_le_bytes(bytes[pos + 7..pos + 11].try_into().unwrap()) as usize
        } else {
            0
        };
        println!(
            "hdr @{pos}: type={htype:#x} crc={crc:04x} computed={computed:04x} head_size={head_size} flags={flags:#x} pack={pack}"
        );
        if htype == 0x7b {
            break;
        }
        pos += head_size + pack;
    }

    // Decoder:
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
}