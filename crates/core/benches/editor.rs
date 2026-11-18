//! Benchmarks for the work done between a hotkey press and a visible overlay, and
//! for the per-frame work while editing. Run with `cargo bench -p bluesnap-core`.
//!
//! "frame" benchmarks reproduce what the overlay does for one mouse move: feed the
//! event to the editor, re-rasterize the annotation layers that changed and convert
//! them to the BGRA layout uploaded to the GPU.

use std::collections::HashSet;
use std::hint::black_box;
use std::sync::Arc;

use bluesnap_core::format::{decode_bs, encode_bs};
use bluesnap_core::pixels::{pixmap_from_rgba, split_bgrx};
use bluesnap_core::render::{swap_red_blue, to_bgra_straight};
use bluesnap_core::{
    Composition, Document, Editor, FontSet, Fonts, ImageId, Point, Rect, RedactArea, RedactEffect, TextStyle, Tool,
    VisualBand,
};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

const W4K: u32 = 3840;
const H4K: u32 = 2160;

/// Noise-like screenshot so compression-friendly patterns don't flatter anything.
fn screenshot(w: u32, h: u32) -> Vec<u8> {
    let mut seed = 0x1234_5678u32;
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let [r, g, b, _] = seed.to_le_bytes();
        out.extend_from_slice(&[r, g, b, 255]);
    }
    out
}

fn fonts() -> Arc<Fonts> {
    Arc::new(Fonts::sans_only(FontSet::load(&[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        r"C:\Windows\Fonts\segoeui.ttf",
    ])))
}

fn editor(w: u32, h: u32) -> Editor {
    let doc = Document::from_rgba(w, h, screenshot(w, h)).unwrap();
    let windows = (0..50).map(|i| Rect::new(i as f32 * 30.0, i as f32 * 20.0, 800.0, 600.0));
    Editor::new(doc, fonts(), windows, 2.0)
}

fn drag(e: &mut Editor, from: (f32, f32), to: (f32, f32)) {
    e.pointer_down(Point::new(from.0, from.1));
    for i in 1..=8 {
        let t = i as f32 / 8.0;
        e.pointer_move(Point::new(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t), false);
    }
    e.pointer_up(Point::new(to.0, to.1));
}

/// Full-screen selection with 20 annotations of every kind.
fn annotated(w: u32, h: u32) -> Editor {
    let mut e = editor(w, h);
    drag(&mut e, (0.0, 0.0), (w as f32, h as f32));
    let tools = [Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Pen, Tool::Redact];
    for i in 0..20 {
        let tool = tools[i % tools.len()];
        e.toggle_tool(tool);
        let (x, y) = (100.0 + (i % 5) as f32 * 600.0, 100.0 + (i / 5) as f32 * 450.0);
        drag(&mut e, (x, y), (x + 400.0, y + 300.0));
        e.toggle_tool(tool);
    }
    e.toggle_tool(Tool::Text);
    e.pointer_down(Point::new(200.0, 1900.0));
    e.text_replace(None, "Benchmark text 基准测试 with some length to it", false, None);
    e.finish_text();
    e.toggle_tool(Tool::Text);
    e
}

/// What the overlay does after an event: re-rasterize and convert the layers
/// whose revision changed, exactly like the app's per-layer cache.
#[derive(Default)]
struct Presenter {
    static_rev: Option<u64>,
    active_rev: Option<u64>,
}

impl Presenter {
    fn present(&mut self, e: &Editor) -> usize {
        let mut bytes = 0;
        if self.static_rev != Some(e.static_layer_revision()) {
            self.static_rev = Some(e.static_layer_revision());
            bytes += e.render_static_layer(e.screen()).map_or(0, |(_, p)| to_bgra_straight(&p).len());
        }
        if self.active_rev != Some(e.active_layer_revision()) {
            self.active_rev = Some(e.active_layer_revision());
            bytes += e.render_active_layer(e.screen()).map_or(0, |(_, p)| to_bgra_straight(&p).len());
        }
        bytes
    }
}

