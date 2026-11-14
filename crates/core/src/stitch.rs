//! Scrolling screenshots: stitches frames of a scrolling region into one tall image.
//!
//! Frames are hashed in narrow column strips per row. For each new frame:
//!
//! - strips identical at the same position in both frames are fixed content
//!   (side panels, margins); the scrolling area is the span between them;
//! - the span is split into bands that each look for the vertical shift that
//!   lines the frames up and then vote, so a band disturbed by a moving scrollbar
//!   or an animation doesn't spoil the match;
//! - strips at the span's edges that don't follow the shift (scrollbar thumbs and
//!   tracks) are dropped from matching and from the output;
//! - rows that stay put at the top and bottom (sticky headers and footers) are
//!   found once and kept out of the middle, and only newly revealed rows are added.

use std::ops::Range;

use tiny_skia::Pixmap;

use crate::pixels::pixmap_from_rgba;

/// Width of a column strip in pixels.
const STRIP: usize = 8;
/// Rows of overlap needed to trust a match.
const MIN_OVERLAP: usize = 24;
/// Distinct (non-uniform) rows that must match for a shift to be accepted.
const MIN_MATCHING_ROWS: usize = 8;
/// Share of distinct overlapping rows allowed to differ (animations, carets).
const MAX_MISMATCH: f32 = 0.05;
/// The scrolling middle must keep at least this many rows after removing
/// sticky header and footer.
const MIN_MIDDLE: usize = 40;
/// A strip whose rows differ in at most this share of rows is fixed content.
const FIXED_TOLERANCE: f32 = 0.01;
/// An edge strip whose rows don't follow the shift in more than this share of
/// rows is a scrollbar (or similar) and is trimmed.
const NOISY_EDGE: f32 = 0.03;
/// Bands that vote on the shift: as many as fit with at least this many strips.
const MIN_BAND_STRIPS: usize = 6;
const MAX_BANDS: usize = 4;
/// A change confined to one run of at most this many strips (48 px) is a
/// scrollbar fading in or out, a caret or a hover effect, not a scroll.
const MAX_LOCAL_CHANGE: usize = 6;
/// Before the first scroll, this many frames in a row without a match mean the
/// first frame was bad (e.g. caught the overlay fading out): start over.
const RESTART_AFTER: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// First frame stored.
    Started,
    /// Same content as last time (not scrolled yet, or scrolled back to it).
    Unchanged,
    /// This many new rows were added at the bottom.
    Appended(u32),
    /// The content moved up; nothing is added until it moves past the end again.
    ScrolledUp,
    /// No overlap with the last frame: the user scrolled too far between frames.
    LostTrack,
    /// The image reached its height limit; further frames are ignored.
    Full,
}

struct Frame {
    rgba: Vec<u8>,
    width: usize,
    height: usize,
    strips: usize,
    /// Hash of every (row, strip) cell, row-major.
    cells: Vec<u64>,
    /// The cell's color when the whole cell is one color.
    flat: Vec<Option<u32>>,
}

impl Frame {
    fn new(width: usize, rgba: Vec<u8>) -> Self {
        let height = rgba.len() / (width * 4);
        let strips = width.div_ceil(STRIP);
        let mut cells = Vec::with_capacity(height * strips);
        let mut flat = Vec::with_capacity(height * strips);
        for row in rgba.chunks_exact(width * 4) {
            for cell in row.chunks(STRIP * 4) {
                cells.push(hash(cell));
                let px = cell.as_chunks::<4>().0;
                flat.push(px.iter().all(|p| *p == px[0]).then(|| u32::from_le_bytes(px[0])));
            }
        }
        Self { rgba, width, height, strips, cells, flat }
    }

    fn cell(&self, row: usize, strip: usize) -> u64 {
        self.cells[row * self.strips + strip]
    }

