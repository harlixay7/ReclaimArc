//! ZIP test fixture generator for valid (Stored, Deflate, multi-file, ZIP64,
//! data-descriptor) and adversarial / malformed ZIP archives.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

use crate::model::{Redirection, RedirectionKind};

/// One file entry specification for generating a fixture.
#[derive(Debug, Clone)]
pub struct ZipFixtureFile {
    pub name: String,
    pub data: Vec<u8>,
    pub is_directory: bool,
    pub deflate: bool,
    pub redirection: Option<Redirection>,
}

impl ZipFixtureFile {
    pub fn stored(name: &str, data: &[u8]) -> Self {
        ZipFixtureFile {
            name: name.to_string(),
            data: data.to_vec(),
            is_directory: false,
            deflate: false,
            redirection: None,
        }
    }

    pub fn deflated(name: &str, data: &[u8]) -> Self {
        ZipFixtureFile {
            name: name.to_string(),
            data: data.to_vec(),
            is_directory: false,
            deflate: true,
            redirection: None,
        }
    }

    pub fn dir(name: &str) -> Self {
        ZipFixtureFile {
            name: name.to_string(),
            data: Vec::new(),
            is_directory: true,
            deflate: false,
            redirection: None,
        }
    }

    pub fn symlink(name: &str, target: &str) -> Self {
        ZipFixtureFile {
            name: name.to_string(),
            data: target.as_bytes().to_vec(),
            is_directory: false,
            deflate: false,
            redirection: Some(Redirection {
                kind: RedirectionKind::UnixSymlink,
                target: target.to_string(),
            }),
        }
    }
}

/// Options for generating a ZIP fixture.
#[derive(Debug, Clone, Default)]
pub struct ZipFixtureOptions {
    pub force_zip64: bool,
    pub comment: Option<String>,
}

/// Write a valid ZIP archive containing `files` to `path`.
pub fn write_zip(
    path: &Path,
    files: &[ZipFixtureFile],
    options: &ZipFixtureOptions,
) -> Result<PathBuf, std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(path)?;
    let mut zip_writer = zip::ZipWriter::new(file);

    if let Some(ref c) = options.comment {
        let _ = zip_writer.set_raw_comment(c.as_bytes().to_vec().into());
    }

    for f in files {
        let method = if f.deflate {
            CompressionMethod::Deflated
        } else {
            CompressionMethod::Stored
        };

        let mut file_options = SimpleFileOptions::default()
            .compression_method(method)
            .large_file(options.force_zip64);

        if f.redirection.is_some() {
            // Set Unix symlink external attributes (S_IFLNK | 0o777)
            // Upper 16 bits: 0o120777
            let _unix_mode = 0o120777u32 << 16;
            file_options = file_options.unix_permissions(0o777);
            zip_writer.start_file(&f.name, file_options)?;
            zip_writer.write_all(&f.data)?;
            // Overwrite attribute in zip-rs by direct Unix attribute convention if needed
            continue;
        }

        if f.is_directory {
            zip_writer.add_directory(&f.name, file_options)?;
        } else {
            zip_writer.start_file(&f.name, file_options)?;
            zip_writer.write_all(&f.data)?;
        }
    }

    zip_writer.finish()?;
    Ok(path.to_path_buf())
}

/// Write an adversarial ZIP with intentionally corrupt entry CRC.
pub fn write_corrupt_crc_zip(path: &Path) -> Result<PathBuf, std::io::Error> {
    let files = vec![ZipFixtureFile::deflated(
        "corrupt.txt",
        b"Original uncorrupted content for test. Repeating content Repeating content.",
    )];
    let p = write_zip(path, &files, &ZipFixtureOptions::default())?;

    // Tamper with payload byte in the archive
    let mut data = std::fs::read(&p)?;
    if data.len() > 50 {
        data[48] ^= 0xFF; // Flip bits in compressed payload
    }
    std::fs::write(&p, data)?;
    Ok(p)
}

