//! Character offsets shared by the canvas input handler and status messages.

use std::ops::Range;

/// `offset` within `text`, moved back to a character boundary.
pub(crate) fn clamp_offset(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// `r` with both ends clamped (see [`clamp_offset`]) and in order.
pub(crate) fn clamp_range(text: &str, r: &Range<usize>) -> Range<usize> {
    let (a, b) = (clamp_offset(text, r.start), clamp_offset(text, r.end));
    a.min(b)..a.max(b)
}

pub(crate) fn utf16_to_byte(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        if units >= offset {
            return i;
        }
        units += c.len_utf16();
    }
    text.len()
}

pub(crate) fn byte_to_utf16(text: &str, offset: usize) -> usize {
    text[..clamp_offset(text, offset)].chars().map(char::len_utf16).sum()
}

pub(crate) fn range_from_utf16(text: &str, r: &Range<usize>) -> Range<usize> {
    clamp_range(text, &(utf16_to_byte(text, r.start)..utf16_to_byte(text, r.end)))
}

pub(crate) fn range_to_utf16(text: &str, r: &Range<usize>) -> Range<usize> {
    let r = clamp_range(text, r);
    byte_to_utf16(text, r.start)..byte_to_utf16(text, r.end)
}

/// `s` shortened to at most `max` characters, with `…` when truncated.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_from_stale_layouts_stay_inside_characters() {
        let text = "a密b";
        // Past the end, and inside the 3-byte 密.
        assert_eq!(clamp_offset(text, 99), 5);
        assert_eq!(clamp_offset(text, 2), 1);
        assert_eq!(clamp_offset(text, 3), 1);
        assert_eq!(clamp_offset(text, 4), 4);
        // Reversed and out-of-range ranges come back usable.
        let (from, to) = (9, 2);
        assert_eq!(clamp_range(text, &(from..to)), 1..5);
        let r = clamp_range(text, &(3..99));
        assert_eq!(&text[r], "密b");
    }
}
