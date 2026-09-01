//! Multi-line input editor with cursor movement, word navigation, kill ring,
//! undo, history, and a pluggable autocomplete hook.

use crate::component::Component;
use crate::keys::{InputEvent, Key, KeyEvent};
use crate::text::display_width;
use crate::theme::Theme;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;

#[derive(Debug, Clone, PartialEq)]
struct PendingPaste {
    placeholder: String,
    text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditorState {
    pub lines: Vec<String>,
    /// (row, grapheme column)
    pub cursor: (usize, usize),
    pending_pastes: Vec<PendingPaste>,
    next_paste_id: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorSubmission {
    pub display_text: String,
    pub text: String,
}

pub struct Editor {
    state: EditorState,
    undo_stack: Vec<EditorState>,
    kill_ring: Vec<String>,
    pub history: Vec<String>,
    history_states: Vec<EditorState>,
    history_pos: Option<usize>,
    history_draft: Option<EditorState>,
    pub border_color_token: String,
    theme: Theme,
    pub placeholder: String,
}

impl Editor {
    pub fn new(theme: Theme) -> Self {
        Editor {
            state: EditorState {
                lines: vec![String::new()],
                cursor: (0, 0),
                pending_pastes: Vec::new(),
                next_paste_id: 1,
            },
            undo_stack: Vec::new(),
            kill_ring: Vec::new(),
            history: Vec::new(),
            history_states: Vec::new(),
            history_pos: None,
            history_draft: None,
            border_color_token: "border".into(),
            theme,
            placeholder: String::new(),
        }
    }

    pub fn text(&self) -> String {
        self.state.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    pub fn set_text(&mut self, text: &str) {
        self.leave_history_navigation();
        self.push_undo();
        self.state.pending_pastes.clear();
        self.state.next_paste_id = 1;
        self.set_text_state(text);
    }

    fn set_text_state(&mut self, text: &str) {
        self.state.lines = text.split('\n').map(String::from).collect();
        if self.state.lines.is_empty() {
            self.state.lines.push(String::new());
        }
        let row = self.state.lines.len() - 1;
        let col = grapheme_count(&self.state.lines[row]);
        self.state.cursor = (row, col);
    }

    pub fn clear(&mut self) {
        self.leave_history_navigation();
        self.clear_state();
    }

    fn clear_state(&mut self) {
        self.push_undo();
        self.state = empty_state();
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Take the visible and expanded text and reset the editor.
    pub fn take_submission(&mut self) -> EditorSubmission {
        let display_text = self.text();
        let text = expand_pending_pastes(&display_text, &self.state.pending_pastes);
        if !display_text.trim().is_empty()
            && (self.history.last() != Some(&display_text)
                || self
                    .history_states
                    .last()
                    .is_some_and(|state| state.pending_pastes != self.state.pending_pastes))
        {
            self.history.push(display_text.clone());
            self.history_states.push(self.state.clone());
            if self.history.len() > 100 {
                self.history.remove(0);
                self.history_states.remove(0);
            }
        }
        self.history_pos = None;
        self.history_draft = None;
        self.state = empty_state();
        self.undo_stack.clear();
        EditorSubmission { display_text, text }
    }

    /// Take only the expanded text and reset the editor.
    pub fn take(&mut self) -> String {
        self.take_submission().text
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(self.state.clone());
        if self.undo_stack.len() > 200 {
            self.undo_stack.remove(0);
        }
    }

    pub fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            self.state = prev;
        }
    }

    fn current_line(&mut self) -> &mut String {
        let row = self.state.cursor.0;
        &mut self.state.lines[row]
    }

    pub fn insert(&mut self, text: &str) {
        self.leave_history_navigation();
        self.push_undo();
        for c in text.chars() {
            if c == '\n' {
                self.split_line();
            } else if c != '\r' {
                let (row, col) = self.state.cursor;
                let line = &mut self.state.lines[row];
                let byte = grapheme_byte_offset(line, col);
                line.insert(byte, c);
                self.state.cursor.1 += 1;
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let char_count = normalized.chars().count();
        if char_count > LARGE_PASTE_CHAR_THRESHOLD || normalized.contains('\n') {
            let placeholder = format!(
                "[Pasted text #{} {char_count} chars]",
                self.state.next_paste_id
            );
            self.insert(&placeholder);
            self.state.next_paste_id += 1;
            self.state.pending_pastes.push(PendingPaste {
                placeholder,
                text: normalized,
            });
        } else {
            self.insert(&normalized);
        }
    }

    fn split_line(&mut self) {
        let (row, col) = self.state.cursor;
        let line = &mut self.state.lines[row];
        let byte = grapheme_byte_offset(line, col);
        let rest = line.split_off(byte);
        self.state.lines.insert(row + 1, rest);
        self.state.cursor = (row + 1, 0);
    }

    pub fn newline(&mut self) {
        self.leave_history_navigation();
        self.push_undo();
        self.split_line();
    }

    pub fn backspace(&mut self) {
        self.leave_history_navigation();
        let (row, col) = self.state.cursor;
        if col > 0 {
            self.push_undo();
            let line = self.current_line();
            let start = grapheme_byte_offset(line, col - 1);
            let end = grapheme_byte_offset(line, col);
            line.replace_range(start..end, "");
            self.state.cursor.1 -= 1;
        } else if row > 0 {
            self.push_undo();
            let removed = self.state.lines.remove(row);
            let prev = &mut self.state.lines[row - 1];
            let prev_len = grapheme_count(prev);
            prev.push_str(&removed);
            self.state.cursor = (row - 1, prev_len);
        }
    }

    pub fn delete_forward(&mut self) {
        self.leave_history_navigation();
        let (row, col) = self.state.cursor;
        let line_len = grapheme_count(&self.state.lines[row]);
        if col < line_len {
            self.push_undo();
            let line = self.current_line();
            let start = grapheme_byte_offset(line, col);
            let end = grapheme_byte_offset(line, col + 1);
            line.replace_range(start..end, "");
        } else if row + 1 < self.state.lines.len() {
            self.push_undo();
            let next = self.state.lines.remove(row + 1);
            self.state.lines[row].push_str(&next);
        }
    }

    pub fn move_cursor(&mut self, key: &KeyEvent) {
        let (row, col) = self.state.cursor;
        match key.key {
            Key::Left if key.ctrl || key.alt => {
                self.state.cursor.1 = word_left(&self.state.lines[row], col)
            }
            Key::Right if key.ctrl || key.alt => {
                self.state.cursor.1 = word_right(&self.state.lines[row], col)
            }
            Key::Left => {
                if col > 0 {
                    self.state.cursor.1 -= 1;
                } else if row > 0 {
                    self.state.cursor = (row - 1, grapheme_count(&self.state.lines[row - 1]));
                }
            }
            Key::Right => {
                if col < grapheme_count(&self.state.lines[row]) {
                    self.state.cursor.1 += 1;
                } else if row + 1 < self.state.lines.len() {
                    self.state.cursor = (row + 1, 0);
                }
            }
            Key::Up => {
                if row > 0 {
                    self.state.cursor =
                        (row - 1, col.min(grapheme_count(&self.state.lines[row - 1])));
                }
            }
            Key::Down => {
                if row + 1 < self.state.lines.len() {
                    self.state.cursor =
                        (row + 1, col.min(grapheme_count(&self.state.lines[row + 1])));
                }
            }
            Key::Home => self.state.cursor.1 = 0,
            Key::End => self.state.cursor.1 = grapheme_count(&self.state.lines[row]),
            _ => {}
        }
    }

    /// Kill to end of line (Ctrl+K).
    pub fn kill_to_end(&mut self) {
        self.leave_history_navigation();
        self.push_undo();
        let (_, col) = self.state.cursor;
        let line = self.current_line();
        let byte = grapheme_byte_offset(line, col);
        let killed = line.split_off(byte);
        if !killed.is_empty() {
            self.kill_ring.push(killed);
        }
    }

    /// Kill to start of line (Ctrl+U).
    pub fn kill_to_start(&mut self) {
        self.leave_history_navigation();
        self.push_undo();
        let (_, col) = self.state.cursor;
        let line = self.current_line();
        let byte = grapheme_byte_offset(line, col);
        let killed: String = line.drain(..byte).collect();
        if !killed.is_empty() {
            self.kill_ring.push(killed);
        }
        self.state.cursor.1 = 0;
    }

    /// Yank last kill (Ctrl+Y).
    pub fn yank(&mut self) {
        if let Some(text) = self.kill_ring.last().cloned() {
            self.insert(&text);
        }
    }

    /// Delete word backwards (Ctrl+W / Alt+Backspace).
    pub fn delete_word_back(&mut self) {
        self.leave_history_navigation();
        let (_, col) = self.state.cursor;
        if col == 0 {
            self.backspace();
            return;
        }
        self.push_undo();
        let line = self.current_line();
        let target = word_left(line, col);
        let start = grapheme_byte_offset(line, target);
        let end = grapheme_byte_offset(line, col);
        let killed: String = line.drain(start..end).collect();
        self.kill_ring.push(killed);
        self.state.cursor.1 = target;
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.history_draft = Some(self.state.clone());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(pos);
        self.push_undo();
        self.state = self.history_states[pos].clone();
    }

    pub fn history_next(&mut self) {
        let Some(pos) = self.history_pos else { return };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.push_undo();
            self.state = self.history_states[pos + 1].clone();
        } else {
            self.history_pos = None;
            if let Some(draft) = self.history_draft.take() {
                self.push_undo();
                self.state = draft;
            } else {
                self.clear_state();
            }
        }
    }

    fn leave_history_navigation(&mut self) {
        if self.history_pos.take().is_some() {
            self.history_draft = None;
        }
    }

    fn history_up_or_cursor(&mut self, key: &KeyEvent) {
        let (row, col) = self.state.cursor;
        if row == 0 && (self.history_pos.is_some() || col == 0 || self.is_empty()) {
            self.history_prev();
        } else if row == 0 {
            self.state.cursor.1 = 0;
        } else {
            self.move_cursor(key);
        }
    }

    fn history_down_or_cursor(&mut self, key: &KeyEvent) {
        let (row, col) = self.state.cursor;
        let last_row = self.state.lines.len().saturating_sub(1);
        if self.history_pos.is_some() && row == last_row {
            self.history_next();
        } else if row == last_row {
            self.state.cursor.1 = grapheme_count(&self.state.lines[row]);
        } else {
            let _ = col;
            self.move_cursor(key);
        }
    }

    /// Handle a decoded input event. Returns text when submitted.
    pub fn handle_event(&mut self, event: &InputEvent) -> Option<EditorSubmission> {
        match event {
            InputEvent::Paste(text) => {
                self.paste(text);
                None
            }
            InputEvent::Key(key) => self.handle_key(key),
        }
    }

    fn handle_key(&mut self, key: &KeyEvent) -> Option<EditorSubmission> {
        match (&key.key, key.ctrl, key.alt, key.shift) {
            (Key::Enter, false, false, false) => return Some(self.take_submission()),
            (Key::Enter, _, true, _) | (Key::Enter, _, _, true) | (Key::Enter, true, _, _) => {
                self.newline()
            }
            (Key::Backspace, false, false, _) => self.backspace(),
            (Key::Backspace, _, true, _) | (Key::Char('w'), true, _, _) => self.delete_word_back(),
            (Key::Delete, ..) => self.delete_forward(),
            (Key::Up, ..) => self.history_up_or_cursor(key),
            (Key::Down, ..) => self.history_down_or_cursor(key),
            (Key::Left | Key::Right | Key::Home | Key::End, ..) => self.move_cursor(key),
            (Key::Char('a'), true, ..) => self.state.cursor.1 = 0,
            (Key::Char('e'), true, ..) => {
                let row = self.state.cursor.0;
                self.state.cursor.1 = grapheme_count(&self.state.lines[row]);
            }
            (Key::Char('k'), true, ..) => self.kill_to_end(),
            (Key::Char('u'), true, ..) => self.kill_to_start(),
            (Key::Char('y'), true, ..) => self.yank(),
            (Key::Char('z'), true, ..) => self.undo(),
            (Key::Char(c), false, false, _) => {
                let ch = *c;
                self.insert(&ch.to_string());
            }
            _ => {}
        }
        None
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.state.cursor
    }

    /// Text on the active line before the cursor.
    pub fn current_line_before_cursor(&self) -> String {
        let (row, col) = self.state.cursor;
        let line = &self.state.lines[row];
        line[..grapheme_byte_offset(line, col)].to_string()
    }

    /// Replace a number of graphemes immediately before the cursor.
    pub fn replace_before_cursor(&mut self, count: usize, replacement: &str) {
        let (row, col) = self.state.cursor;
        let start_col = col.saturating_sub(count);
        let line = &self.state.lines[row];
        let start = grapheme_byte_offset(line, start_col);
        let end = grapheme_byte_offset(line, col);
        self.push_undo();
        self.state.lines[row].replace_range(start..end, replacement);
        self.state.cursor = (row, start_col + grapheme_count(replacement));
    }

    /// Replace an exact text prefix immediately before the cursor.
    pub fn replace_prefix_before_cursor(&mut self, prefix: &str, replacement: &str) -> bool {
        if !self.current_line_before_cursor().ends_with(prefix) {
            return false;
        }
        self.replace_before_cursor(grapheme_count(prefix), replacement);
        true
    }
}

impl Component for Editor {
    fn render(&mut self, width: usize) -> Vec<String> {
        let inner = width.saturating_sub(4).max(1);
        let border = self.theme.fg(&self.border_color_token, "│");
        let top = self.theme.fg(
            &self.border_color_token,
            &format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
        );
        let bottom = self.theme.fg(
            &self.border_color_token,
            &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        );

        let mut lines = vec![top];
        let (cursor_row, cursor_col) = self.state.cursor;
        let show_placeholder = self.is_empty() && !self.placeholder.is_empty();
        for (row, line) in self.state.lines.iter().enumerate() {
            if show_placeholder && row == 0 {
                let placeholder = self
                    .theme
                    .dim(&crate::text::truncate_to_width(&self.placeholder, inner));
                let shown = format!("{}{placeholder}", crate::renderer::CURSOR_MARKER);
                push_editor_row(&mut lines, &border, shown, inner);
                continue;
            }

            let ranges = visual_ranges(line, inner, (row == cursor_row).then_some(cursor_col));
            let graphemes = line.graphemes(true).collect::<Vec<_>>();
            let range_count = ranges.len();
            for (range_index, (start, end)) in ranges.into_iter().enumerate() {
                let segment = graphemes[start..end].concat();
                let shown = if row == cursor_row
                    && cursor_col >= start
                    && (cursor_col < end || (cursor_col == end && range_index + 1 == range_count))
                {
                    render_cursor_line(&segment, cursor_col - start)
                } else {
                    segment
                };
                push_editor_row(&mut lines, &border, shown, inner);
            }
        }
        lines.push(bottom);
        lines
    }
}

fn push_editor_row(lines: &mut Vec<String>, border: &str, shown: String, inner: usize) {
    let shown = if display_width(&shown) > inner {
        let fitted = crate::text::truncate_to_width(&crate::text::strip_ansi(&shown), inner);
        if shown.contains(crate::renderer::CURSOR_MARKER) {
            format!("{}{fitted}", crate::renderer::CURSOR_MARKER)
        } else {
            fitted
        }
    } else {
        shown
    };
    let pad = inner.saturating_sub(display_width(&shown));
    lines.push(format!("{border} {shown}{} {border}", " ".repeat(pad)));
}

fn visual_ranges(line: &str, width: usize, cursor_col: Option<usize>) -> Vec<(usize, usize)> {
    let graphemes = line.graphemes(true).collect::<Vec<_>>();
    if graphemes.is_empty() {
        return vec![(0, 0)];
    }

    let mut ranges = Vec::new();
    let mut start = 0;
    let mut row_width = 0;
    for (index, grapheme) in graphemes.iter().enumerate() {
        let grapheme_width = UnicodeWidthStr::width(*grapheme);
        if index > start && row_width + grapheme_width > width {
            ranges.push((start, index));
            start = index;
            row_width = 0;
        }
        row_width += grapheme_width;
    }
    ranges.push((start, graphemes.len()));

    if cursor_col == Some(graphemes.len()) && row_width >= width {
        ranges.push((graphemes.len(), graphemes.len()));
    }
    ranges
}

fn render_cursor_line(line: &str, cursor_col: usize) -> String {
    let graphemes: Vec<&str> = line.graphemes(true).collect();
    let before: String = graphemes[..cursor_col.min(graphemes.len())].concat();
    let at: &str = graphemes.get(cursor_col).copied().unwrap_or(" ");
    let after: String = if cursor_col < graphemes.len() {
        graphemes[cursor_col + 1..].concat()
    } else {
        String::new()
    };
    format!("{before}{}{at}{after}", crate::renderer::CURSOR_MARKER)
}

fn grapheme_count(s: &str) -> usize {
    s.graphemes(true).count()
}

fn empty_state() -> EditorState {
    EditorState {
        lines: vec![String::new()],
        cursor: (0, 0),
        pending_pastes: Vec::new(),
        next_paste_id: 1,
    }
}

fn expand_pending_pastes(display_text: &str, pending_pastes: &[PendingPaste]) -> String {
    pending_pastes
        .iter()
        .fold(display_text.to_string(), |text, paste| {
            text.replacen(&paste.placeholder, &paste.text, 1)
        })
}

fn grapheme_byte_offset(s: &str, col: usize) -> usize {
    s.grapheme_indices(true)
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_left(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line
        .graphemes(true)
        .map(|g| g.chars().next().unwrap_or(' '))
        .collect();
    let mut i = col;
    while i > 0 && !is_word_char(chars[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_word_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

fn word_right(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line
        .graphemes(true)
        .map(|g| g.chars().next().unwrap_or(' '))
        .collect();
    let mut i = col;
    while i < chars.len() && !is_word_char(chars[i]) {
        i += 1;
    }
    while i < chars.len() && is_word_char(chars[i]) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        Editor::new(Theme::dark())
    }

    #[test]
    fn insert_and_submit() {
        let mut e = editor();
        e.insert("hello");
        assert_eq!(e.text(), "hello");
        let out = e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Enter,
            ..Default::default()
        }));
        assert_eq!(
            out,
            Some(EditorSubmission {
                display_text: "hello".into(),
                text: "hello".into(),
            })
        );
        assert!(e.is_empty());
        assert_eq!(e.history, vec!["hello"]);
    }

    #[test]
    fn multiline_and_backspace_join() {
        let mut e = editor();
        e.insert("ab");
        e.newline();
        e.insert("cd");
        assert_eq!(e.text(), "ab\ncd");
        e.state_cursor_to(1, 0);
        e.backspace();
        assert_eq!(e.text(), "abcd");
    }

    #[test]
    fn word_navigation_and_kill() {
        let mut e = editor();
        e.insert("foo bar_baz qux");
        e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Left,
            ctrl: true,
            ..Default::default()
        }));
        assert_eq!(e.cursor().1, 12); // start of qux
        e.kill_to_end();
        assert_eq!(e.text(), "foo bar_baz ");
        e.yank();
        assert_eq!(e.text(), "foo bar_baz qux");
    }