/// Write a ZIP fixture using explicit Data Descriptors (bit 3).
pub fn write_data_descriptor_zip(
    path: &Path,
    with_signature: bool,
    is_zip64: bool,
) -> Result<PathBuf, std::io::Error> {
    let mut buf = Vec::new();
    let name = b"descriptor_test.txt";
    let payload = b"Hello from data descriptor test payload! 1234567890.";
    let crc = crc32fast::hash(payload);
    let comp_size = payload.len() as u64;
    let uncomp_size = payload.len() as u64;

    let h1_start = 0u64;

    // Local Header (bit 3 set, CRC & sizes in local header set to 0)
    buf.extend_from_slice(&0x04034b50u32.to_le_bytes()); // Local sig
    buf.extend_from_slice(&20u16.to_le_bytes()); // Version
    buf.extend_from_slice(&0x0008u16.to_le_bytes()); // Bit 3 set!
    buf.extend_from_slice(&0u16.to_le_bytes()); // Method (Stored)
    buf.extend_from_slice(&0u16.to_le_bytes()); // Mod time
    buf.extend_from_slice(&0u16.to_le_bytes()); // Mod date
    buf.extend_from_slice(&0u32.to_le_bytes()); // Local CRC (0 with bit 3)
    buf.extend_from_slice(&0u32.to_le_bytes()); // Local comp size (0 with bit 3)
    buf.extend_from_slice(&0u32.to_le_bytes()); // Local uncomp size (0 with bit 3)
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes()); // Name len
    buf.extend_from_slice(&0u16.to_le_bytes()); // Extra len
    buf.extend_from_slice(name);
    buf.extend_from_slice(payload); // Compressed / Stored payload

    // Data Descriptor
    if with_signature {
        buf.extend_from_slice(&0x08074b50u32.to_le_bytes()); // Descriptor signature
    }
    if is_zip64 {
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&comp_size.to_le_bytes());
        buf.extend_from_slice(&uncomp_size.to_le_bytes());
    } else {
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(comp_size as u32).to_le_bytes());
        buf.extend_from_slice(&(uncomp_size as u32).to_le_bytes());
    }

    // Central Directory
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes()); // Central header sig
    buf.extend_from_slice(&20u16.to_le_bytes()); // Version made by
    buf.extend_from_slice(&20u16.to_le_bytes()); // Version needed
    buf.extend_from_slice(&0x0008u16.to_le_bytes()); // Flags (bit 3 set)
    buf.extend_from_slice(&0u16.to_le_bytes()); // Method (Stored)
    buf.extend_from_slice(&0u16.to_le_bytes()); // Time
    buf.extend_from_slice(&0u16.to_le_bytes()); // Date
    buf.extend_from_slice(&crc.to_le_bytes()); // Central CRC
    buf.extend_from_slice(&(comp_size as u32).to_le_bytes()); // Central Comp Size
    buf.extend_from_slice(&(uncomp_size as u32).to_le_bytes()); // Central Uncomp Size
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes()); // Name len
    buf.extend_from_slice(&0u16.to_le_bytes()); // Extra len
    buf.extend_from_slice(&0u16.to_le_bytes()); // Comment len
    buf.extend_from_slice(&0u16.to_le_bytes()); // Disk num
    buf.extend_from_slice(&0u16.to_le_bytes()); // Internal attr
    buf.extend_from_slice(&0u32.to_le_bytes()); // External attr
    buf.extend_from_slice(&(h1_start as u32).to_le_bytes()); // Local header offset
    buf.extend_from_slice(name);

    let cd_len = (buf.len() as u32) - cd_start;

    // EOCD
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Disk
    buf.extend_from_slice(&0u16.to_le_bytes()); // CD Disk
    buf.extend_from_slice(&1u16.to_le_bytes()); // Disk entries
    buf.extend_from_slice(&1u16.to_le_bytes()); // Total entries
    buf.extend_from_slice(&cd_len.to_le_bytes()); // CD Size
    buf.extend_from_slice(&cd_start.to_le_bytes()); // CD Offset
    buf.extend_from_slice(&0u16.to_le_bytes()); // Comment len

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

