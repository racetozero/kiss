//! Viewport-aware differential renderer for the terminal main screen.
//!
//! The renderer keeps the complete logical frame and a map from logical rows
//! to the visible terminal viewport. This distinction matters after output has
//! scrolled: cursor movement sequences operate on screen rows, not on logical
//! frame indices. The renderer is the only frame writer in the TUI.

use crate::text::{display_width, strip_ansi, truncate_to_width};
use std::borrow::Cow;
use std::io::Write;
use std::sync::Arc;

/// Zero-width APC sequence inserted by a focused component at its cursor.
/// The renderer removes it and positions the terminal cursor at that cell.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorPosition {
    row: usize,
    col: usize,
}

struct PreparedFrame {
    source: Vec<Arc<str>>,
    lines: Vec<Arc<str>>,
    cursor_position: Option<CursorPosition>,
}

struct ReusableLines<'a> {
    source: &'a [Arc<str>],
    prepared: &'a [Arc<str>],
}

pub struct DiffRenderer {
    previous: Vec<Arc<str>>,
    source: Vec<Arc<str>>,
    width: usize,
    height: usize,
    /// Logical row at the end of the rendered content.
    cursor_row: usize,
    /// Logical row that contains the real terminal cursor.
    hardware_cursor_row: usize,
    /// Greatest frame height since the last clear.
    max_lines_rendered: usize,
    /// Logical row shown at the top of the previous visible viewport.
    previous_viewport_top: usize,
    cursor_position: Option<CursorPosition>,
    invalidated: bool,
}

impl Default for DiffRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffRenderer {
    pub fn new() -> Self {
        Self {
            previous: Vec::new(),
            source: Vec::new(),
            width: 0,
            height: 0,
            cursor_row: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 0,
            previous_viewport_top: 0,
            cursor_position: None,
            invalidated: false,
        }
    }

    /// Force the next frame to rebuild the visible screen from known content.
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    pub fn line_count(&self) -> usize {
        self.previous.len()
    }

    /// Render one logical frame for a terminal of `width` by `height` cells.
    pub fn render_frame(
        &mut self,
        new_lines: &[String],
        width: usize,
        height: usize,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        let can_reuse = self.width == width;
        let prepared = prepare_lines(
            new_lines,
            width,
            height,
            can_reuse.then_some(ReusableLines {
                source: &self.source,
                prepared: &self.previous,
            }),
        );

        let first_render = self.width == 0 && self.height == 0 && self.previous.is_empty();
        let width_changed = self.width != 0 && self.width != width;
        let height_changed = self.height != 0 && self.height != height;

        if first_render {
            self.full_render(prepared, width, height, false, out)?;
            return Ok(());
        }
        if self.invalidated
            || width_changed
            || height_changed
            || prepared.lines.len() < self.max_lines_rendered
        {
            self.full_render(prepared, width, height, true, out)?;
            return Ok(());
        }

        let (first_changed, last_changed) = changed_range(&self.previous, &prepared.lines);
        let Some(first_changed) = first_changed else {
            self.position_hardware_cursor(prepared.cursor_position, false, out)?;
            self.width = width;
            self.height = height;
            return Ok(());
        };
        let last_changed = last_changed.expect("a first changed row has a last changed row");

        // A differential write cannot change a row that has left the visible
        // viewport. Rebuild the frame so the logical-to-screen map is valid.
        if first_changed < self.previous_viewport_top || first_changed >= prepared.lines.len() {
            self.full_render(prepared, width, height, true, out)?;
            return Ok(());
        }

        let appended = prepared.lines.len() > self.previous.len();
        let append_start = appended && first_changed == self.previous.len() && first_changed > 0;
        let mut previous_viewport_top = self.previous_viewport_top;
        let mut viewport_top = previous_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;
        let previous_viewport_bottom = previous_viewport_top + height - 1;
        let move_target_row = if append_start {
            first_changed - 1
        } else {
            first_changed
        };

        let mut buffer = String::from("\x1b[?2026h");

        // A cursor-down command cannot move below the viewport. Scroll with
        // real newlines when the target logical row is below it.
        if move_target_row > previous_viewport_bottom {
            let current_screen_row = hardware_cursor_row
                .saturating_sub(previous_viewport_top)
                .min(height - 1);
            let move_to_bottom = height - 1 - current_screen_row;
            if move_to_bottom > 0 {
                buffer.push_str(&format!("\x1b[{move_to_bottom}B"));
            }
            let scroll = move_target_row - previous_viewport_bottom;
            buffer.push_str(&"\r\n".repeat(scroll));
            previous_viewport_top += scroll;
            viewport_top += scroll;
            hardware_cursor_row = move_target_row;
        }

        let current_screen_row = hardware_cursor_row.saturating_sub(previous_viewport_top);
        let target_screen_row = move_target_row.saturating_sub(viewport_top);
        push_vertical_move(&mut buffer, current_screen_row, target_screen_row);
        buffer.push_str(if append_start { "\r\n" } else { "\r" });

        let render_end = last_changed.min(prepared.lines.len() - 1);
        for (offset, line) in prepared.lines[first_changed..=render_end]
            .iter()
            .enumerate()
        {
            if offset > 0 {
                buffer.push_str("\r\n");
            }
            buffer.push_str("\x1b[2K");
            buffer.push_str(line);
        }
        buffer.push_str("\x1b[?2026l");
        out.write_all(buffer.as_bytes())?;

        let final_cursor_row = render_end;
        self.cursor_row = prepared.lines.len().saturating_sub(1);
        self.hardware_cursor_row = final_cursor_row;
        self.max_lines_rendered = self.max_lines_rendered.max(prepared.lines.len());
        self.previous_viewport_top =
            previous_viewport_top.max(final_cursor_row.saturating_sub(height - 1));
        self.previous = prepared.lines;
        self.source = prepared.source;
        self.width = width;
        self.height = height;
        self.invalidated = false;
        self.position_hardware_cursor(prepared.cursor_position, true, out)?;
        Ok(())
    }

