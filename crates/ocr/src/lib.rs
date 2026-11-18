//! Text recognition with embedded PP-OCRv6 models (`small` detection + `medium`
//! recognition), run by tract (pure Rust, no ONNX Runtime). Used where the OS
//! has no accurate built-in CJK/multilingual OCR API.
//!
//! Recognition compiles once with a symbolic width dimension (`[1, 3, 48, W]`),
//! keeping a single plan (~180 MB) for text lines of any length instead of
//! cloning and recompiling the 34.5M-parameter network per width. The engine is
//! loaded on first use and dropped after a minute idle so a background tray app
//! doesn't hold its buffers forever.

use std::collections::HashSet;
use std::io::Cursor;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use geo_types::{LineString, Polygon};
use pure_onnx_ocr::{
    DetInferenceSession, DetLimitType, DetPolygonUnclipper, DetPolygonUnclipperConfig, DetPostProcessor,
    DetPostProcessorConfig, DetPreProcessor, DetPreProcessorConfig, PaddleInferenceConfig, PreprocessedRecBatch,
    RecDictionary, RecInferenceOutput, RecPostProcessor, RecPostProcessorConfig, RecPreProcessor,
    RecPreProcessorConfig, crop_quad, min_area_quad,
};
use rayon::prelude::*;
use tract_linalg::multithread::{Executor, multithread_tract_scope};
use tract_onnx::prelude::*;

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

/// Minimum mean CTC token probability to keep a detected box (PaddleOCR's
/// default `drop_score`), filtering out avatars, icons and decorative borders.
const MIN_CONFIDENCE: f32 = 0.5;

/// Minimum detection cap on the longest side, so small/medium regions are never
/// downscaled even on high-DPI displays.
const MIN_DET_MAX_SIDE: u32 = 1600;
const HARD_DET_MAX_SIDE: u32 = 4000;

struct Engine {
    det_session: DetInferenceSession,
    det_base_cfg: DetPreProcessorConfig,
    det_post: DetPostProcessor,
    unclipper: DetPolygonUnclipper,
    rec_plan: Arc<TypedRunnableModel>,
    rec_pre: RecPreProcessor,
    rec_post: RecPostProcessor,
    pool: rayon::ThreadPool,
    executor: Executor,
}

struct Warm {
    engine: Engine,
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

fn load_symbolic_rec(bytes: &[u8], input_height: u32) -> TractResult<Arc<TypedRunnableModel>> {
    let mut model = tract_onnx::onnx().with_ignore_output_shapes(true).model_for_read(&mut Cursor::new(bytes))?;
    // PaddleOCR 3.x exports record intermediate `DynamicDimension.*` symbols in
    // `value_info`; clear non-input/non-const outlet facts so tract re-infers them.
    let inputs: HashSet<_> = model.inputs.iter().copied().collect();
    for node_id in 0..model.nodes.len() {
        if model.nodes[node_id].op_is::<tract_onnx::tract_hir::ops::konst::Const>() {
            continue;
        }
        for slot in 0..model.nodes[node_id].outputs.len() {
            let outlet = OutletId::new(node_id, slot);
            if !inputs.contains(&outlet) {
                model.set_outlet_fact(outlet, InferenceFact::default())?;
            }
        }
    }
    let width = model.symbols.sym("width");
    model.set_input_fact(
        0,
        InferenceFact::dt_shape(
            f32::datum_type(),
            tvec![TDim::from(1), TDim::from(3), TDim::from(input_height as i64), width.into()],
        ),
    )?;
    model.into_typed()?.into_decluttered()?.into_optimized()?.into_runnable()
}

fn build_engine() -> anyhow::Result<Engine> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let det_cfg = PaddleInferenceConfig::from_yaml_str(include_str!("../models/det.yml"))
        .map_err(|e| anyhow!("reading det.yml: {e}"))?;
    let rec_yml = include_str!("../models/rec.yml");
    let rec_cfg = PaddleInferenceConfig::from_yaml_str(rec_yml).map_err(|e| anyhow!("reading rec.yml: {e}"))?;

    let mut det_session =
        DetInferenceSession::from_bytes(include_bytes!("../models/det.onnx")).map_err(|e| anyhow!("loading det.onnx: {e}"))?;
    det_session.set_inference_threads(threads);
    det_session.set_plan_cache_capacity(2);

    let mut det_base_cfg = DetPreProcessorConfig {
        limit_side_len: 64,
        limit_type: DetLimitType::Min,
        max_side_limit: HARD_DET_MAX_SIDE,
        ..Default::default()
    };
    if let Some(order) = det_cfg.color_order {
        det_base_cfg.color_order = order;
    }
    if let Some(mean) = det_cfg.normalize_mean {
        det_base_cfg.mean = mean;
    }
    if let Some(std) = det_cfg.normalize_std {
        det_base_cfg.std = std;
    }

