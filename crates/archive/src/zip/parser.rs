//! Independent ZIP / ZIP64 structural parser, dual-parser cross-validator,
//! local header resolver, data descriptor verifier, physical envelope disjointness defense,
//! and retirement proof generator.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::ArchiveError;
use crate::model::{
    ArchiveInfo, CapabilityMatrix, DecoderRequirements, Entry, PackedRange, RecoveryUnit,
    Redirection, RedirectionKind, RetirementProof, VolumeInfo,
};

pub const ZIP_LOCAL_HEADER_SIG: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
pub const ZIP_CENTRAL_HEADER_SIG: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
pub const ZIP_DATA_DESCRIPTOR_SIG: [u8; 4] = [0x50, 0x4b, 0x07, 0x08];
pub const ZIP_ZIP64_EOCD_SIG: [u8; 4] = [0x50, 0x4b, 0x06, 0x06];
pub const ZIP_ZIP64_LOCATOR_SIG: [u8; 4] = [0x50, 0x4b, 0x06, 0x07];
pub const ZIP_EOCD_SIG: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
pub const ZIP_SPANNED_SIG: [u8; 4] = [0x50, 0x4b, 0x07, 0x08];

/// Structural analysis result from independent parsing.
#[derive(Debug, Clone)]
pub struct ZipAnalysis {
    pub info: ArchiveInfo,
    pub retirement_proofs: Vec<RetirementProof>,
}

/// Official standard IBM CP437 extended character table for byte values 0x80..=0xFF.
/// Exact mapping specified in Unicode character database and IBM CP437 standard.
pub const CP437_EXTENDED: [char; 128] = [
    // 0x80 - 0x8F
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å',
    // 0x90 - 0x9F
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ',
    // 0xA0 - 0xAF
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»',
    // 0xB0 - 0xBF
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐',
    // 0xC0 - 0xCF
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧',
    // 0xD0 - 0xDF
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀',
    // 0xE0 - 0xEF
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩',
    // 0xF0 - 0xFF:
    // 0xF4 -> U+2320 (TOP HALF INTEGRAL '⌠')
    // 0xF5 -> U+2321 (BOTTOM HALF INTEGRAL '⌡')
    // 0xFF -> U+00A0 (NO-BREAK SPACE '\u{00A0}')
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{00A0}',
];

/// Decode bytes using IBM CP437 mapping.
pub fn decode_cp437(raw_bytes: &[u8]) -> String {
    raw_bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                CP437_EXTENDED[(b - 0x80) as usize]
            }
        })
        .collect()
}

