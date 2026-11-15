//! Benchmarks for the work done between a hotkey press and a visible overlay, and
//! for the per-frame work while editing. Run with `cargo bench -p bluesnap-core`.
//!
//! "frame" benchmarks reproduce what the overlay does for one mouse move: feed the
//! event to the editor, re-rasterize the annotation layers that changed and convert
//! them to the BGRA layout uploaded to the GPU.

use std::hint::black_box;
use std::sync::Arc;

use bluesnap_core::pixels::split_bgrx;
use bluesnap_core::render::{swap_red_blue, to_bgra_straight};
use bluesnap_core::{Document, Editor, FontSet, Fonts, Point, RedactArea, RedactEffect, Rect, TextStyle, Tool};
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
    let windows = (0..50).map(|i| Rect::new(i as f32 * 30.0, i as f32 * 20.0, 800.0, 600.0)).collect();
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
                black_box(Editor::new(doc, Arc::new(Fonts::empty()), Vec::new(), 2.0))
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

criterion_group!(benches, startup, frames, output, redaction, tools);
criterion_main!(benches);
