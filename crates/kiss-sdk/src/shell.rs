//! Running a shell command *the user* asked for, as opposed to one the model
//! asked for.
//!
//! The model-facing `bash` tool (in `crates/kiss-agent/src/tools/bash.rs`)
//! reports a non-zero exit status as a tool error, because that is what makes
//! the model notice and react. A direct command from an SDK caller is different:
//! the caller wants the exit code as data, not as a failure. This module is
//! therefore a small, separate runner rather than a wrapper around that tool.

use anyhow::{Context as _, Result};
use std::io::{BufWriter, Write as _};
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

/// Bytes of output kept inline. Anything beyond this is written to a file and
/// the caller is told where, so a `find /` cannot exhaust memory in the client.
pub const MAX_INLINE_BYTES: usize = 64 * 1024;

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
    // An empty setting means the setting is not set.
    let shell_path = shell_path.filter(|value| !value.trim().is_empty());
    let command_prefix = command_prefix.filter(|value| !value.trim().is_empty());
    let mut child = kiss_agent::tools::shell::spawn(shell_path, command, command_prefix, cwd)?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut collected: Vec<u8> = Vec::new();
    let mut spill_file = None;
    let mut stdout_buffer = [0u8; 8192];
    let mut stderr_buffer = [0u8; 8192];
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut cancelled = false;

    while stdout_open || stderr_open {
        let bytes = tokio::select! {
            read = stdout.read(&mut stdout_buffer), if stdout_open => match read {
                Ok(0) => { stdout_open = false; continue; }
                Ok(n) => &stdout_buffer[..n],
                Err(error) => return Err(error).context("Could not read shell output. Run the command again with standard output connected to a readable pipe."),
            },
            read = stderr.read(&mut stderr_buffer), if stderr_open => match read {
                Ok(0) => { stderr_open = false; continue; }
                Ok(n) => &stderr_buffer[..n],
                Err(error) => return Err(error).context("Could not read shell errors. Run the command again with standard error connected to a readable pipe."),
            },
            _ = cancel.cancelled() => {
                cancelled = true;
                kiss_agent::tools::shell::kill_group(&mut child);
                break;
            }
        };
        if spill_file.is_none() && collected.len() + bytes.len() > MAX_INLINE_BYTES {
            spill_file = Some(spill(&collected)?);
        }
        if let Some((file, _)) = &mut spill_file {
            file.write_all(bytes).context(OUTPUT_FILE_ERROR)?;
        }
        collected.extend_from_slice(bytes);
        // Keep up to three preceding bytes so a split UTF-8 character can be
        // decoded before the final text tail is selected.
        let discard = collected.len().saturating_sub(MAX_INLINE_BYTES + 3);
        collected.drain(..discard);
        on_update(String::from_utf8_lossy(bytes).into_owned());
    }

    let status = child.wait().await.ok();
    let exit_code = status.and_then(|status| status.code());

    let text = String::from_utf8_lossy(&collected).into_owned();
    let (output, truncated, full_output_path) = if let Some((mut file, path)) = spill_file {
        file.flush().context(OUTPUT_FILE_ERROR)?;
        let start = text.ceil_char_boundary(text.len().saturating_sub(MAX_INLINE_BYTES));
        let tail = text[start..].to_string();
        let path = path.display().to_string();
        let notice = format!("\n\n[output truncated. Full output: {path}]");
        (format!("{tail}{notice}"), true, Some(path))
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

const OUTPUT_FILE_ERROR: &str = "The command output could not be saved. Use a writable temporary directory with free disk space. Correct the temporary directory permissions or free disk space, then run the command again.";

fn spill(bytes: &[u8]) -> Result<(BufWriter<std::fs::File>, std::path::PathBuf)> {
    let directory = std::env::temp_dir().join("kiss-sdk-bash");
    std::fs::create_dir_all(&directory).context(OUTPUT_FILE_ERROR)?;
    let path = directory.join(format!("{}.log", uuid::Uuid::new_v4()));
    let file = std::fs::File::create_new(&path).context(OUTPUT_FILE_ERROR)?;
    let mut file = BufWriter::new(file);
    file.write_all(bytes).context(OUTPUT_FILE_ERROR)?;
    Ok((file, path))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn large_shell_output_keeps_a_tail_and_saves_all_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let text = "é日🙂".repeat(32_768);
        std::fs::write(directory.path().join("output.txt"), &text).unwrap();
        let result = run(
            "cat output.txt",
            directory.path(),
            Some("bash"),
            None,
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.truncated);
        let path = result.full_output_path.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
        let start = text.ceil_char_boundary(text.len() - MAX_INLINE_BYTES);
        assert_eq!(
            result.output,
            format!(
                "{}\n\n[output truncated. Full output: {path}]",
                &text[start..]
            )
        );
        std::fs::remove_file(path).unwrap();
    }
}
