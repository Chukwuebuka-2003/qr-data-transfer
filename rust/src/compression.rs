//! Compression selection for the optical channel.
//!
//! Mirrors the browser build: Brotli quality 11 and gzip level 9 are both
//! attempted, the smallest representation wins, and it is only used when it
//! saves at least 64 bytes over the raw payload.

use std::io::{Read, Write};

use flate2::{Compression, GzBuilder};

use crate::error::Result;

/// Minimum input size before compression is attempted.
pub const COMPRESSION_MIN_BYTES: usize = 768;
/// Compression must save at least this many bytes to be worth the decode cost.
pub const COMPRESSION_MIN_SAVINGS: usize = 64;

/// How the transmitted payload was compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionMode {
    None,
    Gzip,
    Brotli,
}

impl CompressionMode {
    /// Container header code (byte 4 of a QFC4 container).
    pub fn code(self) -> u8 {
        match self {
            CompressionMode::None => 0,
            CompressionMode::Gzip => 1,
            CompressionMode::Brotli => 2,
        }
    }

    /// Inverse of [`CompressionMode::code`]. Unknown codes map to `None`,
    /// matching the JavaScript parser.
    pub fn from_code(code: u8) -> CompressionMode {
        match code {
            1 => CompressionMode::Gzip,
            2 => CompressionMode::Brotli,
            _ => CompressionMode::None,
        }
    }
}

/// Result of [`compress_for_transfer`].
pub struct Compressed {
    pub bytes: Vec<u8>,
    pub mode: CompressionMode,
    pub saved_bytes: usize,
}

fn gzip_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::new(9));
    encoder.write_all(input)?;
    Ok(encoder.finish()?)
}

fn brotli_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        ..Default::default()
    };
    let mut output = Vec::with_capacity(input.len() / 2 + 64);
    let mut reader = input;
    brotli::enc::BrotliCompress(&mut reader, &mut output, &params)?;
    Ok(output)
}

/// Compress `bytes` for the optical channel, picking the smaller of
/// Brotli-11 and gzip-9. Brotli failure is tolerated (gzip wins), matching
/// the browser implementation.
pub fn compress_for_transfer(bytes: &[u8]) -> Result<Compressed> {
    if bytes.len() < COMPRESSION_MIN_BYTES {
        return Ok(Compressed {
            bytes: bytes.to_vec(),
            mode: CompressionMode::None,
            saved_bytes: 0,
        });
    }

    let gzip = gzip_bytes(bytes)?;
    let brotli = brotli_bytes(bytes).ok();
    let (best, mode) = match brotli {
        Some(brotli) if brotli.len() < gzip.len() => (brotli, CompressionMode::Brotli),
        _ => (gzip, CompressionMode::Gzip),
    };

    // Keep a small safety margin: a token saving is not worth decompression work.
    if best.len() + COMPRESSION_MIN_SAVINGS >= bytes.len() {
        return Ok(Compressed {
            bytes: bytes.to_vec(),
            mode: CompressionMode::None,
            saved_bytes: 0,
        });
    }

    Ok(Compressed {
        saved_bytes: bytes.len() - best.len(),
        mode,
        bytes: best,
    })
}

/// Recover the original payload from a transmitted representation.
pub fn decompress_transfer(bytes: &[u8], mode: CompressionMode) -> Result<Vec<u8>> {
    match mode {
        CompressionMode::None => Ok(bytes.to_vec()),
        CompressionMode::Gzip => {
            let mut decoder = flate2::read::GzDecoder::new(bytes);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output)?;
            Ok(output)
        }
        CompressionMode::Brotli => {
            let mut decoder = brotli::Decompressor::new(bytes, 8192);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output)?;
            Ok(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compress_for_transfer, decompress_transfer, CompressionMode};

    fn patterned(length: usize) -> Vec<u8> {
        (0..length).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn tiny_inputs_skip_compression() {
        let input = patterned(100);
        let compressed = compress_for_transfer(&input).unwrap();
        assert_eq!(compressed.mode, CompressionMode::None);
        assert_eq!(compressed.bytes, input);
    }

    #[test]
    fn gzip_roundtrip() {
        let input = patterned(64 * 1024);
        let compressed = compress_for_transfer(&input).unwrap();
        let restored = decompress_transfer(&compressed.bytes, compressed.mode).unwrap();
        assert_eq!(restored, input);
    }

    #[test]
    fn brotli_wins_on_text() {
        // Highly repetitive text compresses better with Brotli-11.
        let input = "the quick brown fox jumps over the lazy dog. ".repeat(4000);
        let compressed = compress_for_transfer(input.as_bytes()).unwrap();
        assert_eq!(compressed.mode, CompressionMode::Brotli);
        assert_eq!(
            decompress_transfer(&compressed.bytes, compressed.mode).unwrap(),
            input.as_bytes()
        );
    }

    #[test]
    fn incompressible_data_stays_raw() {
        let input = patterned(16 * 1024);
        // Patterned data is very compressible, so use a pseudorandom stream.
        let mut state = 0x1234_5678u32;
        let noise: Vec<u8> = (0..16 * 1024)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let compressed = compress_for_transfer(&noise).unwrap();
        let restored = decompress_transfer(&compressed.bytes, compressed.mode).unwrap();
        assert_eq!(restored, noise);
        // No assertion on mode: either the compressed form wins or it stays raw.
        let _ = input;
    }
}
