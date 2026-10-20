//! Frontend-neutral composer: plain text, a cursor and the editing events.
//!
//! The TUI used to keep this in a `tui-textarea` widget, which tied the
//! projection to a terminal toolkit: a GUI would have to emulate crossterm
//! key events and inherit that widget's IME and cursor behaviour. This type is
//! what both frontends edit; [`crate::App`] consumes it and each frontend maps
//! its own input into [`InputEvent`].

/// One editing operation, independent of any toolkit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Insert(char),
    /// Insert a whole string (paste, clipboard, IME commit). Newlines split
    /// lines like [`InputEvent::Newline`] does.
    InsertStr(String),
    Newline,
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Home,
    End,
    /// Empty the composer, e.g. after a submit or a slash command.
    Clear,
}

impl InputEvent {
    /// Whether the event can change the text (used to decide on redraws).
    pub fn changes_text(&self) -> bool {
        matches!(
            self,
            Self::Insert(_)
                | Self::InsertStr(_)
                | Self::Newline
                | Self::Backspace
                | Self::Delete
                | Self::Clear
        )
    }
}

/// A multi-line text buffer with a cursor.
///
/// The cursor is `(row, col)` where `col` counts characters, not bytes, so
/// CJK and emoji behave. There is no selection: nothing in qcode needs one yet,
/// and a GUI can add it without changing this contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl Composer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    /// Build a composer from text, cursor at the end.
    pub fn with_text(text: &str) -> Self {
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        let row = lines.len() - 1;
        let col = lines[row].chars().count();
        Self { lines, row, col }
    }

    /// The whole buffer, newline-joined.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when the buffer contains nothing but whitespace.
    pub fn is_blank(&self) -> bool {
        self.text().trim().is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// One-based `(row, column)` for status bars.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row + 1, self.col + 1)
    }

    /// Apply one event. Returns `true` when the text changed.
    pub fn apply(&mut self, event: InputEvent) -> bool {
        let changed = event.changes_text();
        match event {
            InputEvent::Insert(ch) => self.insert_str(&ch.to_string()),
            InputEvent::InsertStr(text) => self.insert_str(&text),
            InputEvent::Newline => self.split_line(),
            InputEvent::Backspace => self.backspace(),
            InputEvent::Delete => self.delete(),
            InputEvent::MoveLeft => self.move_left(),
            InputEvent::MoveRight => self.move_right(),
            InputEvent::MoveUp => self.move_vertical(-1),
            InputEvent::MoveDown => self.move_vertical(1),
            InputEvent::Home => self.col = 0,
            InputEvent::End => self.col = self.current_len(),
            InputEvent::Clear => {
                self.lines = vec![String::new()];
                self.row = 0;
                self.col = 0;
            }
        }
        changed
    }

    fn current_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    /// Byte offset of character `index` in the current line.
    fn byte_index(line: &str, index: usize) -> usize {
        line.char_indices()
            .nth(index)
            .map(|(offset, _)| offset)
            .unwrap_or(line.len())
    }

    fn insert_str(&mut self, text: &str) {
        let mut parts = text.split('\n');
        let head = parts.next().unwrap_or_default();
        if !head.is_empty() {
            let line = &mut self.lines[self.row];
            let at = Self::byte_index(line, self.col);
            line.insert_str(at, head);
            self.col += head.chars().count();
        }
        for part in parts {
            self.split_line();
            if !part.is_empty() {
                self.lines[self.row].push_str(part);
                self.col = part.chars().count();
            }
        }
    }

    fn split_line(&mut self) {
        let line = &mut self.lines[self.row];
        let at = Self::byte_index(line, self.col);
        let tail = line.split_off(at);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let end = Self::byte_index(line, self.col);
            let start = Self::byte_index(line, self.col - 1);
            line.replace_range(start..end, "");
            self.col -= 1;
        } else if self.row > 0 {
            // Join with the previous line, cursor at the seam.
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.current_len();
            self.lines[self.row].push_str(&current);
        }
    }

    fn delete(&mut self) {
        if self.col < self.current_len() {
            let line = &mut self.lines[self.row];
            let start = Self::byte_index(line, self.col);
            let end = Self::byte_index(line, self.col + 1);
            line.replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.current_len();
        }
    }

    fn move_right(&mut self) {
        if self.col < self.current_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn move_vertical(&mut self, delta: isize) {
        let target = self.row as isize + delta;
        if target < 0 || target as usize >= self.lines.len() {
            return;
        }
        self.row = target as usize;
        self.col = self.col.min(self.current_len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> Composer {
        let mut composer = Composer::new();
        for ch in text.chars() {
            composer.apply(InputEvent::Insert(ch));
        }
        composer
    }

    #[test]
    fn typing_appends_at_the_cursor() {
        let mut composer = typed("hello");
        composer.apply(InputEvent::MoveLeft);
        composer.apply(InputEvent::Insert('X'));
        assert_eq!(composer.text(), "hellXo");
        assert_eq!(composer.cursor(), (1, 6));
    }

    #[test]
    fn newline_splits_the_line_and_backspace_joins_it_back() {
        let mut composer = typed("abcd");
        composer.apply(InputEvent::MoveLeft);
        composer.apply(InputEvent::MoveLeft);
        composer.apply(InputEvent::Newline);
        assert_eq!(composer.text(), "ab\ncd");
        assert_eq!(composer.line_count(), 2);
        assert_eq!(composer.cursor(), (2, 1));

        composer.apply(InputEvent::Backspace);
        assert_eq!(composer.text(), "abcd");
        assert_eq!(composer.cursor(), (1, 3));
    }

    #[test]
    fn delete_at_the_end_of_a_line_joins_the_next_one() {
        let mut composer = Composer::with_text("one\ntwo");
        composer.apply(InputEvent::Home);
        composer.apply(InputEvent::MoveUp);
        composer.apply(InputEvent::End);
        composer.apply(InputEvent::Delete);
        assert_eq!(composer.text(), "onetwo");
    }

    #[test]
    fn vertical_movement_clamps_the_column() {
        let mut composer = Composer::with_text("long line\nx");
        // Start at the end of the short second line, then move up: the column
        // cannot stay past its end, so it clamps to 1.
        composer.apply(InputEvent::MoveUp);
        assert_eq!(composer.cursor(), (1, 2), "clamped to the short line");
        composer.apply(InputEvent::End);
        assert_eq!(composer.cursor(), (1, 10));
        composer.apply(InputEvent::MoveDown);
        assert_eq!(composer.cursor(), (2, 2), "clamped again on the way down");
    }

    #[test]
    fn cjk_counts_characters_not_bytes() {
        let mut composer = typed("中文");
        composer.apply(InputEvent::MoveLeft);
        composer.apply(InputEvent::Insert('a'));
        assert_eq!(composer.text(), "中a文");
        composer.apply(InputEvent::Backspace);
        assert_eq!(composer.text(), "中文");
    }

    #[test]
    fn pasting_multiple_lines_splits_them() {
        let mut composer = typed("ab");
        composer.apply(InputEvent::InsertStr("cd\nef\ngh".to_string()));
        assert_eq!(composer.text(), "abcd\nef\ngh");
        assert_eq!(composer.cursor(), (3, 3));
    }

    #[test]
    fn clear_and_blank() {
        let mut composer = typed("x\ny");
        assert!(!composer.is_blank());
        composer.apply(InputEvent::Clear);
        assert!(composer.is_empty());
        assert!(composer.is_blank());
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn movement_events_do_not_report_a_text_change() {
        assert!(!InputEvent::MoveLeft.changes_text());
        assert!(!InputEvent::Home.changes_text());
        assert!(InputEvent::Insert('a').changes_text());
        assert!(InputEvent::Clear.changes_text());
    }
}
