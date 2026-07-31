//! QRFerry — Rust port of the QF4 optical transfer codec.
//!
//! The original project (`deedy/qr-data-transfer`) moves files between two
//! devices as a live animated QR stream. This crate is a native rewrite of its
//! protocol core:
//!
//! - `container` — the QFC4 file envelope (metadata + checksums + payload)
//! - `frame` — the QF4 per-symbol optical frame (header + payload + CRC-32)
//! - `compression` — Brotli-11 / gzip-9 selection matching the browser build
//! - `transfer` — RFC 6330 RaptorQ (fountain code) encode/decode over the
//!   transport payload format used by the original
//! - `presets` — the six optical channel profiles (Robust .. 1 Mbps dual)
//! - `qrencode` — QR rendering of frames into RGBA images
//! - `sender` — the playable stream engine (interleaving, lane pacing)
//! - `crc32` — the table-driven CRC-32 used at every integrity layer
//!
//! The RaptorQ layer uses the same `raptorq` crate (v2.0.1) that the original
//! compiles to WebAssembly, with the same source-block geometry, so packets
//! produced here are byte-identical to the browser sender's stream.

pub mod compression;
pub mod container;
pub mod crc32;
pub mod error;
pub mod frame;
pub mod presets;
pub mod qrencode;
pub mod sender;
pub mod transfer;

pub use compression::{compress_for_transfer, decompress_transfer, CompressionMode};
pub use container::{
    build_optical_container, parse_optical_container, OpticalFileMeta, PreparedOpticalFile,
    CONTAINER_HEADER_BYTES, MAX_FILE_BYTES,
};
pub use error::{Error, Result};
pub use frame::{
    parse_frame, serialize_frame, OpticalFrame, FRAME_CRC_BYTES, FRAME_HEADER_BYTES,
    OPTICAL_FRAME_OVERHEAD, PROTOCOL_REVISION,
};
pub use presets::{get_preset, nominal_rate, Ecc, TransferPreset, TRANSFER_PRESETS};
pub use qrencode::{render_frame, QrImage, QUIET_ZONE_MODULES};
pub use sender::{format_bytes, Sender};
pub use transfer::{
    classify_raptorq_packets, create_optical_transfer, evenly_interleave, raptor_packet_key,
    OpticalTransfer, RaptorQDecoder, RAPTORQ_MAX_SOURCE_SYMBOLS_PER_BLOCK,
    RAPTORQ_PAYLOAD_ID_BYTES,
};
