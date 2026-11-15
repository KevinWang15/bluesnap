//! CPU rasterization of annotations with tiny-skia.
//!
//! The same code renders the live annotation layer and the final exported image.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Paint, PathBuilder, Pattern, Pixmap, SpreadMode, Stroke, Transform,
};

use crate::geom::{Point, Rect};
pub use crate::pixels::{swap_red_blue, to_bgra_straight, to_rgba_straight};
use crate::pixels::{blur_margin, crop, gaussian_blur, par_map, pixelate, pixmap_from_rgba};
use crate::shape::{Geometry, Metrics, RedactShape, Shape, arrow_head_len};
use crate::style::{Color, REDACT_GRAY, RedactEffect};

/// The frozen screenshot an editing session works on.
pub struct Document {
    image: Pixmap,
    effects: EffectCache,
}

impl Document {
    /// Wraps opaque RGBA8 pixels. Returns `None` if `rgba` does not match the size.
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let image = pixmap_from_rgba(width, height, rgba)?;
        Some(Self { image, effects: EffectCache::default() })
    }

    pub fn width(&self) -> u32 {
        self.image.width()
    }

    pub fn height(&self) -> u32 {
        self.image.height()
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width() as f32, self.height() as f32)
    }

    pub fn image(&self) -> &Pixmap {
        &self.image
    }

    /// The screenshot under `area` with `effect` applied at `amount` (see
    /// [`RedactEffect::amount`]), and the image position of its top-left corner
    /// (the area is rounded out to whole tiles).
    pub fn effect_region(&self, effect: RedactEffect, amount: u32, area: Rect) -> Option<(Pixmap, (u32, u32))> {
        self.effects.region(&self.image, effect, amount, area)
    }
}

/// Side of a cached effect tile, in pixels.
const EFFECT_TILE: u32 = 256;
/// Effect variants (effect and amount) kept; slider drags cycle through many.
const EFFECT_VARIANTS: usize = 4;

/// Effect images of the screenshot, computed in tiles on demand so only the
/// pixels under redactions are ever processed, even in very large images.
#[derive(Default)]
struct EffectCache {
    /// Most recently used last; tiles keyed by (column, row).
    variants: RefCell<Vec<((RedactEffect, u32), EffectTiles)>>,
}

/// One effect variant's computed tiles, by (column, row).
type EffectTiles = HashMap<(u32, u32), Pixmap>;

impl EffectCache {
    fn region(&self, image: &Pixmap, effect: RedactEffect, amount: u32, area: Rect) -> Option<(Pixmap, (u32, u32))> {
        let (w, h) = (image.width(), image.height());
        let area = area.round_out().intersect(&Rect::new(0.0, 0.0, w as f32, h as f32))?;
        let (c0, c1) = (area.x as u32 / EFFECT_TILE, (area.right() as u32).div_ceil(EFFECT_TILE));
        let (r0, r1) = (area.y as u32 / EFFECT_TILE, (area.bottom() as u32).div_ceil(EFFECT_TILE));
        let mut variants = self.variants.borrow_mut();
        let key = (effect, amount);
        let at = match variants.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                if variants.len() == EFFECT_VARIANTS {
                    variants.remove(0);
                }
                variants.push((key, HashMap::new()));
                variants.len() - 1
            }
        };
        let entry = variants.remove(at);
        variants.push(entry);
        let tiles = &mut variants.last_mut().expect("just pushed").1;

        let tile_rect = |(col, row): (u32, u32)| {
            Rect::from_edges(
                (col * EFFECT_TILE) as f32,
                (row * EFFECT_TILE) as f32,
                ((col + 1) * EFFECT_TILE).min(w) as f32,
                ((row + 1) * EFFECT_TILE).min(h) as f32,
            )
        };
        let wanted: Vec<(u32, u32)> = (r0..r1).flat_map(|row| (c0..c1).map(move |col| (col, row))).collect();
        let missing: Vec<(u32, u32)> = wanted.iter().copied().filter(|k| !tiles.contains_key(k)).collect();
        for (key, tile) in missing.iter().zip(par_map(&missing, |&k| effect_tile(image, effect, amount, tile_rect(k)))) {
            tiles.insert(*key, tile?);
        }

        let (x0, y0) = (c0 * EFFECT_TILE, r0 * EFFECT_TILE);
        let (x1, y1) = ((c1 * EFFECT_TILE).min(w), (r1 * EFFECT_TILE).min(h));
        let mut out = Pixmap::new(x1 - x0, y1 - y0)?;
        let out_stride = (x1 - x0) as usize * 4;
        for key in wanted {
            let (tile, pixels) = (tile_rect(key), &tiles[&key]);
            let (tx, ty) = (tile.x as u32 - x0, tile.y as u32 - y0);
            let tile_stride = pixels.width() as usize * 4;
            for (i, line) in pixels.data().chunks_exact(tile_stride).enumerate() {
                let at = (ty as usize + i) * out_stride + tx as usize * 4;
                out.data_mut()[at..at + tile_stride].copy_from_slice(line);
            }
        }
        Some((out, (x0, y0)))
    }
}

