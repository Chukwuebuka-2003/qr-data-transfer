//! The QFC4 file envelope: metadata, checksums, and the transmitted payload.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! offset  size  field
//! 0       4     magic "QFC4" (0x51 0x46 0x43 0x34)
//! 4       1     compression: 0 none, 1 gzip, 2 brotli
//! 5       1     filename byte length
//! 6       1     mime byte length
//! 7       4     original file size
//! 11      4     CRC-32 of the original file
//! 15      4     transmitted (compressed) size
//! 19      4     CRC-32 of the transmitted payload
//! 23      n     filename bytes (UTF-8, <= 255)
//! 23+n    m     mime bytes (UTF-8, <= 255)
//! 23+n+m  p     transmitted payload
//! ```
//!
//! The container is the object protected by RaptorQ: its CRC-32 doubles as
//! the transfer session id and the final integrity check on the receiver.

use crate::compression::CompressionMode;
use crate::crc32::crc32;
use crate::error::{Error, Result};

pub const CONTAINER_MAGIC: [u8; 4] = [0x51, 0x46, 0x43, 0x34]; // "QFC4"
pub const CONTAINER_HEADER_BYTES: usize = 23;
pub const MAX_FILE_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_NAME_BYTES: usize = 255;

/// File metadata carried inside the container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpticalFileMeta {
    pub filename: String,
    pub mime: String,
    pub file_size: u32,
    pub transmitted_size: u32,
    pub file_crc: u32,
    pub transmitted_crc: u32,
    pub compression: CompressionMode,
}

/// A prepared transfer input: metadata plus the serialized container.
#[derive(Clone, Debug)]
pub struct PreparedOpticalFile {
    pub meta: OpticalFileMeta,
    pub container: Vec<u8>,
}