    let mut rec_pre_cfg = RecPreProcessorConfig::default();
    if let Some(order) = rec_cfg.color_order {
        rec_pre_cfg.color_order = order;
    }
    if let Some([_, height, _]) = rec_cfg.rec_image_shape.filter(|s| s[0] == 3 && s[1] > 0) {
        rec_pre_cfg.target_height = height;
    }
    // `PP-OCRv6_medium_rec` dynamic shapes start at `[1, 3, 48, 160]`; avoiding
    // unnecessary padding to 320 nearly halves recognition work on short lines.
    rec_pre_cfg.max_width = 160;
    rec_pre_cfg.width_alignment = 16;

    let rec_plan = load_symbolic_rec(include_bytes!("../models/rec.onnx"), rec_pre_cfg.target_height)
        .map_err(|e| anyhow!("loading rec.onnx: {e}"))?;

    let dictionary = Arc::new(
        RecDictionary::from_inference_yml_str(rec_yml)
            .map_err(|e| anyhow!("reading OCR dictionary: {e}"))?
            .with_space_char(),
    );
    let rec_post = RecPostProcessor::new(
        Arc::clone(&dictionary),
        RecPostProcessorConfig { blank_id: dictionary.blank_id(), ..Default::default() },
    );

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("bluesnap-ocr-{i}"))
        .build()
        .context("creating OCR thread pool")?;
    let executor = if threads > 1 {
        Executor::multithread_with_name(threads, "bluesnap-ocr-linalg")
    } else {
        Executor::SingleThread
    };

    Ok(Engine {
        det_session,
        det_base_cfg,
        det_post: DetPostProcessor::new(DetPostProcessorConfig::default()),
        unclipper: DetPolygonUnclipper::new(DetPolygonUnclipperConfig::default()),
        rec_plan,
        rec_pre: RecPreProcessor::new(rec_pre_cfg),
        rec_post,
        pool,
        executor,
    })
}

impl Engine {
    fn recognize_batch(&self, batch: &PreprocessedRecBatch) -> anyhow::Result<RecInferenceOutput> {
        let outputs = self.rec_plan.run(tvec![batch.tensor.clone().into()]).map_err(|e| anyhow!("rec inference: {e}"))?;
        let view = outputs[0].to_plain_array_view::<f32>().map_err(|e| anyhow!("rec output: {e}"))?;
        let logits = view.into_dimensionality::<tract_ndarray::Ix3>().map_err(|e| anyhow!("rec shape: {e}"))?.to_owned();
        let (shape, (vec, _)) = (logits.dim(), logits.into_raw_vec_and_offset());
        let time_steps = shape.1;
        let scale = if batch.max_width > 0 { time_steps as f32 / batch.max_width as f32 } else { 0.0 };
        let valid_timesteps = batch
            .valid_widths
            .iter()
            .map(|&w| ((scale * w as f32).round() as usize).clamp(1, time_steps))
            .collect();
        let logits = ndarray::Array3::from_shape_vec(shape, vec).context("rec logits buffer")?;
        Ok(RecInferenceOutput { logits, valid_timesteps })
    }

    fn recognize_crop(&self, crop: &image::RgbImage) -> anyhow::Result<Option<String>> {
        let batch = self.rec_pre.process_images(std::slice::from_ref(crop)).map_err(|e| anyhow!("recognition preprocessing: {e}"))?;
        let out = self.recognize_batch(&batch)?;
        let sequences = self.rec_post.process(&out).map_err(|e| anyhow!("recognition postprocessing: {e}"))?;
        Ok(sequences.into_iter().next()
            .filter(|s| s.confidence >= MIN_CONFIDENCE && !s.text.trim().is_empty())
            .map(|s| s.text))
    }