    fn full_render(
        &mut self,
        prepared: PreparedFrame,
        width: usize,
        height: usize,
        clear: bool,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        let mut buffer = String::from("\x1b[?2026h");
        if clear {
            // Clear the visible screen and stale scrollback before the full
            // logical frame is replayed.
            buffer.push_str("\x1b[2J\x1b[H\x1b[3J");
        } else {
            buffer.push('\r');
        }
        for (index, line) in prepared.lines.iter().enumerate() {
            if index > 0 {
                buffer.push_str("\r\n");
            }
            buffer.push_str("\x1b[2K");
            buffer.push_str(line);
        }
        buffer.push_str("\x1b[?2026l");
        out.write_all(buffer.as_bytes())?;

        self.cursor_row = prepared.lines.len().saturating_sub(1);
        self.hardware_cursor_row = self.cursor_row;
        self.max_lines_rendered = prepared.lines.len();
        let buffer_length = height.max(prepared.lines.len());
        self.previous_viewport_top = buffer_length.saturating_sub(height);
        self.previous = prepared.lines;
        self.source = prepared.source;
        self.width = width;
        self.height = height;
        self.invalidated = false;
        self.cursor_position = None;
        self.position_hardware_cursor(prepared.cursor_position, true, out)?;
        Ok(())
    }

    fn position_hardware_cursor(
        &mut self,
        position: Option<CursorPosition>,
        force: bool,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        if !force && position == self.cursor_position {
            return Ok(());
        }

        match position {
            Some(position) => {
                let mut buffer = String::new();
                push_vertical_move(&mut buffer, self.hardware_cursor_row, position.row);
                buffer.push_str(&format!("\x1b[{}G\x1b[1 q\x1b[?25h", position.col + 1));
                out.write_all(buffer.as_bytes())?;
                self.hardware_cursor_row = position.row;
            }
            None => {
                if force || self.cursor_position.is_some() {
                    out.write_all(b"\x1b[?25l")?;
                }
            }
        }
        self.cursor_position = position;
        Ok(())
    }
}

fn prepare_lines(
    lines: &[String],
    width: usize,
    height: usize,
    reusable: Option<ReusableLines<'_>>,
) -> PreparedFrame {
    let viewport_top = lines.len().saturating_sub(height);
    let mut position = None;
    let mut source_lines = Vec::with_capacity(lines.len());
    let mut prepared = Vec::with_capacity(lines.len());
    for (row, source) in lines.iter().enumerate() {
        let marker = (row >= viewport_top)
            .then(|| source.find(CURSOR_MARKER))
            .flatten();
        if let Some(index) = marker
            && position.is_none()
            && row >= viewport_top
        {
            position = Some(CursorPosition {
                row,
                col: display_width(&source[..index]).min(width.saturating_sub(1)),
            });
        }
        if marker.is_none()
            && let Some(old) = reusable.as_ref()
            && old
                .source
                .get(row)
                .is_some_and(|old| old.as_ref() == source)
        {
            source_lines.push(old.source[row].clone());
            prepared.push(old.prepared[row].clone());
            continue;
        }
        source_lines.push(Arc::from(source.as_str()));
        let without_marker = marker
            .map(|_| Cow::Owned(source.replace(CURSOR_MARKER, "")))
            .unwrap_or_else(|| Cow::Borrowed(source));
        let line = if display_width(&without_marker) > width {
            truncate_to_width(&strip_ansi(&without_marker), width)
        } else {
            without_marker.into_owned()
        };
        let mut output = String::with_capacity(line.len() + 4);
        output.push_str(&line);
        output.push_str("\x1b[0m");
        prepared.push(Arc::from(output));
    }
    PreparedFrame {
        source: source_lines,
        lines: prepared,
        cursor_position: position,
    }
}

