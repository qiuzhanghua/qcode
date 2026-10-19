//! Text helpers shared by the store and the CLI.
//!
//! All offsets are UTF-8 byte offsets, and no function splits a character.

/// Longest prefix of `text` that fits in `max_bytes` bytes and ends on a
/// character boundary.
pub fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Byte ranges of each line, including the trailing `\n` when present.
///
/// An empty input has no lines; a trailing newline does not produce an empty
/// final line.
pub fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;

    for (index, character) in text.char_indices() {
        if character == '\n' {
            ranges.push((start, index + 1));
            start = index + 1;
        }
    }
    if start < text.len() {
        ranges.push((start, text.len()));
    }

    ranges
}

/// Collapse runs of whitespace into one space and trim both ends.
pub fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
