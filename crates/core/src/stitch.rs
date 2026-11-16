//! Scrolling screenshots: stitches frames of a scrolling region into one tall image.
//!
//! Frames are reduced to cells: the average brightness of each row of each
//! narrow column strip. Matching compares cells with a tolerance, after a
//! little vertical smoothing, because apps often redraw scrolled content at
//! fractional pixel offsets (display scaling, trackpads), so frames are rarely
//! exact shifted copies. The stitched image is still built from the real pixels.
//!
//! For each new frame:
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
/// Distinct (non-uniform) rows needed as evidence for a shift.
const MIN_MATCHING_ROWS: usize = 8;
/// Cells at the same position this close (brightness levels) are unchanged.
const SAME_CELL: u8 = 3;
/// Cells whose brightness spans at most this are a single color.
const FLAT_CELL: u8 = 6;
/// Average smoothed-cell difference above which two rows don't match.
const ROW_TOL: f32 = 14.0;
/// Share of distinct overlapping rows allowed not to match (animations, carets).
const MAX_BAD_ROWS: f32 = 0.10;
/// Average row difference over the overlap that a shift must stay below.
const AVG_TOL: f32 = 5.0;
/// A candidate shift is abandoned once its average over at least this many
/// distinct rows exceeds `AVG_TOL` by `HOPELESS` times: wrong shifts average
/// far above it, so this rejects them after a few dozen rows.
const EARLY_ROWS: usize = 32;
const HOPELESS: f32 = 2.5;
/// Smoothed cells further apart than this don't follow the shift (scrollbars).
const NOISY_CELL: u8 = 8;
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
    /// Average brightness of every (row, strip) cell, row-major.
    mean: Vec<u8>,
    /// `mean` smoothed vertically ([1, 2, 1] / 4), for matching across
    /// sub-pixel offsets.
    smooth: Vec<u8>,
    /// Whether the cell is (nearly) a single color.
    flat: Vec<bool>,
}

impl Frame {
    fn new(width: usize, rgba: Vec<u8>) -> Self {
        let height = rgba.len() / (width * 4);
        let strips = width.div_ceil(STRIP);
        let mut mean = Vec::with_capacity(height * strips);
        let mut flat = Vec::with_capacity(height * strips);
        for row in rgba.chunks_exact(width * 4) {
            for cell in row.chunks(STRIP * 4) {
                let (mut sum, mut lo, mut hi) = (0u32, u8::MAX, 0u8);
                let px = cell.as_chunks::<4>().0;
                for p in px {
                    let l = ((p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8) as u8;
                    (sum, lo, hi) = (sum + l as u32, lo.min(l), hi.max(l));
                }
                mean.push((sum / px.len() as u32) as u8);
                flat.push(hi - lo <= FLAT_CELL);
            }
        }
        let mut smooth = mean.clone();
        for r in 0..height {
            let (up, down) = (r.saturating_sub(1), (r + 1).min(height - 1));
            for s in 0..strips {
                let v = mean[up * strips + s] as u16 + 2 * mean[r * strips + s] as u16 + mean[down * strips + s] as u16;
                smooth[r * strips + s] = ((v + 2) / 4) as u8;
            }
        }
        Self { rgba, width, height, strips, mean, smooth, flat }
    }

    fn at(&self, row: usize, strip: usize) -> usize {
        row * self.strips + strip
    }

    /// Whether the cell is unchanged between the frames at the same position.
    fn same(&self, other: &Frame, row: usize, strip: usize) -> bool {
        let i = self.at(row, strip);
        self.mean[i].abs_diff(other.mean[i]) <= SAME_CELL
    }

    /// Whether `row` is one color across `strips` (matches anywhere).
    fn uniform(&self, row: usize, strips: &Range<usize>) -> bool {
        let cells = self.at(row, strips.start)..self.at(row, strips.end);
        let (lo, hi) = self.mean[cells.clone()].iter().fold((u8::MAX, 0), |(lo, hi), &m| (lo.min(m), hi.max(m)));
        self.flat[cells].iter().all(|f| *f) && hi - lo <= FLAT_CELL
    }

    /// Average difference of vertically smoothed cells, without any tolerance for
    /// sub-pixel offsets: it changes steadily with the offset, so it measures it.
    fn smooth_distance(&self, row: usize, other: &Frame, other_row: usize, strips: &Range<usize>) -> f32 {
        let a = &self.smooth[self.at(row, strips.start)..self.at(row, strips.end)];
        let b = &other.smooth[other.at(other_row, strips.start)..other.at(other_row, strips.end)];
        a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u32).sum::<u32>() as f32 / strips.len() as f32
    }

