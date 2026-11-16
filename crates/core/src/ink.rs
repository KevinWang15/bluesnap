//! Fitting boxes to the text actually drawn in a screenshot.
//!
//! OCR gives a box per line; a sensitive value is usually part of one. Font
//! metrics alone misplace it when the screen uses another font (monospace
//! terminals, CJK), so [`place_span`] reads the pixels: words map onto ink
//! clusters, edges snap to the gaps between glyphs, and the box grows to cover
//! descenders the OCR box left out.

use std::ops::Range;

use tiny_skia::Pixmap;

use crate::geom::Rect;
use crate::text::FontSet;

/// Channel difference from the background that counts as ink.
const INK_CONTRAST: i32 = 48;

/// Where bytes `range` of `text` (an OCR line found at `line`, in `image`
/// pixels) are drawn.
///
/// When the line's words match its ink clusters one to one (split at its
/// widest gaps), each word maps onto its cluster whatever the font, and
/// positions inside a word are interpolated with proportional (`fonts`) or
/// monospace metrics, whichever predicts the word positions better. Otherwise
/// (OCR often drops the spaces between CJK words) the span is estimated from
/// `fonts` over the whole line and its edges snapped to nearby gaps. Spans
/// reaching either end of the line end at its first or last ink.
pub fn place_span(image: &Pixmap, fonts: &FontSet, line: Rect, text: &str, range: Range<usize>) -> Rect {
    if range.start > range.end
        || range.end > text.len()
        || !text.is_char_boundary(range.start)
        || !text.is_char_boundary(range.end)
    {
        return line;
    }
    let estimate = fonts.span_rect(line, text, range.clone());
    let Some((x0, columns)) = ink_columns(image, line) else { return estimate };
    let (Some(first), Some(last)) = (columns.iter().position(|&c| c), columns.iter().rposition(|&c| c)) else {
        return estimate;
    };
    let all_gaps = gaps(&columns);
    let words = words(text);
    let (mut left, mut right) = match word_clusters(&columns, &all_gaps, words.len()) {
        Some(clusters) => {
            let metrics = Metrics::fitting(fonts, text, &words, &clusters);
            let x_at = |offset: usize, edge: Edge| -> f32 {
                let i = words
                    .iter()
                    .position(|w| match edge {
                        Edge::Start => offset >= w.start && offset < w.end,
                        Edge::End => offset > w.start && offset <= w.end,
                    })
                    .unwrap_or(match edge {
                        Edge::Start => 0,
                        Edge::End => words.len() - 1,
                    });
                let (w, (c0, c1)) = (&words[i], clusters[i]);
                let word = &text[w.clone()];
                let share = metrics.share(word, offset.clamp(w.start, w.end) - w.start);
                let x = c0 as f32 + share * (c1 - c0) as f32;
                x0 as f32 + snap_edge(&all_gaps, x, edge, 1.0, 0.45 * line.h).unwrap_or(x)
            };
            (x_at(range.start, Edge::Start) - 1.0, x_at(range.end, Edge::End) + 1.0)
        }
        None => {
            let snapped = snap_with(&all_gaps, x0, line.h, estimate);
            (snapped.x, snapped.right())
        }
    };
    if range.start <= text.len() - text.trim_start().len() {
        left = (x0 + first) as f32 - 1.0;
    }
    if range.end >= text.trim_end().len() {
        right = (x0 + last) as f32 + 2.0;
    }
    if right - left < 2.0 {
        return estimate;
    }
    let (top, bottom) = fit_vertically(image, line, left, right);
    Rect::from_edges(left, top, right, bottom)
}

