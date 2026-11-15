//! Text layout and glyph outlines for text annotations.
//!
//! Text is rasterized by the same CPU renderer for the live preview and for the
//! exported image, so what the user sees is exactly what gets copied or saved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ab_glyph::{Font, FontArc, FontRef, GlyphId, OutlineCurve, ScaleFont, VariableFont};
use tiny_skia::{Path as SkPath, PathBuilder};

use crate::geom::{Point, Rect};

/// The typeface families text is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FontFamily {
    Sans,
    Rounded,
    Mono,
}

impl FontFamily {
    pub const ALL: [FontFamily; 3] = [FontFamily::Sans, FontFamily::Rounded, FontFamily::Mono];
}

/// Where a font's bytes come from.
#[derive(Clone, Debug)]
pub enum FontData {
    File(PathBuf),
    /// Bundled in the binary.
    Static(&'static [u8]),
}

/// A font to load: its data, the face within a collection, and for variable
/// fonts the weight (CSS scale, 100–900) to use.
#[derive(Clone, Debug)]
pub struct FontSource {
    pub data: FontData,
    pub index: u32,
    pub weight: Option<f32>,
}

impl FontSource {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self { data: FontData::File(path.into()), index: 0, weight: None }
    }

    pub fn bundled(data: &'static [u8]) -> Self {
        Self { data: FontData::Static(data), index: 0, weight: None }
    }

    /// The `index`th face of a collection (`.ttc`).
    pub fn face(mut self, index: u32) -> Self {
        self.index = index;
        self
    }

    /// The weight to set on a variable font (ignored by static fonts).
    pub fn weight(mut self, weight: f32) -> Self {
        self.weight = Some(weight);
        self
    }

    /// The font, shared with every other set that lists the same source:
    /// fallback fonts (CJK faces of 20–50 MB) appear in each family's list.
    fn load(&self) -> Option<FontArc> {
        type Key = (String, u32, Option<u32>);
        static LOADED: OnceLock<Mutex<HashMap<Key, Option<FontArc>>>> = OnceLock::new();
        let id = match &self.data {
            FontData::File(path) => path.display().to_string(),
            FontData::Static(bytes) => format!("static:{:p}", bytes.as_ptr()),
        };
        let key = (id, self.index, self.weight.map(f32::to_bits));
        let mut loaded = LOADED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        loaded
            .entry(key)
            .or_insert_with(|| {
                let bytes = match &self.data {
                    FontData::File(path) => mapped(path)?,
                    FontData::Static(bytes) => bytes,
                };
                let mut font = FontRef::try_from_slice_and_index(bytes, self.index).ok()?;
                if let Some(weight) = self.weight {
                    font.set_variation(b"wght", weight);
                }
                Some(FontArc::new(font))
            })
            .clone()
    }
}

/// `path` mapped into memory for the rest of the process. Fonts are read
/// lazily this way: only the glyphs used are paged in, and those pages are
/// shared with every other app using the font, instead of a private copy of
/// every fallback font. Each file is mapped once.
fn mapped(path: &Path) -> Option<&'static [u8]> {
    /// Mapped files by path; `None` for files that couldn't be mapped.
    type Maps = Mutex<HashMap<PathBuf, Option<&'static [u8]>>>;
    static MAPS: OnceLock<Maps> = OnceLock::new();
    let mut maps = MAPS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    *maps.entry(path.to_path_buf()).or_insert_with(|| {
        let file = std::fs::File::open(path).ok()?;
        // SAFETY: system and app font files aren't modified while in use; if
        // one were, glyphs could render wrongly, but the mapping stays valid.
        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        Some(&**Box::leak(Box::new(map)))
    })
}

/// A primary font plus fallbacks, consulted per character in order.
pub struct FontSet {
    fonts: Vec<FontArc>,
}

impl FontSet {
    /// Loads every readable font in `paths`; unreadable or invalid files are skipped.
    pub fn load<P: AsRef<Path>>(paths: &[P]) -> Self {
        let sources: Vec<FontSource> = paths.iter().map(|p| FontSource::file(p.as_ref())).collect();
        Self::from_sources(&sources)
    }

