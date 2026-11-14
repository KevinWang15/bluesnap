//! Turns OCR line boxes into plain text in reading order.

use crate::geom::Rect;

/// Groups boxes into rows (by vertical overlap of their centers), orders rows
/// top to bottom and boxes left to right, and joins them with spaces and newlines.
pub fn join_lines(mut lines: Vec<(Rect, String)>) -> String {
    lines.sort_by(|a, b| a.0.center().y.total_cmp(&b.0.center().y));
    let mut rows: Vec<Vec<(Rect, String)>> = Vec::new();
    for line in lines {
        let same_row = rows.last().is_some_and(|row| {
            let (r, _) = &row[row.len() - 1];
            let tolerance = r.h.min(line.0.h) / 2.0;
            (r.center().y - line.0.center().y).abs() <= tolerance
        });
        match rows.last_mut() {
            Some(row) if same_row => row.push(line),
            _ => rows.push(vec![line]),
        }
    }
    rows.into_iter()
        .map(|mut row| {
            row.sort_by(|a, b| a.0.x.total_cmp(&b.0.x));
            row.into_iter().map(|(_, t)| t.trim().to_string()).collect::<Vec<_>>().join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

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
