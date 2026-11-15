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
use crate::shape::{Geometry, Metrics, RedactShape, Shape, arrow_head_len, counter_box, counter_font_size, text_box};
use crate::style::{COUNTER_DARK_TEXT, Color, HIGHLIGHT_ALPHA, REDACT_GRAY, RedactEffect, TextBackdrop, TextStyle};
use crate::text::{FontSet, Fonts};

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
        Geometry::Highlight(pts) => draw_highlight(pixmap, pts, w, shape.style.color, ts),
        Geometry::Counter { center, number, format } => {
            draw_counter(pixmap, *center, &format.label(*number), w, shape.style.color, m.fonts.sans(), ts)
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
        Geometry::Text { origin, content, style } => {
            draw_text(pixmap, m.fonts.get(style.family()), content, *origin, w, *style, shape.style.color, ts)
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

/// `text` in `style` and `color` at `size` px, cropped to what it covers: the
/// previews in a style picker, drawn exactly as annotations are.
pub fn text_sample(fonts: &Fonts, text: &str, style: TextStyle, color: Color, size: f32) -> Option<Pixmap> {
    let set = fonts.get(style.family());
    let layout = set.layout(text, size);
    let b = text_box(Point::new(0.0, 0.0), &layout, size, style).inflate(1.0).round_out();
    let mut pixmap = Pixmap::new(b.w as u32, b.h as u32)?;
    draw_text(&mut pixmap, set, text, Point::new(0.0, 0.0), size, style, color, Transform::from_translate(-b.x, -b.y));
    Some(pixmap)
}

/// Text in `style`: its outline or backdrop, then the glyphs. `size` is the
/// font size.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    pixmap: &mut Pixmap,
    fonts: &FontSet,
    content: &str,
    origin: Point,
    size: f32,
    style: TextStyle,
    color: Color,
    ts: Transform,
) {
    let layout = fonts.layout(content, size);
    let glyphs = fonts.outline_path(&layout, origin);
    let backdrop = text_box(origin, &layout, size, style);
    match style.backdrop() {
        TextBackdrop::None => {}
        TextBackdrop::Outline => {
            if let Some(glyphs) = &glyphs {
                let outline = Stroke { width: size * 0.16, line_join: LineJoin::Round, ..Stroke::default() };
                pixmap.stroke_path(glyphs, &solid(color.contrast()), &outline, ts, None);
            }
        }
        TextBackdrop::Box => fill_rounded(pixmap, backdrop, size * 0.16, color, ts),
        TextBackdrop::Pill => fill_rounded(pixmap, backdrop, backdrop.h.min(size * 1.2) / 2.0, color, ts),
        TextBackdrop::Card => {
            // A light card with a hairline border; dark for light colors, which
            // wouldn't show on it.
            let (card, border) = if color.is_light() {
                (Color::rgb(0x1F1F1F), Color { a: 60, ..Color::rgb(0xFFFFFF) })
            } else {
                (Color::rgb(0xFFFFFF), Color { a: 50, ..Color::rgb(0x000000) })
            };
            let radius = size * 0.16;
            fill_rounded(pixmap, backdrop, radius, card, ts);
            if let Some(path) = rounded_rect(backdrop.inflate(-size / 48.0), radius) {
                let line = Stroke { width: (size / 24.0).max(1.0), ..Stroke::default() };
                pixmap.stroke_path(&path, &solid(border), &line, ts, None);
            }
        }
    }
    if let Some(glyphs) = glyphs {
        pixmap.fill_path(&glyphs, &solid(style.ink(color)), FillRule::Winding, ts, None);
    }
}

fn fill_rounded(pixmap: &mut Pixmap, r: Rect, radius: f32, color: Color, ts: Transform) {
    if let Some(path) = rounded_rect(r, radius) {
        pixmap.fill_path(&path, &solid(color), FillRule::Winding, ts, None);
    }
}

/// `r` with corners rounded by `radius` (clamped to half its sides).
fn rounded_rect(r: Rect, radius: f32) -> Option<tiny_skia::Path> {
    let k = radius.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
    // Cubic Bézier control distance approximating a quarter circle.
    let c = k * 0.552_284_8;
    let (x0, y0, x1, y1) = (r.x, r.y, r.right(), r.bottom());
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + k, y0);
    pb.line_to(x1 - k, y0);
    pb.cubic_to(x1 - k + c, y0, x1, y0 + k - c, x1, y0 + k);
    pb.line_to(x1, y1 - k);
    pb.cubic_to(x1, y1 - k + c, x1 - k + c, y1, x1 - k, y1);
    pb.line_to(x0 + k, y1);
    pb.cubic_to(x0 + k - c, y1, x0, y1 - k + c, x0, y1 - k);
    pb.line_to(x0, y0 + k);
    pb.cubic_to(x0, y0 + k - c, x0 + k - c, y0, x0 + k, y0);
    pb.close();
    pb.finish()
}

