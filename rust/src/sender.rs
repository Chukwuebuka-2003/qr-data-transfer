//! The sender engine: file -> compressed container -> RaptorQ stream ->
//! rendered QR frames, paced per preset.
//!
//! Mirrors `send-client.tsx`: source and repair packets are evenly
//! interleaved, single-lane modes advance one lane at `fps`, dual-lane modes
//! alternate two lanes so each stays stable for two display refreshes.

use crate::compression::compress_for_transfer;
use crate::container::build_optical_container;
use crate::error::{Error, Result};
use crate::presets::{get_preset, TransferPreset};
use crate::qrencode::{render_frame, QrImage};
use crate::transfer::{create_optical_transfer, evenly_interleave, OpticalTransfer};

/// A prepared, playable QR stream.
pub struct Sender {
    pub transfer: OpticalTransfer,
    pub preset: TransferPreset,
    /// Interleaved play order over `transfer.packets`.
    order: Vec<usize>,
    /// Global playback cursor: every tick advances by one, so the combined
    /// stream always carries `fps` symbols per second (dual modes show a new
    /// packet per tick, alternating which lane it is drawn on, exactly like
    /// the browser sender).
    cursor: usize,
    /// Total frames shown (across all lanes).
    pub frames_played: u64,
}

impl Sender {
    /// Prepare a sender for `file` using the named preset.
    pub fn prepare(file: &[u8], filename: &str, mime: &str, preset_key: &str) -> Result<Sender> {
        let preset =
            get_preset(preset_key).ok_or_else(|| Error::UnknownPreset(preset_key.to_string()))?;
        Self::prepare_with_preset(file, filename, mime, preset)
    }

    /// Prepare a sender with an explicit preset.
    pub fn prepare_with_preset(
        file: &[u8],
        filename: &str,
        mime: &str,
        preset: &TransferPreset,
    ) -> Result<Sender> {
        let compressed = compress_for_transfer(file)?;
        let prepared =
            build_optical_container(file, &compressed.bytes, filename, mime, compressed.mode)?;
        let transfer =
            create_optical_transfer(&prepared, preset.symbol_size, preset.repair_percent)?;
        let order = evenly_interleave(
            &transfer.source_packet_indices,
            &transfer.repair_packet_indices,
        );
        Ok(Sender {
            transfer,
            preset: *preset,
            order,
            cursor: 0,
            frames_played: 0,
        })
    }

    /// Number of frames in one full cycle.
    pub fn cycle_len(&self) -> usize {
        self.order.len()
    }

    /// Index of the packet the next tick will render (0-based within the
    /// cycle); useful for UI progress.
    pub fn current_frame(&self) -> usize {
        self.cursor % self.order.len()
    }

    /// Render the next frame. In dual-lane modes this alternates lanes at the
    /// total stream rate, so each lane is updated at `fps / lanes` and stays
    /// stable for two display refreshes, while the combined stream carries
    /// `fps` symbols per second.
    pub fn next_frame(&mut self) -> Result<QrImage> {
        let index = self.order[self.cursor % self.order.len()];
        self.cursor += 1;
        self.frames_played += 1;
        render_frame(
            &self.transfer.packets[index],
            self.preset.version,
            self.preset.ecc,
            u32::from(self.preset.render_scale),
        )
    }

    /// Render the frame the next tick would show, without advancing playback.
    pub fn peek(&self) -> Result<QrImage> {
        let index = self.order[self.cursor % self.order.len()];
        render_frame(
            &self.transfer.packets[index],
            self.preset.version,
            self.preset.ecc,
            u32::from(self.preset.render_scale),
        )
    }

    /// Estimate the number of seconds needed to transmit the full cycle once
    /// at the nominal rate (mirrors `estimateDuration` in the browser build).
    pub fn estimated_seconds(&self) -> f64 {
        f64::from(self.transfer.source_packet_count) / (f64::from(self.preset.fps) * 0.78)
    }
}

