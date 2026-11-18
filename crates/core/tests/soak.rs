//! Soak, fuzz, Z-order visual-equivalence, and GPU-cache-invalidation tests for
//! the multi-image Composition Editor, `VisualBand` pipeline, and `.bs` BSON format.

use std::sync::Arc;

use bluesnap_core::bson::{self, Doc, Value};
use bluesnap_core::editor::MAX_HISTORY;
use bluesnap_core::format::{decode_bs, encode_bs};
use bluesnap_core::geom::Handle;
use bluesnap_core::pixels::{pixmap_from_rgba, to_rgba_straight};
use bluesnap_core::render::draw_placed_image;
use bluesnap_core::shape::{Geometry, Metrics, RedactShape, Shape};
use bluesnap_core::{
    ArrowStyle, BACKGROUND_PALETTE, Color, Composition, CounterFormat, Editor, Element, FontSet, Fonts, ImageAsset,
    LayerOrder, PALETTE, Pixmap, PlacedImage, Point, Rect, RedactArea, RedactEffect, SizeLevel, Style, TextKey,
    TextStyle, Tool, VisualBand,
};

fn test_fonts() -> Arc<Fonts> {
    Arc::new(Fonts::sans_only(FontSet::load(&[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        r"C:\Windows\Fonts\segoeui.ttf",
    ])))
}

fn patterned_pixmap(w: u32, h: u32, seed0: u32) -> Pixmap {
    let mut seed = seed0.max(1);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let [r, g, b, _] = seed.to_le_bytes();
            rgba.extend_from_slice(&[
                r.wrapping_add((x * 3) as u8),
                g.wrapping_add((y * 5) as u8),
                b,
                255,
            ]);
        }
    }
    pixmap_from_rgba(w, h, rgba).unwrap()
}

fn solid_pixmap(w: u32, h: u32, c: [u8; 4]) -> Pixmap {
    pixmap_from_rgba(w, h, c.repeat((w * h) as usize)).unwrap()
}

/// Stateful simulator of `CanvasView::sync_gpu_resources` + `LayerBandPool`:
/// retains cached `Pixmap`s per `VisualBand::Shapes` slot at native zoom and
/// only renders them again when the revision, range, clip or active state changes.
/// Catching any missing `touch_shapes()` / `touch_active()` or `CanvasBackdrop`
/// invalidation bug immediately!
type BandKey = (u64, usize, usize, Rect, bool);
type BandSlot = (Option<BandKey>, Vec<(Rect, Pixmap)>);

#[derive(Default)]
struct GpuBandSimulator {
    slots: Vec<BandSlot>,
}

impl GpuBandSimulator {
    /// Syncs shape layer bands against `editor`, returning the total number of
    /// rasterized layer bytes produced on this frame (`0` on full cache hit).
    fn sync(&mut self, editor: &Editor, clip: Rect) -> usize {
        let bands = editor.visual_bands();
        let shape_count = bands.iter().filter(|b| matches!(b, VisualBand::Shapes { .. })).count();
        self.slots.truncate(shape_count);
        while self.slots.len() < shape_count {
            self.slots.push((None, Vec::new()));
        }
        let mut slot_idx = 0;
        let mut rasterized_bytes = 0;
        for band in bands {
            if let VisualBand::Shapes { range, revision, active } = band {
                let key = (revision, range.start, range.end, clip, active);
                let (cached_key, cached_img) = &mut self.slots[slot_idx];
                if *cached_key != Some(key) {
                    *cached_key = Some(key);
                    cached_img.clear();
                    editor.render_band_parts(range, active, clip, true, |r, p| {
                        rasterized_bytes += p.data().len();
                        cached_img.push((r, p));
                    });
                }
                slot_idx += 1;
            }
        }
        rasterized_bytes
    }

    /// Composites the current live GPU scene (`background + VisualBand`s using
    /// the cached `LayerBandPool` textures) onto an opaque canvas `Pixmap`.
    fn composite(&mut self, editor: &Editor) -> Pixmap {
        let screen = editor.active_canvas_rect();
        self.sync(editor, screen);
        let (w, h) = ((screen.w.round() as u32).max(1), (screen.h.round() as u32).max(1));
        let mut out = Pixmap::new(w, h).unwrap();
        let bg = editor.background_color().unwrap_or(Color::rgb(0xFFFFFF));
        out.fill(tiny_skia::Color::from_rgba8(bg.r, bg.g, bg.b, bg.a));

        let mut slot_idx = 0;
        for band in editor.visual_bands() {
            match band {
                VisualBand::Image(placed) => {
                    draw_placed_image(&mut out, &placed, Point::new(screen.x, screen.y));
                }
                VisualBand::Shapes { .. } => {
                    for (region, layer) in &self.slots[slot_idx].1 {
                        out.draw_pixmap(
                            (region.x - screen.x).round() as i32,
                            (region.y - screen.y).round() as i32,
                            layer.as_ref(),
                            &tiny_skia::PixmapPaint::default(),
                            tiny_skia::Transform::identity(),
                            None,
                        );
                    }
                    slot_idx += 1;
                }
            }
        }
        out
    }
}

