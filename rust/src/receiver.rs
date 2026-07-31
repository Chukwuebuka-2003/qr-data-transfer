//! The receiver: camera or offline frames -> crop -> downscale -> ZXing-C++
//! decode -> QF4 parse -> RaptorQ recovery -> container and file verification.
//!
//! This is a native port of the browser `/scan` pipeline. The decode core is
//! camera-agnostic: any [`FrameSource`] (real camera, PNG sequence, test
//! harness) feeds [`Receiver::process_frame`] with RGB frames, and the
//! receiver returns the recovered file when the stream completes.
//!
//! Scanning mirrors the browser build: a centered crop (square for single
//! lane, 2:1 band for dual), nearest-neighbor downscale to an adaptive scan
//! width, a robust (tryHarder) decode attempt on every Nth failure, per-frame
//! CRC verification, deduplication by `session:payload-id`, and four layers of
//! checksum verification before the file is released.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zxingcpp::{BarcodeFormat, BarcodeReader, Binarizer, ImageFormat, ImageView, TextMode};

use crate::compression::decompress_transfer;
use crate::container::{parse_optical_container, OpticalFileMeta};
use crate::crc32::crc32;
use crate::error::{Error, Result};
use crate::frame::parse_frame;
use crate::transfer::{raptor_packet_key, RaptorQDecoder};

/// Window (in seconds) for rolling delivery/scan rate samples.
const RATE_WINDOW_SECS: f64 = 3.5;
/// Window (in seconds) for the optical throughput samples.
const THROUGHPUT_WINDOW_SECS: f64 = 4.0;

/// Receiver configuration.
#[derive(Clone, Copy, Debug)]
pub struct ReceiverOptions {
    /// Dual-lane mode: 2:1 crop and up to two symbols decoded per exposure.
    pub dual: bool,
    /// Every Nth consecutive decode failure uses the robust (tryHarder)
    /// path. 6 matches the browser build.
    pub robust_every: u64,
}

impl Default for ReceiverOptions {
    fn default() -> Self {
        ReceiverOptions {
            dual: false,
            robust_every: 6,
        }
    }
}

/// A decoded video frame in RGB (3 bytes per pixel, row-major).
#[derive(Clone, Debug)]
pub struct Frame {
    pub rgb: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Anything that can produce a sequence of video frames.
pub trait FrameSource {
    /// Return the next frame, or `None` when an offline source is exhausted.
    fn next_frame(&mut self) -> Result<Option<Frame>>;
}

/// A frame source that replays a PNG sequence (`frame_000001.png`, ...) as
/// produced by `qrferry-send --out`. Useful for offline scanning and tests.
pub struct PngDirSource {
    dir: PathBuf,
    next_index: u64,
}

impl PngDirSource {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        if !dir.is_dir() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("not a directory: {}", dir.display()),
            )));
        }
        Ok(PngDirSource { dir, next_index: 1 })
    }
}

impl FrameSource for PngDirSource {
    fn next_frame(&mut self) -> Result<Option<Frame>> {
        let path = self.dir.join(format!("frame_{:06}.png", self.next_index));
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read(&path)?;
        let decoder = png::Decoder::new(&data[..]);
        let mut reader = decoder.read_info()?;
        let mut buffer = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buffer)?;
        let (width, height) = (info.width as usize, info.height as usize);
        let channels = info.color_type.samples() as usize;
        let pixels = &buffer[..width * height * channels];
        let mut rgb = Vec::with_capacity(width * height * 3);
        if channels >= 4 {
            for pixel in pixels.chunks_exact(4) {
                rgb.extend_from_slice(&pixel[..3]);
            }
        } else {
            rgb.extend_from_slice(pixels);
        }
        self.next_index += 1;
        Ok(Some(Frame { rgb, width, height }))
    }
}