    fn row_cells(&self, row: usize, strips: &Range<usize>) -> &[u8] {
        &self.mean[self.at(row, strips.start)..self.at(row, strips.end)]
    }

    /// Average cell difference between our `row` and `other`'s `other_row`,
    /// allowing for up to half a pixel of extra shift: the row is also compared
    /// with the blends of `other_row` and each of its neighbors, which is what a
    /// half-pixel offset produces, and the closest of the three counts.
    fn row_distance(&self, row: usize, other: &Frame, other_row: usize, strips: &Range<usize>) -> f32 {
        let (b, a) = (self.row_cells(row, strips), other.row_cells(other_row, strips));
        let exact: u32 = b.iter().zip(a).map(|(x, y)| x.abs_diff(*y) as u32).sum();
        let blended = |n: &[u8]| -> u32 {
            b.iter().zip(a).zip(n).map(|((x, y), z)| (2 * *x as i32 - *y as i32 - *z as i32).unsigned_abs() / 2).sum()
        };
        let mut best = exact;
        if other_row > 0 {
            best = best.min(blended(other.row_cells(other_row - 1, strips)));
        }
        if other_row + 1 < other.height {
            best = best.min(blended(other.row_cells(other_row + 1, strips)));
        }
        best as f32 / strips.len() as f32
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
    /// Content offset of `last` relative to the first frame, in fractional
    /// pixels. Rows are appended so the body tracks its rounded value, which
    /// keeps sub-pixel shifts from accumulating into drift.
    position: f32,
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
            position: 0.0,
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
        self.position = 0.0;
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
        if prev.height != cur.height || local_change(prev, &cur) {
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
            let changed = (0..n).filter(|&r| !prev.same(&cur, r, s)).count();
            changed as f32 <= n as f32 * FIXED_TOLERANCE
        };
        let Some(first) = (0..cur.strips).find(|&s| !fixed(s)) else { return Progress::Unchanged };
        let last = (0..cur.strips).rev().find(|&s| !fixed(s)).expect("a strip changed");
        let mut stored = first..last + 1;

        let shift = match estimate_shift(prev, &cur, &stored, &(0..n)) {
            Some(shift) if shift > 0.0 => shift,
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
        let dy = shift.round() as usize;
        let (top, bottom) = sticky_bands(prev, &cur, &stored, &(0..n), Some(dy));
        let middle = top..n - bottom;

        // Blank margins never change either, but belong to the scrolling page,
        // unlike a side panel with content of its own: extend over them.
        let blank = |s: usize| {
            let flat = middle.clone().filter(|&r| cur.flat[cur.at(r, s)]).count();
            flat as f32 >= middle.len() as f32 * (1.0 - FIXED_TOLERANCE)
        };
        while stored.start > 0 && blank(stored.start - 1) {
            stored.start -= 1;
        }
        while stored.end < cur.strips && blank(stored.end) {
            stored.end += 1;
        }
        let min_keep = stored.len().div_ceil(2).max(stored.len().saturating_sub(16));
        let keep = trim_noisy_edges(prev, &cur, stored.clone(), &middle, dy, min_keep);

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
        let (extra_top, _) = sticky_bands(prev, &cur, &layout.keep, &(layout.top..n - layout.bottom), None);
        let middle = layout.top + extra_top..n - layout.bottom;
        // If the sticky bands stopped holding (e.g. a header that hides on
        // scroll), try again over the whole height.
        let shift = estimate_shift(prev, &cur, &layout.keep, &middle)
            .or_else(|| estimate_shift(prev, &cur, &layout.keep, &(0..n)));
        let shift = match shift {
            Some(shift) if shift > 0.0 => shift,
            Some(_) => return Progress::ScrolledUp,
            None => return Progress::LostTrack,
        };
        let dy = shift.round() as usize;
        let (detected_top, _) = sticky_bands(prev, &cur, &layout.keep, &(0..n - layout.bottom), Some(dy));
        let active_top = layout.top.max(detected_top);
        let active_middle = active_top..n - layout.bottom;
        let min_keep = layout.stored.len().div_ceil(2).max(layout.stored.len().saturating_sub(16));
        let keep = trim_noisy_edges(prev, &cur, layout.keep.clone(), &active_middle, dy, min_keep);
        let layout = self.layout.as_mut().expect("checked above");
        layout.top = active_top;
        layout.keep = keep;
        self.append(cur, shift)
    }

    /// Adds the rows of `cur` revealed by scrolling `shift` more pixels, above
    /// its footer.
    fn append(&mut self, cur: Frame, shift: f32) -> Progress {
        let new_position = self.position + shift;
        let shift = (new_position.round() - self.position.round()).max(0.0) as usize;
        self.position = new_position;
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

/// Whether the frames are the same, or differ only within one narrow run of strips.
fn local_change(prev: &Frame, cur: &Frame) -> bool {
    let changed = |s: usize| {
        let rows = (0..cur.height).filter(|&r| !prev.same(cur, r, s)).count();
        rows as f32 > cur.height as f32 * FIXED_TOLERANCE
    };
    let Some(first) = (0..cur.strips).find(|&s| changed(s)) else { return true };
    let last = (0..cur.strips).rev().find(|&s| changed(s)).expect("found one");
    last - first < MAX_LOCAL_CHANGE && last - first + 1 < cur.strips
}

/// Rows held fixed at the top and bottom of `rows` over `strips`, including
/// semi-transparent / blurred bars whose background tint changes as content
/// scrolls behind them while their own horizontal features stay put.
fn sticky_bands(prev: &Frame, cur: &Frame, strips: &Range<usize>, rows: &Range<usize>, shift: Option<usize>) -> (usize, usize) {
    let n = rows.len();
    if strips.is_empty() || n <= MIN_MIDDLE {
        return (0, 0);
    }
    let tolerance = strips.len() / 10;
    let unchanged = |r: usize| strips.clone().filter(|&s| !prev.same(cur, r, s)).count() <= tolerance;

    // A frosted-glass bar (`backdrop-filter: blur`) shifts in brightness when
    // dark/light sections pass behind it, but its horizontal steps (logo, nav
    // links, buttons) stay at the same columns.
    let translucent_fixed = |r: usize| {
        if strips.len() < 6 {
            return false;
        }
        let pairs = strips.len() - 1;
        let (mut bg_pairs, mut bg_steady, mut features, mut matched) = (0usize, 0usize, 0usize, 0usize);
        for s in strips.start + 1..strips.end {
            let (pa, pb) = (prev.at(r, s - 1), prev.at(r, s));
            let (ca, cb) = (cur.at(r, s - 1), cur.at(r, s));
            let dp = prev.mean[pb] as i16 - prev.mean[pa] as i16;
            let dc = cur.mean[cb] as i16 - cur.mean[ca] as i16;
            let dd = (dp - dc).abs();
            let detail = (!prev.flat[pb] && !cur.flat[cb]) || dp.abs() > FLAT_CELL as i16 || dc.abs() > FLAT_CELL as i16;
            if detail {
                features += 1;
                // Opaque text/buttons on a translucent bar keep their brightness
                // while the bar's background shifts, scaling edge steps slightly
                // without moving or flipping them.
                if dd <= (dp.abs().max(dc.abs()) / 3).max(6) {
                    matched += 1;
                }
            } else {
                bg_pairs += 1;
                if dd <= 6 {
                    bg_steady += 1;
                }
            }
        }
        // Two vertical card borders produce at most 4 edge transitions; a real
        // bar with a title, icon or links has more regardless of viewport width.
        features >= 6.min(pairs) && bg_steady * 20 >= bg_pairs * 17 && matched * 4 >= features * 3
    };

    // Uniform padding rows inside a translucent bar: allowed only when followed
    // by a confirmed non-uniform stationary row.
    let smooth_tint = |r: usize| {
        let mild = strips
            .clone()
            .filter(|&s| prev.mean[prev.at(r, s)].abs_diff(cur.mean[cur.at(r, s)]) <= 64)
            .count();
        if mild * 10 < strips.len() * 9 {
            return false;
        }
        if strips.len() < 2 {
            return true;
        }
        let pairs = strips.len() - 1;
        let smooth = (strips.start + 1..strips.end)
            .filter(|&s| {
                let dp = prev.mean[prev.at(r, s)] as i16 - prev.mean[prev.at(r, s - 1)] as i16;
                let dc = cur.mean[cur.at(r, s)] as i16 - cur.mean[cur.at(r, s - 1)] as i16;
                (dp - dc).abs() <= 4
            })
            .count();
        smooth * 10 >= pairs * 9
    };

    let scan = |iter: &mut dyn Iterator<Item = usize>| {
        let (mut seen, mut confirmed, mut bridged) = (0usize, 0usize, false);
        for r in iter.take(n / 3) {
            seen += 1;
            if translucent_fixed(r) {
                confirmed = seen;
            } else if unchanged(r) {
                if !bridged || !cur.uniform(r, strips) {
                    confirmed = seen;
                }
            } else if smooth_tint(r) {
                bridged = true;
            } else {
                break;
            }
        }
        confirmed
    };

    let mut top = scan(&mut (rows.start..rows.end));
    let mut bottom = scan(&mut (rows.start..rows.end).rev());

    // Once `shift` is known, include any trailing padding/border rows of a
    // detected translucent bar that also didn't move with `shift`.
    if let Some(dy) = shift.filter(|&d| d > 0) {
        let limit = n / 3;
        if top > 0 {
            while top < limit && rows.start + top + dy < rows.end {
                let r = rows.start + top;
                if cur.row_distance(r, prev, r + dy, strips) <= ROW_TOL {
                    break;
                }
                top += 1;
            }
        }
        if bottom > 0 {
            while bottom < limit && rows.start + dy + bottom < rows.end {
                let r = rows.end - 1 - bottom;
                if cur.row_distance(r - dy, prev, r, strips) <= ROW_TOL {
                    break;
                }
                bottom += 1;
            }
        }
    }

    // Leave a scrolling middle; if the bands would eat it, assume no sticky parts.
    if top + bottom + MIN_MIDDLE > n { (0, 0) } else { (top, bottom) }
}

/// The vertical shift that lines `cur` up with `prev` within `strips` and
/// `rows`, in fractional pixels: positive when the content moved up (the user
/// scrolled down). Bands of strips search independently and vote, weighted by
/// their evidence.
fn estimate_shift(prev: &Frame, cur: &Frame, strips: &Range<usize>, rows: &Range<usize>) -> Option<f32> {
    let (g_top, g_bottom) = sticky_bands(prev, cur, strips, rows, None);
    let rows = &(rows.start + g_top..rows.end - g_bottom);
    let bands = (strips.len() / MIN_BAND_STRIPS).clamp(1, MAX_BANDS);
    let band = |b: usize| strips.start + strips.len() * b / bands..strips.start + strips.len() * (b + 1) / bands;
    let search = |band: Range<usize>| {
        // Rows that don't move in this band (a header over just this part).
        let (top, bottom) = sticky_bands(prev, cur, &band, rows, None);
        let middle = rows.start + top..rows.end - bottom;
        let uniform = |f: &Frame| (0..f.height).map(|r| middle.contains(&r) && f.uniform(r, &band)).collect::<Vec<_>>();
        let (pu, cu) = (uniform(prev), uniform(cur));
        find_shift(prev, cur, &Band { strips: band.clone(), prev_uniform: &pu, cur_uniform: &cu }, &middle)
    };
    // Bands are independent: search them in parallel.
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (1..bands).map(|b| scope.spawn(move || search(band(b)))).collect();
        let mut results = vec![search(band(0))];
        results.extend(handles.into_iter().map(|h| h.join().expect("band search panicked")));
        results
    });
    // (integer shift, total evidence, evidence-weighted sum of fractional shifts)
    let mut votes: Vec<(i32, usize, f32)> = Vec::new();
    for (shift, evidence, fine) in results.into_iter().flatten() {
        match votes.iter_mut().find(|(s, _, _)| *s == shift) {
            Some((_, total, sum)) => (*total, *sum) = (*total + evidence, *sum + fine * evidence as f32),
            None => votes.push((shift, evidence, fine * evidence as f32)),
        }
    }
    votes
        .into_iter()
        .max_by_key(|&(shift, evidence, _)| (evidence, std::cmp::Reverse(shift.abs())))
        .map(|(_, evidence, sum)| sum / evidence as f32)
}

/// A band of strips with its rows' uniformity precomputed for both frames.
struct Band<'a> {
    strips: Range<usize>,
    prev_uniform: &'a [bool],
    cur_uniform: &'a [bool],
}

