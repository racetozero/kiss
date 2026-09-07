//! Styled text helpers: ANSI-aware width, wrapping, truncation.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Zero-width APC sequence inserted by a focused component at its cursor.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Display width of a string, ignoring ANSI escape sequences.
pub fn display_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut width = 0usize;
    let mut visible_start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'\x1b' {
            index += 1;
            continue;
        }

        width += s[visible_start..index].width();
        index += 1;
        match bytes.get(index).copied() {
            Some(b'[') => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            Some(b']' | b'_') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\x07' {
                        index += 1;
                        break;
                    }
                    if bytes[index] == b'\x1b' && bytes.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            Some(_) => index += 1,
            None => {}
        }
        visible_start = index;
    }
    width + s[visible_start..].width()
}

/// Remove ANSI CSI/OSC escape sequences.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']' | '_') => {
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if c == '\x07' || (prev == '\x1b' && c == '\\') {
                        break;
                    }
                    prev = c;
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// Wrap plain text to `width` columns on grapheme boundaries, preferring
/// word breaks. Returns at least one (possibly empty) line.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.as_bytes().contains(&b'\x1b') {
        return wrap_styled_words(text, width);
    }
    wrap_plain_text(text, width)
}

fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
    wrap_plain_text_inner(text, width, false)
}

fn wrap_plain_text_inner(text: &str, width: usize, nul_is_zero_width: bool) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0usize;
        for word in raw_line.split_inclusive(' ') {
            let word_width = wrap_width(word, nul_is_zero_width);
            // A separator at the end of the candidate does not occupy a
            // visible cell after the line is emitted. Count it only after a
            // later word turns it into an internal separator.
            if current_width + wrap_width(word.trim_end_matches(' '), nul_is_zero_width) <= width {
                current.push_str(word);
                current_width += word_width;
                continue;
            }
            if !current.is_empty() {
                lines.push(current.trim_end().to_string());
            }
            // Hard-break words longer than the width.
            let mut piece = String::new();
            let mut piece_width = 0usize;
            for grapheme in word.graphemes(true) {
                let gw = wrap_width(grapheme, nul_is_zero_width);
                if piece_width + gw > width {
                    lines.push(piece.clone());
                    piece.clear();
                    piece_width = 0;
                }
                piece.push_str(grapheme);
                piece_width += gw;
            }
            current = piece;
            current_width = piece_width;
        }
        lines.push(current.trim_end().to_string());
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_width(text: &str, nul_is_zero_width: bool) -> usize {
    if nul_is_zero_width {
        text.split('\0').map(UnicodeWidthStr::width).sum()
    } else {
        text.width()
    }
}

/// Word-wrap styled terminal text while keeping escape sequences at zero
/// width. Active links are closed at each line end and reopened on the next
/// line, which avoids terminal-dependent links that span rows.
fn wrap_styled_words(text: &str, width: usize) -> Vec<String> {
    let mut masked = String::with_capacity(text.len());
    let mut sequences = Vec::new();
    let mut index = 0;
    while index < text.len() {
        if text.as_bytes()[index] == b'\x1b' {
            let end = ansi_sequence_end(text, index);
            sequences.push(&text[index..end]);
            masked.push('\0');
            index = end;
        } else {
            let character = text[index..].chars().next().unwrap();
            masked.push(character);
            index += character.len_utf8();
        }
    }

    let mut sequence_index = 0;
    let mut sgr = String::new();
    let mut hyperlink: Option<String> = None;
    wrap_plain_text_inner(&masked, width, true)
        .into_iter()
        .map(|line| {
            let mut rendered = String::new();
            if sequence_index > 0 {
                rendered.push_str(&sgr);
                if let Some(link) = &hyperlink {
                    rendered.push_str(link);
                }
            }
            for character in line.chars() {
                if character != '\0' {
                    rendered.push(character);
                    continue;
                }
                let sequence = sequences[sequence_index];
                sequence_index += 1;
                rendered.push_str(sequence);
                if sequence.starts_with("\x1b[") && sequence.ends_with('m') {
                    if matches!(sequence, "\x1b[m" | "\x1b[0m") {
                        sgr.clear();
                    } else {
                        sgr.push_str(sequence);
                    }
                } else if let Some(target) = osc8_target(sequence) {
                    hyperlink = (!target.is_empty()).then(|| sequence.to_string());
                }
            }
            if hyperlink.is_some() {
                rendered.push_str("\x1b]8;;\x1b\\");
            }
            rendered
        })
        .collect()
}

/// Hard-wrap terminal text without changing spaces, ANSI styles, links, or a
/// focused component's cursor marker.
pub fn wrap_terminal_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0;
    let mut sgr = String::new();
    let mut hyperlink: Option<String> = None;
    let mut index = 0;

    while index < text.len() {
        if text[index..].starts_with(CURSOR_MARKER) {
            if line_width == width {
                push_terminal_line(&mut lines, &mut line, hyperlink.is_some());
                line.push_str(&sgr);
                if let Some(link) = &hyperlink {
                    line.push_str(link);
                }
                line_width = 0;
            }
            line.push_str(CURSOR_MARKER);
            index += CURSOR_MARKER.len();
            continue;
        }

        if text.as_bytes()[index] == b'\x1b' {
            let end = ansi_sequence_end(text, index);
            let sequence = &text[index..end];
            line.push_str(sequence);
            if sequence.starts_with("\x1b[") && sequence.ends_with('m') {
                if matches!(sequence, "\x1b[m" | "\x1b[0m") {
                    sgr.clear();
                } else {
                    sgr.push_str(sequence);
                }
            } else if let Some(target) = osc8_target(sequence) {
                hyperlink = (!target.is_empty()).then(|| sequence.to_string());
            }
            index = end;
            continue;
        }

        let grapheme = text[index..]
            .graphemes(true)
            .next()
            .expect("index is before the end of text");
        let grapheme_width = grapheme.width();
        if line_width > 0 && line_width + grapheme_width > width {
            push_terminal_line(&mut lines, &mut line, hyperlink.is_some());
            line.push_str(&sgr);
            if let Some(link) = &hyperlink {
                line.push_str(link);
            }
            line_width = 0;
        }
        line.push_str(grapheme);
        line_width += grapheme_width;
        index += grapheme.len();
    }

    lines.push(line);
    lines
}