/// The rows of `line` between `left` and `right`, grown up and down while the
/// rows just outside still have ink there: OCR boxes often stop at the
/// baseline, leaving descenders (`g`, `p`, `_`) showing. Grows at most 0.4 line
/// heights each way, so lines that touch don't merge.
fn fit_vertically(image: &Pixmap, line: Rect, left: f32, right: f32) -> (f32, f32) {
    let Some(background) = background(image, line) else { return (line.y, line.bottom()) };
    let (w, h) = (image.width() as i32, image.height() as i32);
    let (x0, x1) = ((left.floor() as i32).clamp(0, w), (right.ceil() as i32).clamp(0, w));
    let has_ink = |y: i32| (0..h).contains(&y) && (x0..x1).any(|x| is_ink(image, x, y, background));
    let reach = (0.4 * line.h).ceil() as i32;
    let (mut top, mut bottom) = (line.y.floor() as i32, line.bottom().ceil() as i32);
    let (top_limit, bottom_limit) = (top - reach, bottom + reach);
    while top > top_limit && has_ink(top - 1) {
        top -= 1;
    }
    while bottom < bottom_limit && has_ink(bottom) {
        bottom += 1;
    }
    // A pixel of margin where the box grew, for anti-aliased edges.
    let top = if top < line.y.floor() as i32 { top - 1 } else { top };
    let bottom = if bottom > line.bottom().ceil() as i32 { bottom + 1 } else { bottom };
    (top.max(0) as f32, bottom.min(h) as f32)
}

/// How far into a word a byte offset is drawn, as a share of its width.
#[derive(Clone, Copy)]
enum Metrics<'a> {
    Proportional(&'a FontSet),
    /// Every character equally wide (terminals, code).
    Monospace,
}

impl<'a> Metrics<'a> {
    fn share(self, word: &str, offset: usize) -> f32 {
        match self {
            Metrics::Proportional(fonts) => {
                let width = |s: &str| fonts.layout(s, 100.0).width;
                match width(word) {
                    total if total > 0.0 => width(&word[..offset]) / total,
                    _ => Metrics::Monospace.share(word, offset),
                }
            }
            Metrics::Monospace => word[..offset].chars().count() as f32 / word.chars().count().max(1) as f32,
        }
    }

    /// The metrics whose word positions across the whole line best match the
    /// ink `clusters` (one per word).
    fn fitting(fonts: &'a FontSet, text: &str, words: &[Range<usize>], clusters: &[(usize, usize)]) -> Self {
        let (Some(w0), Some(w1)) = (words.first(), words.last()) else { return Metrics::Proportional(fonts) };
        let span = &text[w0.start..w1.end];
        let (ink0, ink1) = (clusters[0].0 as f32, clusters[clusters.len() - 1].1 as f32);
        let error = |m: Metrics| -> f32 {
            words
                .iter()
                .zip(clusters)
                .map(|(w, &(c0, c1))| {
                    let at = |offset: usize| ink0 + m.share(span, offset - w0.start) * (ink1 - ink0);
                    (at(w.start) - c0 as f32).abs() + (at(w.end) - c1 as f32).abs()
                })
                .sum()
        };
        let proportional = Metrics::Proportional(fonts);
        if fonts.is_empty() || error(Metrics::Monospace) < error(proportional) { Metrics::Monospace } else { proportional }
    }
}

/// The ink split into `n` clusters at its `n - 1` widest gaps, if those are
/// clearly word gaps: wider than every other gap and than a hairline.
fn word_clusters(columns: &[bool], all_gaps: &[(usize, usize)], n: usize) -> Option<Vec<(usize, usize)>> {
    let first = columns.iter().position(|&c| c)?;
    let last = columns.iter().rposition(|&c| c)?;
    if n == 0 {
        return None;
    }
    let mut inner: Vec<(usize, usize)> = all_gaps.iter().copied().filter(|(a, b)| *a > first && *b <= last).collect();
    if inner.len() < n - 1 {
        return None;
    }
    inner.sort_by_key(|(a, b)| std::cmp::Reverse(b - a));
    let width = |g: &(usize, usize)| g.1 - g.0;
    if n > 1 {
        let narrowest_word_gap = width(&inner[n - 2]);
        let widest_other = inner.get(n - 1).map_or(0, width);
        if narrowest_word_gap < 2 || narrowest_word_gap <= widest_other {
            return None;
        }
    }
    let mut cuts: Vec<(usize, usize)> = inner[..n - 1].to_vec();
    cuts.sort();
    let mut out = Vec::with_capacity(n);
    let mut start = first;
    for (a, b) in cuts {
        out.push((start, a));
        start = b;
    }
    out.push((start, last + 1));
    Some(out)
}