/// A highlighter stroke: a chisel tip, so the ends are rounded rectangles
/// rather than round caps. The stroke and its ends are combined in a mask and
/// painted once, so overlaps (and crossings) don't get darker.
fn draw_highlight(pixmap: &mut Pixmap, pts: &[Point], w: f32, color: Color, ts: Transform) {
    let Some(path) = smooth_path(pts) else { return };
    let outline = Stroke { width: w, line_cap: LineCap::Butt, line_join: LineJoin::Round, ..Stroke::default() };
    let Some(body) = path.stroke(&outline, 1.0) else { return };
    let Some(mut mask) = tiny_skia::Mask::new(pixmap.width(), pixmap.height()) else { return };
    mask.fill_path(&body, FillRule::Winding, true, ts);
    for cap in highlight_caps(pts, w) {
        mask.fill_path(&cap, FillRule::Winding, true, ts);
    }
    let paint = solid(Color { a: HIGHLIGHT_ALPHA, ..color });
    let all = tiny_skia::Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32).expect("non-empty");
    pixmap.fill_rect(all, &paint, Transform::identity(), Some(&mask));
}

/// Rounded-rectangle ends for a highlighter stroke `w` wide through `pts`:
/// squares around each end, turned along the stroke, extending w/2 past it.
fn highlight_caps(pts: &[Point], w: f32) -> Vec<tiny_skia::Path> {
    let (Some(&first), Some(&last)) = (pts.first(), pts.last()) else { return Vec::new() };
    // The stroke's direction at an end, from a point far enough in to be stable.
    let along = |end: Point, inward: &mut dyn Iterator<Item = Point>| {
        let mut other = None;
        for p in inward {
            other = Some(p);
            if p.distance(end) >= w * 0.5 {
                break;
            }
        }
        let v = other.map_or(Point::new(1.0, 0.0), |o| o - end);
        let len = v.length();
        if len > 0.0 { Point::new(v.x / len, v.y / len) } else { Point::new(1.0, 0.0) }
    };
    let start = along(first, &mut pts.iter().skip(1).copied());
    let end = along(last, &mut pts.iter().rev().skip(1).copied());
    [(first, start), (last, end)].into_iter().filter_map(|(c, d)| rounded_square(c, d, w)).collect()
}

/// A square `w` wide centered on `center`, turned to the unit direction `d`,
/// with corners rounded by a quarter of its width.
fn rounded_square(center: Point, d: Point, w: f32) -> Option<tiny_skia::Path> {
    let (h, r) = (w / 2.0, w * 0.25);
    // Corners in the square's own frame (u along `d`, v across), in order.
    let corners = [Point::new(h, -h), Point::new(h, h), Point::new(-h, h), Point::new(-h, -h)];
    let world = |p: Point| (center.x + d.x * p.x - d.y * p.y, center.y + d.y * p.x + d.x * p.y);
    let toward = |a: Point, b: Point| {
        let v = b - a;
        let len = v.length();
        Point::new(a.x + v.x / len * r, a.y + v.y / len * r)
    };
    let mut pb = PathBuilder::new();
    let (x, y) = world(toward(corners[0], corners[3]));
    pb.move_to(x, y);
    for i in 0..4 {
        let (c, next) = (corners[i], corners[(i + 1) % 4]);
        let ((cx, cy), (ax, ay)) = (world(c), world(toward(c, next)));
        pb.quad_to(cx, cy, ax, ay);
        let (bx, by) = world(toward(next, c));
        pb.line_to(bx, by);
    }
    pb.close();
    pb.finish()
}