/// Write a ZIP fixture with an invalid / mismatched data descriptor.
pub fn write_invalid_data_descriptor_zip(path: &Path) -> Result<PathBuf, std::io::Error> {
    let p = write_data_descriptor_zip(path, true, false)?;
    let mut data = std::fs::read(&p)?;
    // Corrupt the CRC in the descriptor (starts at 30 + name.len() + payload.len() + 4)
    let desc_crc_pos = 30 + 19 + 52 + 4;
    if data.len() > desc_crc_pos + 4 {
        data[desc_crc_pos] ^= 0xFF;
    }
    std::fs::write(&p, data)?;
    Ok(p)
}

/// Write a real Unix symlink fixture using Unix external attributes (0o120777 << 16).
pub fn write_real_unix_symlink_zip(
    path: &Path,
    symlink_name: &str,
    target: &str,
) -> Result<PathBuf, std::io::Error> {
    let mut buf = Vec::new();
    let name = symlink_name.as_bytes();
    let payload = target.as_bytes();
    let crc = crc32fast::hash(payload);
    let comp_size = payload.len() as u32;
    let uncomp_size = payload.len() as u32;
    let unix_ext_attr = 0o120777u32 << 16;

    // Local Header
    let h1_start = 0u32;
    buf.extend_from_slice(&0x04034b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Stored
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&comp_size.to_le_bytes());
    buf.extend_from_slice(&uncomp_size.to_le_bytes());
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(payload);

    // Central Directory
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&0x0314u16.to_le_bytes()); // Made by Unix (0x03), version 2.0 (0x14)
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&comp_size.to_le_bytes());
    buf.extend_from_slice(&uncomp_size.to_le_bytes());
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&unix_ext_attr.to_le_bytes()); // Unix S_IFLNK!
    buf.extend_from_slice(&h1_start.to_le_bytes());
    buf.extend_from_slice(name);

    let cd_len = (buf.len() as u32) - cd_start;

    // EOCD
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_len.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

/// Write an adversarial ZIP with prepended executable bytes (SFX stub).
pub fn write_sfx_zip(path: &Path) -> Result<PathBuf, std::io::Error> {
    let files = vec![ZipFixtureFile::stored("inside_sfx.txt", b"SFX test data")];
    let p = write_zip(path, &files, &ZipFixtureOptions::default())?;
    let zip_bytes = std::fs::read(&p)?;

    let mut sfx_bytes = vec![0x90u8; 512]; // 512 bytes fake DOS/PE stub
    sfx_bytes.extend_from_slice(&zip_bytes);

    std::fs::write(&p, sfx_bytes)?;
    Ok(p)
}