/// Parse Info-ZIP Unicode Path Extra Field `0x7075` (`up`).
/// Layout:
/// - tag: `0x7075` (2 bytes)
/// - len: `u16` (2 bytes)
/// - version: `1` (1 byte)
/// - name_crc32: `u32` (4 bytes, CRC32 of standard raw filename)
/// - unicode_path: UTF-8 bytes
pub fn parse_unicode_path_extra(extra_bytes: &[u8], raw_name: &[u8]) -> Option<String> {
    let mut cursor = 0;
    while cursor + 4 <= extra_bytes.len() {
        let tag = u16::from_le_bytes([extra_bytes[cursor], extra_bytes[cursor + 1]]);
        let len = u16::from_le_bytes([extra_bytes[cursor + 2], extra_bytes[cursor + 3]]) as usize;
        cursor += 4;
        if cursor + len > extra_bytes.len() {
            break;
        }
        let data = &extra_bytes[cursor..cursor + len];
        cursor += len;

        if tag == 0x7075 && data.len() >= 5 {
            let version = data[0];
            if version == 1 {
                let name_crc = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                let actual_crc = crc32fast::hash(raw_name);
                if name_crc == actual_crc {
                    if let Ok(s) = std::str::from_utf8(&data[5..]) {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Parse ZIP64 Extended Information Extra Field `0x0001`.
/// APPNOTE 6.3.10 format:
/// Central Directory fields appear if corresponding 32-bit field is sentinel:
/// - uncomp_size (u64, if central uncomp == 0xFFFFFFFF)
/// - comp_size (u64, if central comp == 0xFFFFFFFF)
/// - local_header_offset (u64, if central offset == 0xFFFFFFFF)
/// - disk_start_number (u32, if central disk == 0xFFFF)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Zip64ExtraField {
    pub uncompressed_size: Option<u64>,
    pub compressed_size: Option<u64>,
    pub local_header_offset: Option<u64>,
    pub disk_start_number: Option<u32>,
}

pub fn parse_zip64_extra(
    extra_bytes: &[u8],
    needs_uncomp: bool,
    needs_comp: bool,
    needs_offset: bool,
    needs_disk: bool,
) -> Result<Option<Zip64ExtraField>, ArchiveError> {
    let mut cursor = 0;
    while cursor + 4 <= extra_bytes.len() {
        let tag = u16::from_le_bytes([extra_bytes[cursor], extra_bytes[cursor + 1]]);
        let len = u16::from_le_bytes([extra_bytes[cursor + 2], extra_bytes[cursor + 3]]) as usize;
        cursor += 4;
        if cursor + len > extra_bytes.len() {
            return Err(ArchiveError::invalid(
                "truncated ZIP extra field header in archive",
            ));
        }
        let data = &extra_bytes[cursor..cursor + len];
        cursor += len;

        if tag == 0x0001 {
            let mut field_cursor = 0;
            let mut result = Zip64ExtraField::default();

            if needs_uncomp {
                if field_cursor + 8 > data.len() {
                    return Err(ArchiveError::invalid(
                        "ZIP64 extra field missing expected 8-byte uncompressed size",
                    ));
                }
                result.uncompressed_size = Some(u64::from_le_bytes(
                    data[field_cursor..field_cursor + 8].try_into().unwrap(),
                ));
                field_cursor += 8;
            }

            if needs_comp {
                if field_cursor + 8 > data.len() {
                    return Err(ArchiveError::invalid(
                        "ZIP64 extra field missing expected 8-byte compressed size",
                    ));
                }
                result.compressed_size = Some(u64::from_le_bytes(
                    data[field_cursor..field_cursor + 8].try_into().unwrap(),
                ));
                field_cursor += 8;
            }

            if needs_offset {
                if field_cursor + 8 > data.len() {
                    return Err(ArchiveError::invalid(
                        "ZIP64 extra field missing expected 8-byte local header offset",
                    ));
                }
                result.local_header_offset = Some(u64::from_le_bytes(
                    data[field_cursor..field_cursor + 8].try_into().unwrap(),
                ));
                field_cursor += 8;
            }

            if needs_disk {
                if field_cursor + 4 > data.len() {
                    return Err(ArchiveError::invalid(
                        "ZIP64 extra field missing expected 4-byte disk start number",
                    ));
                }
                result.disk_start_number = Some(u32::from_le_bytes(
                    data[field_cursor..field_cursor + 4].try_into().unwrap(),
                ));
            }

            return Ok(Some(result));
        }
    }
    Ok(None)
}

/// Centralized checked range addition helper to eliminate unchecked integer math.
#[inline]
pub fn checked_range(start: u64, len: u64, limit: u64) -> Result<(u64, u64), ArchiveError> {
    let end = start.checked_add(len).ok_or_else(|| {
        ArchiveError::invalid("arithmetic overflow in archive offset calculation")
    })?;
    if end > limit {
        return Err(ArchiveError::invalid(format!(
            "offset range [{start}, {end}) exceeds archive limit ({limit})"
        )));
    }
    Ok((start, end))
}

/// Decode raw filename bytes with spec-exact rules:
/// - If Bit 11 is set: require valid UTF-8, fail closed if invalid.
/// - If Bit 11 is unset: check for valid Info-ZIP `0x7075` extra field; otherwise decode via IBM CP437.
pub fn decode_filename_spec(
    raw_bytes: &[u8],
    is_utf8_flag_set: bool,
    unicode_path_extra: Option<&str>,
) -> Result<String, ArchiveError> {
    if is_utf8_flag_set {
        std::str::from_utf8(raw_bytes)
            .map(|s| s.to_string())
            .map_err(|_| {
                ArchiveError::invalid("entry has UTF-8 flag set but contains invalid UTF-8 bytes")
            })
    } else if let Some(up) = unicode_path_extra {
        Ok(up.to_string())
    } else {
        Ok(decode_cp437(raw_bytes))
    }
}

/// A physical entry envelope representing the entire byte span occupied by an entry on disk.
#[derive(Debug, Clone)]
pub struct PhysicalEnvelope {
    pub index: usize,
    pub name: String,
    pub envelope_start: u64,
    pub envelope_end: u64,
    pub payload_start: u64,
    pub payload_len: u64,
}

/// Parse and cross-validate a ZIP/ZIP64 archive using independent implementations
/// (rawzip + local header direct validation + zip-rs) and prove safety invariants
/// before progressive reclamation.
pub fn parse_and_validate(
    path: &Path,
    password: Option<&str>,
) -> Result<ZipAnalysis, ArchiveError> {
    let mut file = File::open(path)
        .map_err(|e| ArchiveError::open(format!("cannot open '{}': {e}", path.display())))?;

    let file_len = file
        .metadata()
        .map_err(|e| {
            ArchiveError::open(format!(
                "cannot query metadata for '{}': {e}",
                path.display()
            ))
        })?
        .len();

    if file_len < 22 {
        return Err(ArchiveError::invalid(
            "file is too small to be a valid ZIP archive",
        ));
    }

    // 1. Signature check at file start
    let mut header_sig = [0u8; 4];
    file.read_exact(&mut header_sig)
        .map_err(|e| ArchiveError::invalid(format!("cannot read ZIP signature: {e}")))?;

    let is_empty_zip = header_sig == ZIP_EOCD_SIG;
    let is_standard_zip = header_sig == ZIP_LOCAL_HEADER_SIG;

    if !is_empty_zip && !is_standard_zip {
        return Err(ArchiveError::unsupported(format!(
            "'{}' does not start with a recognized standard ZIP signature",
            path.display()
        )));
    }

    // 2. Parse Central Directory independently with rawzip
    let mut raw_scratch = vec![0u8; 65536];
    let raw_archive = rawzip::ZipArchive::from_seekable(&mut file, &mut raw_scratch)
        .map_err(|e| ArchiveError::invalid(format!("rawzip structural parse failed: {e:?}")))?;

    let cd_start = raw_archive.directory_offset();
    let cd_end = raw_archive.end_offset();

    if cd_start > file_len || cd_end > file_len || cd_start > cd_end {
        return Err(ArchiveError::invalid(format!(
            "invalid central directory boundaries: [{cd_start}, {cd_end}) (file size: {file_len})"
        )));
    }

    let mut raw_entry_scratch = vec![0u8; 65536];
    let mut raw_entries_iter = raw_archive.entries(&mut raw_entry_scratch);

    struct RawEntryData {
        path_bytes: Vec<u8>,
        unicode_path_extra: Option<String>,
        compressed_size_hint: u64,
        uncompressed_size_hint: u64,
        crc32: u32,
        is_dir: bool,
        local_header_offset_hint: u64,
        flags: u16,
        compression_method: u16,
        external_attributes: u32,
    }

    let mut raw_entries = Vec::new();
    let mut detected_zip64_structure = false;

    while let Some(re) = raw_entries_iter
        .next_entry()
        .map_err(|e| ArchiveError::invalid(format!("rawzip entry parse failed: {e:?}")))?
    {
        let mut unicode_path_extra = None;
        let mut has_zip64 = false;

        for (id, field_bytes) in re.extra_fields() {
            if id.as_u16() == 1 {
                has_zip64 = true;
            }
            if id.as_u16() == 0x7075 && field_bytes.len() >= 5 && field_bytes[0] == 1 {
                let name_crc = u32::from_le_bytes([
                    field_bytes[1],
                    field_bytes[2],
                    field_bytes[3],
                    field_bytes[4],
                ]);
                if name_crc == crc32fast::hash(re.file_path().as_bytes()) {
                    if let Ok(s) = std::str::from_utf8(&field_bytes[5..]) {
                        unicode_path_extra = Some(s.to_string());
                    }
                }
            }
        }

        if has_zip64 {
            detected_zip64_structure = true;
        }

        raw_entries.push(RawEntryData {
            path_bytes: re.file_path().as_bytes().to_vec(),
            unicode_path_extra,
            compressed_size_hint: re.compressed_size_hint(),
            uncompressed_size_hint: re.uncompressed_size_hint(),
            crc32: re.crc32(),
            is_dir: re.is_dir(),
            local_header_offset_hint: re.local_header_offset(),
            flags: re.flags().bits(),
            compression_method: re.compression_method().as_u16(),
            external_attributes: re.external_attributes(),
        });
    }

    // 3. Parse with zip-rs
    let zip_file = File::open(path)
        .map_err(|e| ArchiveError::open(format!("cannot open '{}': {e}", path.display())))?;
    let mut zip_archive = zip::ZipArchive::new(zip_file)
        .map_err(|e| ArchiveError::invalid(format!("zip-rs parse failed: {e}")))?;

    let has_overlapping = zip_archive.has_overlapping_files().unwrap_or(true);

    // 4. Cross-check entry counts
    if raw_entries.len() != zip_archive.len() {
        return Err(ArchiveError::invalid(format!(
            "structural parser mismatch: rawzip found {} entries, zip-rs found {}",
            raw_entries.len(),
            zip_archive.len()
        )));
    }

    let mut entries = Vec::with_capacity(zip_archive.len());
    let mut recovery_units = Vec::with_capacity(zip_archive.len());
    let mut retirement_proofs = Vec::with_capacity(zip_archive.len());
    let mut envelopes = Vec::with_capacity(zip_archive.len());
    let mut total_packed: u64 = 0;
    let mut total_unpacked: u64 = 0;

    let mut notes = Vec::new();
    let mut progressive_reclaim = !has_overlapping;
    let mut supports_encryption = false;

    if has_overlapping {
        notes.push(
            "Archive contains overlapping compressed ranges; progressive reclamation disabled."
                .into(),
        );
        progressive_reclaim = false;
    }

    for (i, raw_entry) in raw_entries.iter().enumerate().take(zip_archive.len()) {
        let zip_entry = if let Some(pw) = password {
            zip_archive
                .by_index_decrypt(i, pw.as_bytes())
                .map_err(|e| ArchiveError::invalid(format!("cannot decrypt entry {i}: {e}")))?
        } else {
            zip_archive
                .by_index(i)
                .map_err(|e| ArchiveError::invalid(format!("cannot inspect entry {i}: {e}")))?
        };

        // Raw Structural Name Identity: raw filename from rawzip central directory
        let raw_central_name = raw_entry.path_bytes.as_slice();

        let is_utf8_flag = (raw_entry.flags & 0x0800) != 0;
        let name = decode_filename_spec(
            raw_central_name,
            is_utf8_flag,
            raw_entry.unicode_path_extra.as_deref(),
        )?;

        let is_dir = zip_entry.is_dir();
        if is_dir != raw_entry.is_dir {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') directory flag mismatch between parsers"
            )));
        }

        let unpacked_size = zip_entry.size();
        let packed_size = zip_entry.compressed_size();
        let crc32 = zip_entry.crc32();
        let header_start = zip_entry.header_start();
        let encrypted = zip_entry.encrypted();

        if encrypted || (raw_entry.flags & 0x0001) != 0 {
            supports_encryption = false;
            progressive_reclaim = false;
            notes.push(format!(
                "Entry '{name}' is encrypted; Low-Space extraction disabled."
            ));
        }

        if (raw_entry.flags & 0x2000) != 0 {
            return Err(ArchiveError::unsupported(format!(
                "entry {i} ('{name}') uses central directory encryption/masking (bit 13)"
            )));
        }

        if raw_entry.uncompressed_size_hint != unpacked_size
            && raw_entry.uncompressed_size_hint != 0xFFFF_FFFF
        {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') uncompressed size mismatch: rawzip={}, zip-rs={unpacked_size}",
                raw_entry.uncompressed_size_hint
            )));
        }

        if raw_entry.compressed_size_hint != packed_size
            && raw_entry.compressed_size_hint != 0xFFFF_FFFF
        {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') compressed size mismatch: rawzip={}, zip-rs={packed_size}",
                raw_entry.compressed_size_hint
            )));
        }

        if crc32 != raw_entry.crc32 && (raw_entry.flags & 0x0008) == 0 {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') CRC32 mismatch: rawzip=0x{:08x}, zip-rs=0x{:08x}",
                raw_entry.crc32, crc32
            )));
        }

        if header_start != raw_entry.local_header_offset_hint
            && raw_entry.local_header_offset_hint != 0xFFFF_FFFF
        {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') local header offset mismatch: rawzip={}, zip-rs={}",
                raw_entry.local_header_offset_hint, header_start
            )));
        }

        // Truthful compression preflight: Method check for EVERY non-directory entry
        let compression_method = zip_entry.compression();
        let is_supported_method = matches!(
            compression_method,
            zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
        );

        if !is_supported_method && !is_dir {
            return Err(ArchiveError::unsupported(format!(
                "entry {i} ('{name}') uses unsupported compression method {:?}",
                compression_method
            )));
        }

        // ---------------------------------------------------------------------
        // INDEPENDENT LOCAL HEADER & DATA START VERIFICATION
        // ---------------------------------------------------------------------
        let (_hdr_s, _hdr_e) = checked_range(header_start, 30, cd_start)?;

        file.seek(SeekFrom::Start(header_start)).map_err(|e| {
            ArchiveError::open(format!("cannot seek to local header {header_start}: {e}"))
        })?;

        let mut local_header_buf = [0u8; 30];
        file.read_exact(&mut local_header_buf).map_err(|e| {
            ArchiveError::invalid(format!("cannot read local header at {header_start}: {e}"))
        })?;

        if local_header_buf[..4] != ZIP_LOCAL_HEADER_SIG {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') local header signature mismatch at offset {header_start}"
            )));
        }

        let local_flags = u16::from_le_bytes([local_header_buf[6], local_header_buf[7]]);
        let local_method = u16::from_le_bytes([local_header_buf[8], local_header_buf[9]]);
        let local_crc = u32::from_le_bytes([
            local_header_buf[14],
            local_header_buf[15],
            local_header_buf[16],
            local_header_buf[17],
        ]);
        let local_comp_size = u32::from_le_bytes([
            local_header_buf[18],
            local_header_buf[19],
            local_header_buf[20],
            local_header_buf[21],
        ]);
        let local_uncomp_size = u32::from_le_bytes([
            local_header_buf[22],
            local_header_buf[23],
            local_header_buf[24],
            local_header_buf[25],
        ]);
        let local_name_len =
            u16::from_le_bytes([local_header_buf[26], local_header_buf[27]]) as usize;
        let local_extra_len =
            u16::from_le_bytes([local_header_buf[28], local_header_buf[29]]) as usize;

        // Bit 0 (encryption flag) agreement
        let local_bit0 = (local_flags & 0x0001) != 0;
        let central_bit0 = (raw_entry.flags & 0x0001) != 0;
        if local_bit0 != central_bit0 {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') encryption flag mismatch: local={local_bit0}, central={central_bit0}"
            )));
        }

        // Bit 13 (central masking) must fail closed
        let local_bit13 = (local_flags & 0x2000) != 0;
        let central_bit13 = (raw_entry.flags & 0x2000) != 0;
        if local_bit13 || central_bit13 {
            return Err(ArchiveError::unsupported(format!(
                "entry {i} ('{name}') uses unsupported central directory encryption/masking (bit 13)"
            )));
        }

        // Strict Flag Reconciliation: Bit 3 and Bit 11 must agree between local and central
        let local_bit3 = (local_flags & 0x0008) != 0;
        let central_bit3 = (raw_entry.flags & 0x0008) != 0;
        if local_bit3 != central_bit3 {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') data descriptor flag mismatch: local={local_bit3}, central={central_bit3}"
            )));
        }

        let local_bit11 = (local_flags & 0x0800) != 0;
        let central_bit11 = (raw_entry.flags & 0x0800) != 0;
        if local_bit11 != central_bit11 {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') UTF-8 flag mismatch: local={local_bit11}, central={central_bit11}"
            )));
        }

        if !local_bit3 {
            // When bit 3 is unset, local CRC must match central CRC
            if local_crc != crc32 {
                return Err(ArchiveError::invalid(format!(
                    "entry {i} ('{name}') CRC32 mismatch: local=0x{:08x}, central=0x{:08x}",
                    local_crc, crc32
                )));
            }
            if local_comp_size != 0xFFFF_FFFF && local_comp_size as u64 != packed_size {
                return Err(ArchiveError::invalid(format!(
                    "entry {i} ('{name}') compressed size mismatch: local={local_comp_size}, central={packed_size}"
                )));
            }
            if local_uncomp_size != 0xFFFF_FFFF && local_uncomp_size as u64 != unpacked_size {
                return Err(ArchiveError::invalid(format!(
                    "entry {i} ('{name}') uncompressed size mismatch: local={local_uncomp_size}, central={unpacked_size}"
                )));
            }
        }

        let local_header_total_len = 30u64
            .checked_add(local_name_len as u64)
            .and_then(|h| h.checked_add(local_extra_len as u64))
            .ok_or_else(|| ArchiveError::invalid("overflow in local header total length"))?;

        let (_l_start, local_data_start) =
            checked_range(header_start, local_header_total_len, cd_start)?;

        let mut local_name_buf = vec![0u8; local_name_len];
        file.read_exact(&mut local_name_buf).map_err(|e| {
            ArchiveError::invalid(format!("cannot read local filename at {header_start}: {e}"))
        })?;

        if local_name_buf != raw_central_name {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') filename mismatch between local header and central directory"
            )));
        }

        let mut local_extra_buf = vec![0u8; local_extra_len];
        file.read_exact(&mut local_extra_buf).map_err(|e| {
            ArchiveError::invalid(format!("cannot read local extra at {header_start}: {e}"))
        })?;

        // If local header uses 0xFFFFFFFF sentinels, parse 0x0001 extra field
        if local_comp_size == 0xFFFF_FFFF || local_uncomp_size == 0xFFFF_FFFF {
            let mut found_zip64 = false;
            let mut offset = 0;
            while offset + 4 <= local_extra_buf.len() {
                let tag =
                    u16::from_le_bytes([local_extra_buf[offset], local_extra_buf[offset + 1]]);
                let len =
                    u16::from_le_bytes([local_extra_buf[offset + 2], local_extra_buf[offset + 3]])
                        as usize;
                offset += 4;
                if offset + len > local_extra_buf.len() {
                    break;
                }
                if tag == 0x0001 {
                    found_zip64 = true;
                    let mut field_pos = offset;
                    if local_uncomp_size == 0xFFFF_FFFF {
                        if field_pos + 8 > offset + len {
                            return Err(ArchiveError::invalid(format!(
                                "entry {i} ('{name}') truncated ZIP64 extra field for uncompressed size"
                            )));
                        }
                        let uncomp64 = u64::from_le_bytes(
                            local_extra_buf[field_pos..field_pos + 8]
                                .try_into()
                                .unwrap(),
                        );
                        field_pos += 8;
                        if uncomp64 != unpacked_size {
                            return Err(ArchiveError::invalid(format!(
                                "entry {i} ('{name}') ZIP64 uncompressed size mismatch: local={uncomp64}, central={unpacked_size}"
                            )));
                        }
                    }
                    if local_comp_size == 0xFFFF_FFFF {
                        if field_pos + 8 > offset + len {
                            return Err(ArchiveError::invalid(format!(
                                "entry {i} ('{name}') truncated ZIP64 extra field for compressed size"
                            )));
                        }
                        let comp64 = u64::from_le_bytes(
                            local_extra_buf[field_pos..field_pos + 8]
                                .try_into()
                                .unwrap(),
                        );
                        if comp64 != packed_size {
                            return Err(ArchiveError::invalid(format!(
                                "entry {i} ('{name}') ZIP64 compressed size mismatch: local={comp64}, central={packed_size}"
                            )));
                        }
                    }
                }
                offset += len;
            }
            if !found_zip64 {
                return Err(ArchiveError::invalid(format!(
                    "entry {i} ('{name}') has ZIP64 sentinels but lacks 0x0001 extra field in local header"
                )));
            }
        }

        if local_method != raw_entry.compression_method {
            return Err(ArchiveError::invalid(format!(
                "entry {i} ('{name}') compression method mismatch: local={local_method}, central={}",
                raw_entry.compression_method
            )));
        }

        // Cross-check with zip-rs data_start()
        let proven_data_start = match zip_entry.data_start() {
            Some(ds) => {
                if ds != local_data_start {
                    return Err(ArchiveError::invalid(format!(
                        "entry {i} ('{name}') data start mismatch: local={local_data_start}, zip-rs={ds}"
                    )));
                }
                ds
            }
            None => {
                if !is_dir && packed_size > 0 {
                    // Cannot prove exact data start independently -> reject Low-Space
                    progressive_reclaim = false;
                    notes.push(format!(
                        "Entry '{name}' data start could not be proven; Low-Space extraction disabled."
                    ));
                }
                local_data_start
            }
        };

        // ---------------------------------------------------------------------
        // DATA DESCRIPTOR (BIT 3) VERIFICATION
        // ---------------------------------------------------------------------
        let has_data_descriptor = local_bit3;
        let mut descriptor_len: u64 = 0;

        if has_data_descriptor && !is_dir && packed_size > 0 {
            let desc_offset = proven_data_start
                .checked_add(packed_size)
                .ok_or_else(|| ArchiveError::invalid("overflow computing descriptor offset"))?;

            let (_d_start, _d_end) = checked_range(desc_offset, 12, cd_start)?;

            file.seek(SeekFrom::Start(desc_offset)).map_err(|e| {
                ArchiveError::open(format!("cannot seek to descriptor at {desc_offset}: {e}"))
            })?;

            let mut desc_peek = [0u8; 32];
            let bytes_read = file.read(&mut desc_peek).map_err(|e| {
                ArchiveError::invalid(format!("failed to read data descriptor for entry {i}: {e}"))
            })?;

            if bytes_read < 12 {
                return Err(ArchiveError::invalid(format!(
                    "entry {i} ('{name}') truncated data descriptor"
                )));
            }

            // Identify whether the entry is structured as ZIP64
            let entry_is_zip64 = local_comp_size == 0xFFFF_FFFF
                || local_uncomp_size == 0xFFFF_FFFF
                || packed_size > 0xFFFF_FFFF
                || unpacked_size > 0xFFFF_FFFF;

            // Evaluate all 4 candidate formats without presupposing signature precedence
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            struct DescriptorCandidate {
                len: u64,
                is_zip64: bool,
                has_sig: bool,
            }

            let mut candidates = Vec::new();

            // Candidate 1: 32-bit with signature (16 bytes: 0x08074b50 + CRC(4) + comp(4) + uncomp(4))
            if bytes_read >= 16
                && desc_peek[..4] == ZIP_DATA_DESCRIPTOR_SIG
                && u32::from_le_bytes([desc_peek[4], desc_peek[5], desc_peek[6], desc_peek[7]])
                    == crc32
                && (u32::from_le_bytes([desc_peek[8], desc_peek[9], desc_peek[10], desc_peek[11]])
                    as u64)
                    == packed_size
                && (u32::from_le_bytes([desc_peek[12], desc_peek[13], desc_peek[14], desc_peek[15]])
                    as u64)
                    == unpacked_size
            {
                candidates.push(DescriptorCandidate {
                    len: 16,
                    is_zip64: false,
                    has_sig: true,
                });
            }

            // Candidate 2: 32-bit without signature (12 bytes: CRC(4) + comp(4) + uncomp(4))
            if bytes_read >= 12
                && u32::from_le_bytes([desc_peek[0], desc_peek[1], desc_peek[2], desc_peek[3]])
                    == crc32
                && (u32::from_le_bytes([desc_peek[4], desc_peek[5], desc_peek[6], desc_peek[7]])
                    as u64)
                    == packed_size
                && (u32::from_le_bytes([desc_peek[8], desc_peek[9], desc_peek[10], desc_peek[11]])
                    as u64)
                    == unpacked_size
            {
                candidates.push(DescriptorCandidate {
                    len: 12,
                    is_zip64: false,
                    has_sig: false,
                });
            }

            // Candidate 3: ZIP64 with signature (24 bytes: 0x08074b50 + CRC(4) + comp(8) + uncomp(8))
            if bytes_read >= 24
                && desc_peek[..4] == ZIP_DATA_DESCRIPTOR_SIG
                && u32::from_le_bytes([desc_peek[4], desc_peek[5], desc_peek[6], desc_peek[7]])
                    == crc32
                && u64::from_le_bytes([
                    desc_peek[8],
                    desc_peek[9],
                    desc_peek[10],
                    desc_peek[11],
                    desc_peek[12],
                    desc_peek[13],
                    desc_peek[14],
                    desc_peek[15],
                ]) == packed_size
                && u64::from_le_bytes([
                    desc_peek[16],
                    desc_peek[17],
                    desc_peek[18],
                    desc_peek[19],
                    desc_peek[20],
                    desc_peek[21],
                    desc_peek[22],
                    desc_peek[23],
                ]) == unpacked_size
            {
                candidates.push(DescriptorCandidate {
                    len: 24,
                    is_zip64: true,
                    has_sig: true,
                });
            }

            // Candidate 4: ZIP64 without signature (20 bytes: CRC(4) + comp(8) + uncomp(8))
            if bytes_read >= 20
                && u32::from_le_bytes([desc_peek[0], desc_peek[1], desc_peek[2], desc_peek[3]])
                    == crc32
                && u64::from_le_bytes([
                    desc_peek[4],
                    desc_peek[5],
                    desc_peek[6],
                    desc_peek[7],
                    desc_peek[8],
                    desc_peek[9],
                    desc_peek[10],
                    desc_peek[11],
                ]) == packed_size
                && u64::from_le_bytes([
                    desc_peek[12],
                    desc_peek[13],
                    desc_peek[14],
                    desc_peek[15],
                    desc_peek[16],
                    desc_peek[17],
                    desc_peek[18],
                    desc_peek[19],
                ]) == unpacked_size
            {
                candidates.push(DescriptorCandidate {
                    len: 20,
                    is_zip64: true,
                    has_sig: false,
                });
            }

            // If entry is structured as ZIP64, require ZIP64 descriptor candidate
            if entry_is_zip64 {
                candidates.retain(|c| c.is_zip64);
            }

            // If still multiple candidates (e.g. CRC == 0x08074B50 edge case),
            // validate candidate end offset against the next structure boundary.
            let chosen = if candidates.len() == 1 {
                Some(candidates[0])
            } else if candidates.len() > 1 {
                let mut valid_candidates = Vec::new();
                for c in &candidates {
                    let end_pos = desc_offset + c.len;
                    if end_pos > cd_start {
                        continue;
                    }
                    if end_pos == cd_start {
                        valid_candidates.push(*c);
                        continue;
                    }
                    if file.seek(SeekFrom::Start(end_pos)).is_ok() {
                        let mut next_sig = [0u8; 4];
                        if file.read_exact(&mut next_sig).is_ok()
                            && (next_sig == ZIP_LOCAL_HEADER_SIG
                                || next_sig == ZIP_CENTRAL_HEADER_SIG)
                        {
                            valid_candidates.push(*c);
                        }
                    }
                }
                if valid_candidates.len() == 1 {
                    Some(valid_candidates[0])
                } else if valid_candidates.is_empty() {
                    candidates.into_iter().find(|c| c.has_sig)
                } else {
                    valid_candidates.into_iter().find(|c| c.has_sig)
                }
            } else {
                None
            };

            if let Some(c) = chosen {
                descriptor_len = c.len;
            } else {
                progressive_reclaim = false;
                notes.push(format!(
                    "Entry '{name}' data descriptor failed verification; Low-Space extraction disabled."
                ));
            }
        } else if !has_data_descriptor && !is_dir && packed_size > 0 {
            // Local header CRC and sizes must match central directory when bit 3 is NOT set
            if local_crc != 0 && local_crc != crc32 {
                return Err(ArchiveError::invalid(format!(
                    "entry {i} ('{name}') CRC mismatch between local header (0x{local_crc:08x}) and central directory (0x{crc32:08x})"
                )));
            }

            // Resolve ZIP64 extra field in local header if sentinel present
            let local_zip64 = if local_comp_size == 0xFFFF_FFFF || local_uncomp_size == 0xFFFF_FFFF
            {
                parse_zip64_extra(
                    &local_extra_buf,
                    local_uncomp_size == 0xFFFF_FFFF,
                    local_comp_size == 0xFFFF_FFFF,
                    false,
                    false,
                )?
            } else {
                None
            };

            let resolved_local_comp = if local_comp_size == 0xFFFF_FFFF {
                local_zip64.and_then(|z| z.compressed_size).unwrap_or(0)
            } else {
                local_comp_size as u64
            };

            let resolved_local_uncomp = if local_uncomp_size == 0xFFFF_FFFF {
                local_zip64.and_then(|z| z.uncompressed_size).unwrap_or(0)
            } else {
                local_uncomp_size as u64
            };

            if resolved_local_comp != packed_size {
                return Err(ArchiveError::invalid(format!(
                    "entry {i} ('{name}') compressed size mismatch between local header ({resolved_local_comp}) and central directory ({packed_size})"
                )));
            }
            if resolved_local_uncomp != unpacked_size {
                return Err(ArchiveError::invalid(format!(
                    "entry {i} ('{name}') uncompressed size mismatch between local header ({resolved_local_uncomp}) and central directory ({unpacked_size})"
                )));
            }
        }

        // Validate data end against Central Directory start and file bounds
        let (_d_start, _data_end) = checked_range(proven_data_start, packed_size, cd_start)?;

        // Compute physical envelope end (including descriptor if present)
        let (_env_s, envelope_end) = checked_range(
            proven_data_start,
            packed_size.saturating_add(descriptor_len),
            cd_start,
        )?;

        envelopes.push(PhysicalEnvelope {
            index: i,
            name: name.clone(),
            envelope_start: header_start,
            envelope_end,
            payload_start: proven_data_start,
            payload_len: packed_size,
        });

        // Check symlink / redirection via Unix external attributes
        let unix_mode = raw_entry.external_attributes >> 16;
        let is_unix_symlink = (unix_mode & 0o170000) == 0o120000;
        let is_symlink = is_unix_symlink || zip_entry.is_symlink();

        let redirection = if is_symlink {
            Some(Redirection {
                kind: RedirectionKind::UnixSymlink,
                target: String::new(), // Evaluated safely by engine policy
            })
        } else {
            None
        };

        total_packed = total_packed.saturating_add(packed_size);
        total_unpacked = total_unpacked.saturating_add(unpacked_size);

        let entry_idx = i as u64;
        let mut entry_packed_ranges = Vec::new();

        if !is_dir && packed_size > 0 && !is_symlink {
            let pr = PackedRange {
                volume_index: 0,
                start: proven_data_start,
                len: packed_size,
            };
            entry_packed_ranges.push(pr);

            retirement_proofs.push(RetirementProof {
                volume_index: 0,
                start: proven_data_start,
                len: packed_size,
                unit_seq: entry_idx,
                reason: format!("ZIP entry '{name}' decompressed and durably committed"),
            });
        }

        entries.push(Entry {
            index: entry_idx,
            name: name.clone(),
            packed_size,
            unpacked_size,
            crc32: Some(crc32),
            is_directory: is_dir,
            is_solid: false,
            split_before: false,
            split_after: false,
            encrypted,
            redirection,
        });

        recovery_units.push(RecoveryUnit {
            seq: entry_idx,
            first_entry: entry_idx,
            last_entry: entry_idx,
            packed_ranges: entry_packed_ranges,
            unpacked_bytes: unpacked_size,
        });
    }

    // -------------------------------------------------------------------------
    // 5. PHYSICAL ENTRY ENVELOPE & DISJOINTNESS PROOF
    // -------------------------------------------------------------------------
    envelopes.sort_by_key(|e| e.envelope_start);

    // Rule A: Verify pairwise disjointness of consecutive physical envelopes
    for window in envelopes.windows(2) {
        if window[0].envelope_end > window[1].envelope_start {
            progressive_reclaim = false;
            notes.push(format!(
                "Entries '{}' (end {}) and '{}' (start {}) have overlapping envelopes; progressive reclamation disabled.",
                window[0].name, window[0].envelope_end, window[1].name, window[1].envelope_start
            ));
            break;
        }
    }

    // Rule B: Verify no reclaimable payload intersects ANY metadata of ANY other entry
    if progressive_reclaim {
        for (idx_a, env_a) in envelopes.iter().enumerate() {
            if env_a.payload_len == 0 {
                continue;
            }
            let payload_a_start = env_a.payload_start;
            let payload_a_end = env_a.payload_start + env_a.payload_len;

            for (idx_b, env_b) in envelopes.iter().enumerate() {
                if idx_a == idx_b {
                    continue;
                }
                // Check intersection between payload A and complete envelope B
                let max_start = payload_a_start.max(env_b.envelope_start);
                let min_end = payload_a_end.min(env_b.envelope_end);
                if max_start < min_end {
                    progressive_reclaim = false;
                    notes.push(format!(
                        "Entry '{}' payload [{payload_a_start}, {payload_a_end}) intersects entry '{}' envelope [{env_b_start}, {env_b_end}); progressive reclamation disabled.",
                        env_a.name, env_b.name, env_b_start = env_b.envelope_start, env_b_end = env_b.envelope_end
                    ));
                    break;
                }
            }
            if !progressive_reclaim {
                break;
            }
        }
    }

    // 6. Verify non-zero SFX / prepended offset
    if let Some(first_env) = envelopes.first() {
        if first_env.envelope_start > 0 && !is_empty_zip {
            return Err(ArchiveError::unsupported(
                "Prepended data / SFX ZIP archives are not supported.",
            ));
        }
    }

    // 7. Defense in depth: If progressive reclaim is false, expose zero retirement proofs
    if !progressive_reclaim {
        retirement_proofs.clear();
    }

    let volume_info = vec![VolumeInfo {
        index: 0,
        path: path.to_path_buf(),
        logical_size: file_len,
    }];

    let format_name = if detected_zip64_structure {
        "zip64".to_string()
    } else {
        "zip".to_string()
    };

    let capability = CapabilityMatrix {
        format: format_name.clone(),
        supports_test_integrity: true,
        restartable_units: true,
        progressive_reclaim,
        supports_encryption,
        supports_multipart: false, // Single-volume ZIP in this pass
        notes,
    };

    let info = ArchiveInfo {
        format: format_name,
        packed_size: total_packed,
        unpacked_size: total_unpacked,
        solid_archive: false,
        encrypted_headers: false,
        volumes: volume_info,
        entries,
        recovery_units,
        capability,
        decoder_requirements: DecoderRequirements {
            scratch_bytes: 0,
            redecodes_prefix: false,
        },
    };

    Ok(ZipAnalysis {
        info,
        retirement_proofs,
    })
}
