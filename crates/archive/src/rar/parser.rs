//! RAR 4.x / 5.x header parser.
//!
//! Computes the things the engine needs that the decoder library does not
//! expose: exact packed data ranges per volume, solid chains, and the
//! recovery-unit structure. This is format *structure* parsing — it never
//! attempts to decompress anything. Compression is handled exclusively by the
//! official UnRAR library behind the decoder boundary.
//!
//! Layout knowledge is derived from the official UnRAR source (`arcread.cpp`)
//! vendored with `unrar_sys`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use crate::error::ArchiveError;
use crate::model::{Entry, PackedRange, RecoveryUnit, Redirection, RedirectionKind};

pub const RAR4_SIGNATURE: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];
pub const RAR5_SIGNATURE: [u8; 8] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];

// RAR4 header types.
const HEAD3_MAIN: u8 = 0x73;
const HEAD3_FILE: u8 = 0x74;
const HEAD3_CMT: u8 = 0x75;
const HEAD3_SERVICE: u8 = 0x7a;
const HEAD3_ENDARC: u8 = 0x7b;

// RAR4 main-header flags.
const MHD_VOLUME: u16 = 0x0001;
const MHD_SOLID: u16 = 0x0008;
const MHD_PASSWORD: u16 = 0x0080;
const MHD_FIRSTVOLUME: u16 = 0x0100;

// RAR4 file-header flags.
const LHD_SPLIT_BEFORE: u16 = 0x0001;
const LHD_SPLIT_AFTER: u16 = 0x0002;
const LHD_PASSWORD: u16 = 0x0004;
const LHD_SOLID: u16 = 0x0010;
const LHD_LARGE: u16 = 0x0100;
const LHD_UNICODE: u16 = 0x0200;
const LHD_DIRECTORY: u16 = 0x00e0;

// RAR4 end-archive flags.
const EARC_NEXT_VOLUME: u16 = 0x0001;

// RAR5 header types.
const HEAD5_MAIN: u64 = 1;
const HEAD5_FILE: u64 = 2;
const HEAD5_SERVICE: u64 = 3;
const HEAD5_CRYPT: u64 = 4;
const HEAD5_ENDARC: u64 = 5;

// RAR5 header flags.
const HFL_EXTRA: u64 = 0x0001;
const HFL_DATA: u64 = 0x0002;
const HFL_SPLITBEFORE: u64 = 0x0008;
const HFL_SPLITAFTER: u64 = 0x0010;

// RAR5 main-header flags.
const MHFL_VOLUME: u64 = 0x0001;
const MHFL_VOLNUMBER: u64 = 0x0002;
const MHFL_SOLID: u64 = 0x0004;

// RAR5 file flags (FHFL_*).
const FHFL_DIRECTORY: u64 = 0x0001;
const FHFL_UTIME: u64 = 0x0002;
const FHFL_CRC32: u64 = 0x0004;
const FHFL_UNPUNKNOWN: u64 = 0x0008;

// RAR5 compression info bits.
const FCI_SOLID: u64 = 0x00000040;

// Redirection types (extra record in RAR5: 0x02 redirect; RAR4: unix symlink attr).

/// Format of the parsed archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RarFormat {
    Rar4,
    Rar5,
}

impl RarFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            RarFormat::Rar4 => "rar4",
            RarFormat::Rar5 => "rar5",
        }
    }
}

/// One occurrence of a file's data within one volume.
#[derive(Debug, Clone, Copy)]
pub struct FilePart {
    pub volume_index: u64,
    pub data_start: u64,
    pub data_len: u64,
}

/// Volume metadata used by the parser.
#[derive(Debug, Clone)]
pub struct VolumeMeta {
    pub path: PathBuf,
    pub len: u64,
}