fn changed_range(old: &[Arc<str>], new: &[Arc<str>]) -> (Option<usize>, Option<usize>) {
    let length = old.len().max(new.len());
    let changed = |index: usize| {
        let old_line = old.get(index).map(AsRef::as_ref).unwrap_or("");
        let new_line = new.get(index).map(AsRef::as_ref).unwrap_or("");
        old_line != new_line
    };
    let first = (0..length).find(|index| changed(*index));
    let last = first.and_then(|first| (first..length).rev().find(|index| changed(*index)));
    (first, last)
}

fn push_vertical_move(buffer: &mut String, current_row: usize, target_row: usize) {
    match target_row.cmp(&current_row) {
        std::cmp::Ordering::Less => buffer.push_str(&format!("\x1b[{}A", current_row - target_row)),
        std::cmp::Ordering::Greater => {
            buffer.push_str(&format!("\x1b[{}B", target_row - current_row))
        }
        std::cmp::Ordering::Equal => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(renderer: &mut DiffRenderer, lines: &[&str], width: usize, height: usize) -> String {
        let owned: Vec<String> = lines.iter().map(|line| (*line).to_string()).collect();
        let mut out = Vec::new();
        renderer
            .render_frame(&owned, width, height, &mut out)
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn generated_lines(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| {
                format!(
                    "\x1b[38;2;90;170;255m{index:05}\x1b[0m source/path/module_{index:05}.rs: deterministic renderer benchmark"
                )
            })
            .collect()
    }

    #[derive(Debug)]
    struct VirtualTerminal {
        width: usize,
        height: usize,
        lines: Vec<Vec<char>>,
        viewport_top: usize,
        row: usize,
        col: usize,
    }

    impl VirtualTerminal {
        fn new(width: usize, height: usize) -> Self {
            Self {
                width,
                height,
                lines: vec![Vec::new(); height],
                viewport_top: 0,
                row: 0,
                col: 0,
            }
        }

        fn feed(&mut self, input: &str) {
            let chars: Vec<char> = input.chars().collect();
            let mut index = 0;
            while index < chars.len() {
                match chars[index] {
                    '\x1b' if chars.get(index + 1) == Some(&'[') => {
                        index += 2;
                        let start = index;
                        while index < chars.len() && !('@'..='~').contains(&chars[index]) {
                            index += 1;
                        }
                        if index >= chars.len() {
                            break;
                        }
                        let final_byte = chars[index];
                        let params: String = chars[start..index].iter().collect();
                        self.csi(final_byte, &params);
                    }
                    '\x1b' => {
                        // APC and OSC sequences end at BEL in these tests.
                        while index < chars.len() && chars[index] != '\x07' {
                            index += 1;
                        }
                    }
                    '\r' => self.col = 0,
                    '\n' => self.line_feed(),
                    character if !character.is_control() => self.put(character),
                    _ => {}
                }
                index += 1;
            }
        }

        fn csi(&mut self, final_byte: char, params: &str) {
            let count = params
                .trim_start_matches('?')
                .parse::<usize>()
                .unwrap_or(1)
                .max(1);
            match final_byte {
                'A' => self.row = self.row.saturating_sub(count).max(self.viewport_top),
                'B' => self.row = (self.row + count).min(self.viewport_top + self.height - 1),
                'G' => self.col = count - 1,
                'H' => {
                    self.row = self.viewport_top;
                    self.col = 0;
                }
                'J' if params == "2" => {
                    for row in self.viewport_top..self.viewport_top + self.height {
                        self.ensure_row(row);
                        self.lines[row].clear();
                    }
                }
                'J' if params == "3" => {
                    let visible = self.lines.split_off(self.viewport_top);
                    self.lines = visible;
                    self.row = self.row.saturating_sub(self.viewport_top);
                    self.viewport_top = 0;
                    while self.lines.len() < self.height {
                        self.lines.push(Vec::new());
                    }
                }
                'K' => {
                    self.ensure_row(self.row);
                    self.lines[self.row].clear();
                    self.col = 0;
                }
                _ => {}
            }
        }

        fn line_feed(&mut self) {
            let bottom = self.viewport_top + self.height - 1;
            if self.row == bottom {
                self.viewport_top += 1;
                self.row += 1;
                self.ensure_row(self.row);
            } else {
                self.row += 1;
                self.ensure_row(self.row);
            }
        }

        fn put(&mut self, character: char) {
            self.ensure_row(self.row);
            while self.lines[self.row].len() < self.col {
                self.lines[self.row].push(' ');
            }
            if self.col < self.lines[self.row].len() {
                self.lines[self.row][self.col] = character;
            } else if self.col < self.width {
                self.lines[self.row].push(character);
            }
            self.col = (self.col + 1).min(self.width);
        }

        fn ensure_row(&mut self, row: usize) {
            while self.lines.len() <= row {
                self.lines.push(Vec::new());
            }
        }

        fn history(&self) -> Vec<String> {
            self.lines
                .iter()
                .map(|line| line.iter().collect::<String>().trim_end().to_string())
                .collect()
        }

        fn viewport(&self) -> Vec<String> {
            self.history()[self.viewport_top..self.viewport_top + self.height].to_vec()
        }
    }

    #[test]
    fn identical_frame_is_noop() {
        let mut renderer = DiffRenderer::new();
        render(&mut renderer, &["a", "b"], 80, 24);
        assert!(render(&mut renderer, &["a", "b"], 80, 24).is_empty());
    }

    #[test]
    fn append_only_does_not_repaint_prefix() {
        let mut renderer = DiffRenderer::new();
        render(&mut renderer, &["line-one", "line-two"], 80, 24);
        let output = render(
            &mut renderer,
            &["line-one", "line-two", "line-three"],
            80,
            24,
        );
        assert!(!output.contains("line-one"));
        assert!(!output.contains("line-two"));
        assert!(output.contains("line-three"));
    }

    #[test]
    fn middle_change_preserves_other_rows() {
        let mut renderer = DiffRenderer::new();
        let mut terminal = VirtualTerminal::new(20, 5);
        terminal.feed(&render(&mut renderer, &["aaa", "bbb", "ccc"], 20, 5));
        terminal.feed(&render(&mut renderer, &["aaa", "BBB", "ccc"], 20, 5));
        assert_eq!(&terminal.viewport()[..3], &["aaa", "BBB", "ccc"]);
    }

    #[test]
    fn overflow_append_preserves_history() {
        let mut renderer = DiffRenderer::new();
        let mut terminal = VirtualTerminal::new(20, 5);
        let first: Vec<String> = (0..8).map(|index| format!("PRE {index:02}")).collect();
        let mut output = Vec::new();
        renderer.render_frame(&first, 20, 5, &mut output).unwrap();
        terminal.feed(&String::from_utf8(output).unwrap());

        let mut second = first.clone();
        second.extend((0..8).map(|index| format!("TOOL {index:02}")));
        let mut output = Vec::new();
        renderer.render_frame(&second, 20, 5, &mut output).unwrap();
        terminal.feed(&String::from_utf8(output).unwrap());

        let mut final_frame = second.clone();
        final_frame.extend((0..3).map(|index| format!("POST {index:02}")));
        let mut output = Vec::new();
        renderer
            .render_frame(&final_frame, 20, 5, &mut output)
            .unwrap();
        terminal.feed(&String::from_utf8(output).unwrap());

        let history = terminal.history();
        for expected in first
            .iter()
            .chain(second[first.len()..].iter())
            .chain(final_frame[second.len()..].iter())
        {
            assert!(
                history.iter().any(|line| line == expected),
                "missing preserved row {expected:?}: {history:?}"
            );
        }
        assert_eq!(
            terminal.viewport(),
            vec!["TOOL 06", "TOOL 07", "POST 00", "POST 01", "POST 02"]
        );
    }

    #[test]
    fn change_above_viewport_forces_full_redraw() {
        let mut renderer = DiffRenderer::new();
        let first: Vec<String> = (0..8).map(|index| format!("line {index}")).collect();
        let mut output = Vec::new();
        renderer.render_frame(&first, 20, 5, &mut output).unwrap();
        let mut changed = first;
        changed[0] = "changed".into();
        let mut output = Vec::new();
        renderer.render_frame(&changed, 20, 5, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[2J\x1b[H\x1b[3J"));
    }

    #[test]
    fn shrink_clears_stale_rows_and_can_grow_again() {
        let mut renderer = DiffRenderer::new();
        let mut terminal = VirtualTerminal::new(20, 5);
        terminal.feed(&render(&mut renderer, &["a", "b", "c", "d"], 20, 5));
        terminal.feed(&render(&mut renderer, &["only"], 20, 5));
        assert_eq!(terminal.viewport(), vec!["only", "", "", "", ""]);
        terminal.feed(&render(&mut renderer, &["new-0", "new-1"], 20, 5));
        assert_eq!(terminal.viewport(), vec!["new-0", "new-1", "", "", ""]);
    }

    #[test]
    fn resize_forces_full_redraw() {
        let mut renderer = DiffRenderer::new();
        render(&mut renderer, &["same"], 20, 5);
        assert!(render(&mut renderer, &["same"], 21, 5).contains("\x1b[2J"));
        assert!(render(&mut renderer, &["same"], 21, 6).contains("\x1b[2J"));
    }

    #[test]
    fn resize_redraw_is_one_complete_synchronized_frame() {
        let mut renderer = DiffRenderer::new();
        let lines = generated_lines(200);
        let refs = lines.iter().map(String::as_str).collect::<Vec<_>>();
        render(&mut renderer, &refs, 120, 40);

        let output = render(&mut renderer, &refs, 100, 40);

        assert_eq!(output.matches("\x1b[?2026h").count(), 1);
        assert_eq!(output.matches("\x1b[?2026l").count(), 1);
        assert_eq!(output.matches("\x1b[2J\x1b[H\x1b[3J").count(), 1);
        assert_eq!(
            output.matches("deterministic renderer benchmark").count(),
            200
        );
    }

    #[test]
    fn cursor_marker_is_removed_and_positions_cursor() {
        let mut renderer = DiffRenderer::new();
        let output = render(&mut renderer, &[&format!("ab{CURSOR_MARKER}cd")], 20, 5);
        assert!(!output.contains(CURSOR_MARKER));
        assert!(output.contains("\x1b[3G\x1b[1 q\x1b[?25h"));
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_renderer_frames() {
        let lines_1_800 = generated_lines(1_800);
        kiss_bench::measure("renderer_full_1800", 11, 3, "1800_logical_rows", || {
            let mut renderer = DiffRenderer::new();
            let mut output = Vec::with_capacity(256 * 1024);
            renderer
                .render_frame(&lines_1_800, 120, 40, &mut output)
                .unwrap();
            output.len()
        });

        let lines_10_000 = generated_lines(10_000);
        let mut renderer = DiffRenderer::new();
        let mut output = Vec::with_capacity(1024);
        renderer
            .render_frame(&lines_10_000, 120, 40, &mut output)
            .unwrap();
        kiss_bench::measure(
            "renderer_same_10000",
            15,
            10,
            "10000_unchanged_logical_rows",
            || {
                output.clear();
                renderer
                    .render_frame(&lines_10_000, 120, 40, &mut output)
                    .unwrap();
                output.len()
            },
        );

        let mut changed = lines_10_000.clone();
        let mut toggle = false;
        kiss_bench::measure(
            "renderer_last_change_10000",
            15,
            10,
            "10000_rows_last_row_changes",
            || {
                toggle = !toggle;
                changed[9_999] = if toggle {
                    "changed terminal row a".into()
                } else {
                    "changed terminal row b".into()
                };
                output.clear();
                renderer
                    .render_frame(&changed, 120, 40, &mut output)
                    .unwrap();
                output.len()
            },
        );
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_renderer_resize() {
        let lines = generated_lines(1_800);
        let mut renderer = DiffRenderer::new();
        let mut output = Vec::with_capacity(256 * 1024);
        renderer.render_frame(&lines, 120, 40, &mut output).unwrap();
        output.clear();
        renderer.render_frame(&lines, 100, 40, &mut output).unwrap();
        let resize_bytes = output.len();
        let mut wide = false;
        kiss_bench::measure(
            "renderer_resize_1800",
            11,
            3,
            &format!("1800_logical_rows_{resize_bytes}_output_bytes"),
            || {
                wide = !wide;
                output.clear();
                renderer
                    .render_frame(&lines, if wide { 120 } else { 100 }, 40, &mut output)
                    .unwrap();
                output.len()
            },
        );
    }
}