fn push_terminal_line(lines: &mut Vec<String>, line: &mut String, close_hyperlink: bool) {
    if close_hyperlink {
        line.push_str("\x1b]8;;\x07");
    }
    lines.push(std::mem::take(line));
}

pub(crate) fn ansi_sequence_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    match bytes.get(index) {
        Some(b'[') => {
            index += 1;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        }
        Some(b']' | b'_') => {
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\x07' {
                    index += 1;
                    break;
                }
                if bytes[index] == b'\x1b' && bytes.get(index + 1) == Some(&b'\\') {
                    index += 2;
                    break;
                }
                index += 1;
            }
        }
        Some(_) => index += 1,
        None => {}
    }
    index
}

pub(crate) fn osc8_target(sequence: &str) -> Option<&str> {
    let payload = sequence
        .strip_prefix("\x1b]8;;")?
        .strip_suffix('\x07')
        .or_else(|| sequence.strip_prefix("\x1b]8;;")?.strip_suffix("\x1b\\"))?;
    Some(payload)
}

/// Truncate to `width` columns with an ellipsis when cut.
pub fn truncate_to_width(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    let plain;
    let text = if text.as_bytes().contains(&b'\x1b') {
        plain = strip_ansi(text);
        plain.as_str()
    } else {
        text
    };
    let mut out = String::new();
    let mut w = 0usize;
    for grapheme in text.graphemes(true) {
        let gw = grapheme.width();
        if w + gw + 1 > width {
            break;
        }
        out.push_str(grapheme);
        w += gw;
    }
    out.push('…');
    out
}

/// Pad or truncate to exactly `width` columns.
pub fn fit_to_width(text: &str, width: usize) -> String {
    let w = display_width(text);
    if w > width {
        truncate_to_width(&strip_ansi(text), width)
    } else {
        format!("{text}{}", " ".repeat(width - w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_stripped_from_width() {
        assert_eq!(display_width("\x1b[31mred\x1b[0m"), 3);
        assert_eq!(
            display_width("x\x1b]8;;https://example.com\x07link\x1b]8;;\x07"),
            5
        );
        assert_eq!(display_width("ab\x1b_pi:c\x07cd"), 4);
        assert_eq!(display_width("plain"), 5);
    }

    #[test]
    fn wrap_words_and_cjk() {
        assert_eq!(wrap_text("hello world foo", 11), vec!["hello world", "foo"]);
        // CJK chars are width 2.
        let wrapped = wrap_text("日本語のテキスト", 6);
        assert!(wrapped.iter().all(|l| l.width() <= 6));
    }

    #[test]
    fn word_wrap_preserves_styles_and_reopens_links() {
        let link = "\x1b]8;;https://example.com\x1b\\\x1b[4mhello world\x1b[24m\x1b]8;;\x1b\\";
        let wrapped = wrap_text(link, 5);
        assert_eq!(wrapped.len(), 2, "{wrapped:?}");
        assert!(wrapped.iter().all(|line| display_width(line) == 5));
        assert!(
            wrapped
                .iter()
                .all(|line| line.contains("\x1b]8;;https://example.com\x1b\\"))
        );
        assert!(wrapped.iter().all(|line| line.ends_with("\x1b]8;;\x1b\\")));
    }

    #[test]
    fn hard_break_long_word() {
        let wrapped = wrap_text("abcdefghij", 4);
        assert_eq!(wrapped, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn terminal_wrap_preserves_styles_spaces_links_and_cursor() {
        assert_eq!(
            wrap_terminal_text("\x1b[31mabcdef\x1b[0m", 3),
            ["\x1b[31mabc", "\x1b[31mdef\x1b[0m"]
        );
        assert_eq!(wrap_terminal_text("  abc  ", 4), ["  ab", "c  "]);
        assert_eq!(
            wrap_terminal_text("\x1b]8;;https://x\x07abcdef\x1b]8;;\x07", 3),
            [
                "\x1b]8;;https://x\x07abc\x1b]8;;\x07",
                "\x1b]8;;https://x\x07def\x1b]8;;\x07"
            ]
        );
        assert_eq!(
            wrap_terminal_text(&format!("abcd{CURSOR_MARKER}ef"), 4),
            ["abcd", &format!("{CURSOR_MARKER}ef")]
        );
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello world", 6), "hello…");
        assert_eq!(truncate_to_width("\x1b[31mhello world\x1b[0m", 6), "hello…");
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_ansi_width() {
        let lines = (0..2_000)
            .map(|index| {
                format!(
                    "\x1b[38;2;120;200;255mrow {index:04}\x1b[0m: source/path/module_{index:04}.rs 日本語"
                )
            })
            .collect::<Vec<_>>();
        kiss_bench::measure("ansi_width_2000", 15, 20, "2000_styled_lines", || {
            lines.iter().map(|line| display_width(line)).sum::<usize>()
        });
    }
}
