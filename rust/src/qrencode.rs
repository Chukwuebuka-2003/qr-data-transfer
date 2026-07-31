//! QR rendering for the sender.
//!
//! Renders a QF4 frame into an RGBA image with the standard 4-module quiet
//! zone. Rendering is performed by the `qrcode` crate in raw byte mode, which
//! matches the capacity table used by the browser build (see `presets`).

use qrcode::{Color, EcLevel as QrEcLevel, QrCode, Version};

use crate::error::{Error, Result};
use crate::presets::Ecc;

/// Quiet zone width in modules, per the QR specification.
pub const QUIET_ZONE_MODULES: usize = 4;

/// A rendered QR frame.
#[derive(Clone, Debug)]
pub struct QrImage {
    pub width: usize,
    pub height: usize,
    /// RGBA8, row-major, quiet zone included.
    pub rgba: Vec<u8>,
}

impl QrImage {
    /// Pack pixels for `minifb` (0x00RRGGBB).
    pub fn to_rgb32(&self) -> Vec<u32> {
        self.rgba
            .chunks_exact(4)
            .map(|p| (u32::from(p[0]) << 16) | (u32::from(p[1]) << 8) | u32::from(p[2]))
            .collect()
    }

    /// Pack pixels for the `rxing` decoder (0x00RRGGBB in u32).
    pub fn to_rxing_pixels(&self) -> Vec<u32> {
        self.to_rgb32()
    }
}

fn qr_ec_level(ecc: Ecc) -> QrEcLevel {
    match ecc {
        Ecc::L => QrEcLevel::L,
        Ecc::M => QrEcLevel::M,
        Ecc::Q => QrEcLevel::Q,
        Ecc::H => QrEcLevel::H,
    }
}

/// Render one QF4 frame as a QR code image.
///
/// The bits are built by hand with a single byte-mode segment (mode indicator
/// plus 16-bit character count plus raw bytes) and no ECI header, matching the
/// browser encoder exactly. The `with_version` path is deliberately avoided:
/// its content-dependent segment optimizer can add up to ~40 bits of overhead
/// for adversarial byte runs, which would exceed the exact-capacity boundary
/// that the preset table is built against.
pub fn render_frame(frame: &[u8], version: u8, ecc: Ecc, scale: u32) -> Result<QrImage> {
    use qrcode::bits::Bits;

    let mut bits = Bits::new(Version::Normal(version as i16));
    bits.push_byte_data(frame)
        .map_err(|error| Error::QrEncode(error.to_string()))?;
    bits.push_terminator(qr_ec_level(ecc))
        .map_err(|error| Error::QrEncode(error.to_string()))?;
    let code = QrCode::with_bits(bits, qr_ec_level(ecc))
        .map_err(|error| Error::QrEncode(error.to_string()))?;
    let modules = code.width();
    let colors = code.into_colors();

    let total_modules = modules + 2 * QUIET_ZONE_MODULES;
    let dim = total_modules * scale as usize;
    let mut rgba = vec![255u8; dim * dim * 4];
    let scale = scale as usize;

    for y in 0..modules {
        for x in 0..modules {
            if colors[y * modules + x] == Color::Dark {
                let px = (x + QUIET_ZONE_MODULES) * scale;
                let py = (y + QUIET_ZONE_MODULES) * scale;
                for dy in 0..scale {
                    let row = (py + dy) * dim * 4;
                    for dx in 0..scale {
                        let i = row + (px + dx) * 4;
                        rgba[i] = 0;
                        rgba[i + 1] = 0;
                        rgba[i + 2] = 0;
                    }
                }
            }
        }
    }

    Ok(QrImage {
        width: dim,
        height: dim,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use rxing::common::HybridBinarizer;
    use rxing::{BinaryBitmap, MultiFormatReader, RGBLuminanceSource, Reader};

    use super::{render_frame, QUIET_ZONE_MODULES};
    use crate::frame::{parse_frame, serialize_frame};
    use crate::presets::Ecc;

    fn sample_frame() -> Vec<u8> {
        let payload: Vec<u8> = (0..64).map(|i| (i * 7 % 251) as u8).collect();
        serialize_frame(&payload, 0x1234_5678, 10_000, 9_999, false, 64).unwrap()
    }

    fn decode_rgba(rgba: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
        let pixels: Vec<u32> = rgba
            .chunks_exact(4)
            .map(|p| (u32::from(p[0]) << 16) | (u32::from(p[1]) << 8) | u32::from(p[2]))
            .collect();
        let source =
            RGBLuminanceSource::new_with_width_height_pixels(width, height, &pixels).ok()?;
        let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
        let result = MultiFormatReader::default().decode(&mut bitmap).ok()?;
        Some(result.getRawBytes().to_vec())
    }

    #[test]
    fn rendered_frame_decodes_back_to_payload() {
        let frame = sample_frame();
        for (version, ecc, scale) in [(15, Ecc::M, 7), (30, Ecc::L, 5), (40, Ecc::L, 5)] {
            let image = render_frame(&frame, version, ecc, scale).unwrap();
            let modules = 4 * version as usize + 17;
            assert_eq!(
                image.width,
                (modules + 2 * QUIET_ZONE_MODULES) * scale as usize
            );
            let bytes = decode_rgba(&image.rgba, image.width, image.height)
                .unwrap_or_else(|| panic!("V{}-{:?} did not decode", version, ecc));
            assert_eq!(bytes, frame, "V{}-{:?} payload mismatch", version, ecc);
        }
    }

    #[test]
    fn rendered_frame_parses_as_qf4() {
        let frame = sample_frame();
        let image = render_frame(&frame, 15, Ecc::M, 7).unwrap();
        let bytes = decode_rgba(&image.rgba, image.width, image.height).unwrap();
        let parsed = parse_frame(&bytes).unwrap();
        assert_eq!(parsed.session, 0x1234_5678);
        assert_eq!(parsed.container_length, 10_000);
    }
}
