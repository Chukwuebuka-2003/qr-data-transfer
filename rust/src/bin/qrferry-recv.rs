//! qrferry-recv — receive a file from an animated QR stream.
//!
//! Opens the camera (or replays a PNG sequence) and runs the native scanning
//! pipeline: crop, downscale, ZXing-C++ decode, QF4 parse, RaptorQ recovery,
//! and full checksum verification. The recovered file is saved to `--out`.
//!
//! ```text
//! qrferry-recv [--out <dir>] [--dual] [--source <dir>] [--device <n>]
//!              [--width <n>] [--height <n>] [--fps <n>] [--no-window]
//!              [--max-frames <n>] [--help]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use qrferry::{format_bytes, Frame, FrameSource, Receiver, ReceiverOptions, RecoveredFile};

const USAGE: &str = "\
Usage: qrferry-recv [options]

Options:
  --out <dir>       save the recovered file here (default: current directory)
  --dual            dual-lane mode (2:1 crop, up to two symbols per exposure)
  --source <dir>    replay a PNG frame sequence (qrferry-send --out output)
                    instead of using the camera
  --device <n>      camera index (default 0)
  --width <n>       camera width request (default 1280)
  --height <n>      camera height request (default 720)
  --fps <n>         camera frame rate request (default 30)
  --no-window       run without a preview window
  --max-frames <n>  stop after n frames (offline sources)
  --help            show this help";

struct Options {
    out: PathBuf,
    dual: bool,
    source: Option<PathBuf>,
    device: u32,
    width: u32,
    height: u32,
    fps: u32,
    no_window: bool,
    max_frames: Option<u64>,
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut out = PathBuf::from(".");
    let mut dual = false;
    let mut source = None;
    let mut device = 0u32;
    let mut width = 1280u32;
    let mut height = 720u32;
    let mut fps = 30u32;
    let mut no_window = false;
    let mut max_frames = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", USAGE);
                std::process::exit(0);
            }
            "--out" => out = PathBuf::from(args.next().ok_or("--out requires a value")?),
            "--dual" => dual = true,
            "--source" => {
                source = Some(PathBuf::from(
                    args.next().ok_or("--source requires a value")?,
                ))
            }
            "--device" => {
                device = args
                    .next()
                    .ok_or("--device requires a value")?
                    .parse()
                    .map_err(|_| "invalid --device value")?
            }
            "--width" => {
                width = args
                    .next()
                    .ok_or("--width requires a value")?
                    .parse()
                    .map_err(|_| "invalid --width value")?
            }
            "--height" => {
                height = args
                    .next()
                    .ok_or("--height requires a value")?
                    .parse()
                    .map_err(|_| "invalid --height value")?
            }
            "--fps" => {
                fps = args
                    .next()
                    .ok_or("--fps requires a value")?
                    .parse()
                    .map_err(|_| "invalid --fps value")?
            }
            "--no-window" => no_window = true,
            "--max-frames" => {
                max_frames = Some(
                    args.next()
                        .ok_or("--max-frames requires a value")?
                        .parse()
                        .map_err(|_| "invalid --max-frames value")?,
                )
            }
            other if other.starts_with('-') => return Err(format!("unknown option: {}", other)),
            other => return Err(format!("unexpected argument: {}", other)),
        }
    }

    if source.is_some() && (device != 0 || width != 1280 || height != 720 || fps != 30) {
        // Camera options are ignored with --source; allow them silently.
        let _ = (device, width, height, fps);
    }

    Ok(Options {
        out,
        dual,
        source,
        device,
        width,
        height,
        fps,
        no_window,
        max_frames,
    })
}

/// Camera-backed frame source (nokhwa).
struct CameraSource {
    camera: nokhwa::Camera,
}

impl CameraSource {
    fn open(device: u32, width: u32, height: u32, fps: u32) -> Result<Self, String> {
        use nokhwa::utils::{
            CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
        };
        let format =
            RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(RequestedFormatType::Closest(
                CameraFormat::new_from(width, height, FrameFormat::MJPEG, fps),
            ));
        let mut camera = nokhwa::Camera::new(CameraIndex::Index(device), format)
            .map_err(|error| format!("cannot open camera {}: {}", device, error))?;
        camera
            .open_stream()
            .map_err(|error| format!("cannot start camera stream: {}", error))?;
        Ok(CameraSource { camera })
    }
}

impl FrameSource for CameraSource {
    fn next_frame(&mut self) -> qrferry::Result<Option<Frame>> {
        let buffer = self
            .camera
            .frame()
            .map_err(|error| qrferry::Error::Capture(error.to_string()))?;
        let image = buffer
            .decode_image::<nokhwa::pixel_format::RgbFormat>()
            .map_err(|error| qrferry::Error::Capture(error.to_string()))?;
        let (width, height) = (image.width() as usize, image.height() as usize);
        Ok(Some(Frame {
            rgb: image.into_raw(),
            width,
            height,
        }))
    }
}

fn draw_guide(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    region: (usize, usize, usize, usize),
) {
    let (x, y, w, h) = region;
    let arm = (w / 8).max(8);
    let color = 0x00ff_0000u32; // red
    let draw = |buffer: &mut [u32], px: usize, py: usize| {
        if px < width && py < height {
            buffer[py * width + px] = color;
        }
    };
    for i in 0..arm {
        draw(buffer, x + i, y); // top-left horizontal
        draw(buffer, x, y + i); // top-left vertical
        draw(buffer, x + w - 1 - i, y); // top-right horizontal
        draw(buffer, x + w - 1, y + i); // top-right vertical
        draw(buffer, x + i, y + h - 1); // bottom-left horizontal
        draw(buffer, x, y + h - 1 - i); // bottom-left vertical
        draw(buffer, x + w - 1 - i, y + h - 1); // bottom-right horizontal
        draw(buffer, x + w - 1, y + h - 1 - i); // bottom-right vertical
    }
}