/// Human-readable byte size (`formatBytes` port).
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use rxing::common::HybridBinarizer;
    use rxing::{BinaryBitmap, MultiFormatReader, RGBLuminanceSource, Reader};

    use super::{format_bytes, Sender};
    use crate::frame::parse_frame;
    use crate::transfer::RaptorQDecoder;

    fn decode_qr(image: &crate::qrencode::QrImage) -> Vec<u8> {
        let pixels = image.to_rxing_pixels();
        let source =
            RGBLuminanceSource::new_with_width_height_pixels(image.width, image.height, &pixels)
                .unwrap();
        let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
        MultiFormatReader::default()
            .decode(&mut bitmap)
            .unwrap()
            .getRawBytes()
            .to_vec()
    }

    #[test]
    fn full_optical_roundtrip_through_rendered_qrs() {
        let file: Vec<u8> = (0..50_000).map(|i| (i % 251) as u8).collect();
        let mut sender =
            Sender::prepare(&file, "payload.bin", "application/octet-stream", "robust").unwrap();

        // Play two full cycles; decode every rendered QR with rxing.
        let mut decoder = RaptorQDecoder::new(
            sender.transfer.container_length,
            sender.transfer.symbol_size,
        )
        .unwrap();
        let mut recovered = None;
        let mut shown = 0usize;
        while recovered.is_none() && shown < sender.cycle_len() * 2 {
            let image = sender.next_frame().unwrap();
            let frame_bytes = decode_qr(&image);
            let frame = parse_frame(&frame_bytes).unwrap();
            assert_eq!(frame.session, sender.transfer.session);
            if let Some(container) = decoder.push(&frame.payload).unwrap() {
                recovered = Some(container);
            }
            shown += 1;
        }

        let container = recovered.expect("full optical chain did not reconstruct");
        let (meta, transmitted) = crate::container::parse_optical_container(&container).unwrap();
        let restored =
            crate::compression::decompress_transfer(&transmitted, meta.compression).unwrap();
        assert_eq!(restored, file);
        assert_eq!(meta.filename, "payload.bin");
    }

    #[test]
    fn dual_lane_shows_new_packet_each_tick() {
        let file: Vec<u8> = (0..10_000).map(|i| (i % 251) as u8).collect();
        let mut sender =
            Sender::prepare(&file, "a.bin", "application/octet-stream", "turbo60").unwrap();
        assert_eq!(sender.preset.lanes, 2);
        // Every tick shows a NEW packet (alternating lanes), so the combined
        // stream carries fps symbols per second. After one full cycle every
        // packet has been displayed exactly once.
        let mut seen = std::collections::HashSet::new();
        let mut duplicates = 0usize;
        for _ in 0..sender.cycle_len() {
            let frame = sender.next_frame().unwrap();
            // Decode the rendered QR to identify the packet.
            let pixels = frame.to_rxing_pixels();
            let source = rxing::RGBLuminanceSource::new_with_width_height_pixels(
                frame.width,
                frame.height,
                &pixels,
            )
            .unwrap();
            let mut bitmap = rxing::BinaryBitmap::new(rxing::common::HybridBinarizer::new(source));
            let raw = rxing::MultiFormatReader::default()
                .decode(&mut bitmap)
                .unwrap()
                .getRawBytes()
                .to_vec();
            let parsed = crate::frame::parse_frame(&raw).unwrap();
            let key = crate::transfer::raptor_packet_key(&parsed);
            if !seen.insert(key) {
                duplicates += 1;
            }
        }
        assert_eq!(duplicates, 0, "a packet was shown more than once per cycle");
        assert_eq!(
            seen.len(),
            sender.cycle_len(),
            "not all packets shown in a cycle"
        );
        assert_eq!(sender.frames_played, sender.cycle_len() as u64);
    }

    #[test]
    fn format_bytes_renders() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10 MB");
        assert_eq!(format_bytes(1500 * 1024 * 1024), "1.5 GB");
    }
}
