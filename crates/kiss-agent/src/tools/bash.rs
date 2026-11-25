//! Bash tool: run a shell command, capture merged stdout/stderr, stream
//! partial output (throttled), tail-truncate with the full output spilled to
//! a temp file, honor timeout and cancellation via process-group kill.

use crate::tool::{AgentTool, ToolResult, ToolUpdateSink};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_tail};
use kiss_ai::ContentBlock;
use serde_json::{Value, json};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const UPDATE_THROTTLE: Duration = Duration::from_millis(100);

/// Keep only the live tail. Once output is large, preserve its raw bytes on disk.
#[derive(Default)]
struct BashOutput {
    tail: String,
    partial_first_line: bool,
    pending_utf8: Vec<u8>,
    prefix: Vec<u8>,
    spill: Option<(BufWriter<std::fs::File>, PathBuf)>,
    /// Why the full output could not be saved. Once set, no file is written.
    spill_error: Option<String>,
    total_bytes: usize,
    newlines: usize,
    last_line_bytes: usize,
    /// Whether the last line has a character that is not whitespace.
    last_line_text: bool,
    /// Size of the last finished line with such a character.
    text_line_bytes: usize,
}

impl BashOutput {
    fn append(&mut self, bytes: &[u8]) {
        if let Some((file, _)) = &mut self.spill {
            if let Err(error) = file.write_all(bytes) {
                self.fail_spill(error);
            }
        } else if self.spill_error.is_none() {
            self.prefix.extend_from_slice(bytes);
        }
        let mut pending = std::mem::take(&mut self.pending_utf8);
        pending.extend_from_slice(bytes);
        let mut offset = 0;
        while offset < pending.len() {
            match std::str::from_utf8(&pending[offset..]) {
                Ok(text) => {
                    self.record(text);
                    offset = pending.len();
                }
                Err(error) => {
                    let valid_end = offset + error.valid_up_to();
                    self.record(std::str::from_utf8(&pending[offset..valid_end]).unwrap());
                    offset = valid_end;
                    match error.error_len() {
                        Some(len) => {
                            self.record("\u{fffd}");
                            offset += len;
                        }
                        None => break,
                    }
                }
            }
        }
        pending.drain(..offset);
        self.pending_utf8 = pending;
        if self.spill.is_none()
            && self.spill_error.is_none()
            && (self.total_bytes + usize::from(!self.pending_utf8.is_empty()) * 3
                > DEFAULT_MAX_BYTES
                || self.newlines >= DEFAULT_MAX_LINES)
        {
            let prefix = std::mem::take(&mut self.prefix);
            let opened = tempfile::Builder::new()
                .prefix("kiss-bash-")
                .suffix(".txt")
                .tempfile()
                .and_then(|file| file.keep().map_err(|error| error.error));
            match opened {
                Ok((file, path)) => {
                    let mut file = BufWriter::with_capacity(64 * 1024, file);
                    let written = file.write_all(&prefix);
                    self.spill = Some((file, path));
                    if let Err(error) = written {
                        self.fail_spill(error);
                    }
                }
                Err(error) => self.fail_spill(error),
            }
        }
    }

    /// Stop saving the full output and delete the incomplete file. The
    /// truncated output stays, so a full disk does not fail the command.
    fn fail_spill(&mut self, error: std::io::Error) {
        if let Some((file, path)) = self.spill.take() {
            drop(file);
            let _ = std::fs::remove_file(path);
        }
        self.spill_error = Some(format!(
            "{error}. Free disk space or make the temporary directory {} writable",
            std::env::temp_dir().display()
        ));
    }

