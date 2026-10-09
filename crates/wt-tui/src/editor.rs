use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

/// One Unicode-aware line editor shared by footer prompts and picker filters.
/// Cursor indices are characters, never byte offsets into a UTF-8 string.
#[derive(Clone, Debug, Default)]
pub struct LineEditor {
    chars: Vec<char>,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditResult {
    Changed,
    Unchanged,
    Submit,
    Cancel,
}

impl LineEditor {
    pub fn new(text: &str) -> Self {
        let chars: Vec<_> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn paste(&mut self, text: &str) {
        // Bracketed paste may contain newlines and escape sequences. A footer
        // stays one line and pasting can never submit a command.
        let chars: Vec<_> = text.chars().filter(|ch| !ch.is_control()).collect();
        let count = chars.len();
        self.chars.splice(self.cursor..self.cursor, chars);
        self.cursor += count;
    }

    fn word_left(&self) -> usize {
        let mut cursor = self.cursor;
        while cursor > 0 && separator(self.chars[cursor - 1]) {
            cursor -= 1;
        }
        while cursor > 0 && !separator(self.chars[cursor - 1]) {
            cursor -= 1;
        }
        cursor
    }

    fn word_right(&self) -> usize {
        let mut cursor = self.cursor;
        while cursor < self.chars.len() && !separator(self.chars[cursor]) {
            cursor += 1;
        }
        while cursor < self.chars.len() && separator(self.chars[cursor]) {
            cursor += 1;
        }
        cursor
    }

    pub fn input(&mut self, key: KeyEvent) -> EditResult {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let before = (self.cursor, self.chars.clone());
        match key.code {
            KeyCode::Enter => return EditResult::Submit,
            KeyCode::Esc => return EditResult::Cancel,
            KeyCode::Char('c') if ctrl => return EditResult::Cancel,
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.chars.len(),
            KeyCode::Left if ctrl || alt => self.cursor = self.word_left(),
            KeyCode::Right if ctrl || alt => self.cursor = self.word_right(),
            KeyCode::Char('b') if alt => self.cursor = self.word_left(),
            KeyCode::Char('f') if alt => self.cursor = self.word_right(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Char('u') if ctrl => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => {
                self.chars.truncate(self.cursor);
            }
            KeyCode::Backspace if self.chars.is_empty() => return EditResult::Cancel,
            KeyCode::Backspace if alt || ctrl => {
                let start = self.word_left();
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            KeyCode::Char(ch) if !ctrl && !alt && !ch.is_control() => {
                self.chars.insert(self.cursor, ch);
                self.cursor += 1;
            }
            _ => {}
        }
        if before == (self.cursor, self.chars.clone()) {
            EditResult::Unchanged
        } else {
            EditResult::Changed
        }
    }

    /// Visible text and cursor column; leaves one cell for a cursor at EOF.
    pub fn viewport(&self, width: usize) -> (String, u16) {
        if width == 0 {
            return (String::new(), 0);
        }
        let mut start = self.cursor;
        let mut column = 0;
        while start > 0 {
            let cells = self.chars[start - 1].width().unwrap_or(0);
            if column + cells >= width {
                break;
            }
            column += cells;
            start -= 1;
        }
        let mut cells = 0;
        let text = self.chars[start..]
            .iter()
            .take_while(|ch| {
                cells += ch.width().unwrap_or(0);
                cells <= width
            })
            .collect();
        (text, column as u16)
    }
}

fn separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '-' | '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn unicode_edits_and_word_navigation_preserve_character_boundaries() {
        let mut editor = LineEditor::new("one-two_界面");
        editor.input(key(KeyCode::Left, KeyModifiers::ALT));
        editor.input(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(editor.text(), "one-two_面");
        editor.input(key(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(editor.text(), "one-面");
        editor.input(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "面");
        editor.input(key(KeyCode::End, KeyModifiers::NONE));
        editor.input(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            editor.input(key(KeyCode::Backspace, KeyModifiers::NONE)),
            EditResult::Cancel
        );
    }

    #[test]
    fn long_wide_text_scrolls_cursor_into_view_and_paste_never_submits() {
        let mut editor = LineEditor::new("a界面b");
        assert_eq!(editor.viewport(4), ("面b".into(), 3));
        editor.paste("\nq\r\0");
        assert_eq!(editor.text(), "a界面bq");
        editor.input(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(editor.viewport(4), ("a界".into(), 0));
        assert_eq!(editor.viewport(0), (String::new(), 0));
    }
}