fn assert_pixels_close(live: &Pixmap, exported: &Pixmap, context: &str) {
    assert_eq!(
        (live.width(), live.height()),
        (exported.width(), exported.height()),
        "{context}: dimension mismatch"
    );
    // Allow at most ±3 LSB rounding difference from 8-bit premultiplied layer quantization
    // on anti-aliased shape edges; any stale cache or wrong Z-order differs by 50–255!
    let mut max_diff = 0u8;
    let mut bad_at = (0u32, 0u32, [0u8; 4], [0u8; 4]);
    for (idx, (a, b)) in live.data().as_chunks::<4>().0.iter().zip(exported.data().as_chunks::<4>().0).enumerate() {
        let d = (0..4).map(|c| a[c].abs_diff(b[c])).max().unwrap();
        if d > max_diff {
            max_diff = d;
            let x = (idx as u32) % live.width();
            let y = (idx as u32) / live.width();
            bad_at = (x, y, *a, *b);
        }
    }
    assert!(
        max_diff <= 3,
        "{context}: live preview differs from export by {max_diff} at ({}, {}): live={:?}, export={:?}",
        bad_at.0,
        bad_at.1,
        bad_at.2,
        bad_at.3,
    );
}

#[test]
fn translated_opaque_backdrop_matches_nearest_shader_at_pixel_centers_and_edges() {
    let mut source = patterned_pixmap(640, 480, 42);
    let opaque = ImageAsset::new(source.clone());
    // An unused transparent corner forces the reference through tiny-skia.
    *source.pixels_mut().last_mut().unwrap() = tiny_skia::PremultipliedColorU8::TRANSPARENT;
    let reference = ImageAsset::new(source);
    for (x, y) in [(-3.25, 2.375), (1.5, -0.5), (0.500001, -0.499999), (-0.499999, 0.75), (-17.75, -5.25)] {
        let bounds = Rect::new(x, y, 640.0, 480.0);
        for origin in [Point::default(), Point::new(3.0, 7.0), Point::new(233.0, 184.0), Point::new(420.0, 38.0)] {
            let mut fast = Pixmap::new(256, 129).unwrap();
            let mut expected = fast.clone();
            draw_placed_image(&mut fast, &PlacedImage::new(opaque.clone(), bounds), origin);
            draw_placed_image(&mut expected, &PlacedImage::new(reference.clone(), bounds), origin);
            assert!(fast == expected, "sampling differs at image ({x}, {y}), backdrop {origin:?}");
        }
    }
}

#[test]
fn sparse_redaction_parts_match_a_single_layer_and_skip_empty_pixels() {
    let mut comp = Composition::from_slice(patterned_pixmap(700, 470, 31), 1.0, Vec::new()).unwrap();
    for (x, y, effect) in [
        (20.25, 30.5, RedactEffect::Mosaic),
        (90.5, 44.25, RedactEffect::Blur),
        (430.75, 350.5, RedactEffect::Mosaic),
        (492.25, 362.75, RedactEffect::Blur),
    ] {
        comp.elements.push(Element::Shape(Shape {
            geometry: Geometry::Redact { area: RedactShape::Rect(Rect::new(x, y, 150.0, 60.0)), effect, strength: 0.6 },
            style: Style::default(),
        }));
    }
    let editor = Editor::from_composition(comp, test_fonts());
    for clip in [editor.screen(), Rect::new(72.0, 55.0, 520.0, 360.0)] {
        let (bounds, expected) = editor.render_band(1..5, false, clip).unwrap();
        let mut actual = Pixmap::new(expected.width(), expected.height()).unwrap();
        let mut sizes = Vec::new();
        editor.render_band_parts(1..5, false, clip, true, |rect, image| {
            sizes.push(image.data().len());
            actual.draw_pixmap(
                (rect.x - bounds.x) as i32,
                (rect.y - bounds.y) as i32,
                image.as_ref(),
                &tiny_skia::PixmapPaint::default(),
                tiny_skia::Transform::identity(),
                None,
            );
        });
        assert_eq!(sizes.len(), 2);
        assert!(sizes.iter().sum::<usize>() < expected.data().len() / 2);
        assert!(actual == expected);
        let mut count = 0;
        editor.render_band_parts(1..5, false, clip, false, |r, image| {
            count += 1;
            assert_eq!(r, bounds);
            assert!(image == expected);
        });
        assert_eq!(count, 1);
    }
}

