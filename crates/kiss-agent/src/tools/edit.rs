//! Edit tool: exact-text replacement with uniqueness and overlap checks,
//! BOM and line-ending preservation, diff details.

use crate::tool::{AgentTool, ToolResult, ToolUpdateSink};
use crate::tools::mutation_queue::lock_path;
use crate::tools::path::resolve;
use kiss_ai::ContentBlock;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

pub struct EditTool {
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
struct Edit {
    old_text: String,
    new_text: String,
}

const BOM: &str = "\u{feff}";

/// LF for every line break: CRLF and lone CR become LF.
fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// A replacement found in the LF-normalized text: its byte range and new text.
struct Replacement<'a> {
    start: usize,
    end: usize,
    new_text: &'a str,
}

/// Apply replacements found in `normalize_to_lf(original)` to `original`
/// itself. Text outside the replaced ranges keeps its bytes, including its
/// line endings. Line breaks in new text get the line ending of the line
/// where the replaced range starts.
fn apply_to_original(original: &str, replacements: &[Replacement]) -> String {
    // Only CRLF changes length when normalizing. The LF of the n-th CRLF
    // (original offset p) is at p - n in the normalized text.
    let crlfs: Vec<usize> = original.match_indices("\r\n").map(|(at, _)| at).collect();
    let to_original = |offset: usize| {
        offset
            + crlfs
                .iter()
                .enumerate()
                .take_while(|(n, at)| *at - n < offset)
                .count()
    };
    // The line ending of the line containing `offset`, or of the line before
    // for an unterminated last line. A lone CR counts only in files without
    // any LF.
    let ending_at = |offset: usize| {
        let line_feed = original[offset..]
            .find('\n')
            .map(|at| offset + at)
            .or_else(|| original[..offset].rfind('\n'));
        match line_feed {
            Some(at) if at > 0 && original.as_bytes()[at - 1] == b'\r' => "\r\n",
            Some(_) => "\n",
            None if original.contains('\r') => "\r",
            None => "\n",
        }
    };
    let mut out = String::with_capacity(original.len());
    let mut push = |part: &str| {
        // A CR before a seam and an LF after it would merge into one CRLF.
        if out.ends_with('\r') && part.starts_with('\n') {
            out.push('\n');
        }
        out.push_str(part);
    };
    let mut position = 0;
    for replacement in replacements {
        let start = to_original(replacement.start);
        push(&original[position..start]);
        match ending_at(start) {
            "\n" => push(replacement.new_text),
            ending => push(&replacement.new_text.replace('\n', ending)),
        }
        position = to_original(replacement.end);
    }
    push(&original[position..]);
    out
}

fn apply_edits<'a>(
    content: &str,
    edits: &'a [Edit],
) -> anyhow::Result<(String, Vec<Replacement<'a>>)> {
    // Locate every edit against the *original* content and check overlap.
    let mut spans: Vec<(usize, usize, &Edit)> = Vec::with_capacity(edits.len());
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            anyhow::bail!("edits[{i}].oldText must not be empty");
        }
        let mut matches = content.match_indices(&edit.old_text);
        let Some((start, _)) = matches.next() else {
            anyhow::bail!(
                "edits[{i}].oldText not found in file. Make sure it matches the file content exactly, including whitespace.\nSearched for:\n{}",
                edit.old_text
            );
        };
        if matches.next().is_some() {
            anyhow::bail!(
                "edits[{i}].oldText matches multiple locations in the file. Add surrounding context to make it unique.\nSearched for:\n{}",
                edit.old_text
            );
        }
        let end = start + edit.old_text.len();
        for &(other_start, other_end, _) in &spans {
            if start < other_end && other_start < end {
                anyhow::bail!(
                    "edits[{i}] overlaps another edit. Merge overlapping changes into a single edit."
                );
            }
        }
        spans.push((start, end, edit));
    }
    spans.sort_by_key(|&(start, _, _)| start);

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    let mut replacements = Vec::with_capacity(spans.len());
    for (start, end, edit) in spans {
        out.push_str(&content[cursor..start]);
        out.push_str(&edit.new_text);
        cursor = end;
        replacements.push(Replacement {
            start,
            end,
            new_text: &edit.new_text,
        });
    }
    out.push_str(&content[cursor..]);
    Ok((out, replacements))
}

fn unified_diff(path: &str, old: &str, new: &str) -> (String, Option<usize>) {
    let diff = similar::TextDiff::from_lines(old, new);
    let first_changed = diff
        .ops()
        .iter()
        .find(|op| !matches!(op.tag(), similar::DiffTag::Equal))
        .map(|op| op.new_range().start + 1);
    let patch = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    (patch, first_changed)
}

fn parse_edits(args: &Value) -> anyhow::Result<Vec<Edit>> {
    let mut edits: Vec<Edit> = Vec::new();
    if let Some(list) = args["edits"].as_array() {
        for e in list {
            edits.push(Edit {
                old_text: e["oldText"].as_str().unwrap_or_default().to_string(),
                new_text: e["newText"].as_str().unwrap_or_default().to_string(),
            });
        }
    }
    // Legacy single-pair shape.
    if let (Some(old), Some(new)) = (args["oldText"].as_str(), args["newText"].as_str()) {
        edits.push(Edit {
            old_text: old.to_string(),
            new_text: new.to_string(),
        });
    }
    if edits.is_empty() {
        anyhow::bail!("Edit tool input is invalid. edits must contain at least one replacement.");
    }
    Ok(edits)
}

