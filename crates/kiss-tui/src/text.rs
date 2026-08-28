//! Styled text helpers: ANSI-aware width, wrapping, truncation.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
            let word_width = word.width();
            // A separator at the end of the candidate does not occupy a
            // visible cell after the line is emitted. Count it only after a
            // later word turns it into an internal separator.
            if current_width + word.trim_end_matches(' ').width() <= width {
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
                let gw = grapheme.width();
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
    fn hard_break_long_word() {
        let wrapped = wrap_text("abcdefghij", 4);
        assert_eq!(wrapped, vec!["abcd", "efgh", "ij"]);
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