    #[test]
    fn undo_restores() {
        let mut e = editor();
        e.insert("first");
        e.clear();
        assert!(e.is_empty());
        e.undo();
        assert_eq!(e.text(), "first");
    }

    #[test]
    fn history_recall() {
        let mut e = editor();
        e.insert("one");
        e.take();
        e.insert("two");
        e.take();
        e.history_prev();
        assert_eq!(e.text(), "two");
        e.history_prev();
        assert_eq!(e.text(), "one");
        e.history_next();
        assert_eq!(e.text(), "two");
    }

    #[test]
    fn up_and_down_recall_history_and_restore_the_draft() {
        let mut e = editor();
        e.insert("one");
        e.take();
        e.insert("two");
        e.take();
        e.insert("draft");
        e.state_cursor_to(0, 0);

        e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Up,
            ..Default::default()
        }));
        assert_eq!(e.text(), "two");
        e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Up,
            ..Default::default()
        }));
        assert_eq!(e.text(), "one");
        e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Down,
            ..Default::default()
        }));
        e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Down,
            ..Default::default()
        }));
        assert_eq!(e.text(), "draft");
        assert_eq!(e.cursor(), (0, 0));
    }

    #[test]
    fn history_avoids_consecutive_duplicates_and_stays_bounded() {
        let mut e = editor();
        for _ in 0..2 {
            e.insert("same");
            e.take();
        }
        assert_eq!(e.history, vec!["same"]);
        for index in 0..110 {
            e.insert(&format!("prompt {index}"));
            e.take();
        }
        assert_eq!(e.history.len(), 100);
        assert_eq!(e.history.first().map(String::as_str), Some("prompt 10"));
    }

    #[test]
    fn arrows_move_inside_multiline_text_before_using_history() {
        let mut e = editor();
        e.insert("old");
        e.take();
        e.insert("first\nsecond");
        assert_eq!(e.cursor(), (1, 6));

        e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Up,
            ..Default::default()
        }));
        assert_eq!(e.text(), "first\nsecond");
        assert_eq!(e.cursor(), (0, 5));
        e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Up,
            ..Default::default()
        }));
        assert_eq!(e.cursor(), (0, 0));
        assert_eq!(e.text(), "first\nsecond");
        e.handle_event(&InputEvent::Key(KeyEvent {
            key: Key::Up,
            ..Default::default()
        }));
        assert_eq!(e.text(), "old");
    }

    #[test]
    fn multiline_paste_uses_a_reference_and_expands_on_submit() {
        let mut e = editor();
        e.handle_event(&InputEvent::Paste("a\r\nb\rc".into()));
        assert_eq!(e.text(), "[Pasted text #1 5 chars]");
        assert_eq!(
            e.take_submission(),
            EditorSubmission {
                display_text: "[Pasted text #1 5 chars]".into(),
                text: "a\nb\nc".into(),
            }
        );
    }

    #[test]
    fn paste_references_use_draft_order_and_reset_after_submit() {
        let mut e = editor();
        e.paste("one\ntwo");
        e.insert(" ");
        e.paste(&"x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1));
        assert_eq!(
            e.text(),
            "[Pasted text #1 7 chars] [Pasted text #2 1001 chars]"
        );
        let submission = e.take_submission();
        assert_eq!(submission.text, format!("one\ntwo {}", "x".repeat(1001)));

        e.paste("new\ndraft");
        assert_eq!(e.text(), "[Pasted text #1 9 chars]");
    }

    #[test]
    fn short_single_line_paste_stays_editable() {
        let mut e = editor();
        e.paste("small paste");
        assert_eq!(e.text(), "small paste");
        assert!(e.state.pending_pastes.is_empty());
    }

    #[test]
    fn undo_and_history_restore_paste_payloads() {
        let mut e = editor();
        e.paste("first\nsecond");
        e.undo();
        assert!(e.is_empty());
        e.paste("first\nsecond");
        let expected = e.take_submission();
        e.history_prev();
        assert_eq!(e.take_submission(), expected);
    }

    #[test]
    fn shift_and_alt_enter_insert_newlines() {
        for spec in ["shift+enter", "alt+enter"] {
            let mut e = editor();
            e.insert("first");
            assert_eq!(
                e.handle_event(&InputEvent::Key(KeyEvent::parse(spec).unwrap())),
                None
            );
            e.insert("second");
            assert_eq!(e.text(), "first\nsecond");
        }
    }

    #[test]
    fn render_wraps_long_lines_and_keeps_one_cursor_marker() {
        let mut e = editor();
        e.insert("abcdefghij");
        let rendered = e.render(10);
        assert_eq!(rendered.len(), 4);
        assert!(rendered.iter().all(|line| display_width(line) == 10));
        assert_eq!(
            rendered
                .iter()
                .map(|line| line.matches(crate::renderer::CURSOR_MARKER).count())
                .sum::<usize>(),
            1
        );
        assert!(rendered.iter().all(|line| !line.contains("\x1b[7m")));
    }

    #[test]
    fn exact_width_line_puts_end_cursor_on_a_new_visual_row() {
        let mut e = editor();
        e.insert("abcdef");
        let rendered = e.render(10);
        assert_eq!(rendered.len(), 4);
        assert!(rendered[2].contains(crate::renderer::CURSOR_MARKER));
    }

    #[test]
    fn narrow_render_keeps_wide_text_within_the_border() {
        let mut e = editor();
        e.insert("日");
        let rendered = e.render(5);
        assert!(rendered.iter().all(|line| display_width(line) == 5));
        assert!(
            rendered
                .iter()
                .any(|line| line.contains(crate::renderer::CURSOR_MARKER))
        );
    }

    #[test]
    fn replace_before_cursor_keeps_text_after_cursor() {
        let mut e = editor();
        e.insert("see @sr now");
        e.state_cursor_to(0, 7);
        assert_eq!(e.current_line_before_cursor(), "see @sr");
        e.replace_before_cursor(3, "@src/main.rs ");
        assert_eq!(e.text(), "see @src/main.rs  now");
        assert_eq!(e.cursor(), (0, 17));
    }

    impl Editor {
        fn state_cursor_to(&mut self, row: usize, col: usize) {
            self.state.cursor = (row, col);
        }
    }
}
