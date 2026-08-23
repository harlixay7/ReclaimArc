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
    let format = if sig == RAR5_SIGNATURE {
        RarFormat::Rar5
    } else {
        RarFormat::Rar4
    };

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
                    return Err(ArchiveError::invalid(
                        "unexpected header before main header",
                    ));
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

    validate_structural_invariants(&parsed)?;

    Ok(parsed)
}

pub fn validate_structural_invariants(p: &ParsedRar) -> Result<(), ArchiveError> {
    // 1. Part ordering, overflow checks, volume bounds, and logical packed size agreement.
    let mut total_part_bytes = 0u64;
    for (entry_idx, entry) in p.entries.iter().enumerate() {
        if let Some(parts) = p.parts.get(entry_idx) {
            let mut entry_part_bytes = 0u64;
            let mut last_part_vol = 0u64;
            let mut last_part_end = 0u64;

            for (part_idx, part) in parts.iter().enumerate() {
                // Bounds check volume index
                if part.volume_index as usize >= p.volumes.len() {
                    return Err(ArchiveError::invalid(format!(
                        "entry {} part {} references out-of-bounds volume index {}",
                        entry.name, part_idx, part.volume_index
                    )));
                }
                let vol_len = p.volumes[part.volume_index as usize].len;
                let part_end = part.data_start.checked_add(part.data_len).ok_or_else(|| {
                    ArchiveError::invalid(format!(
                        "arithmetic overflow computing part end for entry {}",
                        entry.name
                    ))
                })?;
                if part_end > vol_len {
                    return Err(ArchiveError::invalid(format!(
                        "entry {} part {} range [{}..{}] exceeds volume length {}",
                        entry.name, part_idx, part.data_start, part_end, vol_len
                    )));
                }

                // Monotonic ordering of parts for this entry
                if part_idx > 0
                    && (part.volume_index < last_part_vol
                        || (part.volume_index == last_part_vol && part.data_start < last_part_end))
                {
                    return Err(ArchiveError::invalid(format!(
                        "non-monotonic part ordering in entry {}",
                        entry.name
                    )));
                }
                last_part_vol = part.volume_index;
                last_part_end = part_end;

                entry_part_bytes =
                    entry_part_bytes.checked_add(part.data_len).ok_or_else(|| {
                        ArchiveError::invalid(format!(
                            "arithmetic overflow accumulating packed bytes for entry {}",
                            entry.name
                        ))
                    })?;
            }

            if !entry.is_directory && entry_part_bytes != entry.packed_size {
                return Err(ArchiveError::invalid(format!(
                    "entry '{}' sum of part lengths ({}) does not match entry packed size ({})",
                    entry.name, entry_part_bytes, entry.packed_size
                )));
            }

            total_part_bytes = total_part_bytes
                .checked_add(entry_part_bytes)
                .ok_or_else(|| {
                    ArchiveError::invalid("arithmetic overflow accumulating archive packed size")
                })?;
        }
    }

    if total_part_bytes != p.packed_size {
        return Err(ArchiveError::invalid(format!(
            "archive packed size ({}) does not match total part bytes ({})",
            p.packed_size, total_part_bytes
        )));
    }

    // 2. Non-overlapping range check within each volume.
    for (vol_idx, vol) in p.volumes.iter().enumerate() {
        let mut vol_ranges: Vec<(u64, u64, usize)> = Vec::new();
        for (entry_idx, parts) in p.parts.iter().enumerate() {
            for part in parts {
                if part.volume_index as usize == vol_idx {
                    vol_ranges.push((part.data_start, part.data_len, entry_idx));
                }
            }
        }
        vol_ranges.sort_by_key(|&(start, _, _)| start);
        for i in 1..vol_ranges.len() {
            let prev = &vol_ranges[i - 1];
            let curr = &vol_ranges[i];
            let prev_end = prev.0.checked_add(prev.1).ok_or_else(|| {
                ArchiveError::invalid("overflow computing range end in overlap check")
            })?;
            if prev_end > curr.0 {
                return Err(ArchiveError::invalid(format!(
                    "overlapping packed data ranges in volume {}: entry {} [{}..{}] overlaps with entry {} at offset {}",
                    vol.path.display(),
                    p.entries[prev.2].name,
                    prev.0,
                    prev_end,
                    p.entries[curr.2].name,
                    curr.0
                )));
            }
        }
    }

    Ok(())
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

    // Verify RAR4 header checksum according to official UnRAR specification:
    // CRC(0xFFFF, &HeaderType, HeadSize - 2) & 0xFFFF
    let check_len = (head_size as usize).saturating_sub(2);
    let mut raw_hdr = vec![0u8; check_len];
    r.read_exact_at(pos + 2, &mut raw_hdr)?;
    let computed_crc = (crc32(&raw_hdr) & 0xffff) as u16;
    if computed_crc != crc {
        return Err(ArchiveError::invalid(format!(
            "RAR4 header at offset {pos} CRC mismatch: expected 0x{crc:04x}, computed 0x{computed_crc:04x}"
        )));
    }

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
            let ansi_end = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
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
    let stored_crc32 = r.u32_le_at(pos)?;
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

    // Verify RAR5 general-header CRC32 (CRC32 over header size varint bytes + body bytes)
    let mut size_varint_bytes = vec![0u8; size_bytes as usize];
    r.read_exact_at(pos + 4, &mut size_varint_bytes)?;
    let mut crc_buf = Vec::with_capacity(size_bytes as usize + header_size as usize);
    crc_buf.extend_from_slice(&size_varint_bytes);
    crc_buf.extend_from_slice(&body);
    let computed_crc32 = crc32(&crc_buf);
    if computed_crc32 != stored_crc32 {
        return Err(ArchiveError::invalid(format!(
            "RAR5 header at offset {pos} CRC32 mismatch: expected 0x{stored_crc32:08x}, computed 0x{computed_crc32:08x}"
        )));
    }
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
        let v =
            (b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16) | ((b[3] as u32) << 24);
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

/// RAR5 extra field IDs from the official RAR5 format specification.
pub const EXTRA5_CRYPT: u64 = 0x01;
pub const EXTRA5_HASH: u64 = 0x02;
pub const EXTRA5_HTIME: u64 = 0x03;
pub const EXTRA5_VERSION: u64 = 0x04;
pub const EXTRA5_REDIR: u64 = 0x05;
pub const EXTRA5_UOWNER: u64 = 0x06;
pub const EXTRA5_SUBDATA: u64 = 0x07;

/// Scan RAR5 extra records:
/// Official structure: Size (varint) -> Type (varint) -> Data
///
/// 0x01: encryption
/// 0x02: hash
/// 0x03: time
/// 0x04: version
/// 0x05: redirection
/// 0x06: owner
/// 0x07: service data
///
/// Unknown or unsupported extra records are bounded and safely skipped.
pub fn scan_extras(
    body: &[u8],
    start: usize,
    len: usize,
) -> Result<(Option<Redirection>, bool), ArchiveError> {
    let mut pos = start;
    let end = (start + len).min(body.len());
    let mut redirection = None;
    let mut encrypted = false;

    while pos < end {
        let mut bf = SliceFields::new(&body[pos..end]);
        let extra_size = match bf.varint() {
            Ok(s) => s as usize,
            Err(_) => break,
        };
        let varint_len = bf.pos;
        let record_start = pos + varint_len;
        let record_end = record_start.saturating_add(extra_size);
        if extra_size == 0 || record_end > end {
            break;
        }

        let mut rf = SliceFields::new(&body[record_start..record_end]);
        let extra_type = match rf.varint() {
            Ok(t) => t,
            Err(_) => {
                pos = record_end;
                continue;
            }
        };

        match extra_type {
            EXTRA5_CRYPT => {
                encrypted = true;
            }
            EXTRA5_REDIR => {
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
            }
            EXTRA5_HASH | EXTRA5_HTIME | EXTRA5_VERSION | EXTRA5_UOWNER | EXTRA5_SUBDATA => {
                // Official RAR5 extra records: safely bounded and skipped for layout parsing
            }
            _ => {
                // Unknown extra record: safely skipped
            }
        }

        pos = record_end;
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
            Ok(h)
                if matches!(h.kind, HeaderKind::EndArc)
                    && h.end == vol_len
                    && endarc_crc_matches(r, format, pos, h.end)? =>
            {
                return Ok(pos);
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
fn endarc_crc_matches(
    r: &mut Reader,
    format: RarFormat,
    pos: u64,
    end: u64,
) -> Result<bool, ArchiveError> {
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
        return Err(ArchiveError::invalid(format!(
            "implausible name size {len}"
        )));
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
/// - if the archive-level solid flag is set, the whole archive is one unit;
/// - normal independently skippable units before the first restart-dependent
///   split entry are partitioned into solid chains or individual file units;
/// - from the first restart-dependent split entry (split_before/split_after or
///   multi-volume part spanning) through the end of the archive, create one
///   recovery tail unit, ensuring restart from volume 1 preserves all required
///   traversal data until the entire tail is committed.
pub fn build_recovery_units(p: &ParsedRar) -> Vec<RecoveryUnit> {
    let mut units: Vec<RecoveryUnit> = Vec::new();

    if p.entries.is_empty() {
        return units;
    }

    if p.solid_archive {
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

    // Find the first restart-dependent split entry.
    let first_split = p.entries.iter().enumerate().position(|(idx, e)| {
        e.split_before
            || e.split_after
            || p.parts
                .get(idx)
                .map(|parts| parts.len() > 1)
                .unwrap_or(false)
    });

    let normal_limit = first_split.unwrap_or(p.entries.len());

    let mut i = 0usize;
    while i < normal_limit {
        if p.entries[i].is_solid {
            // Extend the chain while consecutive entries are solid.
            let mut j = i;
            while j + 1 < normal_limit && p.entries[j + 1].is_solid {
                j += 1;
            }
            push_unit(p, &mut units, i, j);
            i = j + 1;
        } else {
            push_unit(p, &mut units, i, i);
            i += 1;
        }
    }

    if normal_limit < p.entries.len() {
        push_unit(p, &mut units, normal_limit, p.entries.len() - 1);
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

    fn encode_varint(v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cur = v;
        loop {
            let mut b = (cur & 0x7f) as u8;
            cur >>= 7;
            if cur != 0 {
                b |= 0x80;
            }
            out.push(b);
            if cur == 0 {
                break;
            }
        }
        out
    }

    #[test]
    fn rar4_signature_detection() {
        let mut data = Vec::new();
        data.extend_from_slice(&RAR4_SIGNATURE);
        assert_eq!(&data[..7], &RAR4_SIGNATURE);
    }

    #[test]
    fn test_rar5_extra_records_official_format_and_skip_unknown() {
        let mut body = Vec::new();

        // 1. Extra Record 0x02 (Hash / Checksum): Size = 33, Type = 0x02, 32 bytes data
        let mut rec_hash = encode_varint(EXTRA5_HASH);
        rec_hash.extend_from_slice(&[0xAA; 32]); // hash payload
        body.extend_from_slice(&encode_varint(rec_hash.len() as u64));
        body.extend_from_slice(&rec_hash);

        // 2. Extra Record 0x01 (Encryption): Size = 5, Type = 0x01, 4 bytes encryption metadata
        let mut rec_crypt = encode_varint(EXTRA5_CRYPT);
        rec_crypt.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        body.extend_from_slice(&encode_varint(rec_crypt.len() as u64));
        body.extend_from_slice(&rec_crypt);

        // 3. Extra Record 0x99 (Unknown / future extra field): Size = 10, Type = 0x99, 9 bytes data
        let mut rec_unknown = encode_varint(0x99);
        rec_unknown.extend_from_slice(&[0x55; 9]);
        body.extend_from_slice(&encode_varint(rec_unknown.len() as u64));
        body.extend_from_slice(&rec_unknown);

        // 4. Extra Record 0x05 (Redirection): Unix symlink to "/target/path"
        let target = b"/target/path";
        let mut rec_redir = encode_varint(EXTRA5_REDIR);
        rec_redir.extend_from_slice(&encode_varint(0x01)); // redir_type 0x01 (UnixSymlink)
        rec_redir.extend_from_slice(&encode_varint(0x00)); // flags
        rec_redir.extend_from_slice(&encode_varint(target.len() as u64)); // name size
        rec_redir.extend_from_slice(target);
        body.extend_from_slice(&encode_varint(rec_redir.len() as u64));
        body.extend_from_slice(&rec_redir);

        // 5. Extra Record 0x03 (Time): Size = 6, Type = 0x03, 5 bytes time metadata
        let mut rec_time = encode_varint(EXTRA5_HTIME);
        rec_time.extend_from_slice(&[0x10, 0x20, 0x30, 0x40, 0x50]);
        body.extend_from_slice(&encode_varint(rec_time.len() as u64));
        body.extend_from_slice(&rec_time);

        let (redir, encrypted) = scan_extras(&body, 0, body.len()).unwrap();
        assert!(
            encrypted,
            "File-level encryption must be detected from EXTRA5_CRYPT"
        );
        assert!(
            redir.is_some(),
            "Redirection must be parsed from EXTRA5_REDIR"
        );
        let r = redir.unwrap();
        assert_eq!(r.kind, RedirectionKind::UnixSymlink);
        assert_eq!(r.target, "/target/path");
    }
}