/// Average difference of rows of `b` against rows of `a` shifted by `dy`, over
/// distinct rows, or `None` without enough evidence or with too many rows that
/// don't match. Stops early once that is certain.
fn shift_cost(a: &Frame, au: &[bool], b: &Frame, bu: &[bool], band: &Range<usize>, rows: &Range<usize>, dy: usize) -> Option<(f32, usize)> {
    let n = rows.len();
    if dy + MIN_OVERLAP >= n {
        return None;
    }
    let (mut distinct, mut bad, mut total) = (0usize, 0usize, 0f32);
    let budget = ((n - dy) as f32 * MAX_BAD_ROWS) as usize;
    let end = rows.end - dy;
    // Sample evenly across the overlap first so an un-trimmed banner or hover
    // effect at the top cannot trip `EARLY_ROWS` before the body is checked.
    let stride = 16.min((end - rows.start).max(1));
    for rem in 0..stride {
        for i in (rows.start + rem..end).step_by(stride) {
            // Row `i` of `b` should look like row `i + dy` of `a`.
            if bu[i] && au[i + dy] {
                continue;
            }
            distinct += 1;
            let d = b.row_distance(i, a, i + dy, band);
            total += d.min(ROW_TOL * 2.0);
            if d > ROW_TOL {
                bad += 1;
                if bad > budget {
                    return None;
                }
            }
            if distinct >= EARLY_ROWS && total > distinct as f32 * AVG_TOL * HOPELESS {
                return None;
            }
        }
    }
    if distinct < MIN_MATCHING_ROWS || bad as f32 > distinct as f32 * MAX_BAD_ROWS {
        return None;
    }
    Some((total / distinct as f32, distinct))
}