/// Write an adversarial ZIP with overlapping file payload ranges.
pub fn write_overlapping_zip(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut buf = Vec::new();

    // Local header 1: file1.txt (data offset 39, comp size 10)
    let h1_start = 0u32;
    buf.extend_from_slice(&0x04034b50u32.to_le_bytes()); // Local header sig
    buf.extend_from_slice(&20u16.to_le_bytes()); // Version
    buf.extend_from_slice(&0u16.to_le_bytes()); // Flags
    buf.extend_from_slice(&0u16.to_le_bytes()); // Method (Stored)
    buf.extend_from_slice(&0u16.to_le_bytes()); // Mod time
    buf.extend_from_slice(&0u16.to_le_bytes()); // Mod date
    buf.extend_from_slice(&0x12345678u32.to_le_bytes()); // CRC
    buf.extend_from_slice(&10u32.to_le_bytes()); // Comp size
    buf.extend_from_slice(&10u32.to_le_bytes()); // Uncomp size
    buf.extend_from_slice(&9u16.to_le_bytes()); // Name len ("file1.txt")
    buf.extend_from_slice(&0u16.to_le_bytes()); // Extra len
    buf.extend_from_slice(b"file1.txt");
    buf.extend_from_slice(b"0123456789"); // 10 bytes payload (range [39, 49))

    // Central Directory
    let cd_start = buf.len() as u32;

    // Central entry 1: points to local header at 0
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0x12345678u32.to_le_bytes());
    buf.extend_from_slice(&10u32.to_le_bytes());
    buf.extend_from_slice(&10u32.to_le_bytes());
    buf.extend_from_slice(&9u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&h1_start.to_le_bytes());
    buf.extend_from_slice(b"file1.txt");

    // Central entry 2: ALSO points to local header at 0 (identical overlapping payload interval [39, 49)!)
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0x12345678u32.to_le_bytes());
    buf.extend_from_slice(&10u32.to_le_bytes());
    buf.extend_from_slice(&10u32.to_le_bytes());
    buf.extend_from_slice(&9u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&h1_start.to_le_bytes());
    buf.extend_from_slice(b"file2.txt");

    let cd_len = (buf.len() as u32) - cd_start;

    // EOCD
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&cd_len.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