/// Live receiver telemetry.
#[derive(Default, Debug, Clone)]
pub struct ReceiverStats {
    /// Total QR codes decoded (including duplicates and rejects).
    pub qr_reads: u64,
    /// Unique symbols accepted into the RaptorQ decoder.
    pub accepted_frames: u64,
    /// Symbols already seen.
    pub duplicate_frames: u64,
    /// Frames that failed QF4 parsing or CRC.
    pub bad_frames: u64,
    /// Frames with no QR at all.
    pub missed_exposures: u64,
    /// Frames from the legacy QF2/QF3 protocol.
    pub legacy_frames: u64,
    /// Camera/offline frames processed per second (rolling).
    pub delivered_fps: f64,
    /// Decode attempts completed per second (rolling).
    pub scanner_fps: f64,
    /// Decoder latency percentiles in milliseconds (rolling).
    pub decode_p50_ms: f64,
    pub decode_p95_ms: f64,
    /// Unique optical payload bytes per second (rolling).
    pub optical_bytes_per_second: f64,
}

/// A fully verified, recovered file.
#[derive(Clone, Debug)]
pub struct RecoveredFile {
    pub meta: OpticalFileMeta,
    pub bytes: Vec<u8>,
}

impl RecoveredFile {
    /// Sanitized file name (never contains path separators).
    pub fn filename(&self) -> &str {
        Path::new(&self.meta.filename)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("transfer.bin")
    }

    /// Write the recovered file into `dir` and return its path.
    pub fn save(&self, dir: &Path) -> Result<PathBuf> {
        let path = dir.join(self.filename());
        std::fs::write(&path, &self.bytes)?;
        Ok(path)
    }
}

struct ReceiverSession {
    session: u32,
    container_length: u32,
    compressed: bool,
    symbol_size: u16,
    source_packet_count: u32,
    decoder: RaptorQDecoder,
    seen: HashSet<String>,
}

/// The scanning pipeline.
pub struct Receiver {
    options: ReceiverOptions,
    session: Option<ReceiverSession>,
    complete: bool,
    decode_failures: u64,
    reader_fast: BarcodeReader,
    reader_robust: BarcodeReader,
    // Accumulators.
    qr_reads: u64,
    accepted: u64,
    duplicates: u64,
    bad: u64,
    missed: u64,
    legacy: u64,
    frames_processed: u64,
    // Rolling samples.
    delivery_times: VecDeque<Instant>,
    scan_times: VecDeque<Instant>,
    decode_durations: VecDeque<(Duration, Instant)>,
    rate_samples: VecDeque<(Instant, u64)>,
    /// Human-readable state, mirroring the browser's guidance line.
    pub last_guidance: String,
}

fn build_reader(robust: bool) -> BarcodeReader {
    BarcodeReader::default()
        .formats([BarcodeFormat::QRCode])
        .try_harder(robust)
        .try_rotate(false)
        .try_invert(false)
        .try_downscale(false)
        .binarizer(if robust {
            Binarizer::LocalAverage
        } else {
            Binarizer::GlobalHistogram
        })
        .text_mode(TextMode::Plain)
}

impl Receiver {
    pub fn new(options: ReceiverOptions) -> Receiver {
        Receiver {
            options,
            session: None,
            complete: false,
            decode_failures: 0,
            reader_fast: build_reader(false),
            reader_robust: build_reader(true),
            qr_reads: 0,
            accepted: 0,
            duplicates: 0,
            bad: 0,
            missed: 0,
            legacy: 0,
            frames_processed: 0,
            delivery_times: VecDeque::new(),
            scan_times: VecDeque::new(),
            decode_durations: VecDeque::new(),
            rate_samples: VecDeque::new(),
            last_guidance: "Waiting for a QR signal. Center the code in the guide.".to_string(),
        }
    }

    /// The crop region (x, y, width, height) the scanner will use, for
    /// drawing the guide overlay.
    pub fn guide_region(&self, width: usize, height: usize) -> (usize, usize, usize, usize) {
        let (x, y, w, h) = self.crop_region(width, height);
        (x, y, w, h)
    }

    fn crop_region(&self, width: usize, height: usize) -> (usize, usize, usize, usize) {
        let available_w = (width as f64 * 0.96) as usize;
        let available_h = (height as f64 * 0.96) as usize;
        let dual = self.options.dual;
        let source_w = if dual {
            available_w.min(available_h * 2)
        } else {
            available_w.min(available_h)
        };
        let source_h = if dual { source_w / 2 } else { source_w };
        let source_x = (width - source_w) / 2;
        let source_y = (height - source_h) / 2;
        (source_x, source_y, source_w, source_h)
    }

