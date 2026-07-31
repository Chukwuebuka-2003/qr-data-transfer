//! Generate the Rust side of the wire-interop fixture.
//!
//! Writes /tmp/wire/rust_container.bin and /tmp/wire/rust_frames.bin using
//! the exact same inputs as scripts/wire-fixture in the browser build, so
//! the two streams can be compared byte-for-byte and cross-decoded.
use std::fs;

use qrferry::{build_optical_container, create_optical_transfer, parse_frame, CompressionMode};

fn main() {
    let original: Vec<u8> = (0..1000).map(|i| (i % 251) as u8).collect();
    let prepared = build_optical_container(
        &original,
        &original,
        "sample.bin",
        "application/octet-stream",
        CompressionMode::None,
    )
    .expect("container build failed");

    let transfer = create_optical_transfer(&prepared, 256, 30).expect("RaptorQ encode failed");

    let frames: Vec<u8> = transfer
        .packets
        .iter()
        .flat_map(|frame| frame.iter().copied())
        .collect();
    let frame_len = parse_frame(&transfer.packets[0])
        .expect("frame parse failed")
        .payload
        .len()
        + 23; // frame header + crc

    fs::create_dir_all("/tmp/wire").expect("mkdir failed");
    fs::write("/tmp/wire/rust_container.bin", &prepared.container).expect("container write failed");
    fs::write("/tmp/wire/rust_frames.bin", &frames).expect("frames write failed");

    println!(
        "Rust container: {} bytes, frames: {}, frameLen={}",
        prepared.container.len(),
        transfer.packets.len(),
        frame_len
    );
}
