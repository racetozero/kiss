//! Best-effort JSON repair for streamed tool-call arguments.
//!
//! Providers stream tool arguments as raw text; a truncated stream can leave
//! an incomplete JSON document. This parser closes unterminated strings,
//! arrays, and objects so the agent still gets a structured value (the loop
//! separately refuses to execute tool calls from length-truncated messages).

use serde_json::Value;

/// Parse `input` as JSON, repairing trailing truncation if needed.
/// Returns `Value::Object({})` for empty/blank input, `None` only when the
/// input is unsalvageable garbage.
pub fn parse_salvage(input: &str) -> Option<Value> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(Value::Object(serde_json::Map::new()));
    }
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Some(v);
    }
    let repaired = repair(trimmed)?;
    serde_json::from_str(&repaired).ok()
}

fn repair(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for c in input.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' => {
                stack.push('}');
                out.push(c);
            }
            '[' => {
                stack.push(']');
                out.push(c);
            }
            '}' | ']' => {
                if stack.last() == Some(&c) {
                    stack.pop();
                    out.push(c);
                } else {
                    return None; // mismatched close: not simple truncation
                }
            }
            _ => out.push(c),
        }
    }

    if in_string {
        if escaped {
            out.pop(); // drop dangling backslash
        }
        out.push('"');
    }

    // Drop trailing partial tokens before closing: `,`, `"key":`, or a bare
    // dangling key string left after removing a `:`.
    loop {
        let t = out.trim_end();
        if t.ends_with(',') {
            out.truncate(t.len() - 1);
        } else if t.ends_with(':') {
            let cut = t.len() - 1;
            out.truncate(cut);
            // The value never arrived; drop the key string too.
            if let Some(stripped) = strip_trailing_string(out.trim_end()) {
                out = stripped;
            }
        } else {
            break;
        }
    }

    while let Some(close) = stack.pop() {
        out.push(close);
    }
    Some(out)
}

/// If `s` ends with a complete JSON string literal, return `s` without it.
fn strip_trailing_string(s: &str) -> Option<String> {
    if !s.ends_with('"') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = bytes.len().checked_sub(2)?;
    loop {
        if bytes[i] == b'"' {
            // Count preceding backslashes to skip escaped quotes.
            let mut backslashes = 0;
            while i > backslashes && bytes[i - 1 - backslashes] == b'\\' {
                backslashes += 1;
            }
            if backslashes % 2 == 0 {
                return Some(s[..i].trim_end().to_string());
            }
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn complete_json_passes_through() {
        assert_eq!(parse_salvage(r#"{"a": 1}"#), Some(json!({"a": 1})));
    }

    #[test]
    fn empty_is_empty_object() {
        assert_eq!(parse_salvage("  "), Some(json!({})));
    }

    #[test]
    fn truncated_string_closed() {
        assert_eq!(
            parse_salvage(r#"{"path": "/tmp/fo"#),
            Some(json!({"path": "/tmp/fo"}))
        );
    }

    #[test]
    fn truncated_after_key_colon() {
        assert_eq!(parse_salvage(r#"{"path":"#), Some(json!({})));
    }

    #[test]
    fn nested_truncation() {
        assert_eq!(
            parse_salvage(r#"{"edits": [{"oldText": "a", "newText": "b"#),
            Some(json!({"edits": [{"oldText": "a", "newText": "b"}]}))
        );
    }
}
