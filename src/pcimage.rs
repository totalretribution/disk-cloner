use crate::util;
use anyhow::{Context, Result};
use std::path::Path;

/// Bytes of header we need: through `block_size` at offset 84..88.
const HEADER_BYTES: usize = 128;
const MAGIC: &[u8] = b"partclone-image";

/// What a partclone v2 image header tells us about the filesystem inside.
/// `used_bytes` is the exact figure needed to stage the image, so scratch-space
/// checks do not have to guess from the compressed size.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageInfo {
    pub fs: String,
    /// Size of the filesystem the image was taken from.
    pub fs_bytes: u64,
    /// Blocks actually in use — what a sparse staging file will occupy.
    pub used_bytes: u64,
    pub block_size: u64,
}

/// Read the header of a (possibly compressed) partclone image.
/// `Ok(None)` means the format was not recognised, in which case callers fall
/// back to estimating from the compressed file size.
pub fn probe(path: &Path, compression: &str) -> Result<Option<ImageInfo>> {
    let p = path.to_str().context("image path not UTF-8")?;
    let bytes = match compression {
        // head closing the pipe early makes the decompressor exit non-zero;
        // that is expected, so the pipeline status is deliberately ignored.
        // Reading from stdin rather than by name: zstd and gzip both refuse to
        // open a symlink as a named input.
        "zstd" => util::capture_pipeline(&format!(
            "zstd -dc 2>/dev/null < '{}' | head -c {}",
            esc(p),
            HEADER_BYTES
        ))?,
        "gzip" => util::capture_pipeline(&format!(
            "gzip -dc 2>/dev/null < '{}' | head -c {}",
            esc(p),
            HEADER_BYTES
        ))?,
        "none" => util::capture_pipeline(&format!(
            "head -c {} < '{}'",
            HEADER_BYTES,
            esc(p)
        ))?,
        _ => return Ok(None),
    };
    Ok(parse(&bytes))
}

/// Layout of partclone's v2 image header (little-endian). A single NUL follows
/// the magic, which is why every field sits one byte later than the struct
/// definition suggests.
pub fn parse(b: &[u8]) -> Option<ImageInfo> {
    if b.len() < 88 || &b[0..MAGIC.len()] != MAGIC || &b[30..34] != b"0002" {
        return None;
    }
    let fs = String::from_utf8_lossy(&b[36..52])
        .trim_end_matches('\0')
        .to_string();
    let total = u64::from_le_bytes(b[60..68].try_into().ok()?);
    let used = u64::from_le_bytes(b[68..76].try_into().ok()?);
    let block_size = u32::from_le_bytes(b[84..88].try_into().ok()?) as u64;
    if block_size == 0 {
        return None;
    }
    Some(ImageInfo {
        fs,
        fs_bytes: total.saturating_mul(block_size),
        used_bytes: used.saturating_mul(block_size),
        block_size,
    })
}

fn esc(s: &str) -> String {
    s.replace('\'', r"'\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(version: &[u8; 4], total: u64, used: u64, bs: u32) -> Vec<u8> {
        let mut b = vec![0u8; HEADER_BYTES];
        b[0..15].copy_from_slice(MAGIC);
        b[16..22].copy_from_slice(b"0.3.47");
        b[30..34].copy_from_slice(version);
        b[36..41].copy_from_slice(b"EXTFS");
        b[52..60].copy_from_slice(&(total * bs as u64).to_le_bytes());
        b[60..68].copy_from_slice(&total.to_le_bytes());
        b[68..76].copy_from_slice(&used.to_le_bytes());
        b[84..88].copy_from_slice(&bs.to_le_bytes());
        b
    }

    #[test]
    fn reads_used_and_total() {
        let info = parse(&header(b"0002", 115550720, 3395584, 4096)).unwrap();
        assert_eq!(info.fs, "EXTFS");
        assert_eq!(info.block_size, 4096);
        assert_eq!(info.fs_bytes, 115550720 * 4096);
        assert_eq!(info.used_bytes, 3395584 * 4096);
    }

    #[test]
    fn rejects_other_formats() {
        assert!(parse(b"not an image at all").is_none());
        assert!(parse(&header(b"0001", 10, 5, 4096)).is_none());
        let mut short = header(b"0002", 10, 5, 4096);
        short.truncate(80);
        assert!(parse(&short).is_none());
    }

    #[test]
    fn rejects_zero_block_size() {
        assert!(parse(&header(b"0002", 10, 5, 0)).is_none());
    }
}
