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
use tokio_util::sync::CancellationToken;

const UPDATE_THROTTLE: Duration = Duration::from_millis(100);

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
        let mut cmd = super::shell::command(
            self.shell_path.as_deref(),
            &command,
            self.command_prefix.as_deref(),
        );
        let shell = cmd.as_std().get_program().to_string_lossy().into_owned();
        let cwd = args["cwd"]
            .as_str()
            .map(|path| self.cwd.join(path))
            .unwrap_or_else(|| self.cwd.clone());
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Could not start shell {shell}: {e}. Install Bash or set shellPath to an installed Bash, PowerShell, or cmd.exe executable."))?;
        #[cfg(unix)]
        let pgid = child.id().map(|pid| pid as i32);
        #[cfg(not(unix))]
        let pgid = None;

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
            #[cfg(not(unix))]
            let _ = pgid_opt;
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
