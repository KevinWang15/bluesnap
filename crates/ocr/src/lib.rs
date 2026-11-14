//! Text recognition with the PP-OCRv6 small models, embedded in the binary and run
//! by tract (pure Rust, no ONNX Runtime). Used where the OS has no good OCR API.

use std::sync::OnceLock;

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

fn engine() -> anyhow::Result<&'static OcrEngine> {
    static ENGINE: OnceLock<Result<OcrEngine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            OcrEngineBuilder::new()
                .det_model_bytes(include_bytes!("../models/det.onnx").to_vec())
                .det_config_yaml(include_str!("../models/det.yml"))
                .rec_model_bytes(include_bytes!("../models/rec.onnx").to_vec())
                .rec_config_yaml(include_str!("../models/rec.yml"))
                // Detect at native resolution (PaddleOCR 3.x behavior): screenshots
                // often contain small UI text that downscaling would lose.
                .det_limit_type(DetLimitType::Min)
                .det_limit_side_len(64)
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| anyhow!("loading OCR models: {e}"))
}

/// Recognizes text in straight-alpha RGBA pixels. Blocks for a few hundred
/// milliseconds on typical selections; call it off the UI thread.
pub fn recognize(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<Line>> {
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec()).context("buffer size mismatch")?;
    let results = engine()?
        .run_from_image(&image::DynamicImage::ImageRgba8(image))
        .map_err(|e| anyhow!("OCR failed: {e}"))?;
    Ok(results
        .into_iter()
        .filter(|r| !r.text.trim().is_empty())
        .map(|r| {
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for c in r.bounding_box.exterior().coords() {
                (x0, y0, x1, y1) = (x0.min(c.x), y0.min(c.y), x1.max(c.x), y1.max(c.y));
            }
            Line { text: r.text, x: x0 as f32, y: y0 as f32, width: (x1 - x0) as f32, height: (y1 - y0) as f32 }
        })
        .collect())
}