    /// True once a complete, verified file has been recovered.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// True once at least one valid frame has established a session.
    pub fn is_locked(&self) -> bool {
        self.session.is_some()
    }

    /// Recovery progress, capped at 0.99 until the file is verified.
    pub fn progress(&self) -> f32 {
        if self.complete {
            return 1.0;
        }
        match &self.session {
            Some(session) => {
                let expected = session.source_packet_count as f32 + 2.0;
                (self.accepted as f32 / expected).min(0.99)
            }
            None => 0.0,
        }
    }

    /// True when the current decode attempt should use the robust path
    /// (every Nth consecutive failure, matching the browser build).
    fn is_robust_attempt(&self) -> bool {
        self.decode_failures
            .checked_rem(self.options.robust_every)
            .is_some_and(|rem| rem == self.options.robust_every - 1)
    }

    /// Process one RGB frame. Returns the recovered file when the transfer
    /// completes, `None` otherwise.
    pub fn process_frame(
        &mut self,
        rgb: &[u8],
        width: usize,
        height: usize,
    ) -> Result<Option<RecoveredFile>> {
        if self.complete {
            return Ok(None);
        }
        // Frames smaller than this cannot contain a scannable code.
        if width < 160 || height < 160 {
            self.missed += 1;
            return Ok(None);
        }

        self.frames_processed += 1;
        push_sample(&mut self.delivery_times, Instant::now());

        let (scan, scan_w, scan_h) = self.crop_and_downscale(rgb, width, height);
        let robust = self.is_robust_attempt();

        let decode_started = Instant::now();
        let barcodes = self.read(&scan, scan_w, scan_h, robust);
        self.decode_durations
            .push_back((decode_started.elapsed(), decode_started));
        self.trim_duration_window();
        push_sample(&mut self.scan_times, Instant::now());

        if barcodes.is_empty() {
            self.decode_failures += 1;
            self.missed += 1;
            return Ok(None);
        }
        self.decode_failures = 0;

        for bytes in barcodes {
            self.qr_reads += 1;
            if let Some(recovered) = self.accept_bytes(&bytes)? {
                self.complete = true;
                return Ok(Some(recovered));
            }
        }
        Ok(None)
    }

