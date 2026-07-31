//! RaptorQ (RFC 6330) fountain coding over the optical transport format.
//!
//! The browser build compiles a thin wrapper around the `raptorq` crate to
//! WebAssembly. This module reproduces that wrapper natively with the exact
//! same source-block geometry, repair calculation, and transport packet
//! layout, so encoded streams are byte-identical to the original sender and
//! decode with the original receiver (and vice versa).
//!
//! Transport packet layout: a 4-byte payload id (source block number, then a
//! 24-bit encoding symbol id, big-endian) followed by the RaptorQ symbol
//! data. `symbol_size` everywhere refers to the full transport payload size;
//! the RaptorQ symbol inside is `symbol_size - 4` bytes.

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation};

use crate::compression::CompressionMode;
use crate::container::{OpticalFileMeta, PreparedOpticalFile};
use crate::crc32::crc32;
use crate::error::{Error, Result};
use crate::frame::{serialize_frame, OpticalFrame};

pub const RAPTORQ_PAYLOAD_ID_BYTES: usize = 4;
pub const RAPTORQ_MAX_SOURCE_SYMBOLS_PER_BLOCK: u64 = 56_403;

/// An encoded optical transfer: every frame, plus packet classification.
#[derive(Clone, Debug)]
pub struct OpticalTransfer {
    pub meta: OpticalFileMeta,
    /// CRC-32 of the container; identifies the transfer and verifies it.
    pub session: u32,
    pub container_length: u32,
    /// Transport payload size per frame.
    pub symbol_size: u16,
    /// Number of source (systematic) packets required to cover the container.
    pub source_packet_count: u32,
    /// Serialized QF4 frames, source packets of every block followed by their
    /// repair packets.
    pub packets: Vec<Vec<u8>>,
    /// Indices into `packets` that are systematic source packets.
    pub source_packet_indices: Vec<usize>,
    /// Indices into `packets` that are repair packets.
    pub repair_packet_indices: Vec<usize>,
}

/// RFC 6330 `ceil(x / y)`, returning 0 when `value` is 0 (mirrors the WASM
/// wrapper's helper).
fn ceil_div(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    (value - 1) / divisor + 1
}

/// The RaptorQ symbol size stored inside a transport payload.
fn raptorq_symbol_size(transport_payload_size: u16) -> Result<u16> {
    if transport_payload_size <= RAPTORQ_PAYLOAD_ID_BYTES as u16 {
        return Err(Error::InvalidSymbolSize(transport_payload_size));
    }
    Ok(transport_payload_size - RAPTORQ_PAYLOAD_ID_BYTES as u16)
}

/// Build the RFC 6330 object transmission information exactly like the WASM
/// wrapper: alignment 1, one sub-block, at most 56,403 source symbols per
/// source block.
fn raptorq_config(
    data_len: usize,
    transport_payload_size: u16,
) -> Result<ObjectTransmissionInformation> {
    let symbol_size = raptorq_symbol_size(transport_payload_size)?;
    let transfer_length = data_len as u64;
    let total_symbols = ceil_div(transfer_length, u64::from(symbol_size)).max(1);
    let source_blocks = ceil_div(total_symbols, RAPTORQ_MAX_SOURCE_SYMBOLS_PER_BLOCK).max(1);
    if source_blocks > u64::from(u8::MAX) {
        return Err(Error::RaptorQ(
            "RaptorQ object requires more than 255 source blocks",
        ));
    }
    Ok(ObjectTransmissionInformation::new(
        transfer_length,
        symbol_size,
        source_blocks as u8,
        1,
        1,
    ))
}

fn repair_packets_for_block(source_packets: usize, repair_percent: u32) -> Result<u32> {
    let source = source_packets as u128;
    let repair = (source * u128::from(repair_percent)).div_ceil(100);
    if repair > u128::from(u32::MAX) {
        return Err(Error::RaptorQ("repair packet count exceeds u32::MAX"));
    }
    Ok(repair as u32)
}

