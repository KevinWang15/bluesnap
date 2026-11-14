//! Scrolling screenshots: stitches frames of a scrolling region into one tall image.
//!
//! Each frame is compared with the previous accepted one by row hashes. Rows that
//! stay put at the top and bottom (sticky headers and footers) are detected once
//! and kept out of the matching; the rest is searched for the vertical shift that
//! lines the two frames up, and only the newly revealed rows are appended.

use tiny_skia::Pixmap;

use crate::pixels::pixmap_from_rgba;

/// Rows of overlap needed to trust a match.
const MIN_OVERLAP: usize = 24;
/// Distinct (non-uniform) rows that must match for a shift to be accepted.
const MIN_MATCHING_ROWS: usize = 8;
/// Share of distinct overlapping rows allowed to differ (animations, carets).
const MAX_MISMATCH: f32 = 0.05;
/// The scrolling middle must keep at least this many rows after removing
/// sticky header and footer.
const MIN_MIDDLE: usize = 40;

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
    hashes: Vec<u64>,
    /// Rows of a single color: they match anywhere, so they don't count as evidence.
    flat: Vec<bool>,
}

impl Frame {
    fn new(width: usize, rgba: Vec<u8>) -> Self {
        let stride = width * 4;
        let rows = rgba.chunks_exact(stride);
        let hashes = rows.clone().map(hash_row).collect();
        let flat = rows.map(|r| r.as_chunks::<4>().0.iter().all(|px| px[..] == r[..4])).collect();
        Self { rgba, hashes, flat }
    }

    fn height(&self) -> usize {
        self.hashes.len()
    }
}

