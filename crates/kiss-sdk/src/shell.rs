//! Running a shell command *the user* asked for, as opposed to one the model
//! asked for.
//!
//! The model-facing `bash` tool (in `crates/kiss-agent/src/tools/bash.rs`)
//! reports a non-zero exit status as a tool error, because that is what makes
//! the model notice and react. A direct command from an SDK caller is different:
//! the caller wants the exit code as data, not as a failure. This module is
//! therefore a small, separate runner rather than a wrapper around that tool.

use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

/// Bytes of output kept inline. Anything beyond this is written to a file and
/// the caller is told where, so a `find /` cannot exhaust memory in the client.
const MAX_INLINE_BYTES: usize = 64 * 1024;

/// Callback receiving each newly read chunk of combined output.
pub type UpdateSink = Arc<dyn Fn(String) + Send + Sync>;

/// Run one command and collect its combined standard output and error.
pub async fn run(
    command: &str,
    cwd: &Path,
    shell_path: Option<&str>,
    command_prefix: Option<&str>,
    cancel: CancellationToken,
    on_update: UpdateSink,
) -> Result<crate::session::BashResult> {
    let shell = shell_path.unwrap_or("bash");
    let full_command = match command_prefix {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command.to_string(),
    };

    let mut spawner = tokio::process::Command::new(shell);
    spawner
        .arg("-c")
        .arg(&full_command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = spawner
        .spawn()
        .with_context(|| format!("failed to start {shell}"))?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut collected: Vec<u8> = Vec::new();
    let mut stdout_buffer = [0u8; 8192];
    let mut stderr_buffer = [0u8; 8192];
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut cancelled = false;

    loop {
        tokio::select! {
            read = stdout.read(&mut stdout_buffer), if stdout_open => match read {
                Ok(0) | Err(_) => stdout_open = false,
                Ok(n) => {
                    collected.extend_from_slice(&stdout_buffer[..n]);
                    on_update(String::from_utf8_lossy(&stdout_buffer[..n]).into_owned());
                }
            },
            read = stderr.read(&mut stderr_buffer), if stderr_open => match read {
                Ok(0) | Err(_) => stderr_open = false,
                Ok(n) => {
                    collected.extend_from_slice(&stderr_buffer[..n]);
                    on_update(String::from_utf8_lossy(&stderr_buffer[..n]).into_owned());
                }
            },
            _ = cancel.cancelled() => {
                cancelled = true;
                let _ = child.start_kill();
                break;
            }
            else => break,
        }
        if !stdout_open && !stderr_open {
            break;
        }
    }

    let status = child.wait().await.ok();
    let exit_code = status.and_then(|status| status.code());

    let text = String::from_utf8_lossy(&collected).into_owned();
    let (output, truncated, full_output_path) = if collected.len() > MAX_INLINE_BYTES {
        let path = spill(&collected);
        let start = text.len().saturating_sub(MAX_INLINE_BYTES);
        // Cut on a character boundary so the JSON payload stays valid UTF-8.
        let start = (start..text.len())
            .find(|index| text.is_char_boundary(*index))
            .unwrap_or(text.len());
        let tail = text[start..].to_string();
        let notice = match &path {
            Some(path) => format!("\n\n[output truncated; full output: {path}]"),
            None => "\n\n[output truncated]".to_string(),
        };
        (format!("{tail}{notice}"), true, path)
    } else {
        (text, false, None)
    };

    Ok(crate::session::BashResult {
        output,
        exit_code,
        cancelled,
        truncated,
        full_output_path,
    })
}

fn spill(bytes: &[u8]) -> Option<String> {
    let directory = std::env::temp_dir().join("kiss-sdk-bash");
    std::fs::create_dir_all(&directory).ok()?;
    let path = directory.join(format!("{}.log", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).ok()?;
    Some(path.display().to_string())
}
