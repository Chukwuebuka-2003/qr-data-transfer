//! qrferry-send — transmit a file as an animated QR stream.
//!
//! Opens a window (single-lane or dual-lane) and plays the RaptorQ stream at
//! the preset rate, or exports a PNG frame sequence with `--out` for offline
//! playback (projector, tablet, TV).
//!
//! ```text
//! qrferry-send <file> [--preset <key>] [--scale <n>] [--out <dir>] [--frames <n>]
//! ```

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use qrferry::{
    format_bytes, get_preset, nominal_rate, CompressionMode, QrImage, Sender, TransferPreset,
};

const USAGE: &str = "\
Usage: qrferry-send <file> [options]

Options:
  --preset <key>   robust | balanced | turbo | turbo30 | turbo60 | megabit (default: robust)
  --scale <n>      override the preset module pixel scale
  --out <dir>      write PNG frames to <dir> instead of opening a window
  --frames <n>     stop after n frames (default: one full cycle with --out, loop forever in a window)
  --no-loop        stop after one full cycle in window mode
  --help           show this help";

struct Options {
    file: PathBuf,
    preset_key: String,
    scale: Option<u8>,
    out: Option<PathBuf>,
    frames: Option<u64>,
    no_loop: bool,
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut file: Option<PathBuf> = None;
    let mut preset_key = "robust".to_string();
    let mut scale: Option<u8> = None;
    let mut out: Option<PathBuf> = None;
    let mut frames: Option<u64> = None;
    let mut no_loop = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", USAGE);
                std::process::exit(0);
            }
            "--preset" => {
                preset_key = args.next().ok_or("--preset requires a value")?;
            }
            "--scale" => {
                scale = Some(
                    args.next()
                        .ok_or("--scale requires a value")?
                        .parse()
                        .map_err(|_| "invalid --scale value")?,
                );
            }
            "--out" => {
                out = Some(PathBuf::from(args.next().ok_or("--out requires a value")?));
            }
            "--frames" => {
                frames = Some(
                    args.next()
                        .ok_or("--frames requires a value")?
                        .parse()
                        .map_err(|_| "invalid --frames value")?,
                );
            }
            "--no-loop" => no_loop = true,
            other if other.starts_with('-') => return Err(format!("unknown option: {}", other)),
            other => {
                if file.is_some() {
                    return Err("only one file argument is allowed".to_string());
                }
                file = Some(PathBuf::from(other));
            }
        }
    }

    Ok(Options {
        file: file.ok_or("missing file argument")?,
        preset_key,
        scale,
        out,
        frames,
        no_loop,
    })
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let mut file =
        File::open(path).map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    Ok(bytes)
}

/// Convert a library error into the CLI error string.
fn qr<T>(result: qrferry::Result<T>) -> Result<T, String> {
    result.map_err(|error| error.to_string())
}

fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        _ => "application/octet-stream",
    }
}

fn write_png(path: &Path, image: &QrImage) -> Result<(), String> {
    let file =
        File::create(path).map_err(|e| format!("cannot create {}: {}", path.display(), e))?;
    let mut encoder = png::Encoder::new(file, image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("png header failed: {}", e))?;
    writer
        .write_image_data(&image.rgba)
        .map_err(|e| format!("png write failed: {}", e))?;
    Ok(())
}

/// Compose the current lanes side by side into one image (dual modes).
fn compose_lanes(lanes: &[Option<QrImage>; 2], count: usize) -> QrImage {
    let lane = |index: usize| -> QrImage {
        lanes[index]
            .clone()
            .unwrap_or_else(|| lanes[0].clone().expect("lane 0 rendered"))
    };
    if count == 1 {
        return lane(0);
    }
    let first = lane(0);
    let height = first.height;
    let width = (0..count).map(|i| lane(i).width).sum();
    let mut rgba = vec![255u8; width * height * 4];
    let mut x = 0usize;
    for index in 0..count {
        let image = lane(index);
        for row in 0..height {
            let src = &image.rgba[row * image.width * 4..(row + 1) * image.width * 4];
            let dst_start = (row * width + x) * 4;
            rgba[dst_start..dst_start + src.len()].copy_from_slice(src);
        }
        x += image.width;
    }
    QrImage {
        width,
        height,
        rgba,
    }
}