/// `effect` over `tile` of `image`, reading enough pixels around it that the
/// result equals applying the effect to the whole image.
fn effect_tile(image: &Pixmap, effect: RedactEffect, amount: u32, tile: Rect) -> Option<Pixmap> {
    let (w, h) = (image.width(), image.height());
    let (tx0, ty0, tx1, ty1) = (tile.x as u32, tile.y as u32, tile.right() as u32, tile.bottom() as u32);
    let (sx0, sy0, sx1, sy1) = match effect {
        // Blocks on one grid for the whole image.
        RedactEffect::Mosaic => {
            let snap = |v: u32| (v / amount) * amount;
            let up = |v: u32, max: u32| (v.div_ceil(amount) * amount).min(max);
            (snap(tx0), snap(ty0), up(tx1, w), up(ty1, h))
        }
        RedactEffect::Blur => {
            let m = blur_margin(amount as f32 / 2.0) as u32;
            (tx0.saturating_sub(m), ty0.saturating_sub(m), (tx1 + m).min(w), (ty1 + m).min(h))
        }
        RedactEffect::Gray => return None,
    };
    let source = crop(image, sx0, sy0, sx1 - sx0, sy1 - sy0)?;
    let done = match effect {
        RedactEffect::Mosaic => pixelate(&source, amount),
        _ => gaussian_blur(&source, amount as f32 / 2.0),
    };
    crop(&done, tx0 - sx0, ty0 - sy0, tx1 - tx0, ty1 - ty0)
}

/// IME composition underline for a text shape: (index into the rendered shapes, byte range in its content).
pub type Underline = (usize, Range<usize>);

/// Renders `shapes` onto a transparent pixmap covering `region` (pixel aligned).
pub fn render_layer(doc: &Document, region: Rect, shapes: &[&Shape], m: Metrics, underline: Option<Underline>) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(region.w as u32, region.h as u32)?;
    draw_shapes(&mut pixmap, doc, shapes, m, Point::new(region.x, region.y), underline);
    Some(pixmap)
}

/// The final image: the screenshot cropped to `region` with all shapes drawn on top.
pub fn export(doc: &Document, region: Rect, shapes: &[Shape], m: Metrics) -> Option<Pixmap> {
    let region = region.round().intersect(&doc.bounds())?;
    let mut pixmap = crop(doc.image(), region.x as u32, region.y as u32, region.w as u32, region.h as u32)?;
    let shapes: Vec<&Shape> = shapes.iter().collect();
    draw_shapes(&mut pixmap, doc, &shapes, m, Point::new(region.x, region.y), None);
    Some(pixmap)
}

fn draw_shapes(pixmap: &mut Pixmap, doc: &Document, shapes: &[&Shape], m: Metrics, origin: Point, underline: Option<Underline>) {
    let ts = Transform::from_translate(-origin.x, -origin.y);
    for (i, shape) in shapes.iter().enumerate() {
        draw_shape(pixmap, doc, shape, m, ts);
        if let Some((idx, range)) = &underline
            && *idx == i
        {
            draw_underline(pixmap, shape, m, range.clone(), ts);
        }
    }
}

fn solid(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.a);
    paint.anti_alias = true;
    paint
}

fn stroke(width: f32, join: LineJoin) -> Stroke {
    Stroke { width, line_cap: LineCap::Round, line_join: join, ..Stroke::default() }
}

