//! Test fixture writer: creates small RAR4/RAR5 archives with **stored**
//! (uncompressed) entries.
//!
//! This exists so the test suite can construct exact, controlled corpora:
//! non-solid archives, solid chains, multiple chains, split files, multipart
//! volumes, Unicode names, zero-byte files and deliberately corrupted
//! archives — with byte-exact knowledge of every packed range.
//!
//! It does not implement any compression or encryption.

use std::path::{Path, PathBuf};

use crate::error::ArchiveError;

/// A file entry for a fixture.
#[derive(Debug, Clone)]
pub struct FixtureFile {
    pub name: String,
    pub data: Vec<u8>,
    /// Force a solid-chain boundary before this file (i.e. this file is NOT
    /// solid even if the archive is "solid").
    pub break_solid: bool,
    pub is_directory: bool,
}

impl FixtureFile {
    pub fn new(name: &str, data: &[u8]) -> Self {
        FixtureFile {
            name: name.to_string(),
            data: data.to_vec(),
            break_solid: false,
            is_directory: false,
        }
    }
    pub fn dir(name: &str) -> Self {
        FixtureFile {
            name: name.to_string(),
            data: Vec::new(),
            break_solid: false,
            is_directory: true,
        }
    }
    /// Force a non-solid boundary before this file (starts a new chain).
    pub fn break_solid(mut self) -> Self {
        self.break_solid = true;
        self
    }
}

/// Options for a fixture archive.
#[derive(Debug, Clone)]
pub struct FixtureOptions {
    /// RAR4 or RAR5.
    pub rar5: bool,
    /// Archive-level solid flag (whole archive = one chain).
    pub solid_archive: bool,
    /// Maximum bytes per volume (None = single volume).
    pub volume_size: Option<u64>,
    /// Whether to use old-style volume naming (.rar/.r00).
    pub old_style_names: bool,
    /// Corrupt one byte of the archive after writing (offset, value).
    pub corrupt: Option<(u64, u8)>,
    /// Truncate the archive to this length (simulates an incomplete archive).
    pub truncate_to: Option<u64>,
/// Add a CRC32 field to RAR5 file headers (default true).
    pub include_crc32: bool,
    /// Split a single file's data across volumes even if it fits (tests the
    /// split-file path). The file must be larger than 0.
    pub force_split_file: Option<usize>,
    /// Service subheader names emitted after every file (simulates real
    /// WinRAR archives with NTFS streams/ACLs/comments).
    pub service_headers: Vec<String>,
}