#[test]
fn live_bands_match_export_across_z_ordered_images_shapes_and_stacked_redactions() {
    let fonts = test_fonts();

    // Build a multi-layer Z-order torture scene:
    // [0] Bottom noisy image (320x240 at 0,0)
    // [1] Bright green Rect border at (40, 40, 160, 120) — sits UNDER Blur #2
    // [2] Blur redaction at (60, 50, 180, 130) — blurs [0] + [1], must NOT blur [3] or [4]!
    // [3] Arrow crossing over Blur #2
    // [4] Sharp solid-magenta PlacedImage (100x90 at 150, 70) sitting ON TOP of Blur #2!
    // [5] Mosaic redaction at (183, 93, 95, 75) (non-grid-aligned!) sitting ON TOP of [4] & [2]!
    let bottom_img = patterned_pixmap(320, 240, 0xCAFE_BABE);
    let top_magenta = solid_pixmap(100, 90, [220, 20, 180, 255]);

    let mut comp = Composition::new_empty(360, 280, 1.0).unwrap();
    comp.elements = vec![
        Element::Image(PlacedImage::new(ImageAsset::new(bottom_img), Rect::new(0.0, 0.0, 320.0, 240.0))),
        Element::Shape(Shape {
            geometry: Geometry::Rect(Rect::new(40.0, 40.0, 160.0, 120.0)),
            style: Style { color: Color::rgb(0x00E676), size: SizeLevel::Large },
        }),
        Element::Shape(Shape {
            geometry: Geometry::Redact {
                area: RedactShape::Rect(Rect::new(60.0, 50.0, 180.0, 130.0)),
                effect: RedactEffect::Blur,
                strength: 0.7,
            },
            style: Style::default(),
        }),
        Element::Shape(Shape {
            geometry: Geometry::Arrow {
                from: Point::new(50.0, 170.0),
                to: Point::new(230.0, 60.0),
                style: ArrowStyle::Curved,
            },
            style: Style { color: PALETTE[0], size: SizeLevel::Medium },
        }),
        Element::Image(PlacedImage::new(ImageAsset::new(top_magenta), Rect::new(150.0, 70.0, 100.0, 90.0))),
        Element::Shape(Shape {
            geometry: Geometry::Redact {
                area: RedactShape::Rect(Rect::new(183.0, 93.0, 95.0, 75.0)),
                effect: RedactEffect::Mosaic,
                strength: 0.5,
            },
            style: Style::default(),
        }),
    ];

    let mut editor = Editor::from_composition(comp, fonts);
    let mut gpu = GpuBandSimulator::default();

    let live = gpu.composite(&editor);
    let exported = editor.export().unwrap();
    assert_pixels_close(&live, &exported, "initial Z-order stack");

    // Verify top_magenta at (160, 80) — inside Blur #2's rect (60..240, 50..180)
    // but outside Mosaic #5's rect (183..278, 93..168) — is 100% sharp magenta in live preview!
    let px = live.pixel(160, 80).unwrap();
    assert_eq!((px.red(), px.green(), px.blue()), (220, 20, 180), "image above blur must stay sharp");

    // Now reorder `top_magenta` (currently selected at index 4) ToBack -> index 0 (underneath Blur #2!).
    assert_eq!(editor.selected_index(), Some(4));
    assert!(editor.reorder_selected(LayerOrder::ToBack));
    let live_after_back = gpu.composite(&editor);
    let export_after_back = editor.export().unwrap();
    assert_pixels_close(&live_after_back, &export_after_back, "after sending top image ToBack under Blur");

    // Bring it Forward step by step all the way ToFront, verifying stateful GPU cache at every step!
    while editor.reorder_selected(LayerOrder::Forward) {
        let l = gpu.composite(&editor);
        let x = editor.export().unwrap();
        assert_pixels_close(&l, &x, "stepping image Forward through redactions");
    }

    // Verify viewport-clipped `render_band` on a tall 400x3600 canvas matches unclipped export!
    let tall = patterned_pixmap(400, 3600, 0x1357_9BDF);
    let mut tall_comp = Composition::from_slice(tall, 1.0, Vec::new()).unwrap();
    tall_comp.elements.push(Element::Shape(Shape {
        geometry: Geometry::Redact {
            area: RedactShape::Rect(Rect::new(30.0, 1900.0, 260.0, 400.0)),
            effect: RedactEffect::Blur,
            strength: 0.65,
        },
        style: Style::default(),
    }));
    let mut tall_editor = Editor::from_composition(tall_comp, test_fonts());
    let sub_clip = Rect::new(0.0, 2000.0, 400.0, 200.0);
    let (band_rect, band_pix) = tall_editor.render_band(1..2, false, sub_clip).unwrap();
    assert!(band_pix.height() <= 202, "tall canvas band must be clipped to viewport, got {}", band_pix.height());
    let full_export = tall_editor.export().unwrap();
    for y in 10..band_pix.height().saturating_sub(10) {
        for x in 10..band_pix.width().saturating_sub(10) {
            let lp = band_pix.pixel(x, y).unwrap();
            if lp.alpha() == 255 {
                let ep = full_export.pixel(band_rect.x as u32 + x, band_rect.y as u32 + y).unwrap();
                assert_eq!(lp, ep, "clipped tall blur mismatch at ({x}, {y})");
            }
        }
    }
}