    fn run(&self, rgb: image::RgbImage, scale: f32) -> anyhow::Result<Vec<Line>> {
        let (width, height) = rgb.dimensions();
        let factor = if scale.is_finite() { scale.clamp(1.0, 4.0) } else { 1.0 };
        // Cap detection on giant high-DPI full-screen captures while keeping
        // small/medium selections at native resolution (and upscaling tiny
        // strips `< 64 px` via `DetLimitType::Min`). Recognition always crops
        // from the full-resolution `rgb` image so thin CJK strokes stay sharp.
        let logical_max = (width.max(height) as f32 / factor).round() as u32;
        let mut det_cfg = self.det_base_cfg;
        det_cfg.max_side_limit = logical_max.clamp(MIN_DET_MAX_SIDE, HARD_DET_MAX_SIDE);

        let dyn_img = image::DynamicImage::ImageRgb8(rgb);
        let det_input = DetPreProcessor::new(det_cfg).process(&dyn_img).map_err(|e| anyhow!("det preprocess: {e}"))?;
        let det_out = self.det_session.run(&det_input).map_err(|e| anyhow!("det inference: {e}"))?;
        let boxes = self.det_post.db_boxes(&det_out.probability_map, &self.unclipper).map_err(|e| anyhow!("det postprocess: {e}"))?;
        if boxes.is_empty() {
            return Ok(Vec::new());
        }

        let image::DynamicImage::ImageRgb8(rgb) = dyn_img else { unreachable!() };
        let (sx, sy) = det_input.inverse_scale();
        let (w_lim, h_lim) = (width as f64, height as f64);
        let regions: Vec<(image::RgbImage, (f32, f32, f32, f32))> = boxes
            .into_iter()
            .filter_map(|det_box| {
                let mut pts: Vec<geo_types::Coord<f64>> = det_box
                    .quad
                    .iter()
                    .map(|&(x, y)| geo_types::Coord {
                        x: (x * sx).round().clamp(0.0, w_lim),
                        y: (y * sy).round().clamp(0.0, h_lim),
                    })
                    .collect();
                let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for c in &pts {
                    (x0, y0, x1, y1) = (x0.min(c.x), y0.min(c.y), x1.max(c.x), y1.max(c.y));
                }
                pts.push(pts[0]);
                let quad = min_area_quad(&Polygon::new(LineString::from(pts), vec![]))?;
                let crop = crop_quad(&rgb, &quad)?;
                Some((crop, (x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32)))
            })
            .collect();

        let texts: Vec<anyhow::Result<Option<String>>> = match regions.as_slice() {
            [] => Vec::new(),
            [(crop, _)] => vec![multithread_tract_scope(self.executor.clone(), || self.recognize_crop(crop))],
            _ => self.pool.install(|| regions.par_iter().map(|(crop, _)| self.recognize_crop(crop)).collect()),
        };

        let texts = collect_recognized(texts)?;
        Ok(regions
            .into_iter()
            .zip(texts)
            .filter_map(|((_, (x, y, width, height)), text)| Some(Line { text: text?, x, y, width, height }))
            .collect())
    }
}

/// Empty or low-confidence regions are normal. A failed inference means the
/// image was only partially checked and must not be reported as a clean scan.
fn collect_recognized(results: Vec<anyhow::Result<Option<String>>>) -> anyhow::Result<Vec<Option<String>>> {
    let total = results.len();
    let failed = results.iter().filter(|result| result.is_err()).count();
    results.into_iter().collect::<anyhow::Result<_>>()
        .with_context(|| format!("OCR failed for {failed} of {total} text regions"))
}

/// Recognizes text in straight-alpha RGBA pixels. Blocks for a few hundred
/// milliseconds on typical selections; call it off the UI thread.
///
/// `scale` is the screen's pixels per logical pixel. Detection caps giant
/// high-DPI full-screen captures at their logical size (at least 1600 px),
/// while recognition always crops from the full-resolution image so small UI
/// text and dense CJK strokes aren't degraded by downscaling. Boxes are in
/// `width` × `height` pixels.
pub fn recognize(width: u32, height: u32, rgba: &[u8], scale: f32) -> anyhow::Result<Vec<Line>> {
    let pixels = width as usize * height as usize;
    anyhow::ensure!(rgba.len() == pixels * 4, "buffer size mismatch");
    let mut rgb = Vec::with_capacity(pixels * 3);
    for &[r, g, b, _] in rgba.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[r, g, b]);
    }
    let image = image::RgbImage::from_raw(width, height, rgb).context("buffer size mismatch")?;
    let mut warm = state().lock().unwrap_or_else(|e| e.into_inner());
    if warm.is_none() {
        *warm = Some(Warm { engine: build_engine()?, last_used: Instant::now() });
    }
    let warm = warm.as_mut().expect("just loaded");
    let results = warm.engine.run(image, scale);
    warm.last_used = Instant::now();
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_failed_regions_have_different_outcomes() {
        assert_eq!(collect_recognized(vec![Ok(None)]).unwrap(), vec![None]);
        for results in [vec![Err(anyhow!("inference"))], vec![Ok(Some("line".into())), Err(anyhow!("inference"))]] {
            let total = results.len();
            let error = collect_recognized(results).unwrap_err();
            assert_eq!(format!("{error:#}"), format!("OCR failed for 1 of {total} text regions: inference"));
        }
    }
}