    /// Per-row hash and uniform flag over `strips`, for `rows`.
    fn band(&self, strips: &Range<usize>, rows: &Range<usize>) -> (Vec<u64>, Vec<bool>) {
        rows.clone()
            .map(|r| {
                let cells = &self.cells[r * self.strips + strips.start..r * self.strips + strips.end];
                let flats = &self.flat[r * self.strips + strips.start..r * self.strips + strips.end];
                let h = cells.iter().fold(0x9E37_79B9_7F4A_7C15u64, |h, c| (h ^ c).wrapping_mul(0x0000_0100_0000_01B3).rotate_left(23));
                let uniform = flats[0].is_some() && flats.iter().all(|f| *f == flats[0]);
                (h, uniform)
            })
            .unzip()
    }

    /// Pixel columns covered by `strips`.
    fn columns(&self, strips: &Range<usize>) -> Range<usize> {
        strips.start * STRIP..(strips.end * STRIP).min(self.width)
    }

    /// Bytes of `rows`, cropped to pixel `columns`.
    fn crop(&self, rows: Range<usize>, columns: &Range<usize>) -> Vec<u8> {
        let stride = self.width * 4;
        rows.flat_map(|r| &self.rgba[r * stride + columns.start * 4..r * stride + columns.end * 4]).copied().collect()
    }
}