#[test]
fn zero_rasterization_invariants_for_click_select_new_stroke_and_image_drags() {
    let fonts = test_fonts();
    let img1 = patterned_pixmap(300, 200, 11);
    let img2 = patterned_pixmap(240, 180, 22);
    let mut comp = Composition::from_slice(img1, 1.0, Vec::new()).unwrap();
    comp.width = 800;
    comp.height = 600;
    let mut e = Editor::from_composition(comp, fonts);
    e.append_slice(img2, 1.0, Vec::new());
    e.clear_tool_or_selection();

    // Draw 15 mixed non-redaction shapes.
    for i in 0..15 {
        let tool = [Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Pen, Tool::Highlighter][i % 5];
        e.toggle_tool(tool);
        let (x, y) = (40.0 + (i % 5) as f32 * 140.0, 320.0 + (i / 5) as f32 * 80.0);
        e.pointer_down(Point::new(x, y));
        e.pointer_move(Point::new(x + 90.0, y + 50.0), false);
        e.pointer_up(Point::new(x + 90.0, y + 50.0));
        e.toggle_tool(tool);
    }
    e.clear_tool_or_selection();

    let mut gpu = GpuBandSimulator::default();
    let screen = e.screen();
    assert!(gpu.sync(&e, screen) > 0, "initial cold render populates static shape bands");
    assert_eq!(gpu.sync(&e, screen), 0, "second sync without edits must be a 100% cache hit");

    // 1. Plain click to select shape #0 at (40, 320) without dragging: 0 rasterized bytes!
    e.pointer_down(Point::new(40.0, 320.0));
    assert_eq!(gpu.sync(&e, screen), 0, "pointer_down on shape before drag threshold must not split static band");
    e.pointer_up(Point::new(40.0, 320.0));
    assert_eq!(gpu.sync(&e, screen), 0, "pointer_up on unmoved shape must not re-rasterize static band");
    assert!(e.selected_index().is_some());
    e.clear_tool_or_selection();
    assert_eq!(gpu.sync(&e, screen), 0);

    // 2. Degenerate click with Tool::Rect: 0 static band bytes re-rasterized!
    e.toggle_tool(Tool::Rect);
    e.pointer_down(Point::new(750.0, 550.0));
    let down_bytes = gpu.sync(&e, screen);
    // Active 0x0 degenerate rect is tiny (at most a few pixels of stroke cap), static band untouched!
    assert!(down_bytes < 4096, "starting a new stroke must not re-rasterize the 800x600 static band: got {down_bytes} bytes");
    e.pointer_up(Point::new(750.0, 550.0));
    assert_eq!(gpu.sync(&e, screen), 0, "cancelling degenerate stroke on release must reuse static band cache");
    e.toggle_tool(Tool::Rect);

    // 3. Dragging PlacedImage #0 across 10 frames + release: 0 shape bytes re-rasterized!
    e.pointer_down(Point::new(100.0, 100.0));
    assert!(e.selected_is_image());
    assert_eq!(gpu.sync(&e, screen), 0, "clicking PlacedImage must not re-rasterize shapes");
    for step in 1..=10 {
        e.pointer_move(Point::new(100.0 + step as f32 * 8.0, 100.0 + step as f32 * 4.0), false);
        assert_eq!(gpu.sync(&e, screen), 0, "dragging PlacedImage must be 0 CPU shape rasterization");
    }
    e.pointer_up(Point::new(180.0, 140.0));
    assert_eq!(gpu.sync(&e, screen), 0, "releasing PlacedImage must be 0 CPU shape rasterization");

    // 4. Resizing PlacedImage #0 via Bottom handle with Shift held: 0 shape bytes re-rasterized!
    let img_bounds = e.elements()[0].as_image().unwrap().bounds;
    let bottom = img_bounds.handle_point(Handle::Bottom);
    e.pointer_down(bottom);
    assert_eq!(gpu.sync(&e, screen), 0);
    e.pointer_move(Point::new(bottom.x, bottom.y + 40.0), true);
    assert_eq!(gpu.sync(&e, screen), 0);
    e.pointer_up(Point::new(bottom.x, bottom.y + 40.0));
    assert_eq!(gpu.sync(&e, screen), 0);

    let live = gpu.composite(&e);
    let exported = e.export().unwrap();
    assert_pixels_close(&live, &exported, "after zero-rasterization image moves and resizes");
}