/// Best shift of `cur` against `prev` within one band: the integer shift, the
/// number of distinct rows supporting it, and a sub-pixel refinement.
fn find_shift(prev: &Frame, cur: &Frame, band: &Band, rows: &Range<usize>) -> Option<(i32, usize, f32)> {
    let mut best: Option<(f32, i32, usize)> = None;
    let (pu, cu) = (band.prev_uniform, band.cur_uniform);
    for dy in 1..rows.len().saturating_sub(MIN_OVERLAP) {
        for (sign, a, au, b, bu) in [(1, prev, pu, cur, cu), (-1, cur, cu, prev, pu)] {
            let Some((avg, distinct)) = shift_cost(a, au, b, bu, &band.strips, rows, dy) else { continue };
            if avg > AVG_TOL {
                continue;
            }
            // Weight by `sqrt(distinct)` so a tiny periodic overlap (e.g. a CSS
            // checkerboard near the edge) doesn't beat a full-page sub-pixel match.
            let score = (avg + 1.0) / (distinct as f32).sqrt();
            if best.is_none_or(|(best_score, _, _)| score < best_score) {
                best = Some((score, sign * dy as i32, distinct));
            }
        }
    }
    let (_, shift, distinct) = best?;
    // Sub-pixel refinement from the plain (smoothed) cost around the match: pick
    // the closest integer shift, then fit a parabola through its neighbors.
    let cost = |d: i32| refine_cost(prev, cur, band, rows, d);
    let center = (shift - 1..=shift + 1).filter_map(|d| cost(d).map(|c| (c, d))).min_by(|a, b| a.0.total_cmp(&b.0))?.1;
    let fine = match (cost(center - 1), cost(center), cost(center + 1)) {
        (Some(lo), Some(mid), Some(hi)) => {
            let curvature = lo - 2.0 * mid + hi;
            if curvature > 1e-3 { (0.5 * (lo - hi) / curvature).clamp(-0.5, 0.5) } else { 0.0 }
        }
        _ => 0.0,
    };
    Some((shift, distinct, center as f32 + fine))
}

