//! Shared truncation for tool outputs. Two independent limits — whichever is
//! hit first wins: 2000 lines or 50 KiB. Never splits a line except in the
//! bash tail edge case where a single line exceeds the byte limit.

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    /// "lines" | "bytes" when truncated.
    pub truncated_by: Option<String>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
}

/// Keep the head of `content` (read tool).
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();

    if let Some(first) = lines.first()
        && first.len() > max_bytes
    {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some("bytes".into()),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
        };
    }

    let mut out_bytes = 0usize;
    let mut out_lines = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let add = line.len() + usize::from(i > 0);
        if out_lines + 1 > max_lines || out_bytes + add > max_bytes {
            break;
        }
        out_bytes += add;
        out_lines += 1;
    }

    if out_lines == total_lines {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: out_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
        };
    }

    let truncated_by = if out_lines >= max_lines {
        "lines"
    } else {
        "bytes"
    };
    let content_out = lines[..out_lines].join("\n");
    TruncationResult {
        output_bytes: content_out.len(),
        content: content_out,
        truncated: true,
        truncated_by: Some(truncated_by.into()),
        total_lines,
        total_bytes,
        output_lines: out_lines,
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Keep the tail of `content` (bash tool). If the final line alone exceeds
/// the byte budget, keep its last `max_bytes` bytes (partial line).
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();

    if let Some(last) = lines.last()
        && last.len() > max_bytes
    {
        let boundary = ceil_char_boundary(last, last.len() - max_bytes);
        let piece = &last[boundary..];
        return TruncationResult {
            content: piece.to_string(),
            truncated: true,
            truncated_by: Some("bytes".into()),
            total_lines,
            total_bytes,
            output_lines: 1,
            output_bytes: piece.len(),
            last_line_partial: true,
            first_line_exceeds_limit: false,
        };
    }

    let mut out_bytes = 0usize;
    let mut out_lines = 0usize;
    for (i, line) in lines.iter().rev().enumerate() {
        let add = line.len() + usize::from(i > 0);
        if out_lines + 1 > max_lines || out_bytes + add > max_bytes {
            break;
        }
        out_bytes += add;
        out_lines += 1;
    }

    if out_lines == total_lines {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: out_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
        };
    }

    let truncated_by = if out_lines >= max_lines {
        "lines"
    } else {
        "bytes"
    };
    let content_out = lines[total_lines - out_lines..].join("\n");
    TruncationResult {
        output_bytes: content_out.len(),
        content: content_out,
        truncated: true,
        truncated_by: Some(truncated_by.into()),
        total_lines,
        total_bytes,
        output_lines: out_lines,
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Cap a single line's characters (grep matches).
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let cut: String = line.chars().take(max_chars).collect();
    (format!("{cut}…"), true)
}

pub fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn ceil_char_boundary(s: &str, mut index: usize) -> usize {
    while index < s.len() && !s.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_no_truncation() {
        let r = truncate_head("a\nb\nc", 10, 100);
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\nc");
        assert_eq!(r.total_lines, 3);
    }

    #[test]
    fn head_line_limit() {
        let content = (0..10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_head(&content, 3, 1000);
        assert!(r.truncated);
        assert_eq!(r.truncated_by.as_deref(), Some("lines"));
        assert_eq!(r.content, "0\n1\n2");
        assert_eq!(r.output_lines, 3);
    }

    #[test]
    fn head_byte_limit_never_splits_line() {
        let r = truncate_head("aaaa\nbbbb\ncccc", 10, 9);
        assert!(r.truncated);
        assert_eq!(r.truncated_by.as_deref(), Some("bytes"));
        assert_eq!(r.content, "aaaa\nbbbb");
    }

    #[test]
    fn head_first_line_too_big() {
        let big = "x".repeat(100);
        let r = truncate_head(&big, 10, 50);
        assert!(r.first_line_exceeds_limit);
        assert_eq!(r.content, "");
    }

    #[test]
    fn tail_keeps_end() {
        let content = (0..10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_tail(&content, 3, 1000);
        assert_eq!(r.content, "7\n8\n9");
        assert_eq!(r.truncated_by.as_deref(), Some("lines"));
    }

    #[test]
    fn tail_partial_last_line() {
        let content = format!("start\n{}", "y".repeat(100));
        let r = truncate_tail(&content, 10, 40);
        assert!(r.last_line_partial);
        assert_eq!(r.content.len(), 40);
        assert!(r.content.chars().all(|c| c == 'y'));
    }

    #[test]
    fn tail_utf8_boundary() {
        let content = "é".repeat(100); // 2 bytes each
        let r = truncate_tail(&content, 10, 33);
        assert!(r.content.len() <= 33);
        assert!(std::str::from_utf8(r.content.as_bytes()).is_ok());
    }
}