/// Gaps at least this wide (in pixels, for a line `h` high) separate words.
fn word_gap(h: f32) -> f32 {
    (0.22 * h).max(2.0)
}

/// Byte ranges of `text`'s whitespace-separated words.
fn words(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Edge {
    /// Where ink begins after a gap.
    Start,
    /// Where ink stops before a gap.
    End,
}

/// The gap edge nearest `target` (local column) among gaps at least
/// `min_width` wide, within `window` pixels.
fn snap_edge(gaps: &[(usize, usize)], target: f32, edge: Edge, min_width: f32, window: f32) -> Option<f32> {
    gaps.iter()
        .filter(|g| (g.1 - g.0) as f32 >= min_width)
        .map(|g| match edge {
            Edge::Start => g.1 as f32,
            Edge::End => g.0 as f32,
        })
        .filter(|e| (e - target).abs() <= window)
        .min_by(|a, b| (a - target).abs().total_cmp(&(b - target).abs()))
}

/// `span`'s edges moved to the nearest word gap within 1.5 line heights (font
/// estimates drift that far over a long line in another font), else the
/// nearest glyph gap within half a line height.
fn snap_with(gaps: &[(usize, usize)], x0: usize, h: f32, span: Rect) -> Rect {
    let snap = |x: f32, edge| {
        let local = x - x0 as f32;
        snap_edge(gaps, local, edge, word_gap(h), 1.5 * h)
            .or_else(|| snap_edge(gaps, local, edge, 1.0, 0.45 * h))
            .map(|e| x0 as f32 + e)
    };
    let left = snap(span.x, Edge::Start).map_or(span.x, |e| e - 1.0);
    let right = snap(span.right(), Edge::End).map_or(span.right(), |e| e + 1.0);
    if right - left < 2.0 {
        return span;
    }
    Rect::from_edges(left, span.y, right, span.bottom())
}

/// `span` (inside `line`, both in `image` pixels) with its left and right
/// edges moved to the nearest gap between glyphs, if one is close.
#[cfg(test)]
fn snap_span(image: &Pixmap, line: Rect, span: Rect) -> Rect {
    match ink_columns(image, line) {
        Some((x0, columns)) => snap_with(&gaps(&columns), x0, line.h, span),
        None => span,
    }
}

/// For each pixel column of `line` (from the returned x), whether it has ink,
/// judged on the middle rows so that neighboring lines' descenders don't count.
fn ink_columns(image: &Pixmap, line: Rect) -> Option<(usize, Vec<bool>)> {
    let (w, h) = (image.width() as i32, image.height() as i32);
    let x0 = (line.x.floor() as i32).clamp(0, w);
    let x1 = (line.right().ceil() as i32).clamp(0, w);
    let y0 = ((line.y + line.h * 0.15).floor() as i32).clamp(0, h);
    let y1 = ((line.bottom() - line.h * 0.15).ceil() as i32).clamp(0, h);
    if x1 - x0 < 2 || y1 <= y0 {
        return None;
    }
    let background = background(image, line)?;
    let columns = (x0..x1).map(|x| (y0..y1).any(|y| is_ink(image, x, y, background))).collect();
    Some((x0 as usize, columns))
}

/// The line's background: the most common color on its box's edge rows (OCR
/// boxes have a little padding, so those are mostly background).
fn background(image: &Pixmap, line: Rect) -> Option<[i32; 3]> {
    let (w, h) = (image.width() as i32, image.height() as i32);
    let (x0, x1) = ((line.x.floor() as i32).clamp(0, w), (line.right().ceil() as i32).clamp(0, w));
    let (top, bottom) = ((line.y.floor() as i32).clamp(0, h - 1), (line.bottom().ceil() as i32 - 1).clamp(0, h - 1));
    most_common((x0..x1).flat_map(|x| [rgb(image, x, top), rgb(image, x, bottom)]))
}

fn rgb(image: &Pixmap, x: i32, y: i32) -> [i32; 3] {
    let p = image.pixel(x as u32, y as u32).expect("in bounds");
    [p.red() as i32, p.green() as i32, p.blue() as i32]
}

fn is_ink(image: &Pixmap, x: i32, y: i32, background: [i32; 3]) -> bool {
    rgb(image, x, y).iter().zip(background).any(|(a, b)| (a - b).abs() > INK_CONTRAST)
}

fn most_common(colors: impl Iterator<Item = [i32; 3]>) -> Option<[i32; 3]> {
    let mut counts: std::collections::HashMap<[i32; 3], usize> = std::collections::HashMap::new();
    for c in colors {
        *counts.entry(c).or_default() += 1;
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(c, _)| c)
}

/// Runs of columns without ink, as `(start, end)` column ranges.
fn gaps(columns: &[bool]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &ink) in columns.iter().chain([&true]).enumerate() {
        match (ink, start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::{Color, Paint};

    /// A white line 20 px high with black "glyphs" `width` px wide at `xs`.
    fn line_image(glyphs: &[(u32, u32)]) -> Pixmap {
        let mut p = Pixmap::new(200, 30).unwrap();
        p.fill(Color::WHITE);
        let mut paint = Paint::default();
        paint.set_color_rgba8(30, 30, 30, 255);
        for &(x, w) in glyphs {
            p.fill_rect(tiny_skia::Rect::from_xywh(x as f32, 8.0, w as f32, 14.0).unwrap(), &paint, Default::default(), None);
        }
        p
    }

    #[test]
    fn span_edges_snap_to_word_gaps() {
        // "abc: defgh": glyphs 6 px wide with 1 px gaps; a 6 px space after the colon.
        let label = [(10, 6), (17, 6), (24, 6), (31, 2)];
        let value = [(39, 6), (46, 6), (53, 6), (60, 6), (67, 6)];
        let image = line_image(&[&label[..], &value[..]].concat());
        let line = Rect::new(8.0, 5.0, 70.0, 20.0);
        // Estimated a few pixels off on both sides.
        let snapped = snap_span(&image, line, Rect::new(35.0, 5.0, 36.0, 20.0));
        assert_eq!((snapped.x, snapped.right()), (38.0, 74.0));
        assert_eq!((snapped.y, snapped.h), (5.0, 20.0));
    }

    #[test]
    fn without_a_word_gap_edges_snap_to_glyph_gaps() {
        // "user:pass" in a URL: only 1 px gaps.
        let glyphs: Vec<(u32, u32)> = (0..12).map(|i| (10 + i * 7, 6)).collect();
        let image = line_image(&glyphs);
        let line = Rect::new(8.0, 5.0, 90.0, 20.0);
        let snapped = snap_span(&image, line, Rect::new(29.0, 5.0, 33.0, 20.0));
        // Glyph 3 starts at x 31; glyph 7 ends at x 65.
        assert_eq!((snapped.x, snapped.right()), (30.0, 66.0));
    }

    #[test]
    fn words_map_onto_their_ink_clusters_in_any_font() {
        // "ab: cdefgh ij" in a wide font: 9 px glyphs, 1 px gaps, 7 px spaces.
        let mut glyphs = Vec::new();
        let mut x = 10;
        for word in ["ab:", "cdefgh", "ij"] {
            for _ in word.chars() {
                glyphs.push((x, 9));
                x += 10;
            }
            x += 7;
        }
        let image = line_image(&glyphs);
        let line = Rect::new(8.0, 5.0, x as f32 - 7.0 - 8.0 + 2.0, 20.0);
        // Without fonts the estimate is the whole line; the clusters still place the word.
        let r = place_span(&image, &FontSet::empty(), line, "ab: cdefgh ij", 4..10);
        // "cdefgh" is the second cluster: glyphs from x 47 to 106.
        assert_eq!((r.x, r.right()), (46.0, 107.0));
    }

    #[test]
    fn monospace_colons_stay_with_their_label() {
        // "user: report" in a monospace font: 8 px cells, glyphs 6 px, a colon
        // only 2 px wide in the middle of its cell, then a space.
        let mut glyphs = Vec::new();
        for (i, c) in "user: report".chars().enumerate() {
            let x = 10 + 8 * i as u32;
            match c {
                ' ' => {}
                ':' => glyphs.push((x + 3, 2)),
                _ => glyphs.push((x + 1, 6)),
            }
        }
        let image = line_image(&glyphs);
        let line = Rect::new(8.0, 5.0, 100.0, 20.0);
        let r = place_span(&image, &FontSet::empty(), line, "user: report", 6..12);
        // "report" spans cells 6..12: ink from x 59 to 105.
        assert_eq!((r.x, r.right()), (58.0, 106.0));
        // Its last two characters, inside the word, by monospace metrics.
        let r = place_span(&image, &FontSet::empty(), line, "user: report", 10..12);
        assert_eq!((r.x, r.right()), (90.0, 106.0));
    }

    #[test]
    fn word_and_cluster_helpers() {
        assert_eq!(words(" ab  c "), vec![1..3, 5..6]);
        let cols: Vec<bool> = "..xx.xx....xxx..".chars().map(|c| c == 'x').collect();
        let all_gaps = gaps(&cols);
        assert_eq!(word_clusters(&cols, &all_gaps, 2), Some(vec![(2, 7), (11, 14)]));
        assert_eq!(word_clusters(&cols, &all_gaps, 1), Some(vec![(2, 14)]));
        // Three words but only one clear word gap: no mapping.
        assert_eq!(word_clusters(&cols, &all_gaps, 3), None);
        // Bad byte ranges return the whole line without panicking.
        let image = line_image(&[(10, 6), (25, 6)]);
        let line = Rect::new(8.0, 5.0, 30.0, 20.0);
        assert_eq!(place_span(&image, &FontSet::empty(), line, "密 a", 1..4), line);
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = 4..1;
        assert_eq!(place_span(&image, &FontSet::empty(), line, "密 a", reversed), line);
    }

    #[test]
    fn spans_grow_to_cover_descenders() {
        // Glyphs from y 8 to 22, one with a descender to y 27; the OCR box stops at 22.
        let mut image = line_image(&[(10, 6), (17, 6), (24, 6)]);
        let mut paint = Paint::default();
        paint.set_color_rgba8(30, 30, 30, 255);
        image.fill_rect(tiny_skia::Rect::from_xywh(17.0, 22.0, 3.0, 5.0).unwrap(), &paint, Default::default(), None);
        let line = Rect::new(8.0, 6.0, 30.0, 16.0);
        let r = place_span(&image, &FontSet::empty(), line, "abc", 0..3);
        assert_eq!((r.y, r.bottom()), (6.0, 28.0));
    }

    #[test]
    fn spans_without_nearby_gaps_stay() {
        // One long run of ink; its ends are more than 1.5 line heights away.
        let image = line_image(&[(10, 140)]);
        let span = Rect::new(60.0, 5.0, 40.0, 20.0);
        assert_eq!(snap_span(&image, Rect::new(8.0, 5.0, 150.0, 20.0), span), span);
    }
}