    fn read(&mut self, rgb: &[u8], width: usize, height: usize, robust: bool) -> Vec<Vec<u8>> {
        let view = match ImageView::from_slice(rgb, width, height, ImageFormat::RGB) {
            Ok(view) => view,
            Err(_) => return Vec::new(),
        };
        let reader = if robust {
            &mut self.reader_robust
        } else {
            &mut self.reader_fast
        };
        reader.set_max_number_of_symbols(if self.options.dual { 2 } else { 1 });
        match reader.from(&view) {
            Ok(barcodes) => barcodes
                .into_iter()
                .filter(|barcode| {
                    barcode.is_valid()
                        && barcode.format() == BarcodeFormat::QRCode
                        && !barcode.bytes().is_empty()
                })
                .map(|barcode| barcode.bytes())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn crop_and_downscale(
        &self,
        rgb: &[u8],
        width: usize,
        height: usize,
    ) -> (Vec<u8>, usize, usize) {
        let (source_x, source_y, source_w, source_h) = self.crop_region(width, height);
        let high_density = self
            .session
            .as_ref()
            .is_some_and(|session| session.symbol_size > 2200);
        let robust = self.is_robust_attempt();

        let dual = self.options.dual;
        let scan_w = if dual {
            if high_density {
                1800
            } else if robust {
                1680
            } else {
                1440
            }
        } else if high_density {
            1280
        } else if robust {
            1120
        } else {
            960
        }
        .min(source_w);
        let scan_h = if dual { scan_w / 2 } else { scan_w };

        let mut out = vec![0u8; scan_w * scan_h * 3];
        for y in 0..scan_h {
            let sy = source_y + (y * source_h) / scan_h;
            for x in 0..scan_w {
                let sx = source_x + (x * source_w) / scan_w;
                let si = (sy * width + sx) * 3;
                let di = (y * scan_w + x) * 3;
                out[di] = rgb[si];
                out[di + 1] = rgb[si + 1];
                out[di + 2] = rgb[si + 2];
            }
        }
        (out, scan_w, scan_h)
    }

    fn accept_bytes(&mut self, bytes: &[u8]) -> Result<Option<RecoveredFile>> {
        let frame = match parse_frame(bytes) {
            Ok(frame) => frame,
            Err(_) => {
                let prefix = &bytes[..bytes.len().min(8)];
                if prefix.starts_with(b"QF2") || prefix.starts_with(b"QF3") {
                    self.legacy += 1;
                    self.last_guidance =
                        "An older QRFerry sender is visible. Reload the sending screen."
                            .to_string();
                } else {
                    self.bad += 1;
                    self.last_guidance =
                        "A frame was incomplete. Hold steady; the next one can replace it."
                            .to_string();
                }
                return Ok(None);
            }
        };

        if self.session.is_none() {
            let decoder = RaptorQDecoder::new(frame.container_length, frame.symbol_size)?;
            self.session = Some(ReceiverSession {
                session: frame.session,
                container_length: frame.container_length,
                compressed: frame.compressed,
                symbol_size: frame.symbol_size,
                source_packet_count: u64::from(frame.container_length)
                    .div_ceil(u64::from(frame.symbol_size - 4))
                    .max(1) as u32,
                decoder,
                seen: HashSet::new(),
            });
            self.last_guidance = "RaptorQ locked. Keep the full white margin visible.".to_string();
        }

        // Decide the outcome under the session borrow, then apply the side
        // effects once the borrow has ended.
        let mut mismatched = false;
        let mut duplicate = false;
        let mut accepted_packet = false;
        let mut completed: Option<(Vec<u8>, u32, u32, bool)> = None;

        {
            let session = self.session.as_mut().expect("session just created");
            if frame.session != session.session
                || frame.container_length != session.container_length
                || frame.symbol_size != session.symbol_size
            {
                mismatched = true;
            } else {
                let key = raptor_packet_key(&frame);
                if session.seen.insert(key) {
                    let (container_length, session_id, compressed) = (
                        session.container_length,
                        session.session,
                        session.compressed,
                    );
                    match session.decoder.push(&frame.payload) {
                        Ok(Some(container)) => {
                            completed = Some((container, container_length, session_id, compressed))
                        }
                        Ok(None) => accepted_packet = true,
                        Err(error) => return Err(error),
                    }
                } else {
                    duplicate = true;
                }
            }
        }

        if mismatched {
            self.last_guidance =
                "A different transfer crossed the camera view. Stay on one sender.".to_string();
            return Ok(None);
        }
        if duplicate {
            self.duplicates += 1;
            self.last_guidance = "Signal locked. Waiting for a new RaptorQ symbol.".to_string();
            return Ok(None);
        }
        if accepted_packet {
            self.accepted += 1;
            self.rate_samples
                .push_back((Instant::now(), u64::from(frame.symbol_size - 4)));
            self.trim_rate_window();
            self.last_guidance = if frame.symbol_size > 2200 {
                "High-density stream locked. Keep the phone close, square, and still.".to_string()
            } else {
                "Signal locked. RaptorQ is absorbing dropped exposures.".to_string()
            };
        }
        if let Some((container, container_length, session_id, compressed)) = completed {
            return self.finish(container, container_length, session_id, compressed);
        }
        Ok(None)
    }

    fn finish(
        &mut self,
        container: Vec<u8>,
        container_length: u32,
        session: u32,
        compressed: bool,
    ) -> Result<Option<RecoveredFile>> {
        self.last_guidance = if compressed {
            "Optical transfer complete. Decompressing and verifying...".to_string()
        } else {
            "Optical transfer complete. Verifying the file...".to_string()
        };
        if container.len() as u32 != container_length || crc32(&container) != session {
            return Err(Error::InvalidContainer(
                "The recovered RaptorQ object failed its checksum.",
            ));
        }
        let (meta, transmitted) = parse_optical_container(&container)?;
        let bytes = decompress_transfer(&transmitted, meta.compression)?;
        if bytes.len() as u32 != meta.file_size || crc32(&bytes) != meta.file_crc {
            return Err(Error::InvalidContainer(
                "The recovered file checksum did not match.",
            ));
        }
        self.last_guidance =
            "File decompressed and checksum verified. It is safe to save.".to_string();
        Ok(Some(RecoveredFile { meta, bytes }))
    }

    /// Current telemetry.
    pub fn stats(&self) -> ReceiverStats {
        let now = Instant::now();
        ReceiverStats {
            qr_reads: self.qr_reads,
            accepted_frames: self.accepted,
            duplicate_frames: self.duplicates,
            bad_frames: self.bad,
            missed_exposures: self.missed,
            legacy_frames: self.legacy,
            delivered_fps: rolling_fps(&self.delivery_times, now),
            scanner_fps: rolling_fps(&self.scan_times, now),
            decode_p50_ms: percentile_ms(&self.decode_durations, 0.50),
            decode_p95_ms: percentile_ms(&self.decode_durations, 0.95),
            optical_bytes_per_second: rolling_rate(&self.rate_samples, now),
        }
    }

    fn trim_rate_window(&mut self) {
        let now = Instant::now();
        while self.rate_samples.len() > 1
            && now - self.rate_samples[0].0 > Duration::from_secs_f64(THROUGHPUT_WINDOW_SECS)
        {
            self.rate_samples.pop_front();
        }
    }

    fn trim_duration_window(&mut self) {
        let now = Instant::now();
        while self.decode_durations.len() > 1
            && now - self.decode_durations[0].1 > Duration::from_secs_f64(RATE_WINDOW_SECS)
        {
            self.decode_durations.pop_front();
        }
    }
}

fn push_sample(samples: &mut VecDeque<Instant>, now: Instant) {
    samples.push_back(now);
    while samples.len() > 2 && now - samples[0] > Duration::from_secs_f64(RATE_WINDOW_SECS) {
        samples.pop_front();
    }
}

fn rolling_fps(samples: &VecDeque<Instant>, now: Instant) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    let elapsed = (now - samples[0]).as_secs_f64();
    if elapsed <= 0.0 {
        return 0.0;
    }
    (samples.len() - 1) as f64 / elapsed
}

fn percentile_ms(samples: &VecDeque<(Duration, Instant)>, fraction: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut values: Vec<f64> = samples
        .iter()
        .map(|(duration, _)| duration.as_secs_f64() * 1000.0)
        .collect();
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * fraction).floor() as usize;
    values[index]
}

