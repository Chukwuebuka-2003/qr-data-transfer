//! The QF4 optical frame: one RaptorQ transport packet per QR code.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! offset  size  field
//! 0       3     magic "QF4" (0x51 0x46 0x34)
//! 3       1     compressed flag (bit 0)
//! 4       4     session id (CRC-32 of the container)
//! 8       4     container length
//! 12      4     original file size
//! 16      2     transport payload (symbol) size
//! 18      1     protocol revision (1)
//! 19      s     RaptorQ transport packet (4-byte payload id + symbol)
//! 19+s    4     CRC-32 of everything before it
//! ```
//!
//! Every frame is self-describing so the receiver can join mid-stream, drop
//! frames, and accept packets out of order.

use crate::crc32::crc32;
use crate::error::{Error, Result};

pub const FRAME_MAGIC: [u8; 3] = [0x51, 0x46, 0x34]; // "QF4"
pub const FRAME_HEADER_BYTES: usize = 19;
pub const FRAME_CRC_BYTES: usize = 4;
pub const OPTICAL_FRAME_OVERHEAD: usize = FRAME_HEADER_BYTES + FRAME_CRC_BYTES;
pub const PROTOCOL_REVISION: u8 = 1;

/// A validated QF4 frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpticalFrame {
    pub session: u32,
    pub container_length: u32,
    pub original_size: u32,
    pub compressed: bool,
    pub symbol_size: u16,
    pub payload: Vec<u8>,
}

/// Serialize one RaptorQ transport packet into a QF4 frame.
pub fn serialize_frame(
    payload: &[u8],
    session: u32,
    container_length: u32,
    original_size: u32,
    compressed: bool,
    symbol_size: u16,
) -> Result<Vec<u8>> {
    if payload.len() != symbol_size as usize {
        return Err(Error::InvalidSymbolSize(symbol_size));
    }

    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len() + FRAME_CRC_BYTES);
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.push(u8::from(compressed));
    frame.extend_from_slice(&session.to_le_bytes());
    frame.extend_from_slice(&container_length.to_le_bytes());
    frame.extend_from_slice(&original_size.to_le_bytes());
    frame.extend_from_slice(&symbol_size.to_le_bytes());
    frame.push(PROTOCOL_REVISION);
    frame.extend_from_slice(payload);
    let crc = crc32(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());
    Ok(frame)
}