fn report(receiver: &Receiver, frames: u64) {
    let stats = receiver.stats();
    println!(
        "frames={} delivered={:.0}fps scanner={:.0}fps decode p50/p95={:.1}/{:.1}ms accepted={} progress={:.0}% rate={}/s | {}",
        frames,
        stats.delivered_fps,
        stats.scanner_fps,
        stats.decode_p50_ms,
        stats.decode_p95_ms,
        stats.accepted_frames,
        receiver.progress() * 100.0,
        format_bytes(stats.optical_bytes_per_second as u64),
        receiver.last_guidance,
    );
}

fn run() -> Result<(), String> {
    let options = parse_args()?;
    std::fs::create_dir_all(&options.out)
        .map_err(|error| format!("cannot create {}: {}", options.out.display(), error))?;

    let receiver_options = ReceiverOptions {
        dual: options.dual,
        ..Default::default()
    };
    let mut receiver = Receiver::new(receiver_options);

    let mut source: Box<dyn FrameSource> = match &options.source {
        Some(dir) => Box::new(
            qrferry::PngDirSource::open(dir)
                .map_err(|error| format!("cannot open source: {}", error))?,
        ),
        None => Box::new(CameraSource::open(
            options.device,
            options.width,
            options.height,
            options.fps,
        )?),
    };

    println!(
        "QRFerry receiver ({}) — scanning for a QR stream...",
        if options.dual {
            "dual lane"
        } else {
            "single lane"
        }
    );
    if options.source.is_none() && !options.no_window {
        println!("Point the camera at the sender's screen. Esc or close to quit.");
    }

    // Preview window.
    let mut window: Option<minifb::Window> = None;
    if !options.no_window {
        match minifb::Window::new(
            "QRFerry receiver",
            960,
            540,
            minifb::WindowOptions::default(),
        ) {
            Ok(w) => window = Some(w),
            Err(error) => eprintln!("preview unavailable: {}", error),
        }
    }

    let started = Instant::now();
    let mut frames = 0u64;
    let mut last_report = Instant::now();
    let mut displayed = vec![0u32; 960 * 540];

    loop {
        let Some(frame) = source
            .next_frame()
            .map_err(|error| format!("frame error: {}", error))?
        else {
            if options.source.is_some() {
                println!("source exhausted after {} frames", frames);
            }
            break;
        };
        frames += 1;

        if let Some(file) = receiver
            .process_frame(&frame.rgb, frame.width, frame.height)
            .map_err(|error| format!("receiver error: {}", error))?
        {
            let path = save_recovered(&file, &options.out)?;
            let stats = receiver.stats();
            println!();
            println!("Transfer complete.");
            println!(
                "  file:     {} ({} bytes, {})",
                file.filename(),
                file.bytes.len(),
                file.meta.mime
            );
            println!("  saved:    {}", path.display());
            println!(
                "  received: {} unique symbols in {} frames ({:.0}s), {} duplicates, {} bad",
                stats.accepted_frames,
                frames,
                started.elapsed().as_secs_f64(),
                stats.duplicate_frames,
                stats.bad_frames
            );
            println!("  status:   {}", receiver.last_guidance);
            return Ok(());
        }

        // Preview + telemetry.
        if let Some(w) = window.as_mut() {
            let scale_x = frame.width.max(1) as f64 / 960.0;
            let scale_y = frame.height.max(1) as f64 / 540.0;
            let scale = scale_x.max(scale_y);
            let (fw, fh) = (
                (frame.width as f64 / scale) as usize,
                (frame.height as f64 / scale) as usize,
            );
            displayed.fill(0x0020_2020);
            for y in 0..fh {
                let sy = ((y as f64 * scale) as usize).min(frame.height - 1);
                for x in 0..fw {
                    let sx = ((x as f64 * scale) as usize).min(frame.width - 1);
                    let si = (sy * frame.width + sx) * 3;
                    displayed[y * 960 + x] = (u32::from(frame.rgb[si]) << 16)
                        | (u32::from(frame.rgb[si + 1]) << 8)
                        | u32::from(frame.rgb[si + 2]);
                }
            }
            let (gx, gy, gw, gh) = receiver.guide_region(frame.width, frame.height);
            let region = (
                (gx as f64 / scale) as usize,
                (gy as f64 / scale) as usize,
                (gw as f64 / scale) as usize,
                (gh as f64 / scale) as usize,
            );
            draw_guide(&mut displayed, 960, 540, region);
            if w.is_open() && !w.is_key_down(minifb::Key::Escape) {
                let _ = w.update_with_buffer(&displayed, 960, 540);
            } else {
                window = None;
            }
        }

        if last_report.elapsed().as_secs_f64() >= 1.0 {
            report(&receiver, frames);
            last_report = Instant::now();
        }

        if let Some(limit) = options.max_frames {
            if frames >= limit {
                println!("stopped after {} frames (--max-frames)", frames);
                break;
            }
        }
    }

    let stats = receiver.stats();
    println!(
        "No transfer completed. {} unique symbols accepted, {} bad frames, {} duplicates.",
        stats.accepted_frames, stats.bad_frames, stats.duplicate_frames
    );
    Ok(())
}

fn save_recovered(file: &RecoveredFile, dir: &std::path::Path) -> Result<PathBuf, String> {
    file.save(dir)
        .map_err(|error| format!("cannot save recovered file: {}", error))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("qrferry-recv: {}", error);
        eprintln!();
        eprintln!("{}", USAGE);
        std::process::exit(1);
    }
}
