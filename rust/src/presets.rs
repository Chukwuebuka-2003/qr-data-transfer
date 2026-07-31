//! Optical channel presets (port of `transfer-presets.ts`).
//!
//! QR byte-mode capacities come from the RS block table of the original
//! `@raptorqr/core` package (the `getMaxByteCapacity` fast_qr path):
//!
//! ```text
//! capacity = floor((dataCodewords * 8 - (4 + 16)) / 8)
//! ```
//!
//! with a 16-bit character-count field (versions 10-40) and byte mode. The
//! six presets below use V15/V25/V30/V40 which the `qrcode` crate must match
//! exactly; `presets::tests::capacities_match_qrcode_crate` enforces this.

use crate::frame::OPTICAL_FRAME_OVERHEAD;
use crate::transfer::RAPTORQ_PAYLOAD_ID_BYTES;

/// QR error correction level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ecc {
    L,
    M,
    Q,
    H,
}

/// A named transfer profile: QR geometry, symbol size, and playback rate.
#[derive(Clone, Copy, Debug)]
pub struct TransferPreset {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub version: u8,
    pub ecc: Ecc,
    /// Total stream rate in symbols per second (single lane) or symbols per
    /// second across both lanes (dual).
    pub fps: u16,
    /// 1 for single-lane modes, 2 for alternating dual-lane modes.
    pub lanes: u8,
    pub repair_percent: u32,
    /// Module pixel scale used when rendering.
    pub render_scale: u8,
    /// Maximum QR payload bytes for this version/ECC (byte mode).
    pub qr_capacity: u16,
    /// RaptorQ transport payload size per frame (QR capacity minus frame
    /// header and CRC).
    pub symbol_size: u16,
    /// Data bytes carried per frame (transport payload minus the 4-byte
    /// RaptorQ payload id).
    pub useful_bytes_per_frame: u16,
}

#[allow(clippy::too_many_arguments)]
const fn preset(
    key: &'static str,
    label: &'static str,
    description: &'static str,
    version: u8,
    ecc: Ecc,
    fps: u16,
    lanes: u8,
    repair_percent: u32,
    render_scale: u8,
    qr_capacity: u16,
) -> TransferPreset {
    TransferPreset {
        key,
        label,
        description,
        version,
        ecc,
        fps,
        lanes,
        repair_percent,
        render_scale,
        qr_capacity,
        symbol_size: qr_capacity - OPTICAL_FRAME_OVERHEAD as u16,
        useful_bytes_per_frame: qr_capacity
            - OPTICAL_FRAME_OVERHEAD as u16
            - RAPTORQ_PAYLOAD_ID_BYTES as u16,
    }
}

/// The six channel presets, in the same order as the browser UI.
pub const TRANSFER_PRESETS: &[TransferPreset] = &[
    preset(
        "robust",
        "Robust",
        "V15-M · larger camera modules",
        15,
        Ecc::M,
        7,
        1,
        35,
        7,
        412,
    ),
    preset(
        "balanced",
        "Balanced",
        "V25-M · speed with 15% QR repair",
        25,
        Ecc::M,
        10,
        1,
        30,
        6,
        997,
    ),
    preset(
        "turbo",
        "Turbo 15",
        "V30-L · four refreshes per symbol on 60 Hz",
        30,
        Ecc::L,
        15,
        1,
        25,
        6,
        1732,
    ),
    preset(
        "turbo30",
        "Turbo 30",
        "V30-L · two refreshes per symbol on 60 Hz",
        30,
        Ecc::L,
        30,
        1,
        30,
        5,
        1732,
    ),
    preset(
        "turbo60",
        "Turbo 60 · dual",
        "2× V30-L · each lane remains stable at 30 fps",
        30,
        Ecc::L,
        60,
        2,
        35,
        5,
        1732,
    ),
    preset(
        "megabit",
        "1 Mbps · dual lab",
        "2× V40-L · 30 fps per lane · close range",
        40,
        Ecc::L,
        60,
        2,
        35,
        5,
        2953,
    ),
];

/// Look up a preset by key (`robust`, `balanced`, `turbo`, `turbo30`,
/// `turbo60`, `megabit`).
pub fn get_preset(key: &str) -> Option<&'static TransferPreset> {
    TRANSFER_PRESETS.iter().find(|preset| preset.key == key)
}

/// Nominal optical data rate for a preset, in bytes per second.
pub fn nominal_rate(preset: &TransferPreset) -> u32 {
    u32::from(preset.useful_bytes_per_frame) * u32::from(preset.fps)
}

#[cfg(test)]
mod tests {
    use super::{get_preset, nominal_rate, TRANSFER_PRESETS};
    use crate::frame::OPTICAL_FRAME_OVERHEAD;
    use crate::transfer::RAPTORQ_PAYLOAD_ID_BYTES;

    #[test]
    fn sizes_follow_the_wire_formulas() {
        for preset in TRANSFER_PRESETS {
            assert_eq!(
                preset.symbol_size,
                preset.qr_capacity - OPTICAL_FRAME_OVERHEAD as u16,
                "{}",
                preset.key
            );
            assert_eq!(
                preset.useful_bytes_per_frame,
                preset.qr_capacity
                    - OPTICAL_FRAME_OVERHEAD as u16
                    - RAPTORQ_PAYLOAD_ID_BYTES as u16,
                "{}",
                preset.key
            );
        }
    }

    #[test]
    fn capacities_match_qrcode_crate() {
        // The renderer must accept exactly the JS-derived capacity and reject
        // one byte more, for every preset, even with incompressible random
        // payloads (the browser encoder's single-segment byte mode).
        for preset in TRANSFER_PRESETS {
            let fits = crate::qrencode::render_frame(
                &random_bytes(preset.qr_capacity as usize),
                preset.version,
                preset.ecc,
                2,
            );
            let overflows = crate::qrencode::render_frame(
                &random_bytes(preset.qr_capacity as usize + 1),
                preset.version,
                preset.ecc,
                2,
            );
            assert!(
                fits.is_ok(),
                "{}: renderer rejects the JS capacity {}",
                preset.key,
                preset.qr_capacity
            );
            assert!(
                overflows.is_err(),
                "{}: renderer accepts {} bytes but the JS table caps at {}",
                preset.key,
                preset.qr_capacity + 1,
                preset.qr_capacity
            );
        }
    }

    fn random_bytes(length: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9u32;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn preset_lookup_and_rates() {
        let turbo30 = get_preset("turbo30").expect("turbo30 preset");
        assert_eq!(turbo30.symbol_size, 1709);
        assert_eq!(turbo30.useful_bytes_per_frame, 1705);
        assert_eq!(nominal_rate(turbo30), 1705 * 30);
        let robust = get_preset("robust").unwrap();
        assert_eq!(robust.symbol_size, 412 - 23);
        assert_eq!(robust.useful_bytes_per_frame, 412 - 23 - 4);
        assert_eq!(get_preset("balanced").unwrap().symbol_size, 997 - 23);
        assert!(get_preset("nope").is_none());
        assert_eq!(get_preset("megabit").unwrap().qr_capacity, 2953);
    }
}
