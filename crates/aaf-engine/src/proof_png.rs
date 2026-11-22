//! Pure-Rust strict PNG structural, CRC-32, zlib header, and entropy validation.

use super::ProofError;

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const MIN_IDAT_DISTINCT_BYTES: usize = 16;
const MAX_PNG_CHUNK_LEN: u32 = 0x7FFF_FFFF;

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            if (c & 1) != 0 {
                c = 0xEDB8_8320 ^ (c >> 1);
            } else {
                c >>= 1;
            }
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// IEEE 802.3 CRC-32 over PNG chunk type followed by chunk data.
pub(crate) fn crc32(kind: &[u8], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in kind.iter().chain(data.iter()) {
        let idx = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = CRC32_TABLE[idx] ^ (crc >> 8);
    }
    !crc
}

fn valid_color_and_depth(color_type: u8, bit_depth: u8) -> bool {
    match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdatState {
    Before,
    Inside,
    After,
}

fn parse_ihdr(data: &[u8]) -> Result<(u32, u32, u8), ProofError> {
    if data.len() != 13 {
        return Err(ProofError::Invalid(
            "PNG IHDR chunk must be 13 bytes".to_string(),
        ));
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if width == 0 || height == 0 {
        return Err(ProofError::Invalid(
            "PNG dimensions must be non-zero".to_string(),
        ));
    }
    let bit_depth = data[8];
    let color_type = data[9];
    let compression = data[10];
    let filter = data[11];
    let interlace = data[12];
    if !valid_color_and_depth(color_type, bit_depth) {
        return Err(ProofError::Invalid(format!(
            "invalid PNG color_type={color_type} bit_depth={bit_depth}"
        )));
    }
    if compression != 0 || filter != 0 || interlace > 1 {
        return Err(ProofError::Invalid(
            "invalid PNG IHDR compression, filter, or interlace method".to_string(),
        ));
    }
    Ok((width, height, color_type))
}

/// Validates PNG signature, every chunk CRC-32, IHDR fields, contiguous zlib IDAT
/// without preset dictionary, PLTE for indexed color, minimum IDAT payload size and
/// byte diversity, empty trailing IEND, and minimum dimensions.
pub(crate) fn validate_png(
    bytes: &[u8],
    min_width: u32,
    min_height: u32,
    min_bytes: u64,
) -> Result<(u32, u32), ProofError> {
    if bytes.len() < 33 || bytes[..8] != PNG_MAGIC {
        return Err(ProofError::Invalid("not a valid PNG file".to_string()));
    }
    let mut pos = 8usize;
    let mut ihdr: Option<(u32, u32, u8)> = None;
    let mut saw_plte = false;
    let mut idat_state = IdatState::Before;
    let mut idat_total = 0usize;
    let mut zlib_head = [0u8; 2];
    let mut zlib_head_len = 0usize;
    let mut seen_idat_bytes = [false; 256];
    let mut saw_iend = false;

    while pos < bytes.len() {
        if pos + 12 > bytes.len() {
            return Err(ProofError::Invalid(
                "truncated PNG chunk header".to_string(),
            ));
        }
        let len_u32 =
            u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        if len_u32 > MAX_PNG_CHUNK_LEN {
            return Err(ProofError::Invalid("corrupt PNG chunk length".to_string()));
        }
        let len = len_u32 as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        let Some(data_end) = data_start.checked_add(len) else {
            return Err(ProofError::Invalid("corrupt PNG chunk length".to_string()));
        };
        let Some(next_pos) = data_end.checked_add(4) else {
            return Err(ProofError::Invalid("corrupt PNG chunk CRC".to_string()));
        };
        if next_pos > bytes.len() {
            return Err(ProofError::Invalid("truncated PNG chunk".to_string()));
        }
        let data = &bytes[data_start..data_end];
        let stored_crc = u32::from_be_bytes([
            bytes[data_end],
            bytes[data_end + 1],
            bytes[data_end + 2],
            bytes[data_end + 3],
        ]);
        if crc32(kind, data) != stored_crc {
            return Err(ProofError::Invalid("PNG chunk CRC-32 mismatch".to_string()));
        }

        if ihdr.is_none() {
            if kind != b"IHDR" {
                return Err(ProofError::Invalid(
                    "PNG must begin with valid IHDR".to_string(),
                ));
            }
            ihdr = Some(parse_ihdr(data)?);
        } else if kind == b"IHDR" {
            return Err(ProofError::Invalid("duplicate PNG IHDR chunk".to_string()));
        } else if kind == b"PLTE" {
            if idat_state != IdatState::Before {
                return Err(ProofError::Invalid(
                    "PNG PLTE chunk must appear before IDAT".to_string(),
                ));
            }
            if data.is_empty() || !data.len().is_multiple_of(3) {
                return Err(ProofError::Invalid(
                    "invalid PNG PLTE chunk length".to_string(),
                ));
            }
            saw_plte = true;
        } else if kind == b"IDAT" {
            if idat_state == IdatState::After {
                return Err(ProofError::Invalid(
                    "non-contiguous PNG IDAT chunks".to_string(),
                ));
            }
            idat_state = IdatState::Inside;
            idat_total = idat_total.saturating_add(data.len());
            for &b in data {
                if zlib_head_len < 2 {
                    zlib_head[zlib_head_len] = b;
                    zlib_head_len += 1;
                }
                seen_idat_bytes[usize::from(b)] = true;
            }
        } else if kind == b"IEND" {
            if !data.is_empty() {
                return Err(ProofError::Invalid(
                    "PNG IEND chunk must have zero length".to_string(),
                ));
            }
            saw_iend = true;
            pos = next_pos;
            break;
        } else if idat_state == IdatState::Inside {
            idat_state = IdatState::After;
        }
        pos = next_pos;
    }

    let (width, height, color_type) =
        ihdr.ok_or_else(|| ProofError::Invalid("missing PNG IHDR".to_string()))?;
    if !saw_iend {
        return Err(ProofError::Invalid("missing PNG IEND chunk".to_string()));
    }
    if pos != bytes.len() {
        return Err(ProofError::Invalid(
            "unexpected trailing bytes after PNG IEND".to_string(),
        ));
    }
    if color_type == 3 && !saw_plte {
        return Err(ProofError::Invalid(
            "indexed-color PNG (color_type=3) requires a PLTE chunk".to_string(),
        ));
    }
    if idat_total == 0 || zlib_head_len < 2 {
        return Err(ProofError::Invalid(
            "incomplete PNG stream (missing or empty IDAT)".to_string(),
        ));
    }
    let min_idat = usize::try_from((min_bytes / 2).max(64)).unwrap_or(usize::MAX);
    if idat_total < min_idat {
        return Err(ProofError::Invalid(format!(
            "PNG IDAT payload is {idat_total} bytes, below minimum {min_idat} bytes"
        )));
    }
    let cmf = zlib_head[0];
    let flg = zlib_head[1];
    let header_word = u16::from(cmf) * 256 + u16::from(flg);
    if (cmf & 0x0F) != 8 || (cmf >> 4) > 7 || (flg & 0x20) != 0 || !header_word.is_multiple_of(31) {
        return Err(ProofError::Invalid(
            "PNG IDAT does not begin with a valid zlib header".to_string(),
        ));
    }
    let distinct = seen_idat_bytes.iter().filter(|&&b| b).count();
    if distinct < MIN_IDAT_DISTINCT_BYTES {
        return Err(ProofError::Invalid(format!(
            "PNG IDAT payload is too uniform ({distinct} distinct byte values; minimum {MIN_IDAT_DISTINCT_BYTES})"
        )));
    }
    if width < min_width || height < min_height {
        return Err(ProofError::Invalid(format!(
            "PNG dimensions {width}x{height} below minimum {min_width}x{min_height}"
        )));
    }
    Ok((width, height))
}