/// Truncate a UTF-8 string to `maximum_bytes` bytes without splitting a
/// multi-byte character (mirrors `utf8Prefix` in the browser build).
fn utf8_prefix(value: &str, maximum_bytes: usize) -> Vec<u8> {
    let bytes = value.as_bytes();
    if bytes.len() <= maximum_bytes {
        return bytes.to_vec();
    }
    let mut end = maximum_bytes;
    while end > 0 && (bytes[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    bytes[..end].to_vec()
}

/// Build a QFC4 container from the original and transmitted payloads.
pub fn build_optical_container(
    original: &[u8],
    transmitted: &[u8],
    filename: &str,
    mime: &str,
    compression: CompressionMode,
) -> Result<PreparedOpticalFile> {
    if original.len() > MAX_FILE_BYTES || transmitted.len() > MAX_FILE_BYTES {
        return Err(Error::FileTooLarge {
            size: original.len().max(transmitted.len()),
            max: MAX_FILE_BYTES,
        });
    }

    let filename_bytes = utf8_prefix(filename, MAX_NAME_BYTES);
    let mime_bytes = utf8_prefix(mime, MAX_NAME_BYTES);
    let file_crc = crc32(original);
    let transmitted_crc = crc32(transmitted);

    let mut container = Vec::with_capacity(
        CONTAINER_HEADER_BYTES + filename_bytes.len() + mime_bytes.len() + transmitted.len(),
    );
    container.extend_from_slice(&CONTAINER_MAGIC);
    container.push(compression.code());
    container.push(filename_bytes.len() as u8);
    container.push(mime_bytes.len() as u8);
    container.extend_from_slice(&(original.len() as u32).to_le_bytes());
    container.extend_from_slice(&file_crc.to_le_bytes());
    container.extend_from_slice(&(transmitted.len() as u32).to_le_bytes());
    container.extend_from_slice(&transmitted_crc.to_le_bytes());
    container.extend_from_slice(&filename_bytes);
    container.extend_from_slice(&mime_bytes);
    container.extend_from_slice(transmitted);

    let filename = String::from_utf8_lossy(&filename_bytes).into_owned();
    let mime = String::from_utf8_lossy(&mime_bytes).into_owned();

    Ok(PreparedOpticalFile {
        meta: OpticalFileMeta {
            filename: if filename.is_empty() {
                "transfer.bin".to_string()
            } else {
                filename
            },
            mime: if mime.is_empty() {
                "application/octet-stream".to_string()
            } else {
                mime
            },
            file_size: original.len() as u32,
            transmitted_size: transmitted.len() as u32,
            file_crc,
            transmitted_crc,
            compression,
        },
        container,
    })
}

/// Parse and verify a recovered QFC4 container, returning its metadata and
/// the transmitted payload.
pub fn parse_optical_container(container: &[u8]) -> Result<(OpticalFileMeta, Vec<u8>)> {
    if container.len() < CONTAINER_HEADER_BYTES {
        return Err(Error::InvalidContainer(
            "Recovered transfer is missing its file header.",
        ));
    }
    if container[..4] != CONTAINER_MAGIC {
        return Err(Error::InvalidContainer(
            "Recovered transfer has an invalid file header.",
        ));
    }

    let compression = CompressionMode::from_code(container[4]);
    let filename_len = container[5] as usize;
    let mime_len = container[6] as usize;
    let file_size = u32::from_le_bytes(container[7..11].try_into().unwrap());
    let file_crc = u32::from_le_bytes(container[11..15].try_into().unwrap());
    let transmitted_size = u32::from_le_bytes(container[15..19].try_into().unwrap());
    let transmitted_crc = u32::from_le_bytes(container[19..23].try_into().unwrap());

    let payload_offset = CONTAINER_HEADER_BYTES + filename_len + mime_len;
    if payload_offset + transmitted_size as usize != container.len() {
        return Err(Error::InvalidContainer(
            "Recovered transfer length did not match its file header.",
        ));
    }

    let filename = String::from_utf8_lossy(
        &container[CONTAINER_HEADER_BYTES..CONTAINER_HEADER_BYTES + filename_len],
    )
    .into_owned();
    let mime =
        String::from_utf8_lossy(&container[CONTAINER_HEADER_BYTES + filename_len..payload_offset])
            .into_owned();
    let transmitted = container[payload_offset..].to_vec();

    if crc32(&transmitted) != transmitted_crc {
        return Err(Error::InvalidContainer(
            "Recovered optical payload failed its checksum.",
        ));
    }

    Ok((
        OpticalFileMeta {
            filename: if filename.is_empty() {
                "transfer.bin".to_string()
            } else {
                filename
            },
            mime: if mime.is_empty() {
                "application/octet-stream".to_string()
            } else {
                mime
            },
            file_size,
            transmitted_size,
            file_crc,
            transmitted_crc,
            compression,
        },
        transmitted,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        build_optical_container, parse_optical_container, OpticalFileMeta, CONTAINER_HEADER_BYTES,
    };
    use crate::compression::CompressionMode;
    use crate::crc32::crc32;

    fn sample() -> (Vec<u8>, Vec<u8>) {
        let original: Vec<u8> = (0..2048).map(|i| (i % 251) as u8).collect();
        let transmitted: Vec<u8> = original.iter().rev().cloned().collect(); // stand-in "compressed"
        (original, transmitted)
    }

    #[test]
    fn roundtrip_preserves_metadata() {
        let (original, transmitted) = sample();
        let prepared = build_optical_container(
            &original,
            &transmitted,
            "photo.jpg",
            "image/jpeg",
            CompressionMode::Gzip,
        )
        .unwrap();

        let (meta, payload) = parse_optical_container(&prepared.container).unwrap();
        assert_eq!(payload, transmitted);
        assert_eq!(
            meta,
            OpticalFileMeta {
                filename: "photo.jpg".to_string(),
                mime: "image/jpeg".to_string(),
                file_size: original.len() as u32,
                transmitted_size: transmitted.len() as u32,
                file_crc: crc32(&original),
                transmitted_crc: crc32(&transmitted),
                compression: CompressionMode::Gzip,
            }
        );
    }

    #[test]
    fn default_names_when_empty() {
        let (original, transmitted) = sample();
        let prepared =
            build_optical_container(&original, &transmitted, "", "", CompressionMode::None)
                .unwrap();
        assert_eq!(prepared.meta.filename, "transfer.bin");
        assert_eq!(prepared.meta.mime, "application/octet-stream");
    }

    #[test]
    fn long_names_truncate_at_255_bytes() {
        let (original, transmitted) = sample();
        let long_name = "a".repeat(500);
        let prepared = build_optical_container(
            &original,
            &transmitted,
            &long_name,
            "text/plain",
            CompressionMode::None,
        )
        .unwrap();
        assert_eq!(prepared.meta.filename.len(), 255);
        let (meta, _) = parse_optical_container(&prepared.container).unwrap();
        assert_eq!(meta.filename.len(), 255);
    }

    #[test]
    fn utf8_truncation_does_not_split_characters() {
        let (original, transmitted) = sample();
        // "é" is two bytes; 300 bytes of é cannot end mid-character.
        let name = "é".repeat(300);
        let prepared = build_optical_container(
            &original,
            &transmitted,
            &name,
            "text/plain",
            CompressionMode::None,
        )
        .unwrap();
        assert_eq!(prepared.meta.filename.len(), 254); // 255th byte would be a continuation
    }

    #[test]
    fn corrupted_payload_fails_checksum() {
        let (original, mut transmitted) = sample();
        transmitted[42] ^= 0xff;
        let prepared = build_optical_container(
            &original,
            &transmitted,
            "photo.jpg",
            "image/jpeg",
            CompressionMode::None,
        )
        .unwrap();
        let mut container = prepared.container;
        // Corrupt a payload byte (past the header + name + mime).
        let index = CONTAINER_HEADER_BYTES + 5 + 10 + 7;
        container[index] ^= 0x01;
        assert!(parse_optical_container(&container).is_err());
    }

    #[test]
    fn wrong_magic_rejected() {
        let (original, transmitted) = sample();
        let prepared =
            build_optical_container(&original, &transmitted, "a.bin", "", CompressionMode::None)
                .unwrap();
        let mut container = prepared.container;
        container[0] = 0x00;
        assert!(parse_optical_container(&container).is_err());
    }

    #[test]
    fn truncated_container_rejected() {
        let (original, transmitted) = sample();
        let prepared =
            build_optical_container(&original, &transmitted, "a.bin", "", CompressionMode::None)
                .unwrap();
        assert!(
            parse_optical_container(&prepared.container[..prepared.container.len() - 1]).is_err()
        );
    }
}
