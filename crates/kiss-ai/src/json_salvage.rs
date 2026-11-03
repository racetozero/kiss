//! Best-effort JSON repair for streamed tool-call arguments.
//!
//! Providers stream tool arguments as raw text. A truncated stream can leave
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
    let repaired = repair(&repair_strings(trimmed))?;
    serde_json::from_str(&repaired).ok()
}

/// Parse the complete arguments of a finished tool call. Unlike
/// [`parse_salvage`], this never keeps a truncated prefix: text that is not a
/// JSON object is an error that names the text the model sent.
pub fn parse_tool_arguments(input: &str) -> Result<Value, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let parsed = serde_json::from_str::<Value>(trimmed).or_else(|error| {
        let repaired = repair_strings(trimmed);
        if repaired == trimmed {
            Err(error)
        } else {
            serde_json::from_str(&repaired)
        }
    });
    match parsed {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(format!(
            "Tool arguments must be a JSON object\n\nReceived arguments:\n{input}"
        )),
        Err(error) => Err(format!("{error}\n\nReceived arguments:\n{input}")),
    }
}

/// Repair string literals: escape raw control characters and double a
/// backslash that does not start a valid escape, such as `\usepackage`.
fn repair_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    let mut in_string = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if !in_string {
            in_string = c == '"';
            out.push(c);
            continue;
        }
        match c {
            '"' => {
                in_string = false;
                out.push(c);
            }
            '\\' => match chars.peek().copied() {
                Some('u') => {
                    let digits: String = chars.clone().skip(1).take(4).collect();
                    if digits.len() == 4 && digits.chars().all(|d| d.is_ascii_hexdigit()) {
                        out.push_str("\\u");
                        out.push_str(&digits);
                        chars.nth(4);
                    } else {
                        out.push_str("\\\\");
                    }
                }
                Some(next @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')) => {
                    out.push('\\');
                    out.push(next);
                    chars.next();
                }
                _ => out.push_str("\\\\"),
            },
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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
            // The value never arrived. Drop the key string too.
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
    fn invalid_unicode_escapes_are_kept_as_text() {
        let raw = r#"{"path": "a.tex", "content": "\usepackage{x}\n\u00e9"}"#;
        assert_eq!(
            parse_tool_arguments(raw).unwrap(),
            json!({"path": "a.tex", "content": "\\usepackage{x}\né"})
        );
        assert_eq!(parse_salvage(raw), parse_tool_arguments(raw).ok());
    }

    #[test]
    fn malformed_final_arguments_are_errors_with_the_raw_text() {
        let error = parse_tool_arguments(r#"{"path": "a", "content": "#).unwrap_err();
        assert!(error.ends_with("Received arguments:\n{\"path\": \"a\", \"content\": "));
        assert!(parse_tool_arguments("[1]").is_err());
        assert_eq!(parse_tool_arguments(" ").unwrap(), json!({}));
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