#[test]
fn long_session_composition_soak_test() {
    let fonts = test_fonts();
    let mut max_observed_undo = 0usize;
    let mut total_pixel_checks = 0usize;
    let mut total_bs_roundtrips = 0usize;

    for session_seed in [0xD1B5_4A32_9E37_79B9u64, 0x94D0_49BB_1331_11EBu64, 0xBF58_476D_1CE4_E5B9u64] {
        let base = patterned_pixmap(240, 180, session_seed as u32);
        let mut e = Editor::from_composition(Composition::from_slice(base, 1.0, Vec::new()).unwrap(), fonts.clone());
        let mut gpu = GpuBandSimulator::default();

        let mut seed = session_seed;
        let mut rng = |n: u32| -> u32 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed as u32) % n.max(1)
        };

        for step in 0..6_000usize {
            let scr = e.active_canvas_rect();
            let px = scr.x + (rng((scr.w as u32).saturating_add(40)) as f32) - 20.0;
            let py = scr.y + (rng((scr.h as u32).saturating_add(40)) as f32) - 20.0;
            let p = Point::new(px, py);

            match rng(32) {
                0..=5 => e.pointer_down(p),
                6..=12 => e.pointer_move(p, rng(3) == 0),
                13..=16 => e.pointer_up(p),
                17 => e.constrain_changed(rng(2) == 0),
                18 => e.toggle_tool(Tool::ALL[rng(Tool::ALL.len() as u32) as usize]),
                19 => {
                    if rng(3) == 0 {
                        e.clear_tool_or_selection();
                    }
                }
                20 => {
                    if rng(16) == 0 {
                        for _ in 0..rng(4) {
                            e.undo();
                        }
                    } else if rng(3) == 0 {
                        e.undo();
                    }
                }
                21 => {
                    if e.elements().len() > 2 || rng(4) == 0 {
                        e.delete_selected();
                    }
                }
                22 => {
                    let sample = ["Hello", "测试", "🚀", "key=42", "\n", ""][rng(6) as usize];
                    e.text_replace(None, sample, rng(3) == 0, None);
                }
                23 => {
                    let k = [
                        TextKey::Backspace,
                        TextKey::Delete,
                        TextKey::Left,
                        TextKey::Right,
                        TextKey::Home,
                        TextKey::End,
                        TextKey::Newline,
                    ][rng(7) as usize];
                    e.text_key(k);
                }
                24 => e.finish_text(),
                25 => {
                    e.set_color(PALETTE[rng(PALETTE.len() as u32) as usize]);
                    e.set_size(SizeLevel::ALL[rng(3) as usize]);
                }
                26 => {
                    let eff = RedactEffect::ALL[rng(3) as usize];
                    let area = RedactArea::ALL[rng(2) as usize];
                    e.set_redaction(|r| (r.effect, r.area) = (eff, area));
                }
                27 => {
                    e.begin_continuous_edit();
                    for _ in 0..3 {
                        let s = rng(101) as f32 / 100.0;
                        e.set_redaction(|r| r.strength = s);
                        gpu.sync(&e, e.active_canvas_rect());
                    }
                    e.end_continuous_edit();
                }
                28 => {
                    e.set_text_style(TextStyle::ALL[rng(TextStyle::ALL.len() as u32) as usize]);
                    e.set_arrow_style(ArrowStyle::ALL[rng(3) as usize]);
                    e.set_counter_format(CounterFormat::ALL[rng(4) as usize]);
                }
                29 => {
                    let order = [LayerOrder::Backward, LayerOrder::Forward, LayerOrder::ToBack, LayerOrder::ToFront]
                        [rng(4) as usize];
                    e.reorder_selected(order);
                }
                30 => {
                    let img_count = e.elements().iter().filter(|el| matches!(el, Element::Image(_))).count();
                    if img_count < 5 && e.screen().w <= 900.0 && e.screen().h <= 900.0 {
                        let slice = patterned_pixmap(80 + rng(80), 60 + rng(60), step as u32 + 1);
                        let slice_scale = [1.0, 1.5, 2.0][rng(3) as usize];
                        e.append_slice(slice, slice_scale, Vec::new());
                    }
                }
                _ => {
                    if rng(2) == 0 {
                        let _ = e.right_click_at(p);
                        let (bg, _) = BACKGROUND_PALETTE[rng(BACKGROUND_PALETTE.len() as u32) as usize];
                        e.set_background_color(bg);
                    } else if !e.is_dragging() && !e.is_editing_text() && e.screen().w <= 800.0 && e.screen().h <= 800.0 {
                        let handles = e.canvas_handles();
                        if !handles.is_empty() {
                            let h_pt = handles[rng(handles.len() as u32) as usize];
                            let lock = rng(2) == 0;
                            let dx = rng(41) as f32 - 18.0;
                            let dy = rng(41) as f32 - 18.0;
                            e.pointer_down(h_pt);
                            e.pointer_move(Point::new(h_pt.x + dx, h_pt.y + dy), lock);
                            gpu.sync(&e, e.active_canvas_rect());
                            e.pointer_up(Point::new(h_pt.x + dx, h_pt.y + dy));
                            let _ = e.take_canvas_origin_shift();
                        }
                    } else {
                        e.right_click_at(p);
                    }
                }
            }

            // Invariant 1: Undo history strictly bounded by MAX_HISTORY (200).
            let depth = e.undo_depth();
            max_observed_undo = max_observed_undo.max(depth);
            assert!(depth <= MAX_HISTORY, "undo history exceeded MAX_HISTORY: {depth} > {MAX_HISTORY}");

            // Invariant 2: Counter numbers are always contiguous 1..=K in Z-order, and bounds are finite.
            let mut expected_counter = 0u32;
            let m = Metrics { scale: e.scale(), fonts: &fonts };
            for el in e.elements() {
                let b = el.bounds(m);
                assert!(
                    b.x.is_finite() && b.y.is_finite() && b.w.is_finite() && b.h.is_finite() && b.w >= 0.0 && b.h >= 0.0,
                    "non-finite or negative element bounds at step {step}: {b:?}"
                );
                if let Element::Shape(Shape { geometry: Geometry::Counter { number, .. }, .. }) = el {
                    expected_counter += 1;
                    assert_eq!(*number, expected_counter, "counter sequence gap at step {step}");
                }
            }

            gpu.sync(&e, e.active_canvas_rect());
            let _ = e.cursor();
            let _ = e.selected_frame();
            let _ = e.text_caret_rect();
            let _ = e.brush_ring();

            if step % 200 == 199 {
                if e.is_dragging() {
                    e.pointer_up(p);
                }
                if e.is_editing_text() {
                    e.finish_text();
                }
                let live = gpu.composite(&e);
                let exported = e.export().unwrap();
                assert_pixels_close(&live, &exported, &format!("seed {session_seed:#x} step {step} live vs export"));
                total_pixel_checks += 1;

                let comp = e.to_composition().unwrap();
                let bs = encode_bs(&comp).unwrap();
                let decoded = decode_bs(&bs).unwrap();
                assert_eq!(decoded, comp, "seed {session_seed:#x} step {step} .bs structural round-trip");
                let mut reopened = Editor::from_composition(decoded, fonts.clone());
                let reopened_export = reopened.export().unwrap();
                assert_eq!(exported.data(), reopened_export.data(), "seed {session_seed:#x} step {step} .bs pixel round-trip");
                total_bs_roundtrips += 1;
            }
        }

        // Explicitly push 250 guaranteed undoable edits to saturate MAX_HISTORY (200),
        // verify 20 no-op selection clicks do NOT evict any undo entries, then drain all 200 undos.
        if e.is_dragging() {
            e.pointer_up(Point::new(10.0, 10.0));
        }
        e.finish_text();
        e.clear_tool_or_selection();
        for i in 0..250 {
            let added = e.add_redactions(&[Rect::new(2.0 + (i % 4) as f32, 2.0, 14.0, 14.0)]);
            assert_eq!(added, 1);
            assert!(e.undo_depth() <= MAX_HISTORY);
            max_observed_undo = max_observed_undo.max(e.undo_depth());
        }
        assert_eq!(e.undo_depth(), MAX_HISTORY);
        for _ in 0..20 {
            e.pointer_down(Point::new(8.0, 8.0));
            e.pointer_up(Point::new(8.0, 8.0));
        }
        assert_eq!(e.undo_depth(), MAX_HISTORY, "no-op selection clicks must never drop undo history");
        let mut undone = 0;
        while e.undo() {
            undone += 1;
        }
        assert_eq!(undone, MAX_HISTORY);
        assert!(!e.can_undo());
        let live_end = gpu.composite(&e);
        let export_end = e.export().unwrap();
        assert_pixels_close(&live_end, &export_end, "after full undo stack drain");
    }

    assert_eq!(max_observed_undo, MAX_HISTORY);
    assert_eq!(total_pixel_checks, 90);
    assert_eq!(total_bs_roundtrips, 90);
}

