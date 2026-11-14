//! CPU rasterization of annotations with tiny-skia.
//!
//! The same code renders the live annotation layer and the final exported image.

use std::cell::OnceCell;
use std::ops::Range;

use tiny_skia::{
    FillRule, FilterQuality, IntSize, LineCap, LineJoin, Paint, PathBuilder, Pattern, Pixmap, PixmapPaint,
    SpreadMode, Stroke, Transform,
};

use crate::geom::{Point, Rect};
use crate::shape::{Geometry, Metrics, Shape, arrow_head_len};
use crate::style::Color;

/// The frozen screenshot an editing session works on.
pub struct Document {
    image: Pixmap,
    mosaic_block: u32,
    mosaic: OnceCell<Pixmap>,
}

impl Document {
    /// Wraps opaque RGBA8 pixels. Returns `None` if `rgba` does not match the size.
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>, mosaic_block: u32) -> Option<Self> {
        let image = Pixmap::from_vec(rgba, IntSize::from_wh(width, height)?)?;
        Some(Self { image, mosaic_block: mosaic_block.max(2), mosaic: OnceCell::new() })
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

    /// Pixelated copy of the screenshot, computed on first use.
    fn mosaic(&self) -> &Pixmap {
        self.mosaic.get_or_init(|| pixelate(&self.image, self.mosaic_block))
    }
}

fn pixelate(src: &Pixmap, block: u32) -> Pixmap {
    let (w, h) = (src.width(), src.height());
    let mut out = src.clone();
    let stride = w as usize * 4;
    let data = src.data();
    let dst = out.data_mut();
    for by in (0..h).step_by(block as usize) {
        for bx in (0..w).step_by(block as usize) {
            let (x1, y1) = ((bx + block).min(w), (by + block).min(h));
            let mut sum = [0u32; 4];
            for y in by..y1 {
                let row = &data[y as usize * stride..];
                for x in bx..x1 {
                    let i = x as usize * 4;
                    for c in 0..4 {
                        sum[c] += row[i + c] as u32;
                    }
                }
            }
            let n = (x1 - bx) * (y1 - by);
            let avg = sum.map(|s| (s / n) as u8);
            for y in by..y1 {
                let row = &mut dst[y as usize * stride..];
                for x in bx..x1 {
                    let i = x as usize * 4;
                    row[i..i + 4].copy_from_slice(&avg);
                }
            }
        }
    }
    out
}

/// IME composition underline for a text shape: (shape index, byte range in its content).
pub type Underline = (usize, Range<usize>);

/// Renders `shapes` onto a transparent pixmap covering `region` (pixel aligned).
pub fn render_layer(doc: &Document, region: Rect, shapes: &[Shape], m: Metrics, underline: Option<Underline>) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(region.w as u32, region.h as u32)?;
    draw_shapes(&mut pixmap, doc, shapes, m, Point::new(region.x, region.y), underline);
    Some(pixmap)
}

/// The final image: the screenshot cropped to `region` with all shapes drawn on top.
pub fn export(doc: &Document, region: Rect, shapes: &[Shape], m: Metrics) -> Option<Pixmap> {
    let region = region.round().intersect(&doc.bounds())?;
    let mut pixmap = Pixmap::new(region.w as u32, region.h as u32)?;
    pixmap.draw_pixmap(
        -region.x as i32,
        -region.y as i32,
        doc.image().as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    draw_shapes(&mut pixmap, doc, shapes, m, Point::new(region.x, region.y), None);
    Some(pixmap)
}

fn draw_shapes(pixmap: &mut Pixmap, doc: &Document, shapes: &[Shape], m: Metrics, origin: Point, underline: Option<Underline>) {
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
        Geometry::Mosaic(pts) => {
            if let Some(path) = smooth_path(pts) {
                let paint = Paint {
                    shader: Pattern::new(
                        doc.mosaic().as_ref(),
                        SpreadMode::Pad,
                        FilterQuality::Nearest,
                        1.0,
                        Transform::identity(),
                    ),
                    ..Paint::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke(w, LineJoin::Round), ts, None);
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

/// Converts premultiplied RGBA to straight-alpha BGRA, the layout most GPU
/// texture uploads (and gpui) expect.
pub fn to_bgra_straight(pixmap: &Pixmap) -> Vec<u8> {
    let mut out = pixmap.data().to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        let a = px[3];
        px.swap(0, 2);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            }
        }
    }
    out
}

/// Converts premultiplied RGBA to straight-alpha RGBA (what clipboards and PNG want).
pub fn to_rgba_straight(pixmap: &Pixmap) -> Vec<u8> {
    let mut out = to_bgra_straight(pixmap);
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{PALETTE, SizeLevel, Style};
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
        Document::from_rgba(w, h, rgba, 8).unwrap()
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

    #[test]
    fn pixelate_averages_blocks() {
        let d = doc(16, 8);
        let m = d.mosaic();
        // One 8x8 block per half: black stays black, white stays white.
        assert_eq!(m.pixel(3, 3).unwrap().red(), 0);
        assert_eq!(m.pixel(12, 3).unwrap().red(), 255);
    }

    #[test]
    fn bgra_conversion_demultiplies() {
        let mut p = Pixmap::new(1, 1).unwrap();
        p.data_mut().copy_from_slice(&[64, 0, 0, 128]);
        assert_eq!(to_bgra_straight(&p), vec![0, 0, 128, 128]);
    }
}