/// Write an adversarial ZIP where Entry A's payload overlaps Entry B's local header,
/// but Entry A's payload and Entry B's payload do NOT overlap.
///
/// Invariant: Low-Space must reject or demote to zero retirement proofs!
pub fn write_envelope_overlap_zip(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut buf = Vec::new();

    // Entry A: local header at 0, data starts at 38, len 400 (range [38, 438))
    let name_a = b"file_a.txt";
    let payload_a = vec![0x41u8; 400]; // 'A' * 400
    let crc_a = crc32fast::hash(&payload_a);

    let h_a_start = 0u32;
    buf.extend_from_slice(&0x04034b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Stored
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc_a.to_le_bytes());
    buf.extend_from_slice(&(payload_a.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload_a.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(name_a.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(name_a);
    buf.extend_from_slice(&payload_a); // ends at offset 30 + 10 + 400 = 440

    // Entry B: local header starts at offset 200 (INSIDE Entry A payload!),
    // but B's data is placed at offset 500 (AFTER Entry A payload ends).
    // We achieve this by embedding B's local header inside A's payload bytes in `buf`,
    // or by constructing the archive layout with overlapping structural regions:
    let h_b_start = 200u32;
    let name_b = b"file_b.txt";
    let payload_b = vec![0x42u8; 100]; // 'B' * 100
    let crc_b = crc32fast::hash(&payload_b);

    // Overwrite bytes 200..238 of `buf` with Entry B's local header
    let mut b_hdr = Vec::new();
    b_hdr.extend_from_slice(&0x04034b50u32.to_le_bytes());
    b_hdr.extend_from_slice(&20u16.to_le_bytes());
    b_hdr.extend_from_slice(&0u16.to_le_bytes());
    b_hdr.extend_from_slice(&0u16.to_le_bytes()); // Stored
    b_hdr.extend_from_slice(&0u16.to_le_bytes());
    b_hdr.extend_from_slice(&0u16.to_le_bytes());
    b_hdr.extend_from_slice(&crc_b.to_le_bytes());
    b_hdr.extend_from_slice(&(payload_b.len() as u32).to_le_bytes());
    b_hdr.extend_from_slice(&(payload_b.len() as u32).to_le_bytes());
    b_hdr.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    // Use extra field length so that B's data starts at offset 500:
    // 200 + 30 + 10 + extra_len = 500 => extra_len = 260
    let extra_len_b = 260u16;
    b_hdr.extend_from_slice(&extra_len_b.to_le_bytes());
    b_hdr.extend_from_slice(name_b);

    let b_hdr_len = b_hdr.len();
    buf[h_b_start as usize..h_b_start as usize + b_hdr_len].copy_from_slice(&b_hdr);

    // Recompute valid CRC for Entry A over its final modified payload bytes (offset 40..440)
    let real_crc_a = crc32fast::hash(&buf[40..440]);
    buf[14..18].copy_from_slice(&real_crc_a.to_le_bytes());

    // Now append padding up to offset 500, then append payload B
    if buf.len() < 500 {
        buf.resize(500, 0x41);
    }
    buf.extend_from_slice(&payload_b); // offset 500..600

    // Central Directory
    let cd_start = buf.len() as u32;

    // CD Entry A
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&real_crc_a.to_le_bytes());
    buf.extend_from_slice(&(payload_a.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload_a.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(name_a.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&h_a_start.to_le_bytes());
    buf.extend_from_slice(name_a);

    // CD Entry B
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc_b.to_le_bytes());
    buf.extend_from_slice(&(payload_b.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload_b.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&h_b_start.to_le_bytes());
    buf.extend_from_slice(name_b);

    let cd_len = (buf.len() as u32) - cd_start;

    // EOCD
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&cd_len.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

/// Write a ZIP fixture with Info-ZIP Unicode Path Extra Field `0x7075`.
pub fn write_unicode_path_0x7075_zip(
    path: &Path,
    std_name: &[u8],
    unicode_path: &str,
    valid_crc: bool,
) -> Result<PathBuf, std::io::Error> {
    let mut buf = Vec::new();
    let payload = b"Unicode Path Extra Field test payload.";
    let crc = crc32fast::hash(payload);

    let name_crc = if valid_crc {
        crc32fast::hash(std_name)
    } else {
        crc32fast::hash(std_name) ^ 0xFFFFFFFF
    };

    // Construct 0x7075 extra field
    let mut extra_7075 = Vec::new();
    extra_7075.extend_from_slice(&0x7075u16.to_le_bytes()); // tag
    let data_len = 1 + 4 + unicode_path.len();
    extra_7075.extend_from_slice(&(data_len as u16).to_le_bytes());
    extra_7075.push(1u8); // version 1
    extra_7075.extend_from_slice(&name_crc.to_le_bytes());
    extra_7075.extend_from_slice(unicode_path.as_bytes());

    // Local Header
    let h1_start = 0u32;
    buf.extend_from_slice(&0x04034b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Bit 11 is UNSET
    buf.extend_from_slice(&0u16.to_le_bytes()); // Stored
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(std_name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(extra_7075.len() as u16).to_le_bytes());
    buf.extend_from_slice(std_name);
    buf.extend_from_slice(&extra_7075);
    buf.extend_from_slice(payload);

    // Central Directory
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Bit 11 is UNSET
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(std_name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(extra_7075.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&h1_start.to_le_bytes());
    buf.extend_from_slice(std_name);
    buf.extend_from_slice(&extra_7075);

    let cd_len = (buf.len() as u32) - cd_start;

    // EOCD
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_len.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

/// Write a ZIP fixture where bit 11 is unset and raw bytes happen to form valid UTF-8,
/// proving that IBM CP437 semantics take precedence when bit 11 is unset.
pub fn write_cp437_vs_utf8_zip(path: &Path) -> Result<PathBuf, std::io::Error> {
    // 0xC3 0xA9 is valid UTF-8 for 'é' (U+00E9), but in CP437:
    // 0xC3 is '├' and 0xA9 is '⌐'
    let raw_name = vec![0xC3, 0xA9, b'.', b't', b'x', b't'];
    let mut buf = Vec::new();
    let payload = b"CP437 precedence test.";
    let crc = crc32fast::hash(payload);

    let h1_start = 0u32;
    buf.extend_from_slice(&0x04034b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Bit 11 UNSET
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(raw_name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&raw_name);
    buf.extend_from_slice(payload);

    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Bit 11 UNSET
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(raw_name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&h1_start.to_le_bytes());
    buf.extend_from_slice(&raw_name);

    let cd_len = (buf.len() as u32) - cd_start;

    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_len.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zip64DescriptorKind {
    None,
    Signed32,
    Unsigned32,
    SignedZip64,
    UnsignedZip64,
}

/// Write a genuine compact ZIP64 archive with valid 0x0001 extra fields,
/// ZIP64 EOCD record, ZIP64 locator, and standard EOCD sentinels.
pub fn write_real_zip64(
    path: &Path,
    files: &[ZipFixtureFile],
    desc_kind: Zip64DescriptorKind,
) -> Result<PathBuf, std::io::Error> {
    let mut buf = Vec::new();
    let mut central_entries = Vec::new();

    for f in files {
        let name_bytes = f.name.as_bytes();
        let payload = &f.data;
        let crc = crc32fast::hash(payload);
        let uncomp_size = payload.len() as u64;
        let comp_size = payload.len() as u64;

        let has_desc = desc_kind != Zip64DescriptorKind::None;
        let flags = if has_desc { 0x0008u16 } else { 0u16 };
        let local_hdr_offset = buf.len() as u64;

        // ZIP64 extra field for local header (tag 0x0001, len 16: 8-byte uncomp, 8-byte comp)
        let mut local_extra = Vec::new();
        local_extra.extend_from_slice(&0x0001u16.to_le_bytes());
        local_extra.extend_from_slice(&16u16.to_le_bytes());
        local_extra.extend_from_slice(&uncomp_size.to_le_bytes());
        local_extra.extend_from_slice(&comp_size.to_le_bytes());

        // Local Header
        buf.extend_from_slice(&0x04034b50u32.to_le_bytes()); // Local sig
        buf.extend_from_slice(&45u16.to_le_bytes()); // Version needed (4.5 for ZIP64)
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // Stored
        buf.extend_from_slice(&0u16.to_le_bytes()); // Time
        buf.extend_from_slice(&0u16.to_le_bytes()); // Date
        if has_desc {
            buf.extend_from_slice(&0u32.to_le_bytes()); // CRC is 0 with bit 3
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
        } else {
            buf.extend_from_slice(&crc.to_le_bytes());
            buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Comp size sentinel
            buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Uncomp size sentinel
        }
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(local_extra.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&local_extra);

        // Data payload
        buf.extend_from_slice(payload);

        // Data descriptor if enabled
        match desc_kind {
            Zip64DescriptorKind::None => {}
            Zip64DescriptorKind::Signed32 => {
                buf.extend_from_slice(&0x08074b50u32.to_le_bytes());
                buf.extend_from_slice(&crc.to_le_bytes());
                buf.extend_from_slice(&(comp_size as u32).to_le_bytes());
                buf.extend_from_slice(&(uncomp_size as u32).to_le_bytes());
            }
            Zip64DescriptorKind::Unsigned32 => {
                buf.extend_from_slice(&crc.to_le_bytes());
                buf.extend_from_slice(&(comp_size as u32).to_le_bytes());
                buf.extend_from_slice(&(uncomp_size as u32).to_le_bytes());
            }
            Zip64DescriptorKind::SignedZip64 => {
                buf.extend_from_slice(&0x08074b50u32.to_le_bytes());
                buf.extend_from_slice(&crc.to_le_bytes());
                buf.extend_from_slice(&comp_size.to_le_bytes());
                buf.extend_from_slice(&uncomp_size.to_le_bytes());
            }
            Zip64DescriptorKind::UnsignedZip64 => {
                buf.extend_from_slice(&crc.to_le_bytes());
                buf.extend_from_slice(&comp_size.to_le_bytes());
                buf.extend_from_slice(&uncomp_size.to_le_bytes());
            }
        }

        // ZIP64 extra field for central directory header (tag 0x0001, len 24: uncomp(8), comp(8), offset(8))
        let mut central_extra = Vec::new();
        central_extra.extend_from_slice(&0x0001u16.to_le_bytes());
        central_extra.extend_from_slice(&24u16.to_le_bytes());
        central_extra.extend_from_slice(&uncomp_size.to_le_bytes());
        central_extra.extend_from_slice(&comp_size.to_le_bytes());
        central_extra.extend_from_slice(&local_hdr_offset.to_le_bytes());

        central_entries.push((name_bytes.to_vec(), crc, flags, central_extra));
    }

    // Central Directory
    let cd_start = buf.len() as u64;
    for (name_bytes, crc, flags, central_extra) in &central_entries {
        buf.extend_from_slice(&0x02014b50u32.to_le_bytes()); // Central sig
        buf.extend_from_slice(&45u16.to_le_bytes()); // Made by
        buf.extend_from_slice(&45u16.to_le_bytes()); // Needed
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // Stored
        buf.extend_from_slice(&0u16.to_le_bytes()); // Time
        buf.extend_from_slice(&0u16.to_le_bytes()); // Date
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Comp size sentinel
        buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Uncomp size sentinel
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(central_extra.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // Comment len
        buf.extend_from_slice(&0u16.to_le_bytes()); // Disk num
        buf.extend_from_slice(&0u16.to_le_bytes()); // Internal attr
        buf.extend_from_slice(&0u32.to_le_bytes()); // External attr
        buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Offset sentinel
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(central_extra);
    }
    let cd_len = (buf.len() as u64) - cd_start;

    // ZIP64 End of Central Directory Record
    let zip64_eocd_offset = buf.len() as u64;
    buf.extend_from_slice(&0x06064b50u32.to_le_bytes()); // ZIP64 EOCD sig
    buf.extend_from_slice(&44u64.to_le_bytes()); // Size of remaining record
    buf.extend_from_slice(&45u16.to_le_bytes()); // Version made by
    buf.extend_from_slice(&45u16.to_le_bytes()); // Version needed
    buf.extend_from_slice(&0u32.to_le_bytes()); // Disk num
    buf.extend_from_slice(&0u32.to_le_bytes()); // Start disk
    buf.extend_from_slice(&(central_entries.len() as u64).to_le_bytes()); // Disk entries
    buf.extend_from_slice(&(central_entries.len() as u64).to_le_bytes()); // Total entries
    buf.extend_from_slice(&cd_len.to_le_bytes()); // CD size
    buf.extend_from_slice(&cd_start.to_le_bytes()); // CD offset

    // ZIP64 End of Central Directory Locator
    buf.extend_from_slice(&0x06074b50u32.to_le_bytes()); // ZIP64 locator sig
    buf.extend_from_slice(&0u32.to_le_bytes()); // Disk with ZIP64 EOCD
    buf.extend_from_slice(&zip64_eocd_offset.to_le_bytes()); // Offset of ZIP64 EOCD
    buf.extend_from_slice(&1u32.to_le_bytes()); // Total disks

    // Standard EOCD Record with sentinels
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Disk num
    buf.extend_from_slice(&0u16.to_le_bytes()); // Start disk
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // Disk entries sentinel
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // Total entries sentinel
    buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // CD size sentinel
    buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // CD offset sentinel
    buf.extend_from_slice(&0u16.to_le_bytes()); // Comment length

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

/// Write a ZIP fixture where the archive comment contains ZIP64 signatures,
/// to test that signature scanning does NOT falsely classify it as ZIP64.
pub fn write_zip_with_fake_zip64_comment(path: &Path) -> Result<PathBuf, std::io::Error> {
    let files = vec![ZipFixtureFile::stored(
        "file.txt",
        b"Hello from standard ZIP with fake signatures in comment.",
    )];
    let mut comment_bytes = Vec::new();
    comment_bytes.extend_from_slice(b"Comment with fake signatures: ");
    comment_bytes.extend_from_slice(&0x06064b50u32.to_le_bytes());
    comment_bytes.extend_from_slice(b" and ");
    comment_bytes.extend_from_slice(&0x06074b50u32.to_le_bytes());
    let comment_str = unsafe { String::from_utf8_unchecked(comment_bytes) };

    write_zip(
        path,
        &files,
        &ZipFixtureOptions {
            force_zip64: false,
            comment: Some(comment_str),
        },
    )
}

/// Precomputed 4-byte payload whose exact CRC32 equals 0x08074B50 (the data descriptor signature).
pub const CRC_SIG_MATCH_PAYLOAD: [u8; 4] = [0xAC, 0x0A, 0x7A, 0xD5];

/// Write a ZIP fixture where the payload CRC32 equals 0x08074B50 (the descriptor signature).
pub fn write_data_descriptor_crc_edge_case_zip(
    path: &Path,
    with_signature: bool,
) -> Result<PathBuf, std::io::Error> {
    let payload = CRC_SIG_MATCH_PAYLOAD;
    let crc = crc32fast::hash(&payload);
    assert_eq!(crc, 0x08074b50);

    let mut buf = Vec::new();
    let name = b"crc_sig_match.txt";
    let comp_size = payload.len() as u64;
    let uncomp_size = payload.len() as u64;
    let h1_start = 0u64;

    // Local Header (bit 3 set)
    buf.extend_from_slice(&0x04034b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0x0008u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Stored
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(&payload);

    // Data Descriptor
    if with_signature {
        buf.extend_from_slice(&0x08074b50u32.to_le_bytes());
    }
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(comp_size as u32).to_le_bytes());
    buf.extend_from_slice(&(uncomp_size as u32).to_le_bytes());

    // Central Directory
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0x0008u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(comp_size as u32).to_le_bytes());
    buf.extend_from_slice(&(uncomp_size as u32).to_le_bytes());
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&(h1_start as u32).to_le_bytes());
    buf.extend_from_slice(name);

    let cd_len = (buf.len() as u32) - cd_start;

    // EOCD
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_len.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

/// Write a ZIP fixture containing an unsupported compression method (e.g. BZIP2 method 12).
pub fn write_unsupported_compression_zip(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut buf = Vec::new();
    let name = b"unsupported_bzip2.bin";
    let payload = b"Simulated bzip2 compressed payload data...";
    let crc = crc32fast::hash(payload);
    let h1_start = 0u64;

    buf.extend_from_slice(&0x04034b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&12u16.to_le_bytes()); // Method 12 = BZIP2
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(payload);

    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&12u16.to_le_bytes()); // Method 12
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&(h1_start as u32).to_le_bytes());
    buf.extend_from_slice(name);

    let cd_len = (buf.len() as u32) - cd_start;

    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_len.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, buf)?;
    Ok(path.to_path_buf())
}

/// Write a truncated ZIP file missing the end of the central directory.
pub fn write_truncated_zip(path: &Path) -> Result<PathBuf, std::io::Error> {
    let valid_path = write_zip(
        path,
        &[ZipFixtureFile::stored("file.txt", b"truncated content")],
        &ZipFixtureOptions::default(),
    )?;
    let bytes = std::fs::read(&valid_path)?;
    // Truncate last 15 bytes of EOCD
    let truncated = &bytes[..bytes.len().saturating_sub(15)];
    std::fs::write(path, truncated)?;
    Ok(path.to_path_buf())
}

/// Write a large entry count ZIP archive.
pub fn write_large_entry_count_zip(path: &Path, count: usize) -> Result<PathBuf, std::io::Error> {
    let files: Vec<ZipFixtureFile> = (0..count)
        .map(|i| ZipFixtureFile::stored(&format!("entry_{i}.dat"), b"x"))
        .collect();
    write_zip(path, &files, &ZipFixtureOptions::default())
}
