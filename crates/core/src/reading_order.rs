//! Turns OCR line boxes into plain text in reading order.

use crate::geom::Rect;

/// Indices of `boxes` in reading order: grouped into rows (by vertical
/// overlap of their centers), rows top to bottom, boxes left to right.
pub fn rows(boxes: &[Rect]) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..boxes.len()).collect();
    order.sort_by(|&a, &b| boxes[a].center().y.total_cmp(&boxes[b].center().y));
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for i in order {
        let same_row = rows.last().is_some_and(|row| {
            let r = boxes[row[row.len() - 1]];
            let tolerance = r.h.min(boxes[i].h) / 2.0;
            (r.center().y - boxes[i].center().y).abs() <= tolerance
        });
        match rows.last_mut() {
            Some(row) if same_row => row.push(i),
            _ => rows.push(vec![i]),
        }
    }
    for row in &mut rows {
        row.sort_by(|&a, &b| boxes[a].x.total_cmp(&boxes[b].x));
    }
    rows
}

/// The lines' text in reading order (see [`rows`]), joined with spaces within
/// a row and newlines between rows.
pub fn join_lines(lines: Vec<(Rect, String)>) -> String {
    let boxes: Vec<Rect> = lines.iter().map(|(r, _)| *r).collect();
    rows(&boxes)
        .into_iter()
        .map(|row| row.into_iter().map(|i| lines[i].1.trim()).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Horizontal bands to recognize a tall image in, `size` rows each,
/// overlapping by `overlap` rows so no text line is cut in every band that
/// holds it. Each band is (rows to read, rows it owns): a line belongs to the
/// band owning its center, so lines in an overlap are kept once.
pub fn bands(height: u32, size: u32, overlap: u32) -> Vec<(std::ops::Range<u32>, std::ops::Range<u32>)> {
    let step = size.saturating_sub(overlap).max(1);
    if height <= size {
        return vec![(0..height, 0..height)];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < height {
        let read = start..(start + size).min(height);
        let own_start = if start == 0 { 0 } else { start + overlap / 2 };
        let own_end = if read.end == height { height } else { read.end - overlap / 2 };
        out.push((read.clone(), own_start..own_end));
        if read.end == height {
            break;
        }
        start += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_cover_tall_images_with_owned_rows_once() {
        assert_eq!(bands(900, 1200, 64), vec![(0..900, 0..900)]);
        let b = bands(3000, 1200, 64);
        // Reads overlap; owned rows tile the image exactly.
        assert_eq!(b[0], (0..1200, 0..1168));
        assert_eq!(b[1].0, 1136..2336);
        assert!(b.windows(2).all(|w| w[0].1.end == w[1].1.start));
        assert_eq!((b[0].1.start, b.last().unwrap().1.end, b.last().unwrap().0.end), (0, 3000, 3000));
    }

    fn line(x: f32, y: f32, text: &str) -> (Rect, String) {
        (Rect::new(x, y, 100.0, 20.0), text.to_string())
    }

    #[test]
    fn orders_rows_then_columns() {
        let lines = vec![
            line(300.0, 52.0, "right2"),
            line(10.0, 10.0, "title"),
            line(10.0, 50.0, "left2"),
            line(300.0, 12.0, "(note)"),
        ];
        assert_eq!(join_lines(lines), "title (note)\nleft2 right2");
    }

    #[test]
    fn empty_input_is_empty_text() {
        assert_eq!(join_lines(Vec::new()), "");
    }
}