/// Average smoothed difference between `cur` and `prev` shifted by `dy`
/// (positive: content moved up) over the rows that aren't uniform in both.
fn refine_cost(prev: &Frame, cur: &Frame, band: &Band, rows: &Range<usize>, dy: i32) -> Option<f32> {
    let (a, au, b, bu, d) = if dy >= 0 {
        (prev, band.prev_uniform, cur, band.cur_uniform, dy as usize)
    } else {
        (cur, band.cur_uniform, prev, band.prev_uniform, (-dy) as usize)
    };
    if d + MIN_OVERLAP >= rows.len() {
        return None;
    }
    let (mut distinct, mut total) = (0usize, 0f32);
    for i in rows.start..rows.end - d {
        if bu[i] && au[i + d] {
            continue;
        }
        distinct += 1;
        total += b.smooth_distance(i, a, i + d, &band.strips);
    }
    (distinct >= MIN_MATCHING_ROWS).then(|| total / distinct as f32)
}

/// Drops strips at either edge of `keep` that don't follow `shift` (scrollbar
/// thumbs move differently from the content). Judged against the typical strip,
/// because rows that don't move at all (sticky bars) affect every strip equally.
/// Never trims below half.
fn trim_noisy_edges(
    prev: &Frame,
    cur: &Frame,
    mut keep: Range<usize>,
    rows: &Range<usize>,
    shift: usize,
    min_len: usize,
) -> Range<usize> {
    let mismatch = |s: usize| -> Option<f32> {
        let (mut evidence, mut wrong) = (0, 0);
        for r in rows.start..rows.end.saturating_sub(shift) {
            let (a, b) = (prev.at(r + shift, s), cur.at(r, s));
            if prev.flat[a] && cur.flat[b] && prev.mean[a].abs_diff(cur.mean[b]) <= SAME_CELL {
                continue;
            }
            evidence += 1;
            wrong += (prev.smooth[a].abs_diff(cur.smooth[b]) > NOISY_CELL) as usize;
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

    /// A text-like page at twice the resolution: lines of dark "words".
    fn page_2x(w: usize, h: usize) -> Vec<u8> {
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
                    for xx in x..x + len {
                        img[(yy * w + xx) * 4..(yy * w + xx) * 4 + 3].copy_from_slice(&[shade, shade, shade]);
                    }
                }
                x += len + 14;
            }
            y += 48 + 24 * rnd(3) as usize;
        }
        img
    }

    /// A `height`-row frame of `page` (2x) starting at half-pixel offset `y2`.
    fn downsampled(page: &[u8], w2: usize, y2: usize, height: usize) -> Vec<u8> {
        let w = w2 / 2;
        let mut out = vec![255u8; w * height * 4];
        for y in 0..height {
            for x in 0..w {
                for c in 0..3 {
                    let at = |yy: usize, xx: usize| page[(yy * w2 + xx) * 4 + c] as u32;
                    let (r0, r1, c0) = (y2 + 2 * y, y2 + 2 * y + 1, 2 * x);
                    out[(y * w + x) * 4 + c] = ((at(r0, c0) + at(r0, c0 + 1) + at(r1, c0) + at(r1, c0 + 1) + 2) / 4) as u8;
                }
            }
        }
        out
    }


    #[test]
    fn sub_pixel_scrolling_keeps_track_without_drift() {
        // Content redrawn at half-pixel offsets, as with fractional display scaling.
        let (w2, h2, height) = (192, 3200, 240);
        let page = page_2x(w2, h2);
        let mut s = Stitcher::new((w2 / 2) as u32, 100_000);
        let mut y2 = 0;
        for step in [0, 7, 3, 15, 9, 11, 21, 5, 13, 17, 25, 1, 19, 23, 7, 31, 9, 15, 27, 11, 13, 29, 5, 17] {
            y2 += step;
            let p = s.push(height as u32, downsampled(&page, w2, y2, height));
            assert_ne!(p, Progress::LostTrack, "lost track at half-pixel offset {y2}");
        }
        // The page shown so far is (y2 / 2 + height) rows; allow rounding at the end.
        let expected = (y2 as f32 / 2.0 + height as f32).round() as i32;
        let got = s.finish().unwrap().height() as i32;
        assert!((got - expected).abs() <= 2, "height {got}, expected about {expected}");
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

    #[test]
    fn translucent_sticky_header_over_dark_hero_and_light_body_keeps_track() {
        // Reproduces sticky frosted-glass navbars (like `position: sticky; top: 0;
        // background: rgba(11, 17, 32, 0.9); backdrop-filter: blur(...)`) whose
        // background tint shifts as a dark hero section slides underneath and then
        // gives way to sparse white cards.
        let (w, height, nav_h, doc_h) = (192usize, 320usize, 56usize, 2000usize);
        let mut page = vec![248u8; w * doc_h * 4];
        for p in page.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
        // Dark hero at rows 56..360, then sparse cards on a light page.
        for y in nav_h..360 {
            for x in 0..w {
                let shade = if (16..w - 16).contains(&x) && y % 28 < 12 && (x * 7 + y) % 5 != 0 { 220 } else { 14 };
                page[(y * w + x) * 4..(y * w + x) * 4 + 3].fill(shade);
            }
        }
        let mut seed = 0x9E37_79B9u32;
        for y in (380..doc_h).step_by(44) {
            for dy in 0..14 {
                for x in 20..w - 20 {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    if (seed >> (dy % 5)) & 3 == 0 {
                        page[((y + dy) * w + x) * 4..((y + dy) * w + x) * 4 + 3].fill((seed & 63) as u8);
                    }
                }
            }
        }
        let frame_at = |scroll_y: usize| {
            let mut f = rows_w(&page, w, scroll_y, scroll_y + height).to_vec();
            // Composite a 90%-opaque dark navbar with stationary logo/link blocks
            // over a horizontally blurred background tint.
            for r in 0..nav_h {
                let bg = if scroll_y == 0 || scroll_y + r >= 360 { 248u32 } else { 14u32 };
                for x in 0..w {
                    let on_item = (12..38).contains(&r)
                        && ((16..44).contains(&x) || (56..88).contains(&x) || (112..140).contains(&x) || (148..176).contains(&x));
                    let c = if r + 1 == nav_h {
                        38
                    } else if on_item {
                        235
                    } else {
                        ((14 * 9 + bg) / 10) as u8
                    };
                    f[(r * w + x) * 4..(r * w + x) * 4 + 3].fill(c);
                }
            }
            f
        };

        let mut s = Stitcher::new(w as u32, 100_000);
        for (i, y) in (0..=960).step_by(60).enumerate() {
            let p = s.push(height as u32, frame_at(y));
            if i == 0 {
                assert_eq!(p, Progress::Started);
            } else {
                assert!(matches!(p, Progress::Appended(_)), "lost track at scroll y={y}: {p:?}");
            }
        }
        let out = s.finish().unwrap();
        assert_eq!(out.height(), (960 + height) as u32);
    }

    #[test]
    fn periodic_checkerboard_does_not_hijack_subpixel_scroll() {
        // 2x-rendered page with a large 16px checkerboard stage and a small
        // formula box, scrolled at half-pixel offsets.
        let (w2, h2, height) = (256usize, 2400usize, 260usize);
        let mut page = vec![255u8; w2 * h2 * 4];
        for y in 0..h2 {
            for x in 0..w2 {
                // 8px checkerboard at 1x -> 16px squares at 2x, contrast 255 vs 243.
                let shade = if ((x / 16) + (y / 16)) % 2 == 0 { 255 } else { 243 };
                page[(y * w2 + x) * 4..(y * w2 + x) * 4 + 3].fill(shade);
            }
        }
        // Sparse distinct content bands (headers / rendered formulas) every ~260 rows.
        for base in (80..h2 - 80).step_by(260) {
            for dy in 0..36 {
                for x in 32..w2 - 32 {
                    if ((x * 3 + base) / 17 + dy / 7) % 4 == 0 {
                        page[((base + dy) * w2 + x) * 4..((base + dy) * w2 + x) * 4 + 3].fill(30);
                    }
                }
            }
        }
        let mut s = Stitcher::new((w2 / 2) as u32, 100_000);
        let mut y2 = 0usize;
        for step in [0, 85, 91, 79, 95, 83, 89, 77, 93] {
            y2 += step;
            let p = s.push(height as u32, downsampled(&page, w2, y2, height));
            assert_ne!(p, Progress::LostTrack, "lost track at y2={y2}");
        }
        let expected = (y2 as f32 / 2.0 + height as f32).round() as i32;
        let got = s.finish().unwrap().height() as i32;
        assert!((got - expected).abs() <= 2, "height {got}, expected about {expected}");
    }
}
