//! Text layout and glyph outlines for text annotations.
//!
//! Text is rasterized by the same CPU renderer for the live preview and for the
//! exported image, so what the user sees is exactly what gets copied or saved.

use std::path::Path;

use ab_glyph::{Font, FontVec, GlyphId, OutlineCurve, ScaleFont};
use tiny_skia::{Path as SkPath, PathBuilder};

use crate::geom::{Point, Rect};

/// A primary font plus fallbacks, consulted per character in order.
pub struct FontSet {
    fonts: Vec<FontVec>,
}

impl FontSet {
    /// Loads every readable font in `paths`; unreadable or invalid files are skipped.
    pub fn load<P: AsRef<Path>>(paths: &[P]) -> Self {
        let fonts = paths
            .iter()
            .filter_map(|p| std::fs::read(p.as_ref()).ok())
            .filter_map(|data| FontVec::try_from_vec_and_index(data, 0).ok())
            .collect();
        Self { fonts }
    }

    pub fn empty() -> Self {
        Self { fonts: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    fn glyph_for(&self, c: char) -> Option<(usize, GlyphId)> {
        self.fonts
            .iter()
            .enumerate()
            .find_map(|(i, f)| {
                let id = f.glyph_id(c);
                (id.0 != 0).then_some((i, id))
            })
            .or_else(|| self.fonts.first().map(|f| (0, f.glyph_id(c))))
    }

    /// Scale such that one em equals `size` pixels.
    fn px_scale(font: &FontVec, size: f32) -> ab_glyph::PxScale {
        let upem = font.units_per_em().unwrap_or(1000.0);
        ab_glyph::PxScale::from(size * font.height_unscaled() / upem)
    }

    /// Lays out `text` (with `\n` line breaks) at `size` pixels per em, top-left at the origin.
    pub fn layout(&self, text: &str, size: f32) -> TextLayout {
        let (ascent, line_height) = match self.fonts.first() {
            Some(f) => {
                let s = f.as_scaled(Self::px_scale(f, size));
                (s.ascent(), s.ascent() - s.descent())
            }
            None => (size * 0.8, size * 1.2),
        };

        let mut glyphs = Vec::new();
        let mut lines = Vec::new();
        let mut width: f32 = 0.0;
        let mut line_start = 0;
        for (line_index, line) in text.split('\n').enumerate() {
            let baseline = line_index as f32 * line_height + ascent;
            let mut x = 0.0;
            let mut carets = vec![(line_start, 0.0)];
            let mut prev: Option<(usize, GlyphId)> = None;
            for (offset, c) in line.char_indices() {
                if let Some((fi, id)) = self.glyph_for(c) {
                    let font = self.fonts[fi].as_scaled(Self::px_scale(&self.fonts[fi], size));
                    if let Some((pfi, pid)) = prev
                        && pfi == fi
                    {
                        x += font.kern(pid, id);
                    }
                    glyphs.push(PlacedGlyph { font: fi, id, x, baseline });
                    x += font.h_advance(id);
                    prev = Some((fi, id));
                }
                carets.push((line_start + offset + c.len_utf8(), x));
            }
            width = width.max(x);
            lines.push(LineLayout { top: line_index as f32 * line_height, carets });
            line_start += line.len() + 1;
        }

        TextLayout { glyphs, lines, width, line_height, size }
    }

    /// Glyph outlines of `layout` as one fillable path, offset by `origin`.
    pub fn outline_path(&self, layout: &TextLayout, origin: Point) -> Option<SkPath> {
        let mut pb = PathBuilder::new();
        for g in &layout.glyphs {
            let font = &self.fonts[g.font];
            let Some(outline) = font.outline(g.id) else { continue };
            let k = layout.size / font.units_per_em().unwrap_or(1000.0);
            let map = |p: ab_glyph::Point| (origin.x + g.x + p.x * k, origin.y + g.baseline - p.y * k);
            let mut last: Option<ab_glyph::Point> = None;
            for curve in &outline.curves {
                let start = match curve {
                    OutlineCurve::Line(a, _) | OutlineCurve::Quad(a, _, _) | OutlineCurve::Cubic(a, _, _, _) => *a,
                };
                if last != Some(start) {
                    if last.is_some() {
                        pb.close();
                    }
                    let (x, y) = map(start);
                    pb.move_to(x, y);
                }
                match curve {
                    OutlineCurve::Line(_, b) => {
                        let (x, y) = map(*b);
                        pb.line_to(x, y);
                        last = Some(*b);
                    }
                    OutlineCurve::Quad(_, c, b) => {
                        let ((cx, cy), (x, y)) = (map(*c), map(*b));
                        pb.quad_to(cx, cy, x, y);
                        last = Some(*b);
                    }
                    OutlineCurve::Cubic(_, c1, c2, b) => {
                        let ((x1, y1), (x2, y2), (x, y)) = (map(*c1), map(*c2), map(*b));
                        pb.cubic_to(x1, y1, x2, y2, x, y);
                        last = Some(*b);
                    }
                }
            }
            if last.is_some() {
                pb.close();
            }
        }
        pb.finish()
    }
}

struct PlacedGlyph {
    font: usize,
    id: GlyphId,
    x: f32,
    baseline: f32,
}

struct LineLayout {
    top: f32,
    /// Every caret stop on the line as (byte offset into the whole text, x).
    carets: Vec<(usize, f32)>,
}

pub struct TextLayout {
    glyphs: Vec<PlacedGlyph>,
    lines: Vec<LineLayout>,
    pub width: f32,
    pub line_height: f32,
    size: f32,
}

impl TextLayout {
    pub fn height(&self) -> f32 {
        self.lines.len() as f32 * self.line_height
    }

    /// Caret rectangle (relative to the layout origin) at byte offset `index`.
    pub fn caret_rect(&self, index: usize) -> Rect {
        let width = (self.size / 12.0).max(1.0);
        for line in &self.lines {
            if let Some(&(_, x)) = line.carets.iter().find(|(b, _)| *b == index) {
                return Rect::new(x, line.top, width, self.line_height);
            }
        }
        let line = self.lines.last().expect("layout always has one line");
        let x = line.carets.last().map_or(0.0, |c| c.1);
        Rect::new(x, line.top, width, self.line_height)
    }

    /// Byte offset of the caret stop closest to `p` (relative to the layout origin).
    pub fn index_at(&self, p: Point) -> usize {
        let row = ((p.y / self.line_height).floor().max(0.0) as usize).min(self.lines.len() - 1);
        self.lines[row]
            .carets
            .iter()
            .min_by(|a, b| (a.1 - p.x).abs().total_cmp(&(b.1 - p.x).abs()))
            .map_or(0, |c| c.0)
    }
}