fn run() -> Result<(), String> {
    let options = parse_args()?;
    let bytes = read_file(&options.file)?;
    let mime = mime_for(&options.file);
    let filename = options
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("transfer.bin")
        .to_string();

    let base = get_preset(&options.preset_key)
        .ok_or_else(|| format!("unknown preset: {}", options.preset_key))?;
    let preset = match options.scale {
        Some(scale) => TransferPreset {
            render_scale: scale,
            ..*base
        },
        None => *base,
    };
    let sender = qr(Sender::prepare_with_preset(
        &bytes, &filename, mime, &preset,
    ))?;
    let mut sender = sender;
    let preset = sender.preset;
    let transfer = &sender.transfer;

    println!("QRFerry sender");
    println!(
        "  file:       {} ({})",
        filename,
        format_bytes(bytes.len() as u64)
    );
    println!(
        "  compressed: {} ({})",
        format_bytes(u64::from(transfer.meta.transmitted_size)),
        match transfer.meta.compression {
            CompressionMode::None => "none",
            CompressionMode::Gzip => "gzip-9",
            CompressionMode::Brotli => "brotli-11",
        }
    );
    println!(
        "  preset:     {} (V{}-{:?}, {} symbols/s, {}% repair)",
        preset.label, preset.version, preset.ecc, preset.fps, preset.repair_percent
    );
    println!(
        "  channel:    {} bytes/frame -> nominal {}",
        preset.useful_bytes_per_frame,
        format_bytes(u64::from(nominal_rate(&preset)))
    );
    println!(
        "  stream:     {} packets ({} source + {} repair), cycle repeats every {} frames",
        transfer.packets.len(),
        transfer.source_packet_count,
        transfer.repair_packet_indices.len(),
        sender.cycle_len()
    );
    println!(
        "  duration:   ~{:.0} sec for one full pass at nominal rate",
        sender.estimated_seconds()
    );
    if preset.lanes == 2 {
        println!("  dual lane:  keep BOTH codes inside the phone guide (landscape).");
    }

    if let Some(out_dir) = &options.out {
        std::fs::create_dir_all(out_dir)
            .map_err(|e| format!("cannot create {}: {}", out_dir.display(), e))?;
        let count = options.frames.unwrap_or(sender.cycle_len() as u64);
        let mut lanes: [Option<QrImage>; 2] = [None, None];
        let started = Instant::now();
        for index in 0..count {
            let lane = (sender.frames_played % u64::from(preset.lanes)) as usize;
            lanes[lane] = Some(qr(sender.next_frame())?);
            let composed = compose_lanes(&lanes, preset.lanes as usize);
            let path = out_dir.join(format!("frame_{:06}.png", index + 1));
            write_png(&path, &composed)?;
        }
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "wrote {} frames to {} in {:.1}s ({:.1} frames/s)",
            count,
            out_dir.display(),
            elapsed,
            count as f64 / elapsed
        );
        return Ok(());
    }

    // Window mode.
    let first = qr(sender.peek())?;
    let (mut width, height) = (first.width, first.height);
    if preset.lanes == 2 {
        width *= 2;
    }
    let mut window = minifb::Window::new(
        &format!("QRFerry — {}", filename),
        width,
        height,
        minifb::WindowOptions {
            resize: false,
            ..minifb::WindowOptions::default()
        },
    )
    .map_err(|e| format!("cannot open window: {}", e))?;

    let mut lanes: [Option<QrImage>; 2] = [None, None];
    let interval = std::time::Duration::from_secs_f64(1.0 / f64::from(preset.fps));
    let mut next_frame_at = Instant::now();
    let started = Instant::now();
    let mut last_report = Instant::now();

    while window.is_open() && !window.is_key_down(minifb::Key::Escape) {
        let now = Instant::now();
        if now < next_frame_at {
            std::thread::sleep(next_frame_at - now);
            continue;
        }
        let lane = (sender.frames_played % u64::from(preset.lanes)) as usize;
        lanes[lane] = Some(qr(sender.next_frame())?);
        let composed = compose_lanes(&lanes, preset.lanes as usize);
        window
            .update_with_buffer(&composed.to_rgb32(), composed.width, composed.height)
            .map_err(|e| format!("window update failed: {}", e))?;

        next_frame_at += interval;
        if next_frame_at < Instant::now() {
            next_frame_at = Instant::now() + interval;
        }

        if last_report.elapsed().as_secs_f64() >= 1.0 {
            let elapsed = started.elapsed().as_secs_f64();
            let fps = sender.frames_played as f64 / elapsed;
            println!(
                "{} frames, {:.1} fps actual, ~{:.0}s elapsed, cycle {}/{}",
                sender.frames_played,
                fps,
                elapsed,
                (sender.frames_played as usize % sender.cycle_len()) + 1,
                sender.cycle_len()
            );
            last_report = Instant::now();
        }

        if options.no_loop && sender.frames_played >= sender.cycle_len() as u64 {
            break;
        }
        if let Some(limit) = options.frames {
            if sender.frames_played >= limit {
                break;
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("qrferry-send: {}", error);
        eprintln!();
        eprintln!("{}", USAGE);
        std::process::exit(1);
    }
}