#[test]
fn legacy_bs_points_array_and_raw_rgba_assets_remain_backward_compatible() {
    let rgba = [12u8, 34, 56, 255].repeat(8 * 6);
    let mut img_doc = Doc::new();
    img_doc.push("id", Value::Int64(77));
    img_doc.push("width", Value::Int32(8));
    img_doc.push("height", Value::Int32(6));
    img_doc.push("encoding", Value::String("rgba".into()));
    img_doc.push("data", Value::Binary(rgba.clone()));

    let mk_pt = |x: f64, y: f64| {
        let mut d = Doc::new();
        d.push("x", Value::Double(x));
        d.push("y", Value::Double(y));
        Value::Document(d)
    };
    let mk_color = || {
        let mut d = Doc::new();
        d.push("r", Value::Int32(245));
        d.push("g", Value::Int32(34));
        d.push("b", Value::Int32(45));
        d.push("a", Value::Int32(255));
        Value::Document(d)
    };

    let mut placed_doc = Doc::new();
    placed_doc.push("kind", Value::String("image".into()));
    placed_doc.push("image_id", Value::Int64(77));
    let mut bounds = Doc::new();
    bounds.push("x", Value::Double(0.0)).push("y", Value::Double(0.0)).push("w", Value::Double(8.0)).push("h", Value::Double(6.0));
    placed_doc.push("bounds", Value::Document(bounds));

    let mut pen_doc = Doc::new();
    pen_doc.push("kind", Value::String("pen".into()));
    pen_doc.push("points", Value::Array(vec![mk_pt(1.0, 2.0), mk_pt(6.5, 4.25)]));
    pen_doc.push("color", mk_color());
    pen_doc.push("size", Value::String("medium".into()));

    let mut brush_doc = Doc::new();
    brush_doc.push("kind", Value::String("redact".into()));
    brush_doc.push("effect", Value::String("mosaic".into()));
    brush_doc.push("strength", Value::Double(0.4));
    brush_doc.push("area_kind", Value::String("brush".into()));
    brush_doc.push("points", Value::Array(vec![mk_pt(0.5, 0.5), mk_pt(7.0, 5.0)]));
    brush_doc.push("color", mk_color());
    brush_doc.push("size", Value::String("small".into()));

    let mut root = Doc::new();
    root.push("format", Value::String("bluesnap".into()));
    root.push("version", Value::Int32(1));
    root.push("width", Value::Int32(8));
    root.push("height", Value::Int32(6));
    root.push("scale", Value::Double(1.0));
    root.push("images", Value::Array(vec![Value::Document(img_doc)]));
    root.push("elements", Value::Array(vec![Value::Document(placed_doc), Value::Document(pen_doc), Value::Document(brush_doc)]));

    let legacy_bytes = bson::encode(&root).unwrap();
    let comp = decode_bs(&legacy_bytes).unwrap();
    assert_eq!(comp.elements.len(), 3);
    assert_eq!(to_rgba_straight(comp.elements[0].as_image().unwrap().asset.pixmap()), rgba);

    // Re-encoding upgrades strokes to compact `points_bin` and caches PNG losslessly.
    let modern_bytes = encode_bs(&comp).unwrap();
    let roundtripped = decode_bs(&modern_bytes).unwrap();
    assert_eq!(roundtripped, comp);
}