fn rolling_rate(samples: &VecDeque<(Instant, u64)>, now: Instant) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let elapsed = (now - samples[0].0).as_secs_f64().max(0.75);
    let total: u64 = samples.iter().map(|(_, bytes)| bytes).sum();
    total as f64 * 1000.0 / elapsed
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Frame, FrameSource, PngDirSource, Receiver, ReceiverOptions, RecoveredFile};
    use crate::qrencode::QrImage;
    use crate::sender::Sender;

    fn frame_from_image(image: &QrImage) -> Frame {
        let rgb = image
            .rgba
            .chunks_exact(4)
            .flat_map(|pixel| pixel[..3].to_vec())
            .collect();
        Frame {
            rgb,
            width: image.width,
            height: image.height,
        }
    }

    /// Render every packet of a transfer once, as RGB frames.
    fn render_frames(file: &[u8], preset_key: &str) -> (Sender, Vec<Frame>) {
        let mut sender =
            Sender::prepare(file, "payload.bin", "application/octet-stream", preset_key).unwrap();
        let frames: Vec<Frame> = (0..sender.cycle_len())
            .map(|_| frame_from_image(&sender.next_frame().unwrap()))
            .collect();
        (sender, frames)
    }

    /// Compose two lane images side by side (dual-lane sender layout).
    fn compose_dual(left: &Frame, right: &Frame) -> Frame {
        let width = left.width + right.width;
        let height = left.height.max(right.height);
        let mut rgb = vec![255u8; width * height * 3];
        for (frame, offset) in [(left, 0usize), (right, left.width)] {
            for y in 0..frame.height {
                let src = &frame.rgb[y * frame.width * 3..(y + 1) * frame.width * 3];
                let dst = (y * width + offset) * 3;
                rgb[dst..dst + src.len()].copy_from_slice(src);
            }
        }
        Frame { rgb, width, height }
    }

    /// Center a frame on a larger white canvas, emulating a QR code shown on
    /// a sender screen with margins (the receiver's 2% crop must not touch
    /// the code itself).
    fn pad_with_margin(frame: &Frame, factor: f64) -> Frame {
        let width = (frame.width as f64 * factor) as usize;
        let height = (frame.height as f64 * factor) as usize;
        let offset_x = (width - frame.width) / 2;
        let offset_y = (height - frame.height) / 2;
        let mut rgb = vec![255u8; width * height * 3];
        for y in 0..frame.height {
            let src = &frame.rgb[y * frame.width * 3..(y + 1) * frame.width * 3];
            let dst = ((offset_y + y) * width + offset_x) * 3;
            rgb[dst..dst + src.len()].copy_from_slice(src);
        }
        Frame { rgb, width, height }
    }

    fn feed_all(receiver: &mut Receiver, frames: &[Frame]) -> Option<RecoveredFile> {
        for frame in frames {
            if let Some(file) = receiver
                .process_frame(&frame.rgb, frame.width, frame.height)
                .unwrap()
            {
                return Some(file);
            }
        }
        None
    }

    fn random_file(length: usize) -> Vec<u8> {
        let mut state = 0xdead_beefu32;
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
    fn full_recovery_single_lane() {
        let file = random_file(120_000);
        let (_sender, frames) = render_frames(&file, "robust");
        let mut receiver = Receiver::new(ReceiverOptions::default());
        let recovered = feed_all(&mut receiver, &frames).expect("recovery failed");
        assert_eq!(recovered.bytes, file);
        assert_eq!(recovered.meta.filename, "payload.bin");
        assert!(receiver.is_complete());
        assert_eq!(receiver.progress(), 1.0);
        let stats = receiver.stats();
        assert!(stats.accepted_frames >= 1);
        assert_eq!(stats.bad_frames, 0);
    }

    #[test]
    fn full_recovery_dual_lane() {
        let file = random_file(120_000);
        let (_sender, frames) = render_frames(&file, "turbo60");
        // Faithful dual-lane emulation: each tick updates ONE lane with the
        // next packet while the other lane keeps its previous code, matching
        // the browser sender's alternating display. Frames are padded with a
        // white margin like a real sender screen.
        let mut lanes: [Option<Frame>; 2] = [None, None];
        let composed: Vec<Frame> = frames
            .iter()
            .enumerate()
            .map(|(tick, frame)| {
                let lane = tick % 2;
                lanes[lane] = Some(frame.clone());
                let left = lanes[0].clone().unwrap_or_else(|| frame.clone());
                let right = lanes[1].clone().unwrap_or_else(|| frame.clone());
                pad_with_margin(&compose_dual(&left, &right), 1.2)
            })
            .collect();
        let mut receiver = Receiver::new(ReceiverOptions {
            dual: true,
            ..Default::default()
        });
        let recovered = feed_all(&mut receiver, &composed).expect("dual recovery failed");
        assert_eq!(recovered.bytes, file);
        assert!(receiver.is_complete());
    }

    #[test]
    fn recovers_with_erasures() {
        let file = random_file(120_000);
        let (_sender, frames) = render_frames(&file, "balanced");
        // Drop every 6th frame: ~83% delivered, comfortably above the
        // ~124 source symbols required.
        let kept: Vec<Frame> = frames
            .iter()
            .enumerate()
            .filter(|(index, _)| index % 6 != 0)
            .map(|(_, frame)| frame.clone())
            .collect();
        assert!(kept.len() > frames.len() * 4 / 5);
        let mut receiver = Receiver::new(ReceiverOptions::default());
        let recovered = feed_all(&mut receiver, &kept).expect("erasure recovery failed");
        assert_eq!(recovered.bytes, file);
    }

    #[test]
    fn recovers_out_of_order_and_mid_stream() {
        let file = random_file(120_000);
        let (_sender, frames) = render_frames(&file, "balanced");
        // Join after the first 10% and reverse the delivery order.
        let mut tail: Vec<Frame> = frames[frames.len() / 10..].to_vec();
        tail.reverse();
        let mut receiver = Receiver::new(ReceiverOptions::default());
        let recovered = feed_all(&mut receiver, &tail).expect("out-of-order recovery failed");
        assert_eq!(recovered.bytes, file);
    }

    #[test]
    fn foreign_transfer_is_rejected_and_ignored() {
        let file_a = random_file(120_000);
        let file_b = random_file(80_000);
        let (_sender_a, frames_a) = render_frames(&file_a, "balanced");
        let (_sender_b, frames_b) = render_frames(&file_b, "balanced");
        let mut receiver = Receiver::new(ReceiverOptions::default());

        // Lock onto transfer A.
        feed_all(&mut receiver, &frames_a[..5]);
        assert!(receiver.is_locked());
        let accepted_before = receiver.stats().accepted_frames;

        // Interleave the rest of A with foreign frames; foreign frames must
        // be skipped without being accepted.
        let mut mixed = Vec::new();
        let mut a_index = 5usize;
        for foreign in &frames_b {
            for a in frames_a[a_index..].iter().take(2) {
                mixed.push(a.clone());
            }
            a_index = (a_index + 2).min(frames_a.len());
            mixed.push(foreign.clone());
        }
        let recovered = feed_all(&mut receiver, &mixed).expect("recovery failed");
        assert_eq!(recovered.bytes, file_a);
        let stats = receiver.stats();
        assert!(stats.accepted_frames > accepted_before);
        assert!(stats.accepted_frames <= frames_a.len() as u64);
    }

    #[test]
    fn garbage_frames_are_counted_not_crashed() {
        let mut receiver = Receiver::new(ReceiverOptions::default());
        let mut state = 0x1234_5678u32;
        for _ in 0..10 {
            let rgb: Vec<u8> = (0..640 * 480 * 3)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    (state >> 24) as u8
                })
                .collect();
            let recovered = receiver.process_frame(&rgb, 640, 480).unwrap();
            assert!(recovered.is_none());
        }
        let stats = receiver.stats();
        assert!(stats.bad_frames > 0 || stats.missed_exposures > 0);
        assert!(!receiver.is_locked());
    }

    #[test]
    fn legacy_prefix_detected() {
        // A real QR whose payload starts with the QF3 magic must be
        // recognized as a legacy sender and skipped without locking.
        let legacy_payload: Vec<u8> = b"QF3:"
            .iter()
            .chain(std::iter::repeat(&0x42u8))
            .take(60)
            .cloned()
            .collect();
        let image =
            crate::qrencode::render_frame(&legacy_payload, 5, crate::presets::Ecc::M, 6).unwrap();
        let frame = frame_from_image(&image);
        let mut receiver = Receiver::new(ReceiverOptions::default());
        let recovered = receiver
            .process_frame(&frame.rgb, frame.width, frame.height)
            .unwrap();
        assert!(recovered.is_none());
        let stats = receiver.stats();
        assert_eq!(stats.legacy_frames, 1);
        assert!(!receiver.is_locked());
    }

    #[test]
    fn png_dir_source_roundtrip() {
        let file = random_file(60_000);
        let (_sender, frames) = render_frames(&file, "robust");
        let dir = std::env::temp_dir().join(format!("qrferry_png_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (index, frame) in frames.iter().enumerate() {
            let path = dir.join(format!("frame_{:06}.png", index + 1));
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, frame.width as u32, frame.height as u32);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&frame.rgb).unwrap();
        }

        let mut source = PngDirSource::open(&dir).unwrap();
        let mut receiver = Receiver::new(ReceiverOptions::default());
        let mut recovered = None;
        while let Some(frame) = source.next_frame().unwrap() {
            if let Some(file) = receiver
                .process_frame(&frame.rgb, frame.width, frame.height)
                .unwrap()
            {
                recovered = Some(file);
                break;
            }
        }
        let recovered = recovered.expect("PNG sequence did not recover the file");
        assert_eq!(recovered.bytes, file);
        // The source is exhausted once the remaining frames are drained
        // (recovery usually completes before the sequence ends).
        let mut drained = 0u32;
        while source.next_frame().unwrap().is_some() {
            drained += 1;
            assert!(drained < 10_000, "PNG source never terminates");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn progress_tracks_session() {
        let file = random_file(120_000);
        let (_sender, frames) = render_frames(&file, "robust");
        let mut receiver = Receiver::new(ReceiverOptions::default());
        assert_eq!(receiver.progress(), 0.0);
        feed_all(&mut receiver, &frames[..3]);
        assert!(receiver.is_locked());
        let progress = receiver.progress();
        assert!(progress > 0.0 && progress < 1.0);
        let _ = PathBuf::new();
    }
}
