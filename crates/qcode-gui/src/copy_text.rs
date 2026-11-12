//! What the copy buttons put on the clipboard.
//!
//! The transcript stores **markdown source** (`Block::Assistant(String)`), and
//! that is what the buttons have to work from: the rendered plain text only
//! exists inside a `TextView`'s layout, which the button is not allowed to
//! reach into. So a button that says「复制」must convert the source itself, and
//! this module is that conversion — deliberately a pure function pair, because
//! it is the only part of the feature a test can reach (the GUI tests have no
//! window; see `docs/design/DECISIONS.md`).
//!
//! The rule set is a heuristic, the same way [`markdown_segments`] is: strip
//! the marks that survive a paste badly, leave everything that is merely
//! unusual. Pasting into a chat window or a document is the target — copying
//! back into a markdown file is not (that is what「MD」is for).

use crate::render::{MarkdownSegment, markdown_segments};

/// Markdown source → plain text for the clipboard.
///
/// Fenced code keeps its body verbatim and loses the fence: the fence is
/// typography, the body is content. Prose loses headings, quotes, bullets,
/// emphasis, inline-code backticks and link syntax; tables and rules are left
/// as they are, since flattening them would destroy the only thing that makes
/// them readable.
pub fn plain_text_from_markdown(source: &str) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for segment in markdown_segments(source) {
        let block = match segment {
            MarkdownSegment::Code { body, .. } => body,
            MarkdownSegment::Text(text) => plain_text_prose(&text),
        };
        let block = block.trim_matches('\n').trim_end();
        if !block.trim().is_empty() {
            blocks.push(block.to_string());
        }
    }
    blocks.join("\n\n")
}

/// One turn as plain text: the question, then the answer.
///
/// The execution process (reasoning, tool calls) is left out on purpose: it is
/// what the user folded away, and it is not what they mean by "this turn" when
/// they paste it somewhere. Empty halves are dropped, so a turn whose answer
/// never arrived copies as just the question.
pub fn turn_copy_text(question: Option<&str>, answer: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for source in [question, answer].into_iter().flatten() {
        let part = plain_text_from_markdown(source);
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts.join("\n\n")
}

/// One prose run of a markdown document (no fenced code in it).
fn plain_text_prose(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blank = false;
    for line in text.lines() {
        let line = strip_line_markers(line);
        let line = line.trim_end();
        if line.trim().is_empty() {
            // Collapse runs of blank lines: the source's paragraph spacing is
            // not information, and a pasted answer full of double gaps reads
            // as broken.
            if !blank && !lines.is_empty() {
                lines.push(String::new());
            }
            blank = true;
            continue;
        }
        blank = false;
        lines.push(strip_inline_markdown(line));
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Block-level markers at the start of one line: headings, quotes, bullets.
///
/// Indentation of ordinary lines is preserved — an indented code block inside
/// prose has no fence to protect it, and its indent is the only thing that
/// makes it read as code.
fn strip_line_markers(line: &str) -> String {
    let mut rest = line;
    let mut quoted = false;
    // Block quotes nest: `> > x` is a quote in a quote, and both levels are
    // typography.
    while let Some(stripped) = rest.strip_prefix('>') {
        rest = stripped.strip_prefix(' ').unwrap_or(stripped);
        quoted = true;
    }
    let trimmed = if quoted { rest.trim_start() } else { rest };
    if let Some(skip) = heading_marker(trimmed) {
        return trimmed[skip..].to_string();
    }
    if let Some(item) = bullet_marker(trimmed) {
        return item;
    }
    trimmed.to_string()
}

/// `#` … `######` followed by a space; returns the length to skip.
fn heading_marker(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    line[hashes..].starts_with(' ').then_some(hashes + 1)
}

/// `- `, `* ` and `+ ` become a bullet character; `1. ` stays as it is, since
/// an ordered list is self-explanatory and renumbering it would be a lie.
fn bullet_marker(line: &str) -> Option<String> {
    let mut chars = line.chars();
    let first = chars.next()?;
    if !matches!(first, '-' | '*' | '+') || !matches!(chars.next(), Some(' ')) {
        return None;
    }
    Some(format!("• {}", &line[2..]))
}

/// Inline marks inside one line.
fn strip_inline_markdown(line: &str) -> String {
    let line = unwrap_links(line);
    let line = strip_inline_code(&line);
    let line = strip_paired(&line, "**");
    let line = strip_paired(&line, "__");
    strip_paired(&line, "~~")
}

/// `` `x` `` → `x`.
///
/// Only matched backticks are dropped: an unmatched one is more likely a
/// literal backtick than a typo, and deleting it would lose a character.
fn strip_inline_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) => {
                out.push_str(&after[..close]);
                rest = &after[close + 1..];
            }
            None => {
                // Unmatched: keep the backtick and stop looking.
                out.push('`');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `[text](url)` → `text (url)`, `![alt](url)` → `alt (url)`.
///
/// The URL is kept because it *is* the content of a link; dropping it would
/// turn a citation into a bare word. A link with no target keeps its brackets,
/// which is what the reader sees anyway.
fn unwrap_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find("](") else {
            out.push('[');
            rest = after_open;
            continue;
        };
        let target = &after_open[close + 2..];
        let Some(end) = target.find(')') else {
            out.push('[');
            rest = after_open;
            continue;
        };
        // `![alt](url)`: the image marker is the last character written so far.
        if out.ends_with('!') {
            out.pop();
        }
        let label = &after_open[..close];
        let url = target[..end].trim();
        out.push_str(label);
        if !url.is_empty() {
            out.push_str(" (");
            out.push_str(url);
            out.push(')');
        }
        rest = &target[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Drops every occurrence of a paired marker (`**`, `__`, `~~`), but only when
/// the pair is balanced on this line.
///
/// Unbalanced markers stay: a single `**` is a literal asterisk pair, and
/// deleting it would silently change the text. This is also why single `*` and
/// `_` are never touched — `2 * 3 * 4` and `snake_case` are far more common in
/// an answer than italics, and misreading them loses real characters.
fn strip_paired(text: &str, marker: &str) -> String {
    if marker.is_empty() || !text.matches(marker).count().is_multiple_of(2) {
        return text.to_string();
    }
    text.replace(marker, "")
}
