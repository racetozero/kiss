//! Bash tool: run a shell command, capture merged stdout/stderr, stream
//! partial output (throttled), tail-truncate with the full output spilled to
//! a temp file, honor timeout and cancellation via process-group kill.

use crate::tool::{AgentTool, ToolResult, ToolUpdateSink};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_tail};
use kiss_ai::ContentBlock;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const UPDATE_THROTTLE: Duration = Duration::from_millis(100);

pub struct BashTool {
    pub cwd: PathBuf,
    /// Shell binary; defaults to $SHELL-agnostic "bash".
    pub shell_path: Option<String>,
    /// Prefix prepended to every command (settings shellCommandPrefix).
    pub command_prefix: Option<String>,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        BashTool {
            cwd,
            shell_path: None,
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
        format!(
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Bash command to execute"},
                "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"},
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
        let full_command = match &self.command_prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.clone(),
        };
        let shell = self
            .shell_path
            .clone()
            .unwrap_or_else(|| "bash".to_string());

        let mut cmd = Command::new(&shell);
        cmd.arg("-c")
            .arg(&full_command)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn {shell}: {e}"))?;
        #[cfg(unix)]
        let pgid = child.id().map(|pid| pid as i32);

        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut output: Vec<u8> = Vec::new();
        let mut stdout_buf = [0u8; 8192];
        let mut stderr_buf = [0u8; 8192];
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut last_update = std::time::Instant::now() - UPDATE_THROTTLE;
        let mut cancelled = false;
        let mut timed_out = false;

        let deadline = timeout.map(|t| tokio::time::Instant::now() + Duration::from_secs_f64(t));
        let kill_child = |pgid_opt: Option<i32>, child: &mut tokio::process::Child| {
            #[cfg(unix)]
            if let Some(pgid) = pgid_opt {
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
        };

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
                        Ok(n) => output.extend_from_slice(&stdout_buf[..n]),
                        Err(_) => stdout_open = false,
                    }
                }
                n = stderr.read(&mut stderr_buf), if stderr_open => {
                    match n {
                        Ok(0) => stderr_open = false,
                        Ok(n) => output.extend_from_slice(&stderr_buf[..n]),
                        Err(_) => stderr_open = false,
                    }
                }
                _ = cancel.cancelled() => {
                    cancelled = true;
                    kill_child(pgid, &mut child);
                    break;
                }
                _ = timeout_sleep => {
                    timed_out = true;
                    kill_child(pgid, &mut child);
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
                let text = String::from_utf8_lossy(&output);
                let t = truncate_tail(&text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
                update(ToolResult::text(t.content));
            }
        }

        // Drain remaining output unless we killed the process.
        if !cancelled && !timed_out {
            if stdout_open {
                let _ = stdout.read_to_end(&mut output).await;
            }
            if stderr_open {
                let _ = stderr.read_to_end(&mut output).await;
            }
        }
        let status = child.wait().await.ok();
        let exit_code = status.and_then(|s| s.code());

        let text = String::from_utf8_lossy(&output).into_owned();
        let truncation = truncate_tail(&text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut output_text = truncation.content.clone();
        let mut details = Value::Null;

        if truncation.truncated {
            let full_path = spill_full_output(&output);
            let start = truncation.total_lines - truncation.output_lines + 1;
            let end = truncation.total_lines;
            let notice = if truncation.last_line_partial {
                format!(
                    "[Showing last {} of line {end} (line is {}). Full output: {full_path}]",
                    format_size(truncation.output_bytes),
                    format_size(text.split('\n').next_back().map(str::len).unwrap_or(0)),
                )
            } else if truncation.truncated_by.as_deref() == Some("lines") {
                format!(
                    "[Showing lines {start}-{end} of {}. Full output: {full_path}]",
                    truncation.total_lines
                )
            } else {
                format!(
                    "[Showing lines {start}-{end} of {} ({} limit). Full output: {full_path}]",
                    truncation.total_lines,
                    format_size(DEFAULT_MAX_BYTES)
                )
            };
            output_text = format!("{output_text}\n\n{notice}");
            details = json!({"truncation": truncation, "fullOutputPath": full_path});
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
            anyhow::bail!(
                "{}",
                with_status(&format!(
                    "Command timed out after {} seconds",
                    timeout.unwrap_or(0.0)
                ))
            );
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

fn spill_full_output(bytes: &[u8]) -> String {
    let dir = std::env::temp_dir().join("kiss-bash");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("output-{}.txt", kiss_ai::now_ms()));
    let _ = std::fs::write(&path, bytes);
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> BashTool {
        BashTool::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn captures_stdout_and_stderr() {
        let r = tool()
            .execute(
                "1",
                json!({"command": "echo out; echo err 1>&2"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let text = r.output_text();
        assert!(text.contains("out"));
        assert!(text.contains("err"));
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