    /// Loads every usable font in `sources`, in order; the rest are skipped.
    pub fn from_sources(sources: &[FontSource]) -> Self {
        Self { fonts: sources.iter().filter_map(FontSource::load).collect() }
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
    fn px_scale(font: &FontArc, size: f32) -> ab_glyph::PxScale {
        let upem = font.units_per_em().unwrap_or(1000.0);
        ab_glyph::PxScale::from(size * font.height_unscaled() / upem)
    }

    /// Where the bytes `range` of `text` sit within `line`, the box OCR found
    /// for all of `text`: their share of its width as laid out in these fonts
    /// (close enough for proportional UI fonts), padded a little since glyphs
    /// differ. Without fonts, the whole line.
    pub fn span_rect(&self, line: Rect, text: &str, range: std::ops::Range<usize>) -> Rect {
        let width = |s: &str| self.layout(s, 100.0).width;
        let total = width(text);
        if total <= 0.0 || range.end > text.len() || !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
            return line;
        }
        let pad = line.h * 0.15;
        let x0 = line.x + line.w * width(&text[..range.start]) / total - pad;
        let x1 = line.x + line.w * width(&text[..range.end]) / total + pad;
        Rect::from_edges(x0.max(line.x - pad), line.y, x1.min(line.right() + pad), line.bottom())
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

/// A font set per family. Families without fonts fall back to sans-serif.
pub struct Fonts {
    sets: [FontSet; 3],
}

impl Fonts {
    pub fn new(sans: FontSet, rounded: FontSet, mono: FontSet) -> Self {
        Self { sets: [sans, rounded, mono] }
    }

    /// Only sans-serif fonts: every family uses them.
    pub fn sans_only(sans: FontSet) -> Self {
        Self::new(sans, FontSet::empty(), FontSet::empty())
    }

    pub fn empty() -> Self {
        Self::sans_only(FontSet::empty())
    }

    pub fn get(&self, family: FontFamily) -> &FontSet {
        let set = &self.sets[family as usize];
        if set.is_empty() { &self.sets[0] } else { set }
    }

    pub fn sans(&self) -> &FontSet {
        &self.sets[0]
    }

    pub fn is_empty(&self) -> bool {
        self.sets[0].is_empty()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_fonts_load_at_the_requested_weight() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../platform/assets/fonts/Nunito.ttf");
        let ink = |weight: f32| {
            let set = FontSet::from_sources(&[FontSource::file(path).weight(weight)]);
            let glyphs = set.outline_path(&set.layout("H", 100.0), Point::new(0.0, 0.0)).unwrap();
            let mut p = tiny_skia::Pixmap::new(120, 140).unwrap();
            let mut paint = tiny_skia::Paint::default();
            paint.set_color_rgba8(0, 0, 0, 255);
            p.fill_path(&glyphs, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
            p.pixels().iter().filter(|c| c.alpha() > 128).count()
        };
        // Nunito's default instance is ExtraLight (200); bold has far more ink.
        assert!(ink(800.0) > 2 * ink(200.0));
    }

    #[test]
    fn spans_take_their_share_of_the_line() {
        let fonts = FontSet::load(&["/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"]);
        if fonts.is_empty() {
            return;
        }
        // Monospace: 10 characters over 100 px, so 10 px each (plus padding).
        let line = Rect::new(0.0, 0.0, 100.0, 20.0);
        let r = fonts.span_rect(line, "key=secret", 4..10);
        assert!((r.x - 37.0).abs() < 0.5 && (r.right() - 103.0).abs() < 0.5, "{r:?}");
        assert_eq!((r.y, r.h), (0.0, 20.0));
        // Bad ranges and no fonts: the whole line.
        assert_eq!(fonts.span_rect(line, "key", 0..9), line);
        assert_eq!(FontSet::empty().span_rect(line, "key=secret", 4..10), line);
    }
}