fn draw_shape(pixmap: &mut Pixmap, doc: &Document, shape: &Shape, m: Metrics, ts: Transform) {
    let w = shape.size_px(m.scale);
    let paint = solid(shape.style.color);
    match &shape.geometry {
        Geometry::Rect(r) => {
            if let Some(rect) = tiny_skia::Rect::from_xywh(r.x, r.y, r.w, r.h) {
                let path = PathBuilder::from_rect(rect);
                pixmap.stroke_path(&path, &paint, &stroke(w, LineJoin::Miter), ts, None);
            }
        }
        Geometry::Ellipse(r) => {
            if let Some(path) = tiny_skia::Rect::from_xywh(r.x, r.y, r.w, r.h).and_then(PathBuilder::from_oval) {
                pixmap.stroke_path(&path, &paint, &stroke(w, LineJoin::Round), ts, None);
            }
        }
        Geometry::Arrow { from, to } => draw_arrow(pixmap, *from, *to, w, &paint, ts),
        Geometry::Pen(pts) => {
            if let Some(path) = smooth_path(pts) {
                pixmap.stroke_path(&path, &paint, &stroke(w, LineJoin::Round), ts, None);
            }
        }
        Geometry::Redact { area, effect, strength } => {
            let region;
            let paint = match effect {
                RedactEffect::Gray => solid(REDACT_GRAY),
                _ => {
                    // The effect over just the area this shape covers.
                    let amount = effect.amount(*strength, m.scale);
                    let Some((pixels, (x0, y0))) = doc.effect_region(*effect, amount, shape.bounds(m)) else {
                        return;
                    };
                    region = pixels;
                    let mut paint = Paint {
                        shader: Pattern::new(
                            region.as_ref(),
                            SpreadMode::Pad,
                            FilterQuality::Nearest,
                            1.0,
                            Transform::from_translate(x0 as f32, y0 as f32),
                        ),
                        ..Paint::default()
                    };
                    paint.anti_alias = true;
                    paint
                }
            };
            match area {
                RedactShape::Brush(pts) => {
                    if let Some(path) = smooth_path(pts) {
                        pixmap.stroke_path(&path, &paint, &stroke(w, LineJoin::Round), ts, None);
                    }
                }
                RedactShape::Rect(r) => {
                    if let Some(rect) = tiny_skia::Rect::from_xywh(r.x, r.y, r.w, r.h) {
                        pixmap.fill_rect(rect, &paint, ts, None);
                    }
                }
            }
        }
        Geometry::Text { origin, content } => {
            let layout = m.fonts.layout(content, w);
            if let Some(path) = m.fonts.outline_path(&layout, *origin) {
                pixmap.fill_path(&path, &paint, FillRule::Winding, ts, None);
            }
        }
    }
}

fn draw_arrow(pixmap: &mut Pixmap, from: Point, to: Point, w: f32, paint: &Paint, ts: Transform) {
    let v = to - from;
    let len = v.length();
    if len < 1.0 {
        return;
    }
    let dir = Point::new(v.x / len, v.y / len);
    let perp = Point::new(-dir.y, dir.x);
    let head = arrow_head_len(w).min(len);
    let half = head * 0.45;
    let base = Point::new(to.x - dir.x * head, to.y - dir.y * head);

    // The shaft stops inside the head so its round cap doesn't poke out of the tip.
    let shaft_end = Point::new(to.x - dir.x * head * 0.6, to.y - dir.y * head * 0.6);
    let mut pb = PathBuilder::new();
    pb.move_to(from.x, from.y);
    pb.line_to(shaft_end.x, shaft_end.y);
    if let Some(path) = pb.finish() {
        let s = Stroke { width: w, line_cap: LineCap::Round, ..Stroke::default() };
        pixmap.stroke_path(&path, paint, &s, ts, None);
    }

    let mut pb = PathBuilder::new();
    pb.move_to(to.x, to.y);
    pb.line_to(base.x + perp.x * half, base.y + perp.y * half);
    pb.line_to(base.x - perp.x * half, base.y - perp.y * half);
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, paint, FillRule::Winding, ts, None);
    }
}

/// Polyline through `pts`, smoothed with quadratic curves through segment midpoints.
fn smooth_path(pts: &[Point]) -> Option<tiny_skia::Path> {
    let (first, rest) = pts.split_first()?;
    let mut pb = PathBuilder::new();
    pb.move_to(first.x, first.y);
    match rest {
        [] => pb.line_to(first.x + 0.01, first.y),
        [p] => pb.line_to(p.x, p.y),
        _ => {
            for pair in pts.windows(2).skip(1) {
                let (c, n) = (pair[0], pair[1]);
                pb.quad_to(c.x, c.y, (c.x + n.x) / 2.0, (c.y + n.y) / 2.0);
            }
            let last = pts[pts.len() - 1];
            pb.line_to(last.x, last.y);
        }
    }
    pb.finish()
}