/// Encode a prepared container into an optical transfer.
pub fn create_optical_transfer(
    prepared: &PreparedOpticalFile,
    symbol_size: u16,
    repair_percent: u32,
) -> Result<OpticalTransfer> {
    let session = crc32(&prepared.container);
    let config = raptorq_config(prepared.container.len(), symbol_size)?;
    let encoder = Encoder::new(&prepared.container, config);

    let mut packets: Vec<Vec<u8>> = Vec::new();
    for block in encoder.get_block_encoders() {
        let source = block.source_packets();
        let repair_count = repair_packets_for_block(source.len(), repair_percent)?;
        for packet in source {
            packets.push(packet.serialize());
        }
        for packet in block.repair_packets(0, repair_count) {
            packets.push(packet.serialize());
        }
    }

    let frames = packets
        .iter()
        .map(|payload| {
            serialize_frame(
                payload,
                session,
                prepared.container.len() as u32,
                prepared.meta.file_size,
                prepared.meta.compression != CompressionMode::None,
                symbol_size,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let (source_packet_indices, repair_packet_indices) =
        classify_raptorq_packets(&packets, prepared.container.len(), symbol_size)?;

    let source_packet_count = std::cmp::max(
        1,
        ceil_div(
            prepared.container.len() as u64,
            u64::from(symbol_size - RAPTORQ_PAYLOAD_ID_BYTES as u16),
        ),
    ) as u32;

    Ok(OpticalTransfer {
        meta: prepared.meta.clone(),
        session,
        container_length: prepared.container.len() as u32,
        symbol_size,
        source_packet_count,
        packets: frames,
        source_packet_indices,
        repair_packet_indices,
    })
}

/// Classify serialized RaptorQ packets into source and repair indices from
/// their 4-byte payload ids, using the same RFC 6330 source-block geometry
/// as the WASM wrapper.
pub fn classify_raptorq_packets(
    serialized_packets: &[Vec<u8>],
    data_len: usize,
    transport_payload_size: u16,
) -> Result<(Vec<usize>, Vec<usize>)> {
    let source_symbol_size = if transport_payload_size > RAPTORQ_PAYLOAD_ID_BYTES as u16 {
        transport_payload_size - RAPTORQ_PAYLOAD_ID_BYTES as u16
    } else {
        return Err(Error::InvalidSymbolSize(transport_payload_size));
    };

    let total_source_symbols =
        std::cmp::max(1, ceil_div(data_len as u64, u64::from(source_symbol_size)));
    let source_block_count = std::cmp::max(
        1,
        ceil_div(total_source_symbols, RAPTORQ_MAX_SOURCE_SYMBOLS_PER_BLOCK),
    );
    let largest_block_source_count = ceil_div(total_source_symbols, source_block_count);
    let smallest_block_source_count = largest_block_source_count - 1;
    let larger_block_count =
        total_source_symbols - smallest_block_source_count * source_block_count;

    let mut source_packet_indices = Vec::new();
    let mut repair_packet_indices = Vec::new();

    for (packet_index, payload) in serialized_packets.iter().enumerate() {
        if payload.len() < RAPTORQ_PAYLOAD_ID_BYTES {
            return Err(Error::InvalidPacket(
                "RaptorQ packet is too short for a 4-byte payload id",
            ));
        }
        let source_block_number = payload[0];
        if u64::from(source_block_number) >= source_block_count {
            return Err(Error::InvalidPacket(
                "RaptorQ packet references an invalid source block",
            ));
        }
        let encoding_symbol_id =
            (u32::from(payload[1]) << 16) | (u32::from(payload[2]) << 8) | u32::from(payload[3]);
        let source_count_for_block = if u64::from(source_block_number) < larger_block_count {
            largest_block_source_count
        } else {
            smallest_block_source_count
        };
        if encoding_symbol_id < source_count_for_block as u32 {
            source_packet_indices.push(packet_index);
        } else {
            repair_packet_indices.push(packet_index);
        }
    }

    Ok((source_packet_indices, repair_packet_indices))
}

/// Deduplication key for a decoded frame: session plus the 4-byte RaptorQ
/// payload id (matches `raptorPacketKey` in the browser build).
pub fn raptor_packet_key(frame: &OpticalFrame) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        frame.session, frame.payload[0], frame.payload[1], frame.payload[2], frame.payload[3]
    )
}

/// Incremental RaptorQ decoder (port of `RaptorQWasmDecoder`).
pub struct RaptorQDecoder {
    inner: Decoder,
    source_block_count: u8,
}

impl RaptorQDecoder {
    /// Create a decoder for a transfer whose container is `container_length`
    /// bytes with `symbol_size`-byte transport payloads.
    pub fn new(container_length: u32, symbol_size: u16) -> Result<Self> {
        let config = raptorq_config(container_length as usize, symbol_size)?;
        let source_block_count = config.source_blocks();
        Ok(RaptorQDecoder {
            inner: Decoder::new(config),
            source_block_count,
        })
    }

    /// Push one RaptorQ transport packet. Returns the recovered container
    /// (exactly `container_length` bytes) once enough unique symbols have
    /// been received, and `None` otherwise.
    pub fn push(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>> {
        if payload.len() < RAPTORQ_PAYLOAD_ID_BYTES {
            return Err(Error::InvalidPacket("RaptorQ packet is too short"));
        }
        if payload[0] >= self.source_block_count {
            return Err(Error::InvalidPacket(
                "RaptorQ packet references an invalid source block",
            ));
        }
        let packet = EncodingPacket::deserialize(payload);
        Ok(self.inner.decode(packet))
    }
}

/// Interleave source and repair packet indices so repair symbols are
/// sprinkled evenly through the stream (port of `evenlyInterleave`).
pub fn evenly_interleave(source: &[usize], repair: &[usize]) -> Vec<usize> {
    if source.is_empty() {
        return repair.to_vec();
    }
    if repair.is_empty() {
        return source.to_vec();
    }
    let mut order = Vec::with_capacity(source.len() + repair.len());
    let mut repair_index = 0usize;
    let mut accumulator = 0usize;
    for &source_index in source {
        order.push(source_index);
        accumulator += repair.len();
        while repair_index < repair.len() && accumulator >= source.len() {
            order.push(repair[repair_index]);
            repair_index += 1;
            accumulator -= source.len();
        }
    }
    while repair_index < repair.len() {
        order.push(repair[repair_index]);
        repair_index += 1;
    }
    order
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        classify_raptorq_packets, create_optical_transfer, evenly_interleave, raptor_packet_key,
        RaptorQDecoder, RAPTORQ_PAYLOAD_ID_BYTES,
    };
    use crate::compression::CompressionMode;
    use crate::container::{build_optical_container, parse_optical_container};
    use crate::crc32::crc32;
    use crate::frame::parse_frame;

    /// Container size is 23 header bytes + "sample.bin" (10) +
    /// "application/octet-stream" (24) + payload.
    fn container_len(payload: usize) -> usize {
        57 + payload
    }

    fn prepared(length: usize) -> crate::container::PreparedOpticalFile {
        let original: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
        build_optical_container(
            &original,
            &original,
            "sample.bin",
            "application/octet-stream",
            CompressionMode::None,
        )
        .unwrap()
    }

    #[test]
    fn encode_produces_expected_packet_count() {
        // 5057 bytes / (256 - 4) -> 21 source packets, 30% -> 7 repair.
        let transfer = create_optical_transfer(&prepared(5000), 256, 30).unwrap();
        assert_eq!(transfer.container_length as usize, container_len(5000));
        assert_eq!(transfer.source_packet_count, 21);
        assert_eq!(transfer.packets.len(), 28);
        assert_eq!(transfer.source_packet_indices.len(), 21);
        assert_eq!(transfer.repair_packet_indices.len(), 7);
    }

    #[test]
    fn classification_matches_payload_ids() {
        let transfer = create_optical_transfer(&prepared(5000), 256, 30).unwrap();
        let packets: Vec<Vec<u8>> = transfer
            .packets
            .iter()
            .map(|frame| parse_frame(frame).unwrap().payload)
            .collect();
        let (source, repair) =
            classify_raptorq_packets(&packets, transfer.container_length as usize, 256).unwrap();
        assert_eq!(source, transfer.source_packet_indices);
        assert_eq!(repair, transfer.repair_packet_indices);
    }

    #[test]
    fn every_frame_is_self_describing() {
        let transfer = create_optical_transfer(&prepared(5000), 256, 30).unwrap();
        let mut payload_ids = HashSet::new();
        for frame in &transfer.packets {
            let parsed = parse_frame(frame).unwrap();
            assert_eq!(parsed.session, transfer.session);
            assert_eq!(parsed.container_length, transfer.container_length);
            assert_eq!(parsed.original_size, transfer.meta.file_size);
            assert_eq!(parsed.symbol_size, transfer.symbol_size);
            assert_eq!(parsed.payload.len(), 256);
            assert!(
                payload_ids.insert(parsed.payload[..4].to_vec()),
                "duplicate RaptorQ payload id"
            );
            // The key includes the session so cross-transfer collisions are
            // impossible.
            assert!(raptor_packet_key(&parsed).starts_with(&format!("{}:", transfer.session)));
        }
        assert_eq!(payload_ids.len(), transfer.packets.len());
    }

    #[test]
    fn full_roundtrip_with_erasures() {
        let transfer = create_optical_transfer(&prepared(50_000), 512, 35).unwrap();
        assert_eq!(transfer.source_packet_count, 99);
        // Simulate a lossy camera: drop every 10th and every 7th frame.
        let keep: Vec<usize> = (0..transfer.packets.len())
            .filter(|i| i % 10 != 0 && i % 7 != 0)
            .collect();
        let mut decoder =
            RaptorQDecoder::new(transfer.container_length, transfer.symbol_size).unwrap();
        let mut recovered = None;
        for &index in &keep {
            let frame = parse_frame(&transfer.packets[index]).unwrap();
            if let Some(container) = decoder.push(&frame.payload).unwrap() {
                recovered = Some(container);
                break;
            }
        }
        let container = recovered.expect("reconstruction failed with ~77% of frames");
        assert_eq!(container.len(), transfer.container_length as usize);
        assert_eq!(crc32(&container), transfer.session);
        let (meta, _) = parse_optical_container(&container).unwrap();
        assert_eq!(meta, transfer.meta);
    }

    #[test]
    fn mid_stream_join_and_out_of_order() {
        let transfer = create_optical_transfer(&prepared(50_000), 512, 35).unwrap();
        // Join after the first 10% of frames; deliver the rest reversed.
        let order: Vec<usize> = (transfer.packets.len() / 10..transfer.packets.len())
            .rev()
            .collect();
        let mut decoder =
            RaptorQDecoder::new(transfer.container_length, transfer.symbol_size).unwrap();
        for &index in &order {
            let frame = parse_frame(&transfer.packets[index]).unwrap();
            if let Some(container) = decoder.push(&frame.payload).unwrap() {
                assert_eq!(crc32(&container), transfer.session);
                return;
            }
        }
        panic!("reconstruction failed joining mid-stream");
    }

    #[test]
    fn small_transfer_single_block() {
        // 117-byte container fits in one 124-byte symbol.
        let transfer = create_optical_transfer(&prepared(60), 128, 20).unwrap();
        assert_eq!(transfer.source_packet_count, 1);
        let mut decoder =
            RaptorQDecoder::new(transfer.container_length, transfer.symbol_size).unwrap();
        for frame in &transfer.packets {
            let parsed = parse_frame(frame).unwrap();
            if let Some(container) = decoder.push(&parsed.payload).unwrap() {
                assert_eq!(crc32(&container), transfer.session);
                return;
            }
        }
        panic!("single-symbol transfer did not decode");
    }

    #[test]
    fn interleave_spreads_repair_evenly() {
        let source: Vec<usize> = (0..20).collect();
        let repair: Vec<usize> = (100..106).collect();
        let order = evenly_interleave(&source, &repair);
        assert_eq!(order.len(), 26);
        let repair_positions: Vec<usize> = order
            .iter()
            .enumerate()
            .filter(|(_, v)| **v >= 100)
            .map(|(i, _)| i)
            .collect();
        for pair in repair_positions.windows(2) {
            assert!(pair[1] - pair[0] >= 3 && pair[1] - pair[0] <= 6);
        }
    }

    #[test]
    fn decoder_rejects_unknown_source_block() {
        let transfer = create_optical_transfer(&prepared(5000), 256, 30).unwrap();
        let mut decoder =
            RaptorQDecoder::new(transfer.container_length, transfer.symbol_size).unwrap();
        // A packet claiming a source block outside the transfer's geometry
        // must be rejected instead of panicking (the JS would panic).
        let mut foreign = parse_frame(&transfer.packets[0]).unwrap().payload;
        foreign[0] = 0xff;
        assert!(decoder.push(&foreign).is_err());
    }

    #[test]
    fn payload_ids_use_expected_layout() {
        let transfer = create_optical_transfer(&prepared(5000), 256, 30).unwrap();
        for frame in &transfer.packets {
            let parsed = parse_frame(frame).unwrap();
            assert_eq!(parsed.payload.len(), 256);
            // Byte 0 is the block number; bytes 1-3 the ESI (24-bit BE).
            let esi = (u32::from(parsed.payload[1]) << 16)
                | (u32::from(parsed.payload[2]) << 8)
                | u32::from(parsed.payload[3]);
            assert!(esi < (1 << 24));
            assert_eq!(parsed.payload.len() - RAPTORQ_PAYLOAD_ID_BYTES, 252);
        }
    }
}