#[test]
fn adversarial_and_fuzz_bson_and_bs_decoder_soak_test() {
    // 1. PNG-bomb header mismatch: `.bs` claims 4x4, but embedded PNG header claims 65535x65535!
    // Must be rejected immediately from `reader.info()` BEFORE allocating multi-GB pixel memory.
    {
        let mut fake_png = Vec::new();
        fake_png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]); // PNG signature
        fake_png.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        fake_png.extend_from_slice(b"IHDR");
        fake_png.extend_from_slice(&65_535u32.to_be_bytes()); // width = 65535
        fake_png.extend_from_slice(&65_535u32.to_be_bytes()); // height = 65535
        fake_png.push(8); // bit depth 8
        fake_png.push(6); // RGBA
        fake_png.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
        fake_png.extend_from_slice(&0x8768_2B94u32.to_be_bytes()); // CRC (or invalid/valid IHDR)

        let mut img_doc = Doc::new();
        img_doc
            .push("id", Value::Int64(1))
            .push("width", Value::Int32(4))
            .push("height", Value::Int32(4))
            .push("encoding", Value::String("png".into()))
            .push("data", Value::Binary(fake_png));
        let mut root = Doc::new();
        root.push("format", Value::String("bluesnap".into()))
            .push("version", Value::Int32(1))
            .push("width", Value::Int32(4))
            .push("height", Value::Int32(4))
            .push("scale", Value::Double(1.0))
            .push("images", Value::Array(vec![Value::Document(img_doc)]))
            .push("elements", Value::Array(Vec::new()));
        let bytes = bson::encode(&root).unwrap();
        assert!(decode_bs(&bytes).is_err(), "mismatched/oversized PNG bomb IHDR must fail cleanly");
    }

    // 2. Canvas pixel-budget overflow (`65535 x 65535 = 4.29 GP > MAX_IMAGE_PIXELS`).
    {
        let mut root = Doc::new();
        root.push("format", Value::String("bluesnap".into()))
            .push("version", Value::Int32(1))
            .push("width", Value::Int32(65_535))
            .push("height", Value::Int32(65_535))
            .push("scale", Value::Double(1.0))
            .push("images", Value::Array(Vec::new()))
            .push("elements", Value::Array(Vec::new()));
        assert!(decode_bs(&bson::encode(&root).unwrap()).is_err());
    }

    // 3. Extreme finite f32 (`1e30`), NaN, Inf, negative rect sizes, unaligned/empty `points_bin`.
    for bad_coord in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.0e30, -1.0e30] {
        let mut bounds = Doc::new();
        bounds.push("x", Value::Double(bad_coord)).push("y", Value::Double(0.0)).push("w", Value::Double(10.0)).push("h", Value::Double(10.0));
        let mut color = Doc::new();
        color.push("r", Value::Int32(0)).push("g", Value::Int32(0)).push("b", Value::Int32(0)).push("a", Value::Int32(255));
        let mut el = Doc::new();
        el.push("kind", Value::String("rect".into()))
            .push("bounds", Value::Document(bounds))
            .push("color", Value::Document(color))
            .push("size", Value::String("small".into()));
        let mut root = Doc::new();
        root.push("format", Value::String("bluesnap".into()))
            .push("version", Value::Int32(1))
            .push("width", Value::Int32(100))
            .push("height", Value::Int32(100))
            .push("scale", Value::Double(1.0))
            .push("images", Value::Array(Vec::new()))
            .push("elements", Value::Array(vec![Value::Document(el)]));
        assert!(decode_bs(&bson::encode(&root).unwrap()).is_err(), "bad coordinate {bad_coord} must be rejected");
    }

    for bad_bin in [vec![], vec![1, 2, 3, 4, 5, 6, 7]] {
        let mut color = Doc::new();
        color.push("r", Value::Int32(0)).push("g", Value::Int32(0)).push("b", Value::Int32(0)).push("a", Value::Int32(255));
        let mut el = Doc::new();
        el.push("kind", Value::String("pen".into()))
            .push("points_bin", Value::Binary(bad_bin))
            .push("color", Value::Document(color))
            .push("size", Value::String("small".into()));
        let mut root = Doc::new();
        root.push("format", Value::String("bluesnap".into()))
            .push("version", Value::Int32(1))
            .push("width", Value::Int32(100))
            .push("height", Value::Int32(100))
            .push("scale", Value::Double(1.0))
            .push("images", Value::Array(Vec::new()))
            .push("elements", Value::Array(vec![Value::Document(el)]));
        assert!(decode_bs(&bson::encode(&root).unwrap()).is_err());
    }

    // 4. Deeply nested raw BSON exceeding MAX_DEPTH (60 nested documents).
    {
        let mut raw = vec![0u8; 5];
        raw[0] = 5;
        for _ in 0..60 {
            let mut outer = Vec::with_capacity(raw.len() + 8);
            let total = (raw.len() + 4 + 1 + 2 + 1) as i32;
            outer.extend_from_slice(&total.to_le_bytes());
            outer.push(0x03); // TAG_DOCUMENT
            outer.extend_from_slice(b"a\0");
            outer.extend_from_slice(&raw);
            outer.push(0);
            raw = outer;
        }
        assert!(bson::decode(&raw).is_err(), "deep BSON nesting must return Err without stack overflow");
    }

    // 5. Mutation fuzzing over a complete `.bs` seed corpus (truncations, bit-flips, byte splice/overwrite).
    let seed_comp = {
        let img = patterned_pixmap(24, 16, 42);
        let shapes = vec![
            Shape { geometry: Geometry::Rect(Rect::new(2.0, 2.0, 12.0, 8.0)), style: Style::default() },
            Shape { geometry: Geometry::Pen(vec![Point::new(1.0, 1.0), Point::new(10.0, 9.0), Point::new(18.0, 4.0)]), style: Style::default() },
            Shape {
                geometry: Geometry::Redact {
                    area: RedactShape::Brush(vec![Point::new(2.0, 3.0), Point::new(15.0, 12.0)]),
                    effect: RedactEffect::Blur,
                    strength: 0.5,
                },
                style: Style::default(),
            },
            Shape {
                geometry: Geometry::Text { origin: Point::new(4.0, 6.0), content: "fuzz 测试".into(), style: TextStyle::Boxed },
                style: Style::default(),
            },
        ];
        Composition::from_slice(img, 1.0, shapes).unwrap()
    };
    let valid_bs = encode_bs(&seed_comp).unwrap();
    let fonts = Arc::new(Fonts::empty());

    // Every single prefix truncation must return `Err` cleanly.
    for cut in 0..valid_bs.len() {
        assert!(decode_bs(&valid_bs[..cut]).is_err(), "truncated .bs at {cut}/{} must fail", valid_bs.len());
    }

    // 2,000 deterministic random mutations + pure random byte buffers.
    let mut state = 0xFEED_FACE_CAFE_BEEFu64;
    let mut next_u64 = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for case in 0..5_000usize {
        let buf = if case % 4 == 0 {
            // Pure pseudo-random bytes with valid-looking BSON length header half the time.
            let len = (next_u64() as usize % 256).max(1);
            let mut b: Vec<u8> = (0..len).map(|_| next_u64() as u8).collect();
            if case % 8 == 0 && b.len() >= 5 {
                let l = (b.len() as i32).to_le_bytes();
                b[..4].copy_from_slice(&l);
                *b.last_mut().unwrap() = 0;
            }
            b
        } else {
            let mut mutated = valid_bs.clone();
            let edits = 1 + (next_u64() as usize % 6);
            for _ in 0..edits {
                let idx = next_u64() as usize % mutated.len();
                match next_u64() % 4 {
                    0 => mutated[idx] ^= 1 << (next_u64() % 8),
                    1 => mutated[idx] = next_u64() as u8,
                    2 => {
                        mutated.remove(idx);
                        if mutated.is_empty() {
                            mutated.push(0);
                        }
                    }
                    _ => mutated.insert(idx, next_u64() as u8),
                }
            }
            // Fix up outer BSON document length half the time so inner parser logic gets exercised deeply!
            if case % 2 == 0 && mutated.len() >= 5 {
                let l = (mutated.len() as i32).to_le_bytes();
                mutated[..4].copy_from_slice(&l);
                *mutated.last_mut().unwrap() = 0;
            }
            mutated
        };

        let _ = bson::decode(&buf);
        if let Ok(comp) = decode_bs(&buf) {
            // Any mutated `.bs` accepted by `decode_bs` must ALSO render & edit without panicking!
            let mut ed = Editor::from_composition(comp, fonts.clone());
            let _ = ed.export();
        }
    }
}