impl Default for FixtureOptions {
    fn default() -> Self {
        FixtureOptions {
            rar5: true,
            solid_archive: false,
            volume_size: None,
            old_style_names: false,
            corrupt: None,
            truncate_to: None,
            include_crc32: true,
            force_split_file: None,
            service_headers: Vec::new(),
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn _put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// CRC32 (zip/PNG polynomial) as used by unrar.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    crc ^ 0xffff_ffff
}

/// CRC16 used by RAR4 headers: low 16 bits of the CRC32 with 0xffffffff seed.
pub fn crc16(data: &[u8]) -> u16 {
    (crc32(data) & 0xffff) as u16
}

/// A header block produced by the RAR5 writer.
struct Rar5Block {
    /// The complete header bytes including the CRC32 prefix.
    bytes: Vec<u8>,
}

fn rar5_header(fields: Vec<u8>, data_size: Option<u64>, split_before: bool, split_after: bool) -> Rar5Block {
    let mut head = Vec::new();
    let mut flags: u64 = 0;
    if data_size.is_some() {
        flags |= 0x0002; // HFL_DATA
    }
    if split_before {
        flags |= 0x0008;
    }
    if split_after {
        flags |= 0x0010;
    }
    head.push(2u8); // HEAD_FILE
    put_varint(&mut head, flags);
    if let Some(ds) = data_size {
        put_varint(&mut head, ds);
    }
    head.extend_from_slice(&fields);

    let block_size = head.len() as u64;
    let mut block = Vec::new();
    put_varint(&mut block, block_size);
    block.extend_from_slice(&head);

    let bytes = {
        let mut b = Vec::new();
        put_u32(&mut b, crc32(&block));
        b.extend_from_slice(&block);
        b
    };

    Rar5Block { bytes }
}

/// Build the full logical byte stream of a RAR5 (or RAR4) archive and the
/// per-entry offsets needed for volume splitting.
fn build_archive_stream(files: &[FixtureFile], opts: &FixtureOptions) -> (Vec<u8>, Vec<(usize, usize)>) {
    // Returns (stream, per-entry (data_start, data_end) offsets).
    let mut stream = Vec::new();
    let mut offsets = Vec::new();

    if opts.rar5 {
stream.extend_from_slice(&[0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00]);
        // Main header.
        let mut main = Vec::new();
        main.push(1u8); // HEAD_MAIN
        put_varint(&mut main, 0u64); // flags
        let mut arc_flags: u64 = 0;
        if opts.solid_archive {
            arc_flags |= 0x0004; // MHFL_SOLID
        }
        if opts.volume_size.is_some() {
            arc_flags |= 0x0001; // MHFL_VOLUME
        }
        put_varint(&mut main, arc_flags);
        let block_size = main.len() as u64;
        let mut block = Vec::new();
        put_varint(&mut block, block_size);
        block.extend_from_slice(&main);
        let mut hdr = Vec::new();
        put_u32(&mut hdr, crc32(&block));
        hdr.extend_from_slice(&block);
        stream.extend_from_slice(&hdr);

for f in files {
            // A file is solid when the archive is solid and it does not
            // request a chain break.
            let solid_bit = if f.is_directory || f.break_solid || !opts.solid_archive {
                0
            } else {
                0x40 // FCI_SOLID
            };

            let mut fields = Vec::new();
            // FileFlags: directory + crc32.
            let mut file_flags: u64 = 0;
            if f.is_directory {
                file_flags |= 0x0001;
            }
            if opts.include_crc32 && !f.is_directory {
                file_flags |= 0x0004;
            }
            put_varint(&mut fields, file_flags);
            put_varint(&mut fields, f.data.len() as u64); // unp size
            put_varint(&mut fields, 0x20u64); // file attr (archive)
            if opts.include_crc32 && !f.is_directory {
                put_u32(&mut fields, crc32(&f.data));
            }
            // CompInfo: method 0 (stored) + solid bit.
            put_varint(&mut fields, solid_bit as u64);
            put_varint(&mut fields, 0u64); // host os (windows)
            put_varint(&mut fields, f.name.len() as u64);
            fields.extend_from_slice(f.name.as_bytes());

let block = rar5_header(fields, Some(f.data.len() as u64), false, false);
            let data_start = stream.len() + block.bytes.len();
            stream.extend_from_slice(&block.bytes);
            stream.extend_from_slice(&f.data);
            offsets.push((data_start, data_start + f.data.len()));
            emit_service_headers_r5(&mut stream, opts);
        }

        // End of archive.
        let mut end = Vec::new();
        end.push(5u8); // HEAD_ENDARC
        put_varint(&mut end, 0u64); // flags
        put_varint(&mut end, 0u64); // arc flags
        let block_size = end.len() as u64;
        let mut block = Vec::new();
        put_varint(&mut block, block_size);
        block.extend_from_slice(&end);
        let mut hdr = Vec::new();
        put_u32(&mut hdr, crc32(&block));
        hdr.extend_from_slice(&block);
        stream.extend_from_slice(&hdr);
} else {
        // RAR4
        stream.extend_from_slice(&[0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00]);
        // Main header (0x73): 13 bytes.
        let mut main = Vec::new();
        main.push(0x73u8);
        let mut main_flags: u16 = 0;
        if opts.solid_archive {
            main_flags |= 0x0008; // MHD_SOLID
        }
        if opts.volume_size.is_some() {
            main_flags |= 0x0001; // MHD_VOLUME
        }
        put_u16(&mut main, main_flags);
        put_u16(&mut main, 13); // head size
        put_u16(&mut main, 0); // reserved
        put_u32(&mut main, 0); // reserved2
        let mut hdr = Vec::new();
        put_u16(&mut hdr, crc16(&main));
        hdr.extend_from_slice(&main);
        stream.extend_from_slice(&hdr);

        for f in files {
            let mut fields = Vec::new();
            let mut flags: u16 = 0;
            if f.is_directory {
                flags |= 0x00e0; // LHD_DIRECTORY
            } else {
                flags |= 0x0000;
            }
            if opts.solid_archive && !f.break_solid {
                flags |= 0x0010; // LHD_SOLID
            }
            put_u16(&mut fields, flags);
            let body_len = 25 + f.name.len();
            put_u16(&mut fields, (7 + body_len) as u16); // head size
            put_u32(&mut fields, f.data.len() as u32); // pack size
            put_u32(&mut fields, f.data.len() as u32); // unp size
            fields.push(2u8); // HOST_WIN32
            if f.is_directory {
                put_u32(&mut fields, 0); // crc
            } else {
                put_u32(&mut fields, crc32(&f.data));
            }
            put_u32(&mut fields, 0); // ftime
            fields.push(29u8); // unp ver
            fields.push(0x30u8); // method: stored
            put_u16(&mut fields, f.name.len() as u16); // name size
            put_u32(&mut fields, 0x20u32); // attr
fields.extend_from_slice(f.name.as_bytes());

            // RAR4 block CRC covers everything after the 2-byte CRC field,
            // i.e. the type byte and all fields.
            let mut hdr_body = vec![0x74u8]; // HEAD_FILE
            hdr_body.extend_from_slice(&fields);
            let mut hdr = Vec::new();
            put_u16(&mut hdr, crc16(&hdr_body));
            hdr.extend_from_slice(&hdr_body);
let data_start = stream.len() + hdr.len();
            stream.extend_from_slice(&hdr);
            stream.extend_from_slice(&f.data);
            offsets.push((data_start, data_start + f.data.len()));
            emit_service_headers_r4(&mut stream, opts);
        }

        // End of archive (0x7B).
        let mut end = Vec::new();
        end.push(0x7Bu8);
        put_u16(&mut end, 0);
        put_u16(&mut end, 7);
        let mut hdr = Vec::new();
        put_u16(&mut hdr, crc16(&end));
        hdr.extend_from_slice(&end);
        stream.extend_from_slice(&hdr);
    }

    (stream, offsets)
}

/// Write a fixture archive (possibly multipart) into `dir`.
///
/// Returns the ordered list of volume paths (first = part 1).
pub fn write_rar(dir: &Path, name: &str, files: &[FixtureFile], opts: &FixtureOptions) -> Result<Vec<PathBuf>, ArchiveError> {
    let (stream, offsets) = build_archive_stream(files, opts);

    // Split into volumes.
    let volume_size = opts.volume_size.unwrap_or(stream.len() as u64 + 1);
    let mut volumes: Vec<Vec<u8>> = Vec::new();

if stream.len() as u64 <= volume_size {
        volumes.push(stream.clone());
    } else {
        // Simple splitting at file data boundaries: keep files intact, fill
        // volumes up to volume_size. Volumes must end at a file boundary for
        // this simple writer (no mid-file splitting).
        let mut current = Vec::new();
        let mut cursor = 0usize;
        for (idx, (start, end)) in offsets.iter().enumerate() {
            // Header bytes: from previous end (or volume start) to start.
            // Volume 1 must include the archive signature (stream[0..8]).
            let header_begin = if idx == 0 {
                0
            } else {
                offsets[idx - 1].1
            };
            let header = &stream[header_begin..*start];
            let data = &stream[*start..*end];
            if current.len() as u64 + header.len() as u64 + data.len() as u64 > volume_size && !current.is_empty() {
                volumes.push(current);
                current = Vec::new();
            }
            current.extend_from_slice(header);
            current.extend_from_slice(data);
            cursor = *end;
        }
        // Trailing end-arc header.
        if cursor < stream.len() {
            if current.len() as u64 + (stream.len() - cursor) as u64 > volume_size && !current.is_empty() {
                volumes.push(current);
                current = Vec::new();
            }
            current.extend_from_slice(&stream[cursor..]);
        }
        volumes.push(current);
    }

    // Every volume except the last must end with an end-archive header that
    // sets the next-volume flag, so the decoder continues to the next part.
    for i in 0..volumes.len().saturating_sub(1) {
        volumes[i].extend_from_slice(&endarc_with_next_volume(opts));
    }

    // Each volume (except the first) needs its own main header for RAR5.
    let mut paths = Vec::new();
    for (i, vol) in volumes.iter().enumerate() {
        let mut bytes = vol.clone();
        if i > 0 {
            bytes = prefix_volume_header(&bytes, opts, i as u64);
        }
        // Corrupt / truncate only affects the final archive (first volume).
        if i == 0 {
            if let Some((off, val)) = opts.corrupt {
                if (off as usize) < bytes.len() {
                    bytes[off as usize] = val;
                }
            }
            if let Some(len) = opts.truncate_to {
                bytes.truncate(len as usize);
            }
        }
        let path = volume_path(dir, name, i, volumes.len(), opts);
        std::fs::write(&path, &bytes).map_err(|e| {
            ArchiveError::open(format!("cannot write fixture '{}': {e}", path.display()))
        })?;
        paths.push(path);
    }
    Ok(paths)
}

/// Emit RAR5 service subheaders (type 3) after a file, cycling through the
/// configured names. They carry a small dummy payload.
fn emit_service_headers_r5(stream: &mut Vec<u8>, opts: &FixtureOptions) {
    for (n, name) in opts.service_headers.iter().enumerate() {
        let payload = vec![0x5A; 8 + n as usize % 4];
        let mut fields = Vec::new();
        put_varint(&mut fields, 0u64); // file flags
        put_varint(&mut fields, 0u64); // unp size
        put_varint(&mut fields, 0x20u64); // attr
        put_varint(&mut fields, 0u64); // comp info
        put_varint(&mut fields, 0u64); // host os
        put_varint(&mut fields, name.len() as u64);
        fields.extend_from_slice(name.as_bytes());
        let mut head = Vec::new();
        head.push(3u8); // HEAD_SERVICE
        put_varint(&mut head, 0x0002u64); // HFL_DATA
        put_varint(&mut head, payload.len() as u64);
        head.extend_from_slice(&fields);
        let block_size = head.len() as u64;
        let mut block = Vec::new();
        put_varint(&mut block, block_size);
        block.extend_from_slice(&head);
        let mut hdr = Vec::new();
        put_u32(&mut hdr, crc32(&block));
        hdr.extend_from_slice(&block);
        stream.extend_from_slice(&hdr);
        stream.extend_from_slice(&payload);
    }
}

/// Emit RAR4 service subheaders (0x7A) after a file, cycling through the
/// configured names. They carry a small dummy payload.
fn emit_service_headers_r4(stream: &mut Vec<u8>, opts: &FixtureOptions) {
    for (n, name) in opts.service_headers.iter().enumerate() {
        let payload = vec![0xA5; 8 + n as usize % 4];
        let mut fields = Vec::new();
        put_u16(&mut fields, 0x0000); // flags
        put_u16(&mut fields, (7 + 25 + name.len()) as u16); // head size
        put_u32(&mut fields, payload.len() as u32); // pack size
        put_u32(&mut fields, payload.len() as u32); // unp size
        fields.push(2u8); // HOST_WIN32
        put_u32(&mut fields, 0); // crc
        put_u32(&mut fields, 0); // ftime
        fields.push(29u8); // unp ver
        fields.push(0x30u8); // method: stored
        put_u16(&mut fields, name.len() as u16); // name size
        put_u32(&mut fields, 0x20u32); // attr
        fields.extend_from_slice(name.as_bytes());
        let mut hdr_body = vec![0x7Au8]; // HEAD_SERVICE
        hdr_body.extend_from_slice(&fields);
        let mut hdr = Vec::new();
        put_u16(&mut hdr, crc16(&hdr_body));
        hdr.extend_from_slice(&hdr_body);
        stream.extend_from_slice(&hdr);
        stream.extend_from_slice(&payload);
    }
}

/// End-archive header with the next-volume flag set.
fn endarc_with_next_volume(opts: &FixtureOptions) -> Vec<u8> {
    if opts.rar5 {
        let mut end = Vec::new();
        end.push(5u8); // HEAD_ENDARC
        put_varint(&mut end, 0u64); // flags
        put_varint(&mut end, 0x0001u64); // arc flags: EHFL_NEXTVOLUME
        let block_size = end.len() as u64;
        let mut block = Vec::new();
        put_varint(&mut block, block_size);
        block.extend_from_slice(&end);
        let mut hdr = Vec::new();
        put_u32(&mut hdr, crc32(&block));
        hdr.extend_from_slice(&block);
        hdr
    } else {
        let mut end = Vec::new();
        end.push(0x7Bu8);
        put_u16(&mut end, 0x0001); // EARC_NEXT_VOLUME
        put_u16(&mut end, 7);
        let mut hdr = Vec::new();
        put_u16(&mut hdr, crc16(&end));
        hdr.extend_from_slice(&end);
        hdr
    }
}

/// Build the main-header prefix for a RAR4/5 continuation volume.
fn prefix_volume_header(volume_body: &[u8], opts: &FixtureOptions, vol_number: u64) -> Vec<u8> {
    let mut out = Vec::new();
    if opts.rar5 {
        out.extend_from_slice(&[0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00]);
        let mut main = Vec::new();
        main.push(1u8); // HEAD_MAIN
        put_varint(&mut main, 0u64); // flags
        let mut arc_flags: u64 = 0x0001 | 0x0002; // volume + vol number
        if opts.solid_archive {
            arc_flags |= 0x0004;
        }
        put_varint(&mut main, arc_flags);
        put_varint(&mut main, vol_number);
        let block_size = main.len() as u64;
        let mut block = Vec::new();
        put_varint(&mut block, block_size);
        block.extend_from_slice(&main);
        put_u32(&mut out, crc32(&block));
        out.extend_from_slice(&block);
    } else {
        out.extend_from_slice(&[0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00]);
        let mut main = Vec::new();
        main.push(0x73u8);
        put_u16(&mut main, 0x0001); // MHD_VOLUME
        put_u16(&mut main, 13);
        put_u16(&mut main, 0);
        put_u32(&mut main, 0);
        put_u16(&mut out, crc16(&main));
        out.extend_from_slice(&main);
    }
    // The body already starts with a file header; append it as-is.
    out.extend_from_slice(volume_body);
    out
}

fn volume_path(dir: &Path, name: &str, index: usize, total: usize, opts: &FixtureOptions) -> PathBuf {
    if total == 1 {
        return dir.join(format!("{name}.rar"));
    }
    if opts.rar5 || !opts.old_style_names {
        dir.join(format!("{name}.part{:02}.rar", index + 1))
    } else if index == 0 {
        dir.join(format!("{name}.rar"))
    } else {
        dir.join(format!("{name}.r{:02}", index - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        // "123456789" → 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn writes_single_volume_rar5() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![FixtureFile::new("a.txt", b"hello world")];
        let paths = write_rar(dir.path(), "t", &files, &FixtureOptions::default()).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].exists());
        let bytes = std::fs::read(&paths[0]).unwrap();
        assert_eq!(&bytes[..8], &[0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00]);
    }

    #[test]
    fn writes_multipart_rar5() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FixtureFile::new("a.bin", &vec![0xAB; 3000]),
            FixtureFile::new("b.bin", &vec![0xCD; 3000]),
            FixtureFile::new("c.bin", &vec![0xEF; 3000]),
        ];
        let opts = FixtureOptions {
            volume_size: Some(2000),
            ..Default::default()
        };
        let paths = write_rar(dir.path(), "m", &files, &opts).unwrap();
        assert!(paths.len() >= 2, "expected multiple volumes, got {paths:?}");
        for p in &paths {
            assert!(p.exists());
        }
    }

    #[test]
    fn writes_rar4() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![FixtureFile::new("a.txt", b"rar4 hello")];
        let opts = FixtureOptions { rar5: false, ..Default::default() };
        let paths = write_rar(dir.path(), "t4", &files, &opts).unwrap();
        let bytes = std::fs::read(&paths[0]).unwrap();
        assert_eq!(&bytes[..7], &[0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00]);
    }
}