/// Like [`Presenter::present`] from a cold cache (both layers rendered).
fn present(e: &Editor) -> usize {
    Presenter::default().present(e)
}

fn startup(c: &mut Criterion) {
    let mut g = c.benchmark_group("startup");
    g.sample_size(20);
    let rgba = screenshot(W4K, H4K);
    // Old pipeline: the platform turned BGRX into RGBA, then the app swapped it back for the GPU.
    g.bench_function("capture_to_gpu_bgra/4k", |b| b.iter(|| black_box(swap_red_blue(&rgba))));
    // New pipeline: one pass fixes alpha in place (GPU buffer) and makes the RGBA copy.
    g.bench_function("split_bgrx/4k", |b| {
        b.iter_batched(|| rgba.clone(), |mut bgrx| black_box(split_bgrx(&mut bgrx)), BatchSize::LargeInput)
    });
    g.bench_function("editor_new/4k", |b| {
        b.iter_batched(
            || rgba.clone(),
            |rgba| {
                let doc = Document::from_rgba(W4K, H4K, rgba).unwrap();
                black_box(Editor::new(doc, Arc::new(Fonts::empty()), Vec::<Rect>::new(), 2.0))
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

fn frames(c: &mut Criterion) {
    let mut g = c.benchmark_group("frame");
    g.sample_size(20);

    g.bench_function("hover_pick/50_windows", |b| {
        let mut e = editor(W4K, H4K);
        let mut i = 0.0;
        b.iter(|| {
            i = (i + 7.0) % 3000.0;
            e.pointer_move(Point::new(i, i / 2.0), false);
            black_box(e.active_rect());
        })
    });

    g.bench_function("pen_drag/4k_20_shapes", |b| {
        let mut e = annotated(W4K, H4K);
        e.toggle_tool(Tool::Pen);
        e.pointer_down(Point::new(500.0, 500.0));
        let mut presenter = Presenter::default();
        presenter.present(&e);
        let mut i = 0u32;
        b.iter(|| {
            i += 1;
            let t = i as f32;
            e.pointer_move(Point::new(500.0 + (t * 3.0) % 2000.0, 500.0 + (t * 0.37).sin() * 200.0), false);
            black_box(presenter.present(&e))
        })
    });

    g.bench_function("move_selection/4k_20_shapes", |b| {
        let mut e = annotated(W4K, H4K);
        // Shrink the selection so it can move, then drag it around.
        drag(&mut e, (W4K as f32, H4K as f32), (3000.0, 1800.0));
        e.pointer_down(Point::new(1500.0, 900.0));
        let mut presenter = Presenter::default();
        presenter.present(&e);
        let mut i = 0u32;
        b.iter(|| {
            i += 1;
            e.pointer_move(Point::new(1500.0 + (i % 400) as f32, 900.0 + (i % 200) as f32), false);
            black_box(presenter.present(&e))
        })
    });

    g.bench_function("type_text/4k_20_shapes", |b| {
        let mut e = annotated(W4K, H4K);
        e.toggle_tool(Tool::Text);
        e.pointer_down(Point::new(2000.0, 1500.0));
        e.pointer_up(Point::new(2000.0, 1500.0));
        let mut presenter = Presenter::default();
        presenter.present(&e);
        b.iter(|| {
            e.text_replace(None, "a", false, None);
            black_box(presenter.present(&e))
        })
    });

    g.bench_function("cursor/4k_20_shapes", |b| {
        let mut e = annotated(W4K, H4K);
        let mut i = 0u32;
        b.iter(|| {
            i += 1;
            e.pointer_move(Point::new((i * 13 % 3800) as f32, (i * 7 % 2100) as f32), false);
            black_box(e.cursor())
        })
    });
    g.finish();
}

fn output(c: &mut Criterion) {
    let mut g = c.benchmark_group("output");
    g.sample_size(10);
    g.bench_function("export/4k_20_shapes", |b| {
        let mut e = annotated(W4K, H4K);
        b.iter(|| black_box(e.export()))
    });
    g.bench_function("first_mosaic/4k", |b| {
        b.iter_batched(
            || {
                let mut e = editor(W4K, H4K);
                drag(&mut e, (0.0, 0.0), (W4K as f32, H4K as f32));
                e.toggle_tool(Tool::Redact);
                e
            },
            |mut e| {
                drag(&mut e, (100.0, 100.0), (600.0, 400.0));
                black_box(present(&e))
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

fn redaction(c: &mut Criterion) {
    let mut g = c.benchmark_group("redact");
    g.sample_size(10);
    // Dragging the strength slider on a selected 1200x800 blur area: each step
    // re-blurs the rows under it and re-renders the layer.
    g.bench_function("blur_slider_frame/4k_1200x800_area", |b| {
        let mut e = editor(W4K, H4K);
        drag(&mut e, (0.0, 0.0), (W4K as f32, H4K as f32));
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| (r.area, r.effect) = (RedactArea::Rect, RedactEffect::Blur));
        drag(&mut e, (800.0, 600.0), (2000.0, 1400.0));
        let mut presenter = Presenter::default();
        presenter.present(&e);
        e.begin_continuous_edit();
        let mut i = 0u32;
        b.iter(|| {
            i += 1;
            e.set_redaction(|r| r.strength = (i % 50) as f32 / 50.0);
            black_box(presenter.present(&e))
        });
        e.end_continuous_edit();
    });
    g.bench_function("blur_effect/4k_1200x800_area", |b| {
        let doc = Document::from_rgba(W4K, H4K, screenshot(W4K, H4K)).unwrap();
        let mut amount = 10;
        b.iter(|| {
            amount = 10 + (amount + 1) % 30;
            black_box(doc.effect_region(RedactEffect::Blur, amount, Rect::new(800.0, 600.0, 1200.0, 800.0)))
        });
    });
    g.bench_function("blur_rows/1600x30000_doc_1000_rows", |b| {
        let doc = Document::from_rgba(1600, 30_000, screenshot(1600, 30_000)).unwrap();
        let mut amount = 10;
        b.iter(|| {
            amount = 10 + (amount + 1) % 30; // new amount each time: no cache hits
            black_box(doc.effect_region(RedactEffect::Blur, amount, Rect::new(0.0, 14_000.0, 1600.0, 1000.0)))
        });
    });
    g.finish();
}

/// Full-screen selection with 4 of each newer annotation kind too: highlights,
/// counters and text in each style.
fn annotated_all(w: u32, h: u32) -> Editor {
    let mut e = annotated(w, h);
    for i in 0..4 {
        let y = 300.0 + i as f32 * 400.0;
        e.toggle_tool(Tool::Highlighter);
        let mut x = 200.0;
        e.pointer_down(Point::new(x, y));
        while x < 1400.0 {
            x += 4.0;
            e.pointer_move(Point::new(x, y + (x / 40.0).sin() * 20.0), false);
        }
        e.pointer_up(Point::new(x, y));
        e.toggle_tool(Tool::Highlighter);
        e.toggle_tool(Tool::Counter);
        e.pointer_down(Point::new(1600.0 + i as f32 * 80.0, y));
        e.pointer_up(Point::new(1600.0 + i as f32 * 80.0, y));
        e.toggle_tool(Tool::Counter);
    }
    for (i, style) in TextStyle::ALL.into_iter().enumerate() {
        e.toggle_tool(Tool::Text);
        e.set_text_style(style);
        e.pointer_down(Point::new(2200.0, 200.0 + i as f32 * 120.0));
        e.text_replace(None, "Deploy key: 7HQK-2M9X 部署", false, None);
        e.finish_text();
        e.toggle_tool(Tool::Text);
    }
    e
}

fn tools(c: &mut Criterion) {
    let mut g = c.benchmark_group("tools");
    // Committing a shape or changing the selection re-renders every other
    // annotation (now 55 of every kind) into the static layer.
    let e = annotated_all(W4K, H4K);
    g.bench_function("static_layer/4k_55_mixed", |b| b.iter(|| black_box(present(&e))));
    g.bench_function("export/4k_55_mixed", |b| {
        let mut e = annotated_all(W4K, H4K);
        b.iter(|| black_box(e.export()))
    });
    // One mouse move at the end of a long highlighter stroke (600 samples):
    // the stroke is re-swept every frame.
    g.bench_function("highlighter_drag/600_point_stroke", |b| {
        let mut e = annotated(W4K, H4K);
        e.toggle_tool(Tool::Highlighter);
        // Start in the gap between the annotations' columns, so the press draws.
        e.pointer_down(Point::new(550.0, 1000.0));
        for i in 0..600 {
            e.pointer_move(Point::new(550.0 + i as f32 * 4.5, 1000.0 + (i as f32 / 20.0).sin() * 200.0), false);
        }
        let mut presenter = Presenter::default();
        presenter.present(&e);
        let mut i = 0;
        b.iter(|| {
            i += 1;
            e.pointer_move(Point::new(3200.0 + (i % 2) as f32 * 3.0, 1000.0), false);
            black_box(presenter.present(&e))
        });
    });
    g.bench_function("type_boxed_text/4k_55_mixed", |b| {
        let mut e = annotated_all(W4K, H4K);
        e.toggle_tool(Tool::Text);
        e.set_text_style(TextStyle::RoundedBoxed);
        e.pointer_down(Point::new(1200.0, 2050.0));
        assert!(e.is_editing_text());
        let mut presenter = Presenter::default();
        presenter.present(&e);
        b.iter(|| {
            e.text_replace(None, "a", false, None);
            black_box(presenter.present(&e))
        });
    });
    g.finish();

}

/// A text-like page: lines of dark "words" on white, `w` × `h`.
fn text_page(w: usize, h: usize) -> Vec<u8> {
    let mut img = vec![255u8; w * h * 4];
    let mut seed = 0x1234_5679u32;
    let mut rnd = |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed % n
    };
    let mut y = 20;
    while y + 30 < h {
        let mut x = 40 + rnd(40) as usize;
        while x + 120 < w {
            let (len, shade) = (30 + rnd(120) as usize, rnd(120) as u8);
            for yy in y..y + 26 {
                img[(yy * w + x) * 4..(yy * w + x + len) * 4].as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[..3].fill(shade));
            }
            x += len + 14;
        }
        y += 48 + 24 * rnd(3) as usize;
    }
    img
}

fn scrolling(c: &mut Criterion) {
    let mut g = c.benchmark_group("scrolling");
    // A 1600 × 1000 view with a 60-row fixed header, scrolled 60 px per frame.
    let (w, h, header) = (1600usize, 1000usize, 60usize);
    let page = text_page(w, 3000);
    let bar = vec![40u8; w * header * 4];
    let frames: Vec<Vec<u8>> = (0..20)
        .map(|i| {
            let y = i * 60;
            let mut f = bar.clone();
            f.extend_from_slice(&page[y * w * 4..(y + h - header) * w * 4]);
            f
        })
        .collect();
    g.bench_function("stitch_20_frames/1600x1000", |b| {
        b.iter_batched(
            || frames.clone(),
            |frames| {
                let mut s = bluesnap_core::stitch::Stitcher::new(w as u32, 30_000);
                for f in frames {
                    black_box(s.push(h as u32, f));
                }
                s
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

/// Mirrors `CanvasView::sync_gpu_resources` + `LayerBandPool` + `ImageAtlasCache`:
/// uploads each unique `ImageAsset` once, and re-rasterizes + converts (`to_bgra_straight`)
/// only the shape bands whose contents or visible bounds changed.
type BandKey = (u64, usize, usize, Rect, bool);

#[derive(Default)]
struct BandPresenter {
    uploaded_images: HashSet<ImageId>,
    shape_bands: Vec<Option<BandKey>>,
    /// A fractional display zoom requires a single texture per shape band.
    fractional_zoom: bool,
}

impl BandPresenter {
    fn present(&mut self, e: &Editor, clip: Rect) -> usize {
        let bands = e.visual_bands();
        let mut bytes = 0;
        for b in &bands {
            if let VisualBand::Image(img) = b
                && self.uploaded_images.insert(img.asset.id)
            {
                bytes += to_bgra_straight(img.asset.pixmap()).len();
            }
        }
        let shape_count = bands.iter().filter(|b| matches!(b, VisualBand::Shapes { .. })).count();
        self.shape_bands.truncate(shape_count);
        while self.shape_bands.len() < shape_count {
            self.shape_bands.push(None);
        }
        let mut slot = 0;
        for b in bands {
            if let VisualBand::Shapes { range, revision, active } = b {
                let key = (revision, range.start, range.end, clip, active);
                if self.shape_bands[slot] != Some(key) {
                    self.shape_bands[slot] = Some(key);
                    e.render_band_parts(range, active, clip, !self.fractional_zoom, |_, p| {
                        bytes += to_bgra_straight(&p).len();
                    });
                }
                slot += 1;
            }
        }
        bytes
    }
}

/// Multi-image 4K composition with `n_images` screenshots and `n_shapes` mixed annotations.
fn multi_image_editor(n_images: usize, img_w: u32, img_h: u32, n_shapes: usize) -> Editor {
    let first = pixmap_from_rgba(img_w, img_h, screenshot(img_w, img_h)).unwrap();
    let mut comp = Composition::from_slice(first, 2.0, Vec::new()).unwrap();
    comp.width = W4K;
    comp.height = H4K;
    let mut e = Editor::from_composition(comp, fonts());
    for k in 1..n_images {
        let img = pixmap_from_rgba(img_w, img_h, screenshot(img_w, img_h)).unwrap();
        let idx = e.append_slice(img, 2.0, Vec::new()).unwrap();
        // Spread placed images across the 4K canvas.
        let target = ((k % 2) as f32 * 1600.0 + 100.0, (k / 2) as f32 * 950.0 + 80.0);
        let b = e.elements()[idx].bounds(bluesnap_core::shape::Metrics { scale: 2.0, fonts: &Fonts::empty() });
        drag(&mut e, (b.x + 40.0, b.y + 40.0), (target.0 + 40.0, target.1 + 40.0));
    }
    e.clear_tool_or_selection();
    let tools = [Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Pen, Tool::Highlighter];
    for i in 0..n_shapes {
        let tool = tools[i % tools.len()];
        e.toggle_tool(tool);
        let (x, y) = (80.0 + (i % 6) as f32 * 580.0, 80.0 + (i / 6) as f32 * 240.0);
        drag(&mut e, (x, y), (x + 360.0, y + 180.0));
        e.toggle_tool(tool);
    }
    e.clear_tool_or_selection();
    e
}

fn composition(c: &mut Criterion) {
    let mut g = c.benchmark_group("composition");
    g.sample_size(15);

    // Plain click on an existing shape to select it (down + up without moving).
    g.bench_function("click_select_shape/4k_50_shapes", |b| {
        let mut e = multi_image_editor(2, 1600, 1000, 50);
        let clip = e.screen();
        let mut presenter = BandPresenter::default();
        presenter.present(&e, clip);
        b.iter(|| {
            e.clear_tool_or_selection();
            e.pointer_down(Point::new(80.0, 80.0)); // top-left edge of Rect #0
            let down_bytes = presenter.present(&e, clip);
            e.pointer_up(Point::new(80.0, 80.0));
            let up_bytes = presenter.present(&e, clip);
            black_box(down_bytes + up_bytes)
        });
    });

    // Starting a new shape (`pointer_down` + degenerate `pointer_up`) over 50 static shapes.
    g.bench_function("start_new_stroke/4k_50_shapes", |b| {
        let mut e = multi_image_editor(2, 1600, 1000, 50);
        e.toggle_tool(Tool::Rect);
        let clip = e.screen();
        let mut presenter = BandPresenter::default();
        presenter.present(&e, clip);
        b.iter(|| {
            e.pointer_down(Point::new(3700.0, 2050.0));
            let down_bytes = presenter.present(&e, clip);
            e.pointer_up(Point::new(3700.0, 2050.0));
            let up_bytes = presenter.present(&e, clip);
            black_box(down_bytes + up_bytes)
        });
    });

    // Full gesture moving a placed image (`pointer_down` + 8 moves + `pointer_up`) with 40 annotations.
    g.bench_function("drag_placed_image_gesture/4k_4_images_40_shapes", |b| {
        let mut e = multi_image_editor(4, 1200, 800, 40);
        let clip = e.screen();
        let mut presenter = BandPresenter::default();
        presenter.present(&e, clip);
        let mut dir = 1.0f32;
        b.iter(|| {
            // Find current center of image #0 so we hit it cleanly.
            let img_bounds = e.elements()[0].as_image().unwrap().bounds;
            let start = (img_bounds.x + img_bounds.w * 0.5, img_bounds.y + img_bounds.h * 0.5);
            let end = (start.0 + 40.0 * dir, start.1 + 24.0 * dir);
            dir = -dir;
            let mut total = 0;
            e.pointer_down(Point::new(start.0, start.1));
            total += presenter.present(&e, clip);
            for step in 1..=8 {
                let t = step as f32 / 8.0;
                e.pointer_move(Point::new(start.0 + (end.0 - start.0) * t, start.1 + (end.1 - start.1) * t), false);
                total += presenter.present(&e, clip);
            }
            e.pointer_up(Point::new(end.0, end.1));
            total += presenter.present(&e, clip);
            black_box(total)
        });
    });

    // Live Blur redaction on a tall 1600x12000 composition when underlying image moves slightly.
    g.bench_function("live_blur_tall_canvas/1600x12000_small_blur", |b| {
        let tall = pixmap_from_rgba(1600, 12_000, text_page(1600, 12_000)).unwrap();
        let comp = Composition::from_slice(tall, 2.0, Vec::new()).unwrap();
        let mut e = Editor::from_composition(comp, fonts());
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| (r.area, r.effect) = (RedactArea::Rect, RedactEffect::Blur));
        drag(&mut e, (200.0, 5000.0), (500.0, 5120.0));
        e.toggle_tool(Tool::Redact);
        let clip = Rect::new(0.0, 4500.0, 1600.0, 1500.0);
        let mut presenter = BandPresenter::default();
        presenter.present(&e, clip);
        // Select the blur redaction and vary its strength slider.
        e.pointer_down(Point::new(350.0, 5060.0));
        e.pointer_up(Point::new(350.0, 5060.0));
        e.begin_continuous_edit();
        let mut step = 0u32;
        b.iter(|| {
            step += 1;
            // Also invalidate when image shifts vs slider change: test cold backdrop + blur on tall canvas!
            e.set_redaction(|r| r.strength = 0.2 + (step % 25) as f32 * 0.03);
            black_box(presenter.present(&e, clip))
        });
        e.end_continuous_edit();
    });

    // Cold live Blur redaction after image move on a tall 1600x12000 canvas.
    g.bench_function("live_blur_after_image_move/1600x12000", |b| {
        let tall = pixmap_from_rgba(1600, 12_000, text_page(1600, 12_000)).unwrap();
        let comp = Composition::from_slice(tall, 2.0, Vec::new()).unwrap();
        let mut e = Editor::from_composition(comp, fonts());
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| (r.area, r.effect) = (RedactArea::Rect, RedactEffect::Blur));
        drag(&mut e, (200.0, 5000.0), (500.0, 5120.0));
        e.toggle_tool(Tool::Redact);
        let clip = Rect::new(0.0, 4500.0, 1600.0, 1500.0);
        let mut presenter = BandPresenter::default();
        presenter.present(&e, clip);
        e.pointer_down(Point::new(800.0, 2000.0)); // grab underlying tall image
        e.pointer_move(Point::new(820.0, 2000.0), false); // cross drag threshold once before timing
        presenter.present(&e, clip);
        let mut step = 0u32;
        b.iter(|| {
            step += 1;
            e.pointer_move(Point::new(820.0 + (step % 16) as f32 * 3.0, 2000.0), false);
            black_box(presenter.present(&e, clip))
        });
        e.pointer_up(Point::new(820.0, 2000.0));
    });

    // Multi-image export with a large 1600x1000 Blur redaction (`render_composition`).
    g.bench_function("export_multi_image_blur/4k_4_images_1600x1000_blur", |b| {
        let mut e = multi_image_editor(4, 1200, 800, 20);
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| (r.area, r.effect) = (RedactArea::Rect, RedactEffect::Blur));
        drag(&mut e, (600.0, 400.0), (2200.0, 1400.0));
        e.toggle_tool(Tool::Redact);
        b.iter(|| black_box(e.export()));
    });

    g.finish();
}

fn bs_format(c: &mut Criterion) {
    let mut g = c.benchmark_group("bs_format");
    g.sample_size(10);

    // Build a 4-image (4 × 1280x720 noisy screenshots) composition with 30 long pen/highlight strokes.
    let make_scene = || {
        let first = pixmap_from_rgba(1280, 720, screenshot(1280, 720)).unwrap();
        let mut comp = Composition::from_slice(first, 2.0, Vec::new()).unwrap();
        comp.width = 2800;
        comp.height = 1800;
        let mut e = Editor::from_composition(comp, fonts());
        for _ in 1..4 {
            let img = pixmap_from_rgba(1280, 720, screenshot(1280, 720)).unwrap();
            e.append_slice(img, 2.0, Vec::new());
        }
        for s in 0..30 {
            let tool = if s % 2 == 0 { Tool::Pen } else { Tool::Highlighter };
            e.toggle_tool(tool);
            let y0 = 60.0 + (s as f32) * 52.0;
            e.pointer_down(Point::new(60.0, y0));
            for p in 1..=250 {
                let x = 60.0 + p as f32 * 8.0;
                let y = y0 + (p as f32 * 0.25).sin() * 18.0;
                e.pointer_move(Point::new(x, y), false);
            }
            e.pointer_up(Point::new(2060.0, y0));
            e.toggle_tool(tool);
        }
        e.to_composition().unwrap()
    };

    let warm_comp = make_scene();
    let encoded_bytes = encode_bs(&warm_comp).unwrap();

    g.bench_function("encode_cold/4x720p_30_strokes", |b| {
        b.iter_batched(make_scene, |comp| black_box(encode_bs(&comp).unwrap()), BatchSize::LargeInput);
    });

    g.bench_function("encode_resave/4x720p_30_strokes", |b| {
        b.iter(|| black_box(encode_bs(&warm_comp).unwrap()));
    });

    g.bench_function("decode/4x720p_30_strokes", |b| {
        b.iter(|| black_box(decode_bs(&encoded_bytes).unwrap()));
    });

    // Pure vector/stroke serialization benchmark (1 tiny image + 50 strokes × 400 points = 20,000 points).
    let stroke_comp = {
        let tiny = pixmap_from_rgba(16, 16, screenshot(16, 16)).unwrap();
        let mut comp = Composition::from_slice(tiny, 1.0, Vec::new()).unwrap();
        comp.width = 2400;
        comp.height = 1600;
        let mut e = Editor::from_composition(comp, fonts());
        for s in 0..50 {
            e.toggle_tool(Tool::Pen);
            let y0 = 20.0 + s as f32 * 28.0;
            e.pointer_down(Point::new(20.0, y0));
            for p in 1..=400 {
                e.pointer_move(Point::new(20.0 + p as f32 * 5.0, y0 + (p as f32 * 0.2).sin() * 10.0), false);
            }
            e.pointer_up(Point::new(2020.0, y0));
            e.toggle_tool(Tool::Pen);
        }
        e.to_composition().unwrap()
    };
    let stroke_bytes = encode_bs(&stroke_comp).unwrap();

    g.bench_function("encode_strokes/20k_points", |b| {
        b.iter(|| black_box(encode_bs(&stroke_comp).unwrap()));
    });
    g.bench_function("decode_strokes/20k_points", |b| {
        b.iter(|| black_box(decode_bs(&stroke_bytes).unwrap()));
    });

    g.finish();
}

fn mosaic_grid_composition(scale: u32, fonts: &Arc<Fonts>) -> (Composition, Point) {
    let (w, h) = (1920 * scale, 1080 * scale);
    let doc = Document::from_rgba(w, h, screenshot(w, h)).unwrap();
    let mut editor = Editor::for_document(doc, fonts.clone(), 1.0).unwrap();
    let mut rects = Vec::new();
    for row in 0..8 {
        for col in 0..8 {
            let rect = Rect::new(128.0 + col as f32 * 208.0, 96.0 + row as f32 * 120.0, 96.0, 32.0);
            rects.push(rect.scale(scale as f32));
        }
    }
    editor.set_redaction(|r| r.effect = RedactEffect::Mosaic);
    assert_eq!(editor.add_redactions(&rects), rects.len());
    (editor.to_composition().unwrap(), rects[0].center())
}

/// Frame costs for editing a grid of small mosaics over a generated image.
fn mosaic_grid(c: &mut Criterion) {
    let fonts = Arc::new(Fonts::empty());
    let (comp, mosaic) = mosaic_grid_composition(1, &fonts);
    let (large, _) = mosaic_grid_composition(2, &fonts);
    let image = Point::new(32.0, 32.0);
    let mut g = c.benchmark_group("mosaic_grid");
    g.sample_size(10);
    g.bench_function("first_frame", |b| {
        b.iter_batched(
            || Editor::from_composition(comp.clone(), fonts.clone()),
            |e| black_box(BandPresenter::default().present(&e, e.screen())),
            BatchSize::LargeInput,
        );
    });
    for (name, scene, start, index, step, fractional_zoom) in [
        ("move_mosaic", &comp, mosaic, 1, 16.0, false),
        ("move_background", &comp, image, 0, 16.0, false),
        ("move_background_subpixel", &comp, image, 0, 16.25, false),
        ("move_background_fit_1080p", &comp, image, 0, 16.25, true),
        ("move_background_fit_4k", &large, image, 0, 16.25, true),
    ] {
        g.bench_function(name, |b| {
            let mut e = Editor::from_composition(scene.clone(), fonts.clone());
            let clip = e.screen();
            let mut presenter = BandPresenter { fractional_zoom, ..BandPresenter::default() };
            presenter.present(&e, clip);
            e.pointer_down(start);
            assert_eq!(e.selected_index(), Some(index));
            let mut offset = step;
            e.pointer_move(start + Point::new(offset, 0.0), false);
            presenter.present(&e, clip);
            b.iter(|| {
                offset = -offset;
                e.pointer_move(start + Point::new(offset, 0.0), false);
                black_box(presenter.present(&e, clip))
            });
        });
    }
    g.bench_function("adjust_redaction_strength", |b| {
        let mut e = Editor::from_composition(comp.clone(), fonts.clone());
        let clip = e.screen();
        let mut presenter = BandPresenter::default();
        e.pointer_down(mosaic);
        e.pointer_up(mosaic);
        assert_eq!(e.selected_index(), Some(1));
        e.begin_continuous_edit();
        let mut strength = 0.25;
        e.set_redaction(|r| r.strength = strength);
        presenter.present(&e, clip);
        b.iter(|| {
            strength = 1.0 - strength;
            e.set_redaction(|r| r.strength = strength);
            black_box(presenter.present(&e, clip))
        });
        e.end_continuous_edit();
    });
    g.finish();
}

criterion_group!(benches, startup, frames, output, redaction, tools, scrolling, composition, bs_format, mosaic_grid);
criterion_main!(benches);
