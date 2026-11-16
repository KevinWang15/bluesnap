//! Text recognition with the PP-OCRv6 small models, embedded in the binary and run
//! by tract (pure Rust, no ONNX Runtime). Used where the OS has no good OCR API.
//!
//! The engine is loaded on first use (about 30 ms) and dropped again after a
//! minute without use: while warm it holds a few hundred MB of compiled plans
//! and buffers, which a background app shouldn't keep around.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use pure_onnx_ocr::{DetLimitType, OcrEngine, OcrEngineBuilder};

/// One recognized line, with its axis-aligned bounds in image pixels.
pub struct Line {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// How long the engine stays loaded after the last recognition.
const KEEP_WARM: Duration = Duration::from_secs(60);

struct Warm {
    engine: OcrEngine,
    last_used: Instant,
}

fn state() -> &'static Mutex<Option<Warm>> {
    static STATE: OnceLock<Mutex<Option<Warm>>> = OnceLock::new();
    STATE.get_or_init(|| {
        std::thread::Builder::new()
            .name("ocr-unload".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(KEEP_WARM / 4);
                    let mut warm = state().lock().unwrap_or_else(|e| e.into_inner());
                    if warm.as_ref().is_some_and(|w| w.last_used.elapsed() >= KEEP_WARM) {
                        *warm = None;
                    }
                }
            })
            .expect("spawn OCR unload thread");
        Mutex::new(None)
    })
}

fn build_engine() -> anyhow::Result<OcrEngine> {
    // Measured on a 20-thread CPU: 8 threads is as fast as 16 on typical
    // selections and needs less memory (each thread has its own buffers).
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    OcrEngineBuilder::new()
        .det_model_bytes(include_bytes!("../models/det.onnx").to_vec())
        .det_config_yaml(include_str!("../models/det.yml"))
        .rec_model_bytes(include_bytes!("../models/rec.onnx").to_vec())
        .rec_config_yaml(include_str!("../models/rec.yml"))
        // Detect at native resolution (PaddleOCR 3.x behavior): screenshots
        // often contain small UI text that downscaling would lose.
        .det_limit_type(DetLimitType::Min)
        .det_limit_side_len(64)
        // tract compiles a plan per recognition input width; lines of many
        // different lengths overflow the default 16 and recompile.
        .plan_cache_capacity(4, 32)
        .inference_threads(threads)
        .build()
        .map_err(|e| anyhow!("loading OCR models: {e}"))
}

/// Recognizes text in straight-alpha RGBA pixels. Blocks for a few hundred
/// milliseconds on typical selections; call it off the UI thread.
///
/// `scale` is the screen's pixels per logical pixel. On high-DPI screens text
/// is drawn larger than the models need, so the image is read at logical size:
/// detection, most of the time on large images, gets `scale`² times faster,
/// and recognition sizes each line to 48 px tall either way. Boxes are in
/// `width` × `height` pixels.
pub fn recognize(width: u32, height: u32, rgba: &[u8], scale: f32) -> anyhow::Result<Vec<Line>> {
    anyhow::ensure!(rgba.len() == width as usize * height as usize * 4, "buffer size mismatch");
    let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [r, g, b]).collect();
    let mut image = image::RgbImage::from_raw(width, height, rgb).context("buffer size mismatch")?;
    let factor = if scale.is_finite() { scale.clamp(1.0, 4.0) } else { 1.0 };
    let size = ((width as f32 / factor).round().max(1.0) as u32, (height as f32 / factor).round().max(1.0) as u32);
    if size != (width, height) {
        image = pure_onnx_ocr::imgproc::resize_bilinear(&image, size.0, size.1);
    }
    let (sx, sy) = (width as f64 / size.0 as f64, height as f64 / size.1 as f64);
    let mut warm = state().lock().unwrap_or_else(|e| e.into_inner());
    if warm.is_none() {
        *warm = Some(Warm { engine: build_engine()?, last_used: Instant::now() });
    }
    let warm = warm.as_mut().expect("just loaded");
    let results = warm
        .engine
        .run_from_image(&image::DynamicImage::ImageRgb8(image))
        .map_err(|e| anyhow!("OCR failed: {e}"));
    warm.last_used = Instant::now();
    Ok(results?
        .into_iter()
        .filter(|r| !r.text.trim().is_empty())
        .map(|r| {
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for c in r.bounding_box.exterior().coords() {
                (x0, y0, x1, y1) = (x0.min(c.x), y0.min(c.y), x1.max(c.x), y1.max(c.y));
            }
            let (x0, y0, x1, y1) = (x0 * sx, y0 * sy, x1 * sx, y1 * sy);
            Line { text: r.text, x: x0 as f32, y: y0 as f32, width: (x1 - x0) as f32, height: (y1 - y0) as f32 }
        })
        .collect())
}