fn hash_row(row: &[u8]) -> u64 {
    // Fast multiplicative hash over 8-byte words; collisions only cost a
    // spurious match, which the mismatch threshold tolerates.
    let mut h = 0x9E37_79B9_7F4A_7C15u64;
    let (words, rest) = row.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(0x0000_0100_0000_01B3).rotate_left(29);
    }
    for b in rest {
        h = (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

pub struct Stitcher {
    width: usize,
    max_height: usize,
    /// Rows composed so far: the first frame without its footer, then appended rows.
    body: Vec<u8>,
    /// The frame `body` currently ends with.
    last: Option<Frame>,
    /// Sticky header/footer heights, fixed at the first detected scroll.
    sticky: Option<(usize, usize)>,
    full: bool,
}

impl Stitcher {
    pub fn new(width: u32, max_height: u32) -> Self {
        Self { width: width as usize, max_height: max_height as usize, body: Vec::new(), last: None, sticky: None, full: false }
    }

    pub fn width(&self) -> u32 {
        self.width as u32
    }

    /// Height of the image [`Self::finish`] would produce.
    pub fn height(&self) -> u32 {
        let footer = self.sticky.map_or(0, |(_, bottom)| bottom);
        (self.body.len() / (self.width * 4) + footer) as u32
    }

    /// Adds a frame of opaque RGBA pixels. Every frame must have the same size.
    pub fn push(&mut self, height: u32, rgba: Vec<u8>) -> Progress {
        let stride = self.width * 4;
        debug_assert_eq!(rgba.len(), stride * height as usize);
        let cur = Frame::new(self.width, rgba);
        let Some(prev) = &self.last else {
            self.body = cur.rgba.clone();
            self.last = Some(cur);
            return Progress::Started;
        };
        if self.full {
            return Progress::Full;
        }
        if prev.height() != cur.height() || prev.hashes == cur.hashes {
            return Progress::Unchanged;
        }

        let (top, bottom) = self.sticky.unwrap_or_else(|| sticky_bands(prev, &cur));
        let n = cur.height();
        if n < top + bottom + MIN_MIDDLE {
            return Progress::LostTrack;
        }
        let shift = match find_shift(prev, &cur, top, n - bottom) {
            Some(shift) if shift > 0 => shift as usize,
            Some(_) => return Progress::ScrolledUp,
            None if self.sticky.is_none() => return Progress::LostTrack,
            // The sticky bands from the first scroll may not hold any more
            // (e.g. a header that hides on scroll); retry without them.
            None => match find_shift(prev, &cur, 0, n) {
                Some(shift) if shift > 0 => shift as usize,
                Some(_) => return Progress::ScrolledUp,
                None => return Progress::LostTrack,
            },
        };

        if self.sticky.is_none() {
            // The first frame's footer belongs at the very end, not in the middle.
            self.body.truncate((n - bottom) * stride);
            self.sticky = Some((top, bottom));
        }
        let (_, bottom) = self.sticky.expect("just set");
        let end = n - bottom;
        let mut start = end.saturating_sub(shift).max(top);
        let room = self.max_height.saturating_sub(self.body.len() / stride + bottom);
        if end - start > room {
            start = end - room;
            self.full = true;
        }
        self.body.extend_from_slice(&cur.rgba[start * stride..end * stride]);
        self.last = Some(cur);
        if self.full { Progress::Full } else { Progress::Appended((end - start) as u32) }
    }

    /// The stitched image so far: body plus the latest frame's footer.
    pub fn finish(&self) -> Option<Pixmap> {
        let last = self.last.as_ref()?;
        let stride = self.width * 4;
        let mut rgba = self.body.clone();
        if let Some((_, bottom)) = self.sticky {
            let n = last.height();
            rgba.extend_from_slice(&last.rgba[(n - bottom) * stride..]);
        }
        let height = (rgba.len() / stride) as u32;
        pixmap_from_rgba(self.width as u32, height, rgba)
    }
}

/// Rows identical at the same position in both frames, from the top and the bottom.
fn sticky_bands(prev: &Frame, cur: &Frame) -> (usize, usize) {
    let n = cur.height();
    let same = |i: usize| prev.hashes[i] == cur.hashes[i];
    let top = (0..n).take_while(|&i| same(i)).count();
    let bottom = (0..n).rev().take_while(|&i| same(i)).count();
    // Leave a scrolling middle; if the bands would eat it, assume no sticky parts.
    if top + bottom + MIN_MIDDLE > n { (0, 0) } else { (top, bottom) }
}

/// Vertical shift of `cur` against `prev` within rows `lo..hi`: positive when the
/// content moved up (the user scrolled down). `None` when nothing lines up.
fn find_shift(prev: &Frame, cur: &Frame, lo: usize, hi: usize) -> Option<i32> {
    let n = hi - lo;
    let mut best: Option<(usize, i32)> = None;
    for dy in 1..n.saturating_sub(MIN_OVERLAP) {
        for (sign, (a, b)) in [(1, (prev, cur)), (-1, (cur, prev))] {
            // Row `i` of `b` should equal row `i + dy` of `a`.
            let (mut matched, mut distinct) = (0, 0);
            let budget = ((n - dy) as f32 * MAX_MISMATCH) as usize;
            let mut mismatched = 0;
            for i in lo..hi - dy {
                if b.flat[i] && a.flat[i + dy] {
                    continue;
                }
                distinct += 1;
                if a.hashes[i + dy] == b.hashes[i] {
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
    best.map(|(_, dy)| dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 64;

    /// A tall "document" with distinct rows, some blank runs, and repeated runs.
    fn document(height: usize) -> Vec<u8> {
        let mut seed = 0x2545_F491u32;
        let mut out = Vec::with_capacity(W * height * 4);
        for y in 0..height {
            if y % 50 < 6 {
                // Blank spacing rows, like margins between paragraphs.
                out.extend(std::iter::repeat_n([250u8, 250, 250, 255], W).flatten());
                continue;
            }
            for _ in 0..W {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let [r, g, b, _] = seed.to_le_bytes();
                out.extend_from_slice(&[r, g, b, 255]);
            }
        }
        out
    }

    fn rows(img: &[u8], from: usize, to: usize) -> &[u8] {
        &img[from * W * 4..to * W * 4]
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
        let header: Vec<u8> = std::iter::repeat_n([20u8, 40, 200, 255], W * 30).flatten().collect::<Vec<_>>()
            .chunks(4).enumerate().flat_map(|(i, px)| [px[0], (i % 251) as u8, px[2], 255]).collect();
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
        assert_eq!(out.height() as usize, 30 + content + 20);
        assert!(out.data() == &expected[..], "stitched image differs from the document");
    }

    #[test]
    fn too_fast_scrolling_loses_track_and_recovers() {
        let doc = document(3000);
        let mut s = Stitcher::new(W as u32, 100_000);
        let v = |y| rows(&doc, y, y + 200).to_vec();
        assert_eq!(s.push(200, v(0)), Progress::Started);
        assert_eq!(s.push(200, v(500)), Progress::LostTrack);
        // Back within reach of the last accepted frame: continues seamlessly.
        assert_eq!(s.push(200, v(120)), Progress::Appended(120));
        let out = s.finish().unwrap();
        assert!(out.data() == rows(&doc, 0, 320));
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
}