    fn record(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.newlines += text.bytes().filter(|&byte| byte == b'\n').count();
        let mut lines = text.split('\n');
        let first = lines.next().unwrap_or_default();
        self.last_line_bytes += first.len();
        self.last_line_text |= !first.trim().is_empty();
        for line in lines {
            if self.last_line_text {
                self.text_line_bytes = self.last_line_bytes;
            }
            self.last_line_bytes = line.len();
            self.last_line_text = !line.trim().is_empty();
        }
        self.tail.push_str(text);
        if self.tail.len() > DEFAULT_MAX_BYTES + 1 {
            let cut = self
                .tail
                .ceil_char_boundary(self.tail.len() - DEFAULT_MAX_BYTES - 1);
            self.partial_first_line = self.tail.as_bytes()[cut - 1] != b'\n';
            self.tail.drain(..cut);
        }
    }

    fn truncation(&self) -> crate::tools::truncate::TruncationResult {
        // The cut first line shows only when nothing but blank lines follow
        // it, as `truncate_tail` does with the full output.
        let (text, keep_partial) = match self.tail.split_once('\n') {
            Some((_, rest)) if self.partial_first_line && !rest.trim().is_empty() => (rest, false),
            _ => (self.tail.as_str(), self.partial_first_line),
        };
        let mut result = truncate_tail(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        result.total_lines = self.newlines + 1;
        result.total_bytes = self.total_bytes;
        result.last_line_partial = keep_partial;
        result.truncated =
            result.output_lines < result.total_lines || result.output_bytes < result.total_bytes;
        if result.truncated {
            result.truncated_by = Some(super::truncate::TruncatedBy::after(
                result.output_lines,
                DEFAULT_MAX_LINES,
            ));
        }
        result
    }

    fn finish(&mut self) {
        if !self.pending_utf8.is_empty() {
            self.record("\u{fffd}");
            self.pending_utf8.clear();
        }
        if let Some(Err(error)) = self.spill.as_mut().map(|(file, _)| file.flush()) {
            self.fail_spill(error);
        }
    }
}

pub struct BashTool {
    pub cwd: PathBuf,
    /// Shell binary, selected when the tool is constructed unless overridden.
    pub shell_path: Option<String>,
    /// Prefix prepended to every command (settings shellCommandPrefix).
    pub command_prefix: Option<String>,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        BashTool {
            cwd,
            shell_path: Some(super::shell::resolve(None).to_string_lossy().into_owned()),
            command_prefix: None,
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> String {
        let shell = super::shell::resolve(self.shell_path.as_deref());
        let name = shell
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let syntax = match name.as_str() {
            "pwsh" => "Use PowerShell 7 syntax.",
            "powershell" => {
                "Use Windows PowerShell 5.1 syntax. Do not use && or ||; use ; for unconditional commands and if ($?) for conditional commands."
            }
            "cmd" => "Use cmd.exe syntax. Do not use POSIX shell or PowerShell syntax.",
            _ => "Use POSIX shell syntax.",
        };
        format!(
            "Execute shell commands using {} in the current working directory. {syntax} Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
            shell.display(),
            DEFAULT_MAX_BYTES / 1024
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Command to execute in the shell specified in this tool's description"},
                "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"},
                "cwd": {"type": "string", "description": "Working directory (optional, defaults to the session directory)"},
            },
            "required": ["command"],
        })
    }

    async fn execute(
        &self,
        _id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        let command = args["command"].as_str().unwrap_or_default().to_string();
        let timeout = args["timeout"].as_f64();
        if let Some(t) = timeout
            && (!t.is_finite() || t <= 0.0)
        {
            anyhow::bail!("Invalid timeout: must be a finite number of seconds");
        }
        let cwd = args["cwd"]
            .as_str()
            .map(|path| super::path::resolve(&self.cwd, path))
            .unwrap_or_else(|| self.cwd.clone());
        let mut child = super::shell::spawn(
            self.shell_path.as_deref(),
            &command,
            self.command_prefix.as_deref(),
            &cwd,
        )?;

        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut output = BashOutput::default();
        let mut stdout_buf = [0u8; 8192];
        let mut stderr_buf = [0u8; 8192];
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut last_update = std::time::Instant::now() - UPDATE_THROTTLE;
        let mut cancelled = false;
        let mut timed_out = false;

        let group = child.id();
        let started = tokio::time::Instant::now();
        let deadline = timeout.map(|t| started + Duration::from_secs_f64(t));
        // Set when the shell exits while background processes it started
        // still hold the output pipes open.
        let mut shell_exit: Option<(Option<i32>, Duration)> = None;

        loop {
            let timeout_sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                n = stdout.read(&mut stdout_buf), if stdout_open => {
                    match n {
                        Ok(0) => stdout_open = false,
                        Ok(n) => output.append(&stdout_buf[..n]),
                        Err(_) => stdout_open = false,
                    }
                }
                n = stderr.read(&mut stderr_buf), if stderr_open => {
                    match n {
                        Ok(0) => stderr_open = false,
                        Ok(n) => output.append(&stderr_buf[..n]),
                        Err(_) => stderr_open = false,
                    }
                }
                status = child.wait(), if shell_exit.is_none() => {
                    shell_exit = Some((status.ok().and_then(|s| s.code()), started.elapsed()));
                }
                _ = cancel.cancelled() => {
                    cancelled = true;
                    super::shell::kill_group(&mut child, group);
                    break;
                }
                _ = timeout_sleep => {
                    timed_out = true;
                    super::shell::kill_group(&mut child, group);
                    break;
                }
                else => break,
            }
            // The cancellation and no-deadline futures stay pending forever.
            // Therefore, `select!` does not enter its `else` branch after both
            // output pipes reach EOF. Stop explicitly when both readers close.
            if !stdout_open && !stderr_open {
                break;
            }
            if let Some(update) = &on_update
                && last_update.elapsed() >= UPDATE_THROTTLE
            {
                last_update = std::time::Instant::now();
                let t = output.truncation();
                update(ToolResult::text(t.content));
            }
        }

        let exit_code = match shell_exit {
            Some((code, _)) => code,
            None => child.wait().await.ok().and_then(|s| s.code()),
        };

        output.finish();
        let truncation = output.truncation();
        let mut output_text = truncation.content.clone();
        let mut details = Value::Null;

        if truncation.truncated {
            let (full_output, file_key, file_value) = match (&output.spill, &output.spill_error) {
                (Some((_, path)), _) => {
                    let path = path.display().to_string();
                    (format!("Full output: {path}"), "fullOutputPath", path)
                }
                (None, error) => {
                    let error = error.clone().unwrap_or_default();
                    (
                        format!("Could not save the full output: {error}"),
                        "fullOutputError",
                        error,
                    )
                }
            };
            let start = truncation.total_lines - truncation.output_lines + 1;
            let end = truncation.total_lines;
            let notice = if truncation.last_line_partial {
                format!(
                    "[Showing last {} of line {start} (line is {}). {full_output}]",
                    format_size(truncation.output_bytes),
                    // Only blank lines can follow the cut line.
                    format_size(if output.last_line_text {
                        output.last_line_bytes
                    } else {
                        output.text_line_bytes
                    }),
                )
            } else if truncation.truncated_by == Some(super::truncate::TruncatedBy::Lines) {
                format!(
                    "[Showing lines {start}-{end} of {}. {full_output}]",
                    truncation.total_lines
                )
            } else {
                format!(
                    "[Showing lines {start}-{end} of {} ({} limit). {full_output}]",
                    truncation.total_lines,
                    format_size(DEFAULT_MAX_BYTES)
                )
            };
            output_text = format!("{output_text}\n\n{notice}");
            details = json!({"truncation": truncation, file_key: file_value});
        }

        let with_status = |status: &str| {
            if output_text.is_empty() {
                status.to_string()
            } else {
                format!("{output_text}\n\n{status}")
            }
        };
        if cancelled {
            anyhow::bail!("{}", with_status("Command aborted"));
        }
        if timed_out {
            let mut status = format!("Command timed out after {} seconds", timeout.unwrap_or(0.0));
            if let Some((Some(code), after)) = shell_exit {
                // Both facts matter: the command's real result, and that its
                // background processes kept writing until the timeout.
                status.push_str(&format!(
                    ". The shell exited with code {code} after {:.1} seconds, but background processes it started kept writing output until the timeout.",
                    after.as_secs_f64()
                ));
            }
            anyhow::bail!("{}", with_status(&status));
        }
        if let Some(code) = exit_code
            && code != 0
        {
            anyhow::bail!(
                "{}",
                with_status(&format!("Command exited with code {code}"))
            );
        }
        Ok(ToolResult {
            content: vec![ContentBlock::text(if output_text.is_empty() {
                "(no output)".to_string()
            } else {
                output_text
            })],
            details,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> BashTool {
        BashTool::new(std::env::temp_dir())
    }

    #[test]
    fn bounded_output_matches_full_truncation_and_spill() {
        let cases = [
            b"small\noutput".to_vec(),
            "line\n".repeat(3000).into_bytes(),
            "x".repeat(200_000).into_bytes(),
            format!("{}\nend\n", "x".repeat(100_000)).into_bytes(),
            format!("{}\n\n", "x".repeat(100_000)).into_bytes(),
            "\u{00e9}\u{1f642}\n".repeat(20_000).into_bytes(),
            vec![0xff; 60_000],
            [vec![b'x'; 70_000], vec![0xf0, 0x9f, 0x99]].concat(),
        ];
        for bytes in cases {
            for chunk_size in [1, 8192] {
                let mut output = BashOutput::default();
                for chunk in bytes.chunks(chunk_size) {
                    output.append(chunk);
                }
                output.finish();
                let expected = truncate_tail(
                    &String::from_utf8_lossy(&bytes),
                    DEFAULT_MAX_LINES,
                    DEFAULT_MAX_BYTES,
                );
                assert_eq!(output.truncation(), expected, "chunk size {chunk_size}");
                assert!(output.tail.len() <= DEFAULT_MAX_BYTES + 1);
                if expected.truncated {
                    let (_, path) = output.spill.take().unwrap();
                    assert_eq!(std::fs::read(&path).unwrap(), bytes);
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn captures_stdout_and_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(directory.path().join("marker.txt"), "session directory").unwrap();
        std::fs::write(child.join("marker.txt"), "requested directory").unwrap();
        for cwd in [
            None,
            Some("child".to_string()),
            Some(child.to_string_lossy().into_owned()),
        ] {
            let mut args = json!({"command": "echo out; echo err 1>&2; cat marker.txt"});
            if let Some(cwd) = &cwd {
                args["cwd"] = cwd.clone().into();
            }
            let r = BashTool::new(directory.path().to_path_buf())
                .execute("1", args, CancellationToken::new(), None)
                .await
                .unwrap();
            let text = r.output_text();
            assert!(text.contains("out"));
            assert!(text.contains("err"));
            assert!(text.contains(if cwd.is_some() {
                "requested directory"
            } else {
                "session directory"
            }));
        }
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_bash_progress() {
        let bytes = "output line with deterministic text\n"
            .repeat(240_000)
            .into_bytes();
        kiss_bench::measure_pair(
            ("bash_full_progress", "bash_bounded_progress"),
            11,
            1,
            (
                "8mib_1024ish_progress_updates",
                "8mib_1024ish_progress_updates",
            ),
            || {
                let mut output = Vec::new();
                for chunk in bytes.chunks(8192) {
                    output.extend_from_slice(chunk);
                    std::hint::black_box(truncate_tail(
                        &String::from_utf8_lossy(&output),
                        DEFAULT_MAX_LINES,
                        DEFAULT_MAX_BYTES,
                    ));
                }
                truncate_tail(
                    &String::from_utf8_lossy(&output),
                    DEFAULT_MAX_LINES,
                    DEFAULT_MAX_BYTES,
                )
            },
            || {
                let mut output = BashOutput::default();
                for chunk in bytes.chunks(8192) {
                    output.append(chunk);
                    std::hint::black_box(output.truncation());
                }
                output.finish();
                let result = output.truncation();
                if let Some((file, path)) = output.spill.take() {
                    drop(file);
                    std::fs::remove_file(path).unwrap();
                }
                result
            },
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn large_command_preserves_full_output_on_disk() {
        let result = tool()
            .execute(
                "1",
                json!({"command": "printf '%070000d' 0; printf '\\nend\\n'"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let path = result.details["fullOutputPath"].as_str().unwrap();
        let full = std::fs::read(path).unwrap();
        assert_eq!(full.len(), 70_005);
        assert!(full.ends_with(b"\nend\n"));
        let expected = truncate_tail(
            &String::from_utf8_lossy(&full),
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(
            result.details["truncation"],
            serde_json::to_value(&expected).unwrap()
        );
        assert!(result.output_text().starts_with("end\n"));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn long_line_before_blank_lines_shows_its_end_and_size() {
        let result = tool()
            .execute(
                "1",
                json!({"command": "printf '%070000d' 0; printf '\\n\\n'"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let text = result.output_text();
        assert!(text.starts_with("0000"), "{text}");
        assert!(
            text.contains(&format!("of line 1 (line is {}).", format_size(70_000))),
            "{text}"
        );
        std::fs::remove_file(result.details["fullOutputPath"].as_str().unwrap()).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn unsaved_full_output_keeps_the_tail() {
        // The temporary directory is read once per process, so a child test
        // process runs the command with a missing one.
        if std::env::var_os("KISS_TEST_MISSING_TMPDIR").is_none() {
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tools::bash::tests::unsaved_full_output_keeps_the_tail",
                ])
                .env("KISS_TEST_MISSING_TMPDIR", "1")
                .env("TMPDIR", directory.path().join("missing"))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{output:?}"
            );
            return;
        }
        let result = BashTool::new(PathBuf::from("/"))
            .execute(
                "1",
                json!({"command": "printf '%070000d' 0; printf '\\nend\\n'"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let text = result.output_text();
        assert!(text.starts_with("end\n"), "{text}");
        assert!(
            text.contains(". Could not save the full output: "),
            "{text}"
        );
        assert!(result.details["fullOutputError"].is_string());
        assert!(result.details.get("fullOutputPath").is_none());
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_with_output() {
        let err = tool()
            .execute(
                "1",
                json!({"command": "echo boom; exit 3"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("boom"));
        assert!(text.contains("exited with code 3"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_after_the_shell_exited_reports_its_exit_code() {
        let marker = tempfile::tempdir().unwrap();
        let alive = marker.path().join("alive");
        let command = format!(
            "(while true; do echo tick; touch {}; sleep 0.1; done) & exit 3",
            alive.display()
        );
        let err = tool()
            .execute(
                "1",
                json!({"command": command, "timeout": 1}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("Command timed out after 1 seconds. The shell exited with code 3 after"),
            "{text}"
        );
        // The group kill still reached the background loop.
        std::thread::sleep(Duration::from_millis(300));
        std::fs::remove_file(&alive).ok();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!alive.exists());
    }

    #[tokio::test]
    async fn timeout_kills_command() {
        let start = std::time::Instant::now();
        let err = tool()
            .execute(
                "1",
                json!({"command": "sleep 5", "timeout": 1}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(format!("{err:#}").contains("timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_does_not_run_the_next_command_of_a_list() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("next-ran");
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let command = format!("sleep 1; touch '{}'", marker.display());
        assert!(
            tool()
                .execute("1", json!({"command": command}), cancel, None)
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn cancel_aborts() {
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let err = tool()
            .execute("1", json!({"command": "sleep 5"}), cancel, None)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("aborted"));
    }
}