/// A filled circle (or pill, for long labels) with the label centered on it.
fn draw_counter(pixmap: &mut Pixmap, center: Point, label: &str, diameter: f32, color: Color, fonts: &FontSet, ts: Transform) {
    let r = counter_box(center, label, diameter, fonts);
    // A round-capped line as wide as the marker: a circle when it has no length.
    let radius = diameter / 2.0;
    let mut pb = PathBuilder::new();
    pb.move_to(r.x + radius, center.y);
    pb.line_to((r.right() - radius).max(r.x + radius + 0.01), center.y);
    let Some(body) = pb.finish() else { return };
    // A faint dark edge keeps light markers visible on light backgrounds.
    let edge = Color { a: 64, ..Color::rgb(0x000000) };
    pixmap.stroke_path(&body, &solid(edge), &stroke(diameter + 2.0, LineJoin::Round), ts, None);
    pixmap.stroke_path(&body, &solid(color), &stroke(diameter, LineJoin::Round), ts, None);

    let size = counter_font_size(diameter);
    let Some(glyphs) = fonts.outline_path(&fonts.layout(label, size), Point::new(0.0, 0.0)) else { return };
    // Center the glyphs' ink, not their line box, so digits sit in the middle.
    let ink = glyphs.bounds();
    let ts = ts.pre_translate(center.x - (ink.left() + ink.right()) / 2.0, center.y - (ink.top() + ink.bottom()) / 2.0);
    let text = solid(if color.is_light() { COUNTER_DARK_TEXT } else { Color::rgb(0xFFFFFF) });
    pixmap.fill_path(&glyphs, &text, FillRule::Winding, ts, None);
    // Slightly bolder than the regular face, for legibility at small sizes.
    pixmap.stroke_path(&glyphs, &text, &Stroke { width: size * 0.06, ..Stroke::default() }, ts, None);
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
    let Geometry::Text { origin, content, style } = &shape.geometry else { return };
    let layout = m.fonts.get(style.family()).layout(content, shape.size_px(m.scale));
    let (a, b) = (layout.caret_rect(range.start), layout.caret_rect(range.end));
    let thickness = (m.scale * 1.5).max(1.0);
    let y = a.bottom() - thickness;
    if let Some(r) = tiny_skia::Rect::from_xywh(origin.x + a.x, origin.y + y, (b.x - a.x).max(thickness), thickness) {
        pixmap.fill_rect(r, &solid(style.ink(shape.style.color)), ts, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{CounterFormat, PALETTE, RedactEffect, SizeLevel, Style, Tool};
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
        let fonts = crate::text::Fonts::empty();
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

    fn real_fonts() -> Option<crate::text::Fonts> {
        let fonts = FontSet::load(&["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"]);
        (!fonts.is_empty()).then(|| crate::text::Fonts::sans_only(fonts))
    }

    #[test]
    fn highlights_are_translucent_and_dont_darken_where_they_cross() {
        let d = doc(100, 100);
        let fonts = crate::text::Fonts::empty();
        let m = Metrics { scale: 1.0, fonts: &fonts };
        // Through the white half and back over itself.
        let pts = [(60.0, 20.0), (90.0, 50.0), (60.0, 80.0), (90.0, 20.0)].map(|(x, y)| Point::new(x, y)).to_vec();
        let shapes = [Shape { geometry: Geometry::Highlight(pts), style: Tool::Highlighter.default_style() }];
        let out = export(&d, d.bounds(), &shapes, m).unwrap();
        let px = |x, y| {
            let p = out.pixel(x, y).unwrap();
            (p.red(), p.green(), p.blue())
        };
        let yellow = PALETTE[1];
        let mix = |c: u8| (255.0 + (c as f32 - 255.0) * HIGHLIGHT_ALPHA as f32 / 255.0).round() as u8;
        let expected = (mix(yellow.r), mix(yellow.g), mix(yellow.b));
        // On a single pass and where the stroke crosses itself: the same tint.
        for (x, y) in [(66, 26), (75, 50)] {
            let (r, g, b) = px(x, y);
            assert!(r.abs_diff(expected.0) <= 1 && g.abs_diff(expected.1) <= 1 && b.abs_diff(expected.2) <= 1, "{:?} at ({x}, {y})", px(x, y));
        }
    }

    #[test]
    fn highlighter_ends_are_rounded_rectangles() {
        let d = doc(200, 100);
        let fonts = crate::text::Fonts::empty();
        let m = Metrics { scale: 1.0, fonts: &fonts };
        // Large: 30 px wide, horizontal from x 140 to 180 at y 50 (on the white half).
        let pts = vec![Point::new(140.0, 50.0), Point::new(180.0, 50.0)];
        let shapes = [Shape { geometry: Geometry::Highlight(pts), style: Style { size: SizeLevel::Large, ..Tool::Highlighter.default_style() } }];
        let out = export(&d, d.bounds(), &shapes, m).unwrap();
        let tinted = |x, y| out.pixel(x, y).unwrap().blue() < 200;
        // Past the end by up to 15 px, the full height is covered near the axis
        // and near the edges (a round cap would leave (191, 37) bare)…
        assert!(tinted(192, 50) && tinted(191, 37) && tinted(191, 63));
        // …but the very corner is rounded off, and nothing reaches past w/2.
        assert!(!tinted(194, 35) && !tinted(196, 50));
    }

    #[test]
    fn text_styles_draw_their_backdrops() {
        let Some(fonts) = real_fonts() else { return };
        let red = PALETTE[0];
        let px = |p: &Pixmap, x, y| {
            let c = p.pixel(x, y).unwrap();
            (c.red(), c.green(), c.blue(), c.alpha())
        };
        // Boxed and pill: the text color behind the glyphs, white glyphs.
        for style in [TextStyle::Boxed, TextStyle::RoundedBoxed] {
            let sample = text_sample(&fonts, "Hi", style, red, 40.0).unwrap();
            let (w, h) = (sample.width(), sample.height());
            // Inside the padding, left of the glyphs, mid-height: the box.
            assert_eq!(px(&sample, 8, h / 2), (0xF5, 0x22, 0x2D, 255), "{style:?}");
            let white = (0..w).flat_map(|x| (0..h).map(move |y| (x, y))).filter(|&(x, y)| px(&sample, x, y) == (255, 255, 255, 255)).count();
            assert!(white > 30, "{style:?}: glyphs not in the contrasting color");
            // A pill has round ends: its corner is empty.
            if style == TextStyle::RoundedBoxed {
                assert_eq!(px(&sample, 1, 1).3, 0);
            }
        }
        // Card: a white card, a darker border, red glyphs.
        let card = text_sample(&fonts, "Hi", TextStyle::MonospacedBoxed, red, 40.0).unwrap();
        assert_eq!(px(&card, 10, card.height() / 2), (255, 255, 255, 255));
        // Outlined: white around red glyphs, nothing behind.
        let outlined = text_sample(&fonts, "Hi", TextStyle::Outlined, red, 40.0).unwrap();
        let (w, h) = (outlined.width(), outlined.height());
        let all: Vec<_> = (0..w).flat_map(|x| (0..h).map(move |y| (x, y))).map(|(x, y)| px(&outlined, x, y)).collect();
        assert!(all.contains(&(255, 255, 255, 255)) && all.contains(&(0xF5, 0x22, 0x2D, 255)));
        assert_eq!(px(&outlined, 0, 0).3, 0);
    }

    #[test]
    fn counters_are_filled_with_a_centered_label() {
        let Some(fonts) = real_fonts() else { return };
        let d = doc(200, 100);
        let m = Metrics { scale: 1.0, fonts: &fonts };
        let counter = |x, number, color| Shape {
            geometry: Geometry::Counter { center: Point::new(x, 50.0), number, format: CounterFormat::Decimal },
            style: Style { color, size: SizeLevel::Large },
        };
        let out = export(&d, d.bounds(), &[counter(150.0, 1, PALETTE[0]), counter(60.0, 8, PALETTE[5])], m).unwrap();
        let rgb = |x, y| {
            let p = out.pixel(x, y).unwrap();
            (p.red(), p.green(), p.blue())
        };
        // Red disc, radius 20, around a white "1" whose ink is centered on it.
        assert_eq!(rgb(150 - 14, 50), (0xF5, 0x22, 0x2D));
        assert_eq!(rgb(150 + 25, 50), (255, 255, 255));
        let ink: Vec<(u32, u32)> = (131..170)
            .flat_map(|x| (31..70).map(move |y| (x, y)))
            .filter(|&(x, y)| Point::new(x as f32 + 0.5, y as f32 + 0.5).distance(Point::new(150.0, 50.0)) < 17.0)
            .filter(|&(x, y)| rgb(x, y) == (255, 255, 255))
            .collect();
        assert!(ink.len() > 40, "label too small: {} px", ink.len());
        let span = |f: fn(&(u32, u32)) -> u32| (ink.iter().map(f).min().unwrap(), ink.iter().map(f).max().unwrap());
        let ((x0, x1), (y0, y1)) = (span(|p| p.0), span(|p| p.1));
        assert!(((x0 + x1) as f32 / 2.0 - 149.5).abs() <= 1.0 && ((y0 + y1) as f32 / 2.0 - 49.5).abs() <= 1.0, "ink {x0}..{x1} x {y0}..{y1}");
        // White disc on the black half: a dark "8", whose waist is at the center.
        assert_eq!(rgb(60 - 15, 50), (255, 255, 255));
        assert!(rgb(60, 50).0 < 100, "label not dark at the center: {:?}", rgb(60, 50));
    }

    #[test]
    fn redactions_hide_only_their_area() {
        use crate::shape::RedactShape;
        let d = noise_doc(120, 120);
        let fonts = crate::text::Fonts::empty();
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