fn draw_underline(pixmap: &mut Pixmap, shape: &Shape, m: Metrics, range: Range<usize>, ts: Transform) {
    let Geometry::Text { origin, content } = &shape.geometry else { return };
    let layout = m.fonts.layout(content, shape.size_px(m.scale));
    let (a, b) = (layout.caret_rect(range.start), layout.caret_rect(range.end));
    let thickness = (m.scale * 1.5).max(1.0);
    let y = a.bottom() - thickness;
    if let Some(r) = tiny_skia::Rect::from_xywh(origin.x + a.x, origin.y + y, (b.x - a.x).max(thickness), thickness) {
        pixmap.fill_rect(r, &solid(shape.style.color), ts, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{PALETTE, RedactEffect, SizeLevel, Style};
    use crate::text::FontSet;

    fn doc(w: u32, h: u32) -> Document {
        let mut rgba = vec![255u8; (w * h * 4) as usize];
        // Left half black, right half white, so mosaic averages are easy to predict.
        for y in 0..h {
            for x in 0..w / 2 {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
        Document::from_rgba(w, h, rgba).unwrap()
    }

    #[test]
    fn export_crops_and_draws() {
        let d = doc(100, 100);
        let fonts = FontSet::empty();
        let m = Metrics { scale: 1.0, fonts: &fonts };
        let shapes = vec![Shape {
            geometry: Geometry::Rect(Rect::new(60.0, 60.0, 20.0, 20.0)),
            style: Style { color: PALETTE[0], size: SizeLevel::Large },
        }];
        let out = export(&d, Rect::new(50.0, 50.0, 40.0, 40.0), &shapes, m).unwrap();
        assert_eq!((out.width(), out.height()), (40, 40));
        // (10,10) in the crop is the rectangle's top-left corner at (60,60): red.
        let px = out.pixel(10, 10).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (0xF5, 0x22, 0x2D));
        // Interior of the rectangle keeps the white background.
        let px = out.pixel(20, 20).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (255, 255, 255));
    }

    fn noise_doc(w: u32, h: u32) -> Document {
        let mut seed = 99u32;
        let rgba = (0..w * h)
            .flat_map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let [r, g, b, _] = seed.to_le_bytes();
                [r, g, b, 255]
            })
            .collect();
        Document::from_rgba(w, h, rgba).unwrap()
    }

    #[test]
    fn effect_bands_match_the_whole_image_effect() {
        let d = noise_doc(600, 700);
        // An area spanning tile boundaries (256) in both directions, ending
        // inside the last, partial tiles.
        let area = Rect::from_edges(200.0, 200.0, 590.0, 610.0);
        for (effect, amount) in [(RedactEffect::Mosaic, 12), (RedactEffect::Blur, 9)] {
            let (region, (x0, y0)) = d.effect_region(effect, amount, area).unwrap();
            assert_eq!((x0, y0, region.width(), region.height()), (0, 0, 600, 700));
            let whole = match effect {
                RedactEffect::Mosaic => crate::pixels::pixelate(d.image(), amount),
                _ => crate::pixels::gaussian_blur(d.image(), amount as f32 / 2.0),
            };
            assert!(region.data() == whole.data(), "{effect:?} tiles differ from the whole-image effect");
            // A second, smaller request comes from the same cached tiles.
            let (part, (px0, py0)) = d.effect_region(effect, amount, Rect::new(300.0, 300.0, 10.0, 10.0)).unwrap();
            assert_eq!((px0, py0), (256, 256));
            assert_eq!(part.pixel(50, 50), whole.pixel(306, 306));
        }
    }

    #[test]
    fn redactions_hide_only_their_area() {
        use crate::shape::RedactShape;
        let d = noise_doc(120, 120);
        let fonts = FontSet::empty();
        let m = Metrics { scale: 1.0, fonts: &fonts };
        let area = |effect| Shape {
            geometry: Geometry::Redact { area: RedactShape::Rect(Rect::new(20.0, 20.0, 40.0, 40.0)), effect, strength: 0.5 },
            style: Style::default(),
        };
        for effect in [RedactEffect::Gray, RedactEffect::Blur, RedactEffect::Mosaic] {
            let out = export(&d, d.bounds(), &[area(effect)], m).unwrap();
            let inside = out.pixel(40, 40).unwrap();
            let orig = d.image().pixel(40, 40).unwrap();
            if effect == RedactEffect::Gray {
                assert_eq!((inside.red(), inside.green(), inside.blue()), (0x8C, 0x8C, 0x8C));
            } else {
                assert_ne!(inside, orig, "{effect:?} left the pixel as is");
            }
            // Outside the area, untouched.
            for (x, y) in [(5, 5), (70, 40), (40, 70), (100, 100)] {
                assert_eq!(out.pixel(x, y), d.image().pixel(x, y), "{effect:?} changed ({x}, {y})");
            }
        }
    }
}
