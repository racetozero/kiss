//! Read tool: text files with offset/limit + head truncation, images as
//! attachments.

use crate::tool::{AgentTool, ToolResult, ToolUpdateSink};
use crate::tools::path::resolve;
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_head};
use base64::Engine;
use kiss_ai::ContentBlock;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio_util::sync::CancellationToken;

pub struct ReadTool {
    pub cwd: PathBuf,
}

pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[async_trait::async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> String {
        format!(
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
                "offset": {"type": "number", "description": "Line number to start reading from (1-indexed)"},
                "limit": {"type": "number", "description": "Maximum number of lines to read"},
            },
            "required": ["path"],
        })
    }

    async fn execute(
        &self,
        _id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"].as_str().unwrap_or_default();
        let offset = args["offset"].as_f64().map(|v| v as usize);
        let limit = args["limit"].as_f64().map(|v| v as usize);
        let absolute = resolve(&self.cwd, path);
        let start = offset.map(|value| value.saturating_sub(1)).unwrap_or(0);

        let mut file = tokio::fs::File::open(&absolute)
            .await
            .map_err(|e| anyhow::anyhow!("Could not read file: {path}. {e}"))?;
        let mut header = [0u8; 16];
        let header_len = file
            .read(&mut header)
            .await
            .map_err(|e| anyhow::anyhow!("Could not read file: {path}. {e}"))?;
        if header_len == 0 {
            if start > 0 {
                anyhow::bail!("Offset {} is beyond end of file (0 lines total)", start + 1);
            }
            return Ok(ToolResult::text(""));
        }

        if let Some(mime) = detect_image_mime(&header[..header_len]) {
            let mut bytes = Vec::with_capacity(
                file.metadata()
                    .await
                    .ok()
                    .map(|metadata| metadata.len() as usize)
                    .unwrap_or(header_len),
            );
            bytes.extend_from_slice(&header[..header_len]);
            file.read_to_end(&mut bytes).await?;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(ToolResult {
                content: vec![
                    ContentBlock::text(format!("Read image file [{mime}]")),
                    ContentBlock::Image {
                        data,
                        mime_type: mime.to_string(),
                    },
                ],
                ..Default::default()
            });
        }
        file.seek(std::io::SeekFrom::Start(0)).await?;

        let start_display = start + 1;
        let requested_lines = limit.unwrap_or(DEFAULT_MAX_LINES + 1);
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut line_number = 0usize;
        let mut selected_lines = Vec::with_capacity(requested_lines.min(DEFAULT_MAX_LINES + 1));
        let mut selected_bytes = 0usize;
        let mut has_more = false;
        let mut ended_with_newline = false;
        let mut reached_eof = false;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).await? == 0 {
                reached_eof = true;
                break;
            }
            if cancel.is_cancelled() {
                anyhow::bail!("Read cancelled");
            }
            line_number += 1;
            if line_number <= start {
                continue;
            }
            if selected_lines.len() >= requested_lines
                || selected_lines.len() > DEFAULT_MAX_LINES
                || selected_bytes > DEFAULT_MAX_BYTES
            {
                has_more = true;
                break;
            }
            ended_with_newline = line.last() == Some(&b'\n');
            if ended_with_newline {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
            selected_bytes = selected_bytes.saturating_add(line.len() + 1);
            selected_lines.push(String::from_utf8_lossy(&line).into_owned());
        }
        if reached_eof && ended_with_newline {
            line_number += 1;
            if line_number > start {
                if selected_lines.len() >= requested_lines
                    || selected_lines.len() > DEFAULT_MAX_LINES
                    || selected_bytes > DEFAULT_MAX_BYTES
                {
                    has_more = true;
                } else {
                    selected_lines.push(String::new());
                }
            }
        }
        if selected_lines.is_empty() && line_number <= start {
            anyhow::bail!(
                "Offset {} is beyond end of file ({} lines total)",
                offset.unwrap_or(1),
                line_number
            );
        }
        let first_line_len = selected_lines.first().map_or(0, String::len);
        let selected = selected_lines.join("\n");
        let user_limited = limit.map(|_| selected_lines.len());

        let truncation = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let (output, details) = if truncation.first_line_exceeds_limit {
            let size = format_size(first_line_len);
            (
                format!(
                    "[Line {start_display} is {size}, exceeds {} limit. Use bash: sed -n '{start_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
                    format_size(DEFAULT_MAX_BYTES)
                ),
                json!({"truncation": truncation}),
            )
        } else if truncation.truncated {
            let end_display = start_display + truncation.output_lines - 1;
            let next = end_display + 1;
            let notice = if truncation.truncated_by.as_deref() == Some("lines") {
                format!(
                    "[Showing lines {start_display}-{end_display}. Use offset={next} to continue.]"
                )
            } else {
                format!(
                    "[Showing lines {start_display}-{end_display} ({} limit). Use offset={next} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                )
            };
            (
                format!("{}\n\n{notice}", truncation.content),
                json!({"truncation": truncation}),
            )
        } else if let Some(limited) = user_limited {
            if has_more {
                let next = start + limited + 1;
                (
                    format!(
                        "{}\n\n[More lines remain. Use offset={next} to continue.]",
                        truncation.content
                    ),
                    Value::Null,
                )
            } else {
                (truncation.content.clone(), Value::Null)
            }
        } else {
            (truncation.content.clone(), Value::Null)
        };

        Ok(ToolResult {
            content: vec![ContentBlock::text(output)],
            details,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(dir: &std::path::Path) -> ReadTool {
        ReadTool {
            cwd: dir.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn reads_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "l1\nl2\nl3\nl4\nl5").unwrap();
        let t = tool(dir.path());
        let r = t
            .execute(
                "1",
                json!({"path": "f.txt", "offset": 2, "limit": 2}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let text = r.output_text();
        assert!(text.starts_with("l2\nl3"));
        assert!(text.contains("Use offset=4 to continue"));
    }

    #[tokio::test]
    async fn offset_beyond_eof_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "one").unwrap();
        let t = tool(dir.path());
        let err = t
            .execute(
                "1",
                json!({"path": "f.txt", "offset": 10}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("beyond end of file"));
    }

    #[tokio::test]
    async fn text_read_is_lossy_for_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("invalid.txt"), [b'a', 0xff, b'b', b'\n']).unwrap();
        let result = tool(dir.path())
            .execute(
                "1",
                json!({"path": "invalid.txt"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.output_text(), "a�b\n");
    }

    #[tokio::test]
    async fn png_detected_as_image() {
        let dir = tempfile::tempdir().unwrap();
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        std::fs::write(dir.path().join("i.png"), png).unwrap();
        let t = tool(dir.path());
        let r = t
            .execute(
                "1",
                json!({"path": "i.png"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(&r.content[1], ContentBlock::Image { mime_type, .. } if mime_type == "image/png")
        );
    }

    #[tokio::test]
    #[ignore = "release-mode performance benchmark"]
    async fn benchmark_performance_bounded_text_read() {
        let dir = tempfile::tempdir().unwrap();
        let line = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
        std::fs::write(dir.path().join("large.txt"), line.repeat(200_000)).unwrap();
        let tool = tool(dir.path());
        let args = json!({"path": "large.txt", "offset": 190_000, "limit": 50});
        let head_args = json!({"path": "large.txt", "offset": 1, "limit": 50});

        for _ in 0..2 {
            tool.execute("warmup", args.clone(), CancellationToken::new(), None)
                .await
                .unwrap();
        }
        let mut samples = Vec::with_capacity(11);
        for _ in 0..11 {
            let started = std::time::Instant::now();
            let result = tool
                .execute("bench", args.clone(), CancellationToken::new(), None)
                .await
                .unwrap();
            std::hint::black_box(result.output_text().len());
            samples.push(started.elapsed().as_nanos());
        }
        kiss_bench::report(
            "read_offset_13mb",
            &mut samples,
            1,
            "13mb_file_offset_190000_limit_50",
        );

        let mut head_samples = Vec::with_capacity(11);
        for _ in 0..11 {
            let started = std::time::Instant::now();
            let result = tool
                .execute("bench", head_args.clone(), CancellationToken::new(), None)
                .await
                .unwrap();
            std::hint::black_box(result.output_text().len());
            head_samples.push(started.elapsed().as_nanos());
        }
        kiss_bench::report(
            "read_head_13mb",
            &mut head_samples,
            1,
            "13mb_file_offset_1_limit_50",
        );
    }
}