#[async_trait::async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> String {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file."},
                            "newText": {"type": "string", "description": "Replacement text for this targeted edit."},
                        },
                        "required": ["oldText", "newText"],
                    },
                },
            },
            "required": ["path", "edits"],
        })
    }

    fn prepare_arguments(&self, mut args: Value) -> Value {
        // Some models stringify the edits array. Unwrap it.
        if let Some(edits_str) = args["edits"].as_str()
            && let Ok(parsed) = serde_json::from_str::<Value>(edits_str)
            && parsed.is_array()
        {
            args["edits"] = parsed;
        }
        // Legacy top-level oldText/newText: fold into edits so the schema
        // validates.
        if args["edits"].as_array().is_none_or(|a| a.is_empty())
            && let (Some(old), Some(new)) = (args["oldText"].as_str(), args["newText"].as_str())
        {
            args["edits"] = json!([{"oldText": old, "newText": new}]);
        }
        if let Some(obj) = args.as_object_mut() {
            obj.remove("oldText");
            obj.remove("newText");
        }
        args
    }

    async fn execute(
        &self,
        _id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"].as_str().unwrap_or_default().to_string();
        let edits = parse_edits(&args)?;
        let absolute = resolve(&self.cwd, &path);
        let _guard = lock_path(&absolute).await;
        if cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }

        let raw = tokio::fs::read_to_string(&absolute)
            .await
            .map_err(|e| anyhow::anyhow!("Could not edit file: {path}. {e}"))?;
        let (bom, content) = match raw.strip_prefix(BOM) {
            Some(rest) => (BOM, rest.to_string()),
            None => ("", raw),
        };
        let normalized = normalize_to_lf(&content);
        let edits: Vec<Edit> = edits
            .into_iter()
            .map(|edit| Edit {
                old_text: normalize_to_lf(&edit.old_text),
                new_text: normalize_to_lf(&edit.new_text),
            })
            .collect();

        let (new_content, replacements) = apply_edits(&normalized, &edits)?;
        if cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }
        // Diff before writing, so the diff cannot fail after the write.
        let (patch, first_changed_line) = unified_diff(&path, &normalized, &new_content);
        // Splice into the original text, so untouched lines keep their line
        // endings. Without any CR the normalized result is already final.
        let restored = if content == normalized {
            new_content
        } else {
            apply_to_original(&content, &replacements)
        };
        tokio::fs::write(&absolute, format!("{bom}{restored}"))
            .await
            .map_err(|e| anyhow::anyhow!("Could not edit file: {path}. {e}"))?;

        Ok(ToolResult {
            content: vec![ContentBlock::text(format!(
                "Successfully replaced {} block(s) in {path}.",
                edits.len()
            ))],
            details: json!({
                "diff": patch,
                "patch": patch,
                "firstChangedLine": first_changed_line,
            }),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(dir: &std::path::Path, args: Value) -> anyhow::Result<ToolResult> {
        EditTool {
            cwd: dir.to_path_buf(),
        }
        .execute("1", args, CancellationToken::new(), None)
        .await
    }

    #[tokio::test]
    async fn applies_multiple_edits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        run(
            dir.path(),
            json!({"path": "f.txt", "edits": [
                {"oldText": "alpha", "newText": "ALPHA"},
                {"oldText": "gamma", "newText": "GAMMA"},
            ]}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "ALPHA\nbeta\nGAMMA\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_match_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "dup\ndup\n").unwrap();
        let err = run(
            dir.path(),
            json!({"path": "f.txt", "edits": [{"oldText": "dup", "newText": "x"}]}),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("multiple locations"));
    }

    #[tokio::test]
    async fn missing_match_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "content\n").unwrap();
        let err = run(
            dir.path(),
            json!({"path": "f.txt", "edits": [{"oldText": "absent", "newText": "x"}]}),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("not found"));
    }

    #[tokio::test]
    async fn overlapping_edits_fail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "one two three\n").unwrap();
        let err = run(
            dir.path(),
            json!({"path": "f.txt", "edits": [
                {"oldText": "one two", "newText": "a"},
                {"oldText": "two three", "newText": "b"},
            ]}),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("overlaps"));
    }

    #[tokio::test]
    async fn crlf_and_bom_preserved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "\u{feff}a\r\nb\r\n").unwrap();
        run(
            dir.path(),
            json!({"path": "f.txt", "edits": [{"oldText": "b", "newText": "B"}]}),
        )
        .await
        .unwrap();
        let out = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(out, "\u{feff}a\r\nB\r\n");
    }

    #[tokio::test]
    async fn untouched_lines_keep_mixed_and_lone_cr_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "a\r\nb\nc\r\nd\n").unwrap();
        run(
            dir.path(),
            json!({"path": "f.txt", "edits": [{"oldText": "c", "newText": "C1\nC2"}]}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "a\r\nb\nC1\r\nC2\r\nd\n"
        );

        std::fs::write(&file, "x\ry\rz\r").unwrap();
        run(
            dir.path(),
            json!({"path": "f.txt", "edits": [{"oldText": "y\nz", "newText": "Y\nZ"}]}),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x\rY\rZ\r");
    }

    #[tokio::test]
    async fn legacy_argument_shape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "old\n").unwrap();
        let tool = EditTool {
            cwd: dir.path().to_path_buf(),
        };
        let args =
            tool.prepare_arguments(json!({"path": "f.txt", "oldText": "old", "newText": "new"}));
        tool.execute("1", args, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "new\n"
        );
    }
}