fn hash(bytes: &[u8]) -> u64 {
    // Fast multiplicative hash over 8-byte words; collisions only cost a
    // spurious match, which the mismatch threshold tolerates.
    let mut h = 0x9E37_79B9_7F4A_7C15u64;
    let (words, rest) = bytes.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(0x0000_0100_0000_01B3).rotate_left(29);
    }
    for b in rest {
        h = (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Decided at the first scroll.
struct Layout {
    /// Strips stored in the body (the scrolling span).
    stored: Range<usize>,
    /// Strips still trusted for matching and kept in the output (shrinks when
    /// edge strips turn out to be scrollbars).
    keep: Range<usize>,
    /// Sticky header and footer rows.
    top: usize,
    bottom: usize,
}

pub struct Stitcher {
    width: usize,
    max_height: usize,
    /// Composed rows: the first frame (cropped to the stored strips and without
    /// its footer, once the layout is known), then appended rows.
    body: Vec<u8>,
    /// The frame `body` currently ends with.
    last: Option<Frame>,
    layout: Option<Layout>,
    lost_before_start: u32,
    full: bool,
}

impl Stitcher {
    pub fn new(width: u32, max_height: u32) -> Self {
        Self {
            width: width as usize,
            max_height: max_height as usize,
            body: Vec::new(),
            last: None,
            layout: None,
            lost_before_start: 0,
            full: false,
        }
    }

    /// Width of the frames this stitcher takes.
    pub fn width(&self) -> u32 {
        self.width as u32
    }

    fn body_width(&self) -> usize {
        match (&self.layout, &self.last) {
            (Some(layout), Some(frame)) => frame.columns(&layout.stored).len(),
            _ => self.width,
        }
    }

    /// Height of the image [`Self::finish`] would produce.
    pub fn height(&self) -> u32 {
        let footer = self.layout.as_ref().map_or(0, |l| l.bottom);
        (self.body.len() / (self.body_width() * 4) + footer) as u32
    }

    fn restart(&mut self, frame: Frame) -> Progress {
        self.body = frame.rgba.clone();
        self.last = Some(frame);
        self.layout = None;
        self.lost_before_start = 0;
        Progress::Started
    }

    /// Adds a frame of opaque RGBA pixels. Every frame must have the same size.
    pub fn push(&mut self, height: u32, rgba: Vec<u8>) -> Progress {
        debug_assert_eq!(rgba.len(), self.width * 4 * height as usize);
        let cur = Frame::new(self.width, rgba);
        let Some(prev) = &self.last else { return self.restart(cur) };
        if self.full {
            return Progress::Full;
        }
        if prev.height != cur.height || prev.cells == cur.cells || local_change(prev, &cur) {
            return Progress::Unchanged;
        }
        match self.layout.is_some() {
            false => self.first_scroll(cur),
            true => self.next_scroll(cur),
        }
    }

    fn first_scroll(&mut self, cur: Frame) -> Progress {
        let prev = self.last.as_ref().expect("checked by push");
        let n = cur.height;
        let fixed = |s: usize| {
            let changed = (0..n).filter(|&r| prev.cell(r, s) != cur.cell(r, s)).count();
            changed as f32 <= n as f32 * FIXED_TOLERANCE
        };
        let Some(first) = (0..cur.strips).find(|&s| !fixed(s)) else { return Progress::Unchanged };
        let last = (0..cur.strips).rev().find(|&s| !fixed(s)).expect("a strip changed");
        let mut stored = first..last + 1;

        let shift = match estimate_shift(prev, &cur, &stored, &(0..n)) {
            Some(shift) if shift > 0 => shift as usize,
            Some(_) => return Progress::ScrolledUp,
            None => {
                self.lost_before_start += 1;
                if self.lost_before_start >= RESTART_AFTER {
                    return self.restart(cur);
                }
                return Progress::LostTrack;
            }
        };
        // Sticky bars first: rows that don't move would otherwise make every strip
        // whose only content they are (blank margins) look like a scrollbar.
        let (top, bottom) = sticky_bands(prev, &cur, &stored);
        let middle = top..n - bottom;

        // Blank margins never change either, but belong to the scrolling page,
        // unlike a side panel with content of its own: extend over them.
        let blank = |s: usize| {
            let flat = middle.clone().filter(|&r| cur.flat[r * cur.strips + s].is_some()).count();
            flat as f32 >= middle.len() as f32 * (1.0 - FIXED_TOLERANCE)
        };
        while stored.start > 0 && blank(stored.start - 1) {
            stored.start -= 1;
        }
        while stored.end < cur.strips && blank(stored.end) {
            stored.end += 1;
        }
        let keep = trim_noisy_edges(prev, &cur, stored.clone(), &middle, shift);

        // The first frame becomes the start of the body: scrolling span only, and
        // without the footer, which belongs at the very end.
        let columns = prev.columns(&stored);
        self.body = prev.crop(0..n - bottom, &columns);
        self.layout = Some(Layout { stored, keep, top, bottom });
        self.append(cur, shift)
    }

    fn next_scroll(&mut self, cur: Frame) -> Progress {
        let prev = self.last.as_ref().expect("checked by push");
        let layout = self.layout.as_ref().expect("checked by push");
        let n = cur.height;
        let middle = layout.top..n - layout.bottom;
        // If the sticky bands stopped holding (e.g. a header that hides on
        // scroll), try again over the whole height.
        let shift = estimate_shift(prev, &cur, &layout.keep, &middle)
            .or_else(|| estimate_shift(prev, &cur, &layout.keep, &(0..n)));
        let shift = match shift {
            Some(shift) if shift > 0 => shift as usize,
            Some(_) => return Progress::ScrolledUp,
            None => return Progress::LostTrack,
        };
        let keep = trim_noisy_edges(prev, &cur, layout.keep.clone(), &middle, shift);
        self.layout.as_mut().expect("checked above").keep = keep;
        self.append(cur, shift)
    }

    /// Adds the `shift` newly revealed rows of `cur` above its footer.
    fn append(&mut self, cur: Frame, shift: usize) -> Progress {
        let layout = self.layout.as_ref().expect("set before appending");
        let columns = cur.columns(&layout.stored);
        let stride = columns.len() * 4;
        let end = cur.height - layout.bottom;
        let mut start = end.saturating_sub(shift).max(layout.top);
        let room = self.max_height.saturating_sub(self.body.len() / stride + layout.bottom);
        if end - start > room {
            start = end - room;
            self.full = true;
        }
        self.body.extend(cur.crop(start..end, &columns));
        self.last = Some(cur);
        if self.full { Progress::Full } else { Progress::Appended((end - start) as u32) }
    }

    /// The stitched image so far: body plus the latest frame's footer, cropped to
    /// the strips that scrolled cleanly.
    pub fn finish(&self) -> Option<Pixmap> {
        let last = self.last.as_ref()?;
        let Some(layout) = &self.layout else {
            return pixmap_from_rgba(self.width as u32, last.height as u32, self.body.clone());
        };
        let stored = last.columns(&layout.stored);
        let mut rgba = self.body.clone();
        rgba.extend(last.crop(last.height - layout.bottom..last.height, &stored));

        let keep = last.columns(&layout.keep);
        let (x0, x1) = (keep.start - stored.start, keep.end - stored.start);
        let (src, dst) = (stored.len() * 4, keep.len() * 4);
        let rows = rgba.len() / src;
        let mut out = Vec::with_capacity(rows * dst);
        for row in rgba.chunks_exact(src) {
            out.extend_from_slice(&row[x0 * 4..x1 * 4]);
        }
        pixmap_from_rgba(keep.len() as u32, rows as u32, out)
    }
}

/// Whether the frames differ only within one narrow run of strips.
fn local_change(prev: &Frame, cur: &Frame) -> bool {
    let changed = |s: usize| (0..cur.height).any(|r| prev.cell(r, s) != cur.cell(r, s));
    let Some(first) = (0..cur.strips).find(|&s| changed(s)) else { return true };
    let last = (0..cur.strips).rev().find(|&s| changed(s)).expect("found one");
    last - first < MAX_LOCAL_CHANGE && last - first + 1 < cur.strips
}

/// Rows identical at the same position in both frames over `strips`, from the
/// top and from the bottom. A few differing strips per row (a scrollbar) are
/// tolerated.
fn sticky_bands(prev: &Frame, cur: &Frame, strips: &Range<usize>) -> (usize, usize) {
    let n = cur.height;
    let tolerance = strips.len() / 10;
    let same = |r: usize| strips.clone().filter(|&s| prev.cell(r, s) != cur.cell(r, s)).count() <= tolerance;
    let top = (0..n).take_while(|&r| same(r)).count();
    let bottom = (0..n).rev().take_while(|&r| same(r)).count();
    // Leave a scrolling middle; if the bands would eat it, assume no sticky parts.
    if top + bottom + MIN_MIDDLE > n { (0, 0) } else { (top, bottom) }
}

/// The vertical shift that lines `cur` up with `prev` within `strips` and
/// `rows`: positive when the content moved up (the user scrolled down). Bands
/// of strips search independently and vote with their matched rows.
fn estimate_shift(prev: &Frame, cur: &Frame, strips: &Range<usize>, rows: &Range<usize>) -> Option<i32> {
    let bands = (strips.len() / MIN_BAND_STRIPS).clamp(1, MAX_BANDS);
    let mut votes: Vec<(i32, usize)> = Vec::new();
    for b in 0..bands {
        let band = strips.start + strips.len() * b / bands..strips.start + strips.len() * (b + 1) / bands;
        let (ph, pf) = prev.band(&band, rows);
        let (ch, cf) = cur.band(&band, rows);
        // Rows that don't move in this band (a header over just this part).
        let n = ph.len();
        let mut top = (0..n).take_while(|&i| ph[i] == ch[i]).count();
        let mut bottom = (0..n).rev().take_while(|&i| ph[i] == ch[i]).count();
        if top + bottom + MIN_MIDDLE > n {
            (top, bottom) = (0, 0);
        }
        let mid = top..n - bottom;
        if let Some((shift, matched)) = find_shift(&ph[mid.clone()], &pf[mid.clone()], &ch[mid.clone()], &cf[mid]) {
            match votes.iter_mut().find(|(s, _)| *s == shift) {
                Some((_, total)) => *total += matched,
                None => votes.push((shift, matched)),
            }
        }
    }
    votes.into_iter().max_by_key(|&(shift, matched)| (matched, std::cmp::Reverse(shift.abs()))).map(|(s, _)| s)
}

/// Best shift between two row-hash sequences, with its number of matched rows.
fn find_shift(prev: &[u64], prev_flat: &[bool], cur: &[u64], cur_flat: &[bool]) -> Option<(i32, usize)> {
    let n = cur.len();
    let mut best: Option<(usize, i32)> = None;
    for dy in 1..n.saturating_sub(MIN_OVERLAP) {
        for (sign, (a, af, b, bf)) in [(1, (prev, prev_flat, cur, cur_flat)), (-1, (cur, cur_flat, prev, prev_flat))] {
            // Row `i` of `b` should equal row `i + dy` of `a`.
            let (mut matched, mut distinct, mut mismatched) = (0, 0, 0);
            let budget = ((n - dy) as f32 * MAX_MISMATCH) as usize;
            for i in 0..n - dy {
                if bf[i] && af[i + dy] {
                    continue;
                }
                distinct += 1;
                if a[i + dy] == b[i] {
                    matched += 1;
                } else {
                    mismatched += 1;
                    if mismatched > budget {
                        break;
                    }
                }
            }
            let ok = matched >= MIN_MATCHING_ROWS && mismatched <= ((distinct as f32) * MAX_MISMATCH) as usize;
            // Prefer the most matched rows; on ties the smaller shift (more overlap).
            if ok && best.is_none_or(|(m, _)| matched > m) {
                best = Some((matched, sign * dy as i32));
            }
        }
    }
    best.map(|(matched, dy)| (dy, matched))
}

/// Drops strips at either edge of `keep` that don't follow `shift` (scrollbar
/// thumbs move differently from the content). Judged against the typical strip,
/// because rows that don't move at all (sticky bars) affect every strip equally.
/// Never trims below half.
fn trim_noisy_edges(prev: &Frame, cur: &Frame, mut keep: Range<usize>, rows: &Range<usize>, shift: usize) -> Range<usize> {
    let mismatch = |s: usize| -> Option<f32> {
        let (mut evidence, mut wrong) = (0, 0);
        for r in rows.start..rows.end.saturating_sub(shift) {
            let (a, b) = ((r + shift) * prev.strips + s, r * cur.strips + s);
            if prev.flat[a].is_some() && prev.flat[a] == cur.flat[b] {
                continue;
            }
            evidence += 1;
            wrong += (prev.cells[a] != cur.cells[b]) as usize;
        }
        (evidence >= MIN_MATCHING_ROWS).then(|| wrong as f32 / evidence as f32)
    };
    let mut rates: Vec<f32> = keep.clone().filter_map(mismatch).collect();
    if rates.is_empty() {
        return keep;
    }
    rates.sort_by(f32::total_cmp);
    let typical = rates[rates.len() / 2];
    let noisy = |s: usize| mismatch(s).is_some_and(|rate| rate > typical + NOISY_EDGE);
    let min_len = keep.len().div_ceil(2);
    while keep.len() > min_len && noisy(keep.start) {
        keep.start += 1;
    }
    while keep.len() > min_len && noisy(keep.end - 1) {
        keep.end -= 1;
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 64;

    /// A tall "document" `w` pixels wide with distinct rows and blank runs.
    fn document_w(w: usize, height: usize, mut seed: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * height * 4);
        for y in 0..height {
            if y % 50 < 6 {
                // Blank spacing rows, like margins between paragraphs.
                out.extend(std::iter::repeat_n([250u8, 250, 250, 255], w).flatten());
                continue;
            }
            for _ in 0..w {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let [r, g, b, _] = seed.to_le_bytes();
                out.extend_from_slice(&[r, g, b, 255]);
            }
        }
        out
    }

    fn document(height: usize) -> Vec<u8> {
        document_w(W, height, 0x2545_F491)
    }

    fn rows_w(img: &[u8], w: usize, from: usize, to: usize) -> &[u8] {
        &img[from * w * 4..to * w * 4]
    }

    fn rows(img: &[u8], from: usize, to: usize) -> &[u8] {
        rows_w(img, W, from, to)
    }

    /// The viewport at scroll position `y`: `header` rows fixed at the top,
    /// `footer` fixed at the bottom, content in between.
    fn view(doc: &[u8], header: &[u8], footer: &[u8], height: usize, y: usize) -> Vec<u8> {
        let (h, f) = (header.len() / (W * 4), footer.len() / (W * 4));
        let mut v = header.to_vec();
        v.extend_from_slice(rows(doc, y, y + height - h - f));
        v.extend_from_slice(footer);
        v
    }

    #[test]
    fn stitches_scroll_sequence_with_sticky_header_and_footer() {
        let doc = document(2000);
        let header: Vec<u8> = (0..W * 30).flat_map(|i| [20u8, (i % 251) as u8, 200, 255]).collect();
        let footer: Vec<u8> = document(20).into_iter().map(|b| b / 2).collect();
        let height = 300;
        let mut s = Stitcher::new(W as u32, 100_000);
        // Uneven scroll steps, including a pause and a step back up.
        let positions = [0, 0, 37, 90, 91, 160, 120, 230, 400, 555, 700, 760, 900, 1100, 1300, 1450, 1600, 1750];
        let mut results = Vec::new();
        for &y in &positions {
            results.push(s.push(height as u32, view(&doc, &header, &footer, height, y)));
        }
        assert_eq!(results[0], Progress::Started);
        assert_eq!(results[1], Progress::Unchanged);
        assert_eq!(results[6], Progress::ScrolledUp);

        let out = s.finish().unwrap();
        let content = 1750 + height - 30 - 20; // last content row shown
        let mut expected = header.clone();
        expected.extend_from_slice(rows(&doc, 0, content));
        expected.extend_from_slice(&footer);
        assert_eq!((out.width() as usize, out.height() as usize), (W, 30 + content + 20));
        assert_eq!(s.height(), out.height());
        assert!(out.data() == &expected[..], "stitched image differs from the document");
    }

    #[test]
    fn too_fast_scrolling_loses_track_and_recovers() {
        let doc = document(3000);
        let mut s = Stitcher::new(W as u32, 100_000);
        let v = |y| rows(&doc, y, y + 200).to_vec();
        assert_eq!(s.push(200, v(0)), Progress::Started);
        assert_eq!(s.push(200, v(30)), Progress::Appended(30));
        assert_eq!(s.push(200, v(500)), Progress::LostTrack);
        // Back within reach of the last accepted frame: continues seamlessly.
        assert_eq!(s.push(200, v(150)), Progress::Appended(120));
        let out = s.finish().unwrap();
        assert!(out.data() == rows(&doc, 0, 350));
    }

    #[test]
    fn stops_at_the_height_limit() {
        let doc = document(3000);
        let mut s = Stitcher::new(W as u32, 500);
        let mut last = Progress::Started;
        for y in (0..2000).step_by(100) {
            last = s.push(200, rows(&doc, y, y + 200).to_vec());
            if last == Progress::Full {
                break;
            }
        }
        assert_eq!(last, Progress::Full);
        let out = s.finish().unwrap();
        assert_eq!(out.height(), 500);
        assert!(out.data() == rows(&doc, 0, 500));
    }

    /// Content `cw` wide next to `extra` columns produced by `side(row, y)`.
    fn compose(doc: &[u8], cw: usize, y: usize, height: usize, left: usize, right: usize, side: impl Fn(usize, bool) -> [u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        for r in 0..height {
            for _ in 0..left {
                v.extend_from_slice(&side(r, true));
            }
            v.extend_from_slice(rows_w(doc, cw, y + r, y + r + 1));
            for _ in 0..right {
                v.extend_from_slice(&side(r, false));
            }
        }
        v
    }

    #[test]
    fn scrollbar_at_the_edge_is_tolerated_and_trimmed() {
        // 80 px of content plus a 16 px scrollbar whose thumb tracks the scroll
        // position, macOS style: hidden until scrolling starts.
        let (cw, sb, height, doc_h) = (80, 16, 240, 3000);
        let doc = document_w(cw, doc_h, 99);
        let mut s = Stitcher::new((cw + sb) as u32, 100_000);
        let frame = |y: usize, thumb: bool| {
            let top = y * height / doc_h;
            let len = height * height / doc_h + 20;
            compose(&doc, cw, y, height, 0, sb, |r, _| {
                if thumb && (top..top + len).contains(&r) { [90, 90, 90, 255] } else { [235, 235, 235, 255] }
            })
        };
        assert_eq!(s.push(height as u32, frame(0, false)), Progress::Started);
        for y in [40, 100, 180, 260, 330, 420, 500, 610, 700] {
            assert!(matches!(s.push(height as u32, frame(y, true)), Progress::Appended(_)), "frame at {y}");
        }
        let out = s.finish().unwrap();
        assert_eq!(out.width() as usize, cw, "scrollbar columns trimmed");
        let mut expected = Vec::new();
        for r in 0..700 + height {
            expected.extend_from_slice(rows_w(&doc, cw, r, r + 1));
        }
        assert!(out.data() == &expected[..]);
    }

    #[test]
    fn scrollbar_appearing_without_motion_is_not_a_scroll() {
        let (cw, sb, height) = (80, 16, 240);
        let doc = document_w(cw, 2000, 5);
        let frame = |y: usize, thumb: bool| {
            compose(&doc, cw, y, height, 0, sb, |r, _| if thumb && r < 60 { [90, 90, 90, 255] } else { [235, 235, 235, 255] })
        };
        let mut s = Stitcher::new((cw + sb) as u32, 100_000);
        assert_eq!(s.push(height as u32, frame(0, false)), Progress::Started);
        assert_eq!(s.push(height as u32, frame(0, true)), Progress::Unchanged);
        assert!(matches!(s.push(height as u32, frame(50, true)), Progress::Appended(50)));
        assert_eq!(s.push(height as u32, frame(50, false)), Progress::Unchanged);
    }

    #[test]
    fn fixed_sidebar_is_left_out() {
        let (side, cw, height) = (32, 64, 240);
        let doc = document_w(cw, 2000, 7);
        // A sidebar with its own non-uniform content that never moves.
        let sidebar = |r: usize, _: bool| [(r * 7 % 256) as u8, 40, (r * 3 % 256) as u8, 255];
        let mut s = Stitcher::new((side + cw) as u32, 100_000);
        for y in [0, 50, 120, 200, 290] {
            s.push(height as u32, compose(&doc, cw, y, height, side, 0, sidebar));
        }
        let out = s.finish().unwrap();
        assert_eq!(out.width() as usize, cw);
        assert!(out.data() == rows_w(&doc, cw, 0, 290 + height));
    }

    #[test]
    fn bad_first_frame_is_replaced() {
        let doc = document(2000);
        let mut s = Stitcher::new(W as u32, 100_000);
        let v = |y| rows(&doc, y, y + 200).to_vec();
        // Caught the overlay fading out: a dimmed copy of the screen.
        let dimmed: Vec<u8> = v(0).chunks(4).flat_map(|p| [p[0] / 2, p[1] / 2, p[2] / 2, 255]).collect();
        assert_eq!(s.push(200, dimmed), Progress::Started);
        assert_eq!(s.push(200, v(0)), Progress::LostTrack);
        assert_eq!(s.push(200, v(0)), Progress::LostTrack);
        assert_eq!(s.push(200, v(0)), Progress::Started);
        assert_eq!(s.push(200, v(60)), Progress::Appended(60));
        assert!(s.finish().unwrap().data() == rows(&doc, 0, 260));
    }
}