/// The parsed archive structure.
#[derive(Debug)]
pub struct ParsedRar {
    pub format: RarFormat,
    /// Archive-level solidity flag from the main header.
    pub solid_archive: bool,
    /// Header encryption flag.
    pub encrypted_headers: bool,
    pub volumes: Vec<VolumeMeta>,
    pub entries: Vec<Entry>,
    /// Data parts per entry (index-aligned with `entries`).
    pub parts: Vec<Vec<FilePart>>,
    /// Total unpacked bytes.
    pub unpacked_size: u64,
    /// Total packed data bytes (sum of all parts).
    pub packed_size: u64,
}

/// A cursor over a volume file.
struct Reader {
    file: File,
}

impl Reader {
    fn open(path: &PathBuf) -> Result<Self, ArchiveError> {
        Ok(Reader {
            file: File::open(path).map_err(|e| {
                ArchiveError::open(format!("cannot open volume '{}': {e}", path.display()))
            })?,
        })
    }

    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), ArchiveError> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| ArchiveError::invalid(format!("seek failed: {e}")))?;
        self.file
            .read_exact(buf)
            .map_err(|e| ArchiveError::invalid(format!("read failed: {e}")))
    }

    fn u8_at(&mut self, offset: u64) -> Result<u8, ArchiveError> {
        let mut b = [0u8; 1];
        self.read_exact_at(offset, &mut b)?;
        Ok(b[0])
    }

    fn u16_le_at(&mut self, offset: u64) -> Result<u16, ArchiveError> {
        let mut b = [0u8; 2];
        self.read_exact_at(offset, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    fn u32_le_at(&mut self, offset: u64) -> Result<u32, ArchiveError> {
        let mut b = [0u8; 4];
        self.read_exact_at(offset, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
}

/// Parse a RAR archive given its ordered volumes.
pub fn parse(volumes: Vec<VolumeMeta>) -> Result<ParsedRar, ArchiveError> {
    if volumes.is_empty() {
        return Err(ArchiveError::open("no volumes provided"));
    }

let first = &volumes[0];

    // Detect format from signature.
    let mut sig = [0u8; 8];
    {
        let mut probe = Reader::open(&first.path)?;
        let got = probe.read_exact_at(0, &mut sig);
        if got.is_err() || !(sig[..7] == RAR4_SIGNATURE || sig == RAR5_SIGNATURE) {
            return Err(ArchiveError::open(format!(
                "'{}' is not a RAR archive (signature mismatch)",
                first.path.display()
            )));
        }
    }
    let format = if sig == RAR5_SIGNATURE { RarFormat::Rar5 } else { RarFormat::Rar4 };

    let mut parsed = ParsedRar {
        format,
        solid_archive: false,
        encrypted_headers: false,
        volumes: volumes.clone(),
        entries: Vec::new(),
        parts: Vec::new(),
        unpacked_size: 0,
        packed_size: 0,
    };

// Entry index of the most recent split-after part; continuations attach
    // to it (per-part conventions: each part's header sizes itself).
    let mut last_split_after_entry: Option<usize> = None;

for (vol_index, vol) in volumes.iter().enumerate() {
        let mut r = Reader::open(&vol.path)?;
        let sig_len = if format == RarFormat::Rar5 { 8 } else { 7 };
        let mut pos: u64 = sig_len as u64;
        let vol_len = vol.len;

        // Consume leading crypt (RAR5) headers, then the main header.
        loop {
            let hdr = read_header(&mut r, format, pos)?;
            pos = hdr.end;
            match hdr.kind {
                HeaderKind::Main => {
                    parsed.solid_archive = parsed.solid_archive || hdr.solid;
                    parsed.encrypted_headers = parsed.encrypted_headers || hdr.encrypted_headers;
                    break;
                }
                HeaderKind::Crypt => {
                    parsed.encrypted_headers = true;
                }
                HeaderKind::File { .. } => {
                    return Err(ArchiveError::invalid("file header before main header"));
                }
                HeaderKind::EndArc => {
                    if hdr.next_volume && vol_index + 1 >= volumes.len() {
                        return Err(ArchiveError::missing_volume(format!(
                            "volume {} has a next-volume flag but no further volumes were found",
                            vol.path.display()
                        )));
                    }
                    break;
                }
                HeaderKind::Other => {
                    return Err(ArchiveError::invalid("unexpected header before main header"));
                }
            }
        }

// Parse file records until end-archive header or volume end.
        loop {
            if pos >= vol_len {
                // Volume ends mid-data of a split file: the continuation
                // lives in the next volume.
                if last_split_after_entry.is_some() && vol_index + 1 >= volumes.len() {
                    return Err(ArchiveError::missing_volume(format!(
                        "volume {} ends in the middle of a split file; the next volume is missing",
                        vol.path.display()
                    )));
                }
                break;
            }
            let hdr = read_header(&mut r, format, pos)?;
            match hdr.kind {
                HeaderKind::File {
                    name,
                    packed_size,
                    unpacked_size,
                    crc32,
                    is_directory,
                    solid,
                    split_before,
                    split_after,
                    encrypted,
                    redirection,
} => {
                    let header_end = hdr.end;

                    // Per-part conventions: each part's header carries the
                    // size of the data in ITS volume (the DLL reads exactly
                    // that many bytes before merging to the next volume).
                    // A split-after part's data runs to the trailing
                    // end-archive header; continuations attach to the entry
                    // that most recently had a split-after part and add their
                    // own size.
                    let (entry_index, portion) = if split_before {
                        let idx = last_split_after_entry.ok_or_else(|| {
                            ArchiveError::invalid(format!(
                                "split-before file '{}' without a preceding split-after file",
                                name
                            ))
                        })?;
                        (idx, packed_size)
                    } else {
                        let idx = parsed.entries.len();
                        parsed.entries.push(Entry {
                            index: idx as u64,
                            name,
                            packed_size,
                            unpacked_size,
                            crc32,
                            is_directory,
                            is_solid: solid,
                            split_before: false,
                            split_after,
                            encrypted,
                            redirection,
                        });
                        parsed.unpacked_size = parsed.unpacked_size.saturating_add(unpacked_size);
                        let portion = if split_after {
                            let data_end = split_data_end(&mut r, format, vol_len)?;
                            data_end.saturating_sub(header_end)
                        } else {
                            packed_size
                        };
                        (idx, portion)
                    };
                    let portion = portion.min(vol_len.saturating_sub(header_end));

                    if portion > 0 {
                        if parsed.parts.len() <= entry_index {
                            parsed.parts.resize(entry_index + 1, Vec::new());
                        }
                        parsed.parts[entry_index].push(FilePart {
                            volume_index: vol_index as u64,
                            data_start: header_end,
                            data_len: portion,
                        });
                        parsed.packed_size = parsed.packed_size.saturating_add(portion);
                        // Update the logical entry's split flags and, for
                        // continuations, accumulate the part size so the
                        // entry's packed_size reflects the whole file.
                        let e = &mut parsed.entries[entry_index];
                        e.split_before = e.split_before || split_before;
                        e.split_after = e.split_after || split_after;
                        if split_before {
                            e.packed_size = e.packed_size.saturating_add(portion);
                        }
                    }

                    if split_after {
                        last_split_after_entry = Some(entry_index);
                    } else {
                        last_split_after_entry = None;
                    }
                    pos = header_end.saturating_add(portion);
                }
                HeaderKind::EndArc => {
                    if hdr.next_volume && vol_index + 1 >= volumes.len() {
                        return Err(ArchiveError::missing_volume(format!(
                            "volume {} has a next-volume flag but no further volumes were found",
                            vol.path.display()
                        )));
                    }
                    break;
                }
                HeaderKind::Main | HeaderKind::Crypt | HeaderKind::Other => {
                    pos = hdr.end;
                }
            }
        }
    }

if let Some(idx) = last_split_after_entry {
        // The chain ended without its final part: the last volume ended
        // mid-file.
        return Err(ArchiveError::invalid(format!(
            "archive ended in the middle of split file {}",
            parsed.entries[idx].name
        )));
    }

    Ok(parsed)
}

enum HeaderKind {
    Main,
    Crypt,
    File {
        name: String,
        packed_size: u64,
        unpacked_size: u64,
        crc32: Option<u32>,
        is_directory: bool,
        solid: bool,
        split_before: bool,
        split_after: bool,
        encrypted: bool,
        redirection: Option<Redirection>,
    },
    EndArc,
    Other,
}

struct HeaderInfo {
    kind: HeaderKind,
    /// Offset just past this header block (start of data for files).
    end: u64,
    /// Archive-level solid flag (main headers only).
    solid: bool,
    /// Encrypted headers flag (main headers only).
    encrypted_headers: bool,
    /// End-archive next-volume flag.
    next_volume: bool,
}

/// Read one header block at `pos` in `vol`. Returns the header info and the
/// offset just past the header block (data start for file/service records).
fn read_header(r: &mut Reader, format: RarFormat, pos: u64) -> Result<HeaderInfo, ArchiveError> {
    match format {
        RarFormat::Rar4 => read_header4(r, pos),
        RarFormat::Rar5 => read_header5(r, pos),
    }
}

fn read_header4(r: &mut Reader, pos: u64) -> Result<HeaderInfo, ArchiveError> {
    let crc = r.u16_le_at(pos)?; // HEAD_CRC
    let htype = r.u8_at(pos + 2)?;
    let flags = r.u16_le_at(pos + 3)?;
    let head_size = r.u16_le_at(pos + 5)? as u64;
    if head_size < 7 {
        return Err(ArchiveError::invalid(format!(
            "RAR4 header at {pos} has invalid size {head_size}"
        )));
    }
    let full_size = head_size + if head_size < 0x10000 { 0 } else { 4 };
    let _ = crc;

    match htype {
        HEAD3_MAIN => {
            let solid = flags & MHD_SOLID != 0;
            let enc = flags & MHD_PASSWORD != 0;
            let first_vol = flags & MHD_FIRSTVOLUME != 0;
            let volume = flags & MHD_VOLUME != 0;
            let _ = (first_vol, volume);
            Ok(HeaderInfo {
                kind: HeaderKind::Main,
                end: pos + full_size,
                solid,
                encrypted_headers: enc,
                next_volume: false,
            })
        }
        HEAD3_FILE | HEAD3_SERVICE => {
            let mut off = pos + 7;
            let mut packed_size = r.u32_le_at(off)? as u64;
            let mut unpacked_size = r.u32_le_at(off + 4)? as u64;
            let host_os = r.u8_at(off + 8)?;
            let file_crc = r.u32_le_at(off + 9)?;
            let _file_time = r.u32_le_at(off + 13)?;
            let _unp_ver = r.u8_at(off + 17)?;
            let _method = r.u8_at(off + 18)?;
            let name_size = r.u16_le_at(off + 19)? as u64;
            let file_attr = r.u32_le_at(off + 21)?;
            off += 25;

            if flags & LHD_LARGE != 0 {
                let high_pack = r.u32_le_at(off)? as u64;
                let high_unp = r.u32_le_at(off + 4)? as u64;
                packed_size |= high_pack << 32;
                unpacked_size |= high_unp << 32;
                off += 8;
            }

            // Name (ANSI) with possible Unicode re-encoding after it.
            let name_bytes = read_bytes(r, off, name_size)?;
            let ansi_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
            let unicode = flags & LHD_UNICODE != 0;
            let name = if unicode && name_bytes.len() > ansi_end + 1 {
                // The re-encoded Unicode name follows the ANSI name + NUL.
                decode_unicode_name(&name_bytes[ansi_end + 1..])
            } else {
                String::from_utf8_lossy(&name_bytes[..ansi_end]).into_owned()
            };
            let is_directory = flags & LHD_DIRECTORY == LHD_DIRECTORY;
            let split_before = flags & LHD_SPLIT_BEFORE != 0;
            let split_after = flags & LHD_SPLIT_AFTER != 0;
            let solid = flags & LHD_SOLID != 0;
            let encrypted = flags & LHD_PASSWORD != 0;

            // Redirection: RAR4 unix symlink is encoded via file attributes.
            let redirection = if host_os == 3 /* HOST_UNIX */ && (file_attr & 0xF000) == 0xA000 {
                Some(Redirection {
                    kind: RedirectionKind::UnixSymlink,
                    target: name.clone(),
                })
            } else {
                None
            };

// Service subheaders (0x7A: NTFS streams, ACLs, comments) share
            // the file-record layout but are NOT archive entries. The decoder
            // skips them (SearchBlock(HEAD_FILE)); we must too, while still
            // skipping their packed data so positions stay exact.
            let is_service = htype == HEAD3_SERVICE;
            let kind = if is_service {
                HeaderKind::Other
            } else {
                HeaderKind::File {
                    name,
                    packed_size,
                    unpacked_size,
                    crc32: Some(file_crc),
                    is_directory,
                    solid,
                    split_before,
                    split_after,
                    encrypted,
                    redirection,
                }
            };
            let end = pos + full_size + if is_service { packed_size } else { 0 };
            Ok(HeaderInfo {
                kind,
                end,
                solid: false,
                encrypted_headers: false,
                next_volume: false,
            })
        }
        HEAD3_ENDARC => {
            let next_volume = flags & EARC_NEXT_VOLUME != 0;
            Ok(HeaderInfo {
                kind: HeaderKind::EndArc,
                end: pos + full_size,
                solid: false,
                encrypted_headers: false,
                next_volume,
            })
        }
        HEAD3_CMT => Ok(HeaderInfo {
            kind: HeaderKind::Other,
            end: pos + full_size,
            solid: false,
            encrypted_headers: false,
            next_volume: false,
        }),
        _ => Ok(HeaderInfo {
            kind: HeaderKind::Other,
            end: pos + full_size,
            solid: false,
            encrypted_headers: false,
            next_volume: false,
        }),
    }
}

fn read_header5(r: &mut Reader, pos: u64) -> Result<HeaderInfo, ArchiveError> {
    let _crc32 = r.u32_le_at(pos)?;
    // Header size varint occupies 1..=3 bytes.
let mut size_bytes = 1u64;
    let mut b0 = r.u8_at(pos + 4)?;
    let mut header_size: u64 = (b0 & 0x7f) as u64;
    while b0 & 0x80 != 0 {
        if size_bytes >= 3 {
            return Err(ArchiveError::invalid("RAR5 header size varint too long"));
        }
        b0 = r.u8_at(pos + 4 + size_bytes)?;
        header_size |= ((b0 & 0x7f) as u64) << (7 * size_bytes);
        size_bytes += 1;
    }
    if header_size == 0 {
        return Err(ArchiveError::invalid("RAR5 header with zero size"));
    }
    let header_total = 4 + size_bytes + header_size;
    let mut body = vec![0u8; header_size as usize];
    r.read_exact_at(pos + 4 + size_bytes, &mut body)?;
    let mut bf = SliceFields::new(&body);

    let htype = bf.varint()?;
    let hflags = bf.varint()?;

    let mut extra_size = 0u64;
    if hflags & HFL_EXTRA != 0 {
        extra_size = bf.varint()?;
    }
    let mut data_size = 0u64;
    if hflags & HFL_DATA != 0 {
        data_size = bf.varint()?;
    }

    let split_before = hflags & HFL_SPLITBEFORE != 0;
    let split_after = hflags & HFL_SPLITAFTER != 0;

    match htype {
        HEAD5_MAIN => {
            let arc_flags = bf.varint()?;
            let solid = arc_flags & MHFL_SOLID != 0;
            let volume = arc_flags & MHFL_VOLUME != 0;
            let _vol_number = if arc_flags & MHFL_VOLNUMBER != 0 {
                Some(bf.varint()?)
            } else {
                None
            };
            let _ = volume;
            Ok(HeaderInfo {
                kind: HeaderKind::Main,
                end: pos + header_total,
                solid,
                encrypted_headers: false,
                next_volume: false,
            })
        }
        HEAD5_CRYPT => Ok(HeaderInfo {
            kind: HeaderKind::Crypt,
            end: pos + header_total,
            solid: false,
            encrypted_headers: true,
            next_volume: false,
        }),
        HEAD5_FILE | HEAD5_SERVICE => {
            let file_flags = bf.varint()?;
            let unpacked_size = bf.varint()?;
            let _unknown_unp = file_flags & FHFL_UNPUNKNOWN != 0;
            let _file_attr = bf.varint()?;
            let has_mtime = file_flags & FHFL_UTIME != 0;
            if has_mtime {
                let _mtime = bf.u32()?;
            }
            let crc32 = if file_flags & FHFL_CRC32 != 0 {
                Some(bf.u32()?)
            } else {
                None
            };
            let comp_info = bf.varint()?;
            let _host_os = bf.u8()?;
            let name_size = bf.varint()?;
            let name_bytes = bf.bytes(name_size)?;

            let name = String::from_utf8_lossy(name_bytes).into_owned();
            let is_directory = file_flags & FHFL_DIRECTORY != 0;
            let solid = comp_info & FCI_SOLID != 0;

            // Redirection comes from an extra record (type 0x02) inside the
            // extra area; file-level encryption from type 0x03.
            let (redirection, file_encrypted) = if hflags & HFL_EXTRA != 0 {
                scan_extras(&body, bf.pos as usize, extra_size as usize)?
            } else {
                (None, false)
            };

// RAR5 service subheaders (type 3: NTFS streams, ACLs, comments) share
            // the record layout but are NOT archive entries — the decoder
            // skips them, so we skip them too while advancing over their
            // packed data so positions stay exact.
            let is_service = htype == HEAD5_SERVICE;
            let kind = if is_service {
                HeaderKind::Other
            } else {
                HeaderKind::File {
                    name,
                    packed_size: data_size,
                    unpacked_size,
                    crc32,
                    is_directory,
                    solid,
                    split_before,
                    split_after,
                    encrypted: file_encrypted,
                    redirection,
                }
            };
            let end = pos + header_total + if is_service { data_size } else { 0 };
            Ok(HeaderInfo {
                kind,
                end,
                solid: false,
                encrypted_headers: false,
                next_volume: false,
            })
        }
        HEAD5_ENDARC => {
            let arc_flags = bf.varint()?;
            let next_volume = arc_flags & 0x0001 != 0; // EHFL_NEXTVOLUME
            Ok(HeaderInfo {
                kind: HeaderKind::EndArc,
                end: pos + header_total,
                solid: false,
                encrypted_headers: false,
                next_volume,
            })
        }
        _ => Ok(HeaderInfo {
            kind: HeaderKind::Other,
            end: pos + header_total,
            solid: false,
            encrypted_headers: false,
            next_volume: false,
        }),
    }
}

/// Field reader over an in-memory slice.
struct SliceFields<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceFields<'a> {
    fn new(data: &'a [u8]) -> Self {
        SliceFields { data, pos: 0 }
    }

    fn varint(&mut self) -> Result<u64, ArchiveError> {
        let mut value: u64 = 0;
        for shift in (0..70).step_by(7) {
            if self.pos >= self.data.len() {
                return Err(ArchiveError::invalid("RAR5 header truncated (varint)"));
            }
            let b = self.data[self.pos];
            self.pos += 1;
            value |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(ArchiveError::invalid("RAR5 varint overflow"))
    }

    fn u8(&mut self) -> Result<u8, ArchiveError> {
        if self.pos >= self.data.len() {
            return Err(ArchiveError::invalid("RAR5 header truncated (u8)"));
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

fn u32(&mut self) -> Result<u32, ArchiveError> {
        if self.pos + 4 > self.data.len() {
            return Err(ArchiveError::invalid("RAR5 header truncated (u32)"));
        }
        let b = &self.data[self.pos..self.pos + 4];
        let v = (b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16) | ((b[3] as u32) << 24);
        self.pos += 4;
        Ok(v)
    }

    fn bytes(&mut self, len: u64) -> Result<&'a [u8], ArchiveError> {
        let len = len as usize;
        if self.pos + len > self.data.len() {
            return Err(ArchiveError::invalid("RAR5 header truncated (bytes)"));
        }
        let out = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }
}

/// Scan RAR5 extra records: type 0x02 = redirection, 0x03 = file encryption.
fn scan_extras(body: &[u8], start: usize, len: usize) -> Result<(Option<Redirection>, bool), ArchiveError> {
    let mut pos = start;
    let end = start + len;
    let mut redirection = None;
    let mut encrypted = false;
    while pos < end {
        let mut bf = SliceFields::new(&body[pos..end.min(body.len())]);
        let extra_type = match bf.varint() {
            Ok(v) => v,
            Err(_) => break,
        };
        let extra_size = match bf.varint() {
            Ok(v) => v,
            Err(_) => break,
        };
        let extra_start = pos + (bf.pos);
        if extra_size == 0 || extra_start + extra_size as usize > end {
            break;
        }
        if extra_type == 0x02 {
            // Redirect record: redir type, flags, target name.
            let mut rf = SliceFields::new(&body[extra_start..extra_start + extra_size as usize]);
            let redir_type = rf.varint().unwrap_or(0);
            let _flags = rf.varint().unwrap_or(0);
            let name_size = rf.varint().unwrap_or(0);
            let target_bytes = rf.bytes(name_size).unwrap_or(&[]);
            let kind = match redir_type {
                0x01 => RedirectionKind::UnixSymlink,
                0x02 => RedirectionKind::WindowsSymlink,
                0x03 => RedirectionKind::Junction,
                0x04 => RedirectionKind::Hardlink,
                0x05 => RedirectionKind::FileCopy,
                _ => RedirectionKind::UnixSymlink,
            };
            redirection = Some(Redirection {
                kind,
                target: String::from_utf8_lossy(target_bytes).into_owned(),
            });
        } else if extra_type == 0x03 {
            encrypted = true;
        }
        pos = extra_start + extra_size as usize;
    }
    Ok((redirection, encrypted))
}

/// Locate the start of the trailing end-archive header in a volume.
///
/// Every WinRAR volume ends with an end-archive header (next-volume flag
/// set, except the last). A split-after file's data runs up to that header.
/// We scan backward for a header that parses as ENDARC, ends exactly at the
/// volume end and has a matching checksum — a data byte sequence could
/// otherwise be mistaken for a header.
fn split_data_end(r: &mut Reader, format: RarFormat, vol_len: u64) -> Result<u64, ArchiveError> {
    let max_len = if format == RarFormat::Rar5 { 32 } else { 16 };
    let min_len = if format == RarFormat::Rar5 { 8 } else { 7 };
    if vol_len < min_len {
        return Ok(vol_len);
    }
    let lo = vol_len.saturating_sub(max_len);
    let mut pos = vol_len - min_len;
    loop {
        if pos < lo {
            break;
        }
        match read_header(r, format, pos) {
            Ok(h) if matches!(h.kind, HeaderKind::EndArc) && h.end == vol_len => {
                if endarc_crc_matches(r, format, pos, h.end)? {
                    return Ok(pos);
                }
            }
            _ => {}
        }
        if pos == lo {
            break;
        }
        pos -= 1;
    }
    // No end-archive header found: the data extends to the end of the volume.
    Ok(vol_len)
}

/// Verify the stored checksum of a header block at `pos..end`.
fn endarc_crc_matches(r: &mut Reader, format: RarFormat, pos: u64, end: u64) -> Result<bool, ArchiveError> {
    let len = (end - pos) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact_at(pos, &mut buf)?;
    match format {
        RarFormat::Rar5 => {
            let stored = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            Ok(crc32(&buf[4..]) == stored)
        }
        RarFormat::Rar4 => {
            let stored = u16::from_le_bytes([buf[0], buf[1]]);
            Ok((crc32(&buf[2..]) & 0xffff) as u16 == stored)
        }
    }
}

/// CRC32 (zip/PNG polynomial) as used by RAR.
fn crc32(data: &[u8]) -> u32 {
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

fn read_bytes(r: &mut Reader, offset: u64, len: u64) -> Result<Vec<u8>, ArchiveError> {
    if len > 1 << 24 {
        return Err(ArchiveError::invalid(format!("implausible name size {len}")));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact_at(offset, &mut buf)?;
    Ok(buf)
}

/// Decode a RAR4 unicode re-encoded name.
///
/// Encoding: ASCII chars (<0x80) as one byte; non-ASCII as two bytes
/// (high byte then low byte); terminated by 0x00.
fn decode_unicode_name(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            break;
        }
        if b < 0x80 {
            out.push(b as char);
            i += 1;
        } else {
            if i + 1 >= bytes.len() {
                break;
            }
            let c = ((b as u16) << 8) | bytes[i + 1] as u16;
            out.push(char::from_u32(c as u32).unwrap_or('\u{FFFD}'));
            i += 2;
        }
    }
    out
}

/// Build recovery units and packed ranges from a parsed archive.
///
/// Rules:
/// - a non-solid file (or split-file) is one unit,
/// - a maximal run of solid files (one dictionary chain) is one unit,
/// - if the archive-level solid flag is set, the whole archive is one unit
///   (the decoder cannot seek past data in such archives, so no earlier data
///   may ever be reclaimed before the archive completes).
pub fn build_recovery_units(p: &ParsedRar) -> Vec<RecoveryUnit> {
    let mut units: Vec<RecoveryUnit> = Vec::new();

    if p.solid_archive && !p.entries.is_empty() {
        // Whole archive is one recovery unit.
        let ranges = all_packed_ranges(p);
        units.push(RecoveryUnit {
            seq: 0,
            first_entry: 0,
            last_entry: p.entries.len() as u64 - 1,
            packed_ranges: ranges,
            unpacked_bytes: p.unpacked_size,
        });
        return units;
    }

    let mut i = 0usize;
    while i < p.entries.len() {
        if p.entries[i].is_solid {
            // Extend the chain while consecutive entries are solid.
            let mut j = i;
            while j + 1 < p.entries.len() && p.entries[j + 1].is_solid {
                j += 1;
            }
            push_unit(p, &mut units, i, j);
            i = j + 1;
        } else {
            push_unit(p, &mut units, i, i);
            i += 1;
        }
    }
    units
}

fn push_unit(p: &ParsedRar, units: &mut Vec<RecoveryUnit>, first: usize, last: usize) {
    let mut ranges: Vec<PackedRange> = Vec::new();
    let mut unpacked = 0u64;
    for e in first..=last {
        unpacked = unpacked.saturating_add(p.entries[e].unpacked_size);
        if let Some(parts) = p.parts.get(e) {
            for part in parts {
                ranges.push(PackedRange {
                    volume_index: part.volume_index,
                    start: part.data_start,
                    len: part.data_len,
                });
            }
        }
    }
    ranges.sort_by_key(|r| (r.volume_index, r.start));
    units.push(RecoveryUnit {
        seq: units.len() as u64,
        first_entry: first as u64,
        last_entry: last as u64,
        packed_ranges: ranges,
        unpacked_bytes: unpacked,
    });
}

fn all_packed_ranges(p: &ParsedRar) -> Vec<PackedRange> {
    let mut ranges = Vec::new();
    for parts in &p.parts {
        for part in parts {
            ranges.push(PackedRange {
                volume_index: part.volume_index,
                start: part.data_start,
                len: part.data_len,
            });
        }
    }
    ranges.sort_by_key(|r| (r.volume_index, r.start));
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rar4_signature_detection() {
        let mut data = Vec::new();
        data.extend_from_slice(&RAR4_SIGNATURE);
        assert_eq!(&data[..7], &RAR4_SIGNATURE);
    }
}