/// Parse and verify a QF4 frame.
pub fn parse_frame(frame: &[u8]) -> Result<OpticalFrame> {
    if frame.len() < OPTICAL_FRAME_OVERHEAD + 5 {
        return Err(Error::InvalidFrame("Optical frame is too short."));
    }
    if frame[..3] != FRAME_MAGIC {
        return Err(Error::InvalidFrame(
            "This is not a QRFerry v4 optical frame.",
        ));
    }
    if frame[18] != PROTOCOL_REVISION {
        return Err(Error::InvalidFrame("Unsupported optical frame revision."));
    }

    let symbol_size = u16::from_le_bytes([frame[16], frame[17]]);
    if frame.len() != FRAME_HEADER_BYTES + symbol_size as usize + FRAME_CRC_BYTES {
        return Err(Error::InvalidFrame(
            "Optical frame length does not match its header.",
        ));
    }

    let expected_crc = u32::from_le_bytes(
        frame[FRAME_HEADER_BYTES + symbol_size as usize..]
            .try_into()
            .unwrap(),
    );
    let actual_crc = crc32(&frame[..FRAME_HEADER_BYTES + symbol_size as usize]);
    if expected_crc != actual_crc {
        return Err(Error::InvalidFrame("Optical frame checksum failed."));
    }

    Ok(OpticalFrame {
        session: u32::from_le_bytes(frame[4..8].try_into().unwrap()),
        container_length: u32::from_le_bytes(frame[8..12].try_into().unwrap()),
        original_size: u32::from_le_bytes(frame[12..16].try_into().unwrap()),
        compressed: frame[3] & 1 == 1,
        symbol_size,
        payload: frame[FRAME_HEADER_BYTES..FRAME_HEADER_BYTES + symbol_size as usize].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_frame, serialize_frame, FRAME_HEADER_BYTES, OPTICAL_FRAME_OVERHEAD};
    use crate::crc32::crc32;

    fn sample() -> (Vec<u8>, u16) {
        ((0..64).map(|i| (i * 7 % 251) as u8).collect(), 64)
    }

    #[test]
    fn roundtrip_and_exact_layout() {
        let (payload, symbol_size) = sample();
        let frame =
            serialize_frame(&payload, 0x1234_5678, 10_000, 9_999, true, symbol_size).unwrap();

        // Exact byte layout.
        assert_eq!(&frame[..3], b"QF4");
        assert_eq!(frame[3], 1); // compressed
        assert_eq!(&frame[4..8], &0x1234_5678u32.to_le_bytes());
        assert_eq!(&frame[8..12], &10_000u32.to_le_bytes());
        assert_eq!(&frame[12..16], &9_999u32.to_le_bytes());
        assert_eq!(&frame[16..18], &64u16.to_le_bytes());
        assert_eq!(frame[18], 1); // revision
        assert_eq!(&frame[19..19 + 64], &payload[..]);
        let expected_crc = crc32(&frame[..FRAME_HEADER_BYTES + 64]);
        assert_eq!(
            &frame[FRAME_HEADER_BYTES + 64..],
            &expected_crc.to_le_bytes()
        );

        let parsed = parse_frame(&frame).unwrap();
        assert_eq!(parsed.session, 0x1234_5678);
        assert_eq!(parsed.container_length, 10_000);
        assert_eq!(parsed.original_size, 9_999);
        assert!(parsed.compressed);
        assert_eq!(parsed.symbol_size, 64);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn corrupt_byte_fails_crc() {
        let (payload, symbol_size) = sample();
        let mut frame = serialize_frame(&payload, 7, 100, 99, false, symbol_size).unwrap();
        frame[20] ^= 0x40; // inside the payload
        assert!(parse_frame(&frame).is_err());
    }

    #[test]
    fn corrupt_header_byte_fails_crc() {
        let (payload, symbol_size) = sample();
        let mut frame = serialize_frame(&payload, 7, 100, 99, false, symbol_size).unwrap();
        frame[9] ^= 0x01; // container length field
        assert!(parse_frame(&frame).is_err());
    }

    #[test]
    fn truncated_frame_rejected() {
        let (payload, symbol_size) = sample();
        let frame = serialize_frame(&payload, 7, 100, 99, false, symbol_size).unwrap();
        assert!(parse_frame(&frame[..frame.len() - 1]).is_err());
        assert!(parse_frame(&frame[..OPTICAL_FRAME_OVERHEAD + 4]).is_err());
    }

    #[test]
    fn wrong_magic_rejected() {
        let (payload, symbol_size) = sample();
        let mut frame = serialize_frame(&payload, 7, 100, 99, false, symbol_size).unwrap();
        frame[0] = 0x51;
        frame[1] = 0x46;
        frame[2] = 0x33; // "QF3" legacy
        assert!(parse_frame(&frame).is_err());
    }

    #[test]
    fn wrong_revision_rejected() {
        let (payload, symbol_size) = sample();
        let mut frame = serialize_frame(&payload, 7, 100, 99, false, symbol_size).unwrap();
        frame[18] = 2;
        assert!(parse_frame(&frame).is_err());
    }

    #[test]
    fn payload_size_mismatch_rejected() {
        let (payload, symbol_size) = sample();
        // Serialize a frame whose declared symbol size does not match the
        // payload: the length check must fail before the CRC is even read.
        let mut frame = serialize_frame(&payload, 7, 100, 99, false, symbol_size).unwrap();
        frame[16] = 0x3f; // declare 63 bytes for a 64-byte payload
        frame[17] = 0x00;
        assert!(parse_frame(&frame).is_err());
    }
}
