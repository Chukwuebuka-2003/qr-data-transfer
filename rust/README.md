# qrferry — Rust port of the QRFerry QF4 codec

A native rewrite of the protocol core of
[deedy/qr-data-transfer](https://github.com/deedy/qr-data-transfer) (QRFerry):
moving a file between two devices as a live animated QR stream, with the file
never touching a server.

This crate implements the sender/receiver codec layer only. The browser UI
(sender page, phone scanner page) is unchanged and remains the delivery
vehicle; this library is the engine that will drive it.

## Scope

- `container` — the QFC4 file envelope (metadata, CRC-32 checksums, payload)
- `frame` — the QF4 per-symbol optical frame (binary header, RaptorQ packet,
  CRC-32)
- `compression` — Brotli quality 11 / gzip level 9 selection with the same
  minimum-savings rule as the browser build
- `transfer` — RFC 6330 RaptorQ fountain coding: encode a container into a
  stream of self-describing frames, and incrementally decode a stream back
  into the container, tolerating erasures, out-of-order arrival, and
  mid-stream joins
- `presets` — the six channel profiles (Robust .. 1 Mbps dual) with QR
  byte-mode capacities verified against the `qrcode` crate at the exact
  JS-derived boundary
- `qrencode` — QR rendering with a single byte-mode segment (no ECI), matching
  the browser encoder, plus the standard 4-module quiet zone
- `sender` — the playable stream engine: source/repair interleaving, per-lane
  pacing, dual-lane alternation
- `receiver` — the native scanning pipeline: crop, downscale, ZXing-C++ decode,
  QF4 validation, deduplication, RaptorQ recovery, and four-layer checksum
  verification. Camera-agnostic (`FrameSource` trait); ships with a PNG
  sequence source for offline scanning and testing
- `crc32` — table-driven CRC-32 used at every integrity layer

## CLIs

```text
qrferry-send <file> [--preset <key>] [--scale <n>] [--out <dir>] [--frames <n>]
qrferry-recv [--out <dir>] [--dual] [--source <dir>] [--device <n>]
             [--width <n>] [--height <n>] [--fps <n>] [--no-window]
             [--max-frames <n>]
```

- `qrferry-send` window mode (default): plays the QR stream at the preset
  rate; Escape or close to quit. Dual presets render both lanes side by side.
  `--out <dir>` exports a PNG frame sequence (one full cycle by default) for
  offline playback from a projector, tablet, or TV.
- `qrferry-recv` opens the camera (or replays a `--source` PNG sequence),
  shows a live preview with a scan guide, and saves the verified file to
  `--out`. It reports delivered/scanner fps, decode p50/p95, accepted
  symbols, and progress.
- Presets: `robust`, `balanced`, `turbo`, `turbo30`, `turbo60`, `megabit`.

## Wire compatibility

The browser build wraps the `raptorq` crate (v2.0.1) in WebAssembly with a
specific source-block geometry: alignment 1, one sub-block, at most 56,403
source symbols per source block, a 4-byte transport payload id (source block
number + 24-bit encoding symbol id), and `ceil(source * repairPercent / 100)`
repair symbols per block. This crate reproduces that configuration exactly, so
packets and frames produced here are byte-identical to the original sender and
decode with the original receiver, and vice versa.

A transfer is verified at four layers, matching the original: per-frame
CRC-32, RaptorQ object CRC-32 (the session id), transmitted-payload CRC-32,
and original-file CRC-32.

## Usage

```rust
use qrferry::{
    build_optical_container, compress_for_transfer, create_optical_transfer,
    CompressionMode, RaptorQDecoder,
};

// Sender side
let compressed = compress_for_transfer(&file_bytes)?;
let prepared = build_optical_container(
    &file_bytes,
    &compressed.bytes,
    "photo.jpg",
    "image/jpeg",
    compressed.mode,
)?;
let transfer = create_optical_transfer(&prepared, 512, 35)?; // 512-byte symbols, 35% repair

// Receiver side
let mut decoder = RaptorQDecoder::new(transfer.container_length, 512)?;
for frame_bytes in /* decoded QR payloads */ {
    if let Some(container) = decoder.push(&frame_bytes)? {
        // verify container CRC, parse container, decompress, verify file CRC
        break;
    }
}
```

## Build and test

```bash
cargo build --release
cargo test
```

Requires Rust 1.97 or newer. The only dependencies are `raptorq` (RFC 6330
fountain coding), `brotli`, and `flate2`; the release profile strips symbols
and enables LTO for a small static binary.
