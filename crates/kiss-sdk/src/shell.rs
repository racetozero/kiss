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
    // Why the full output could not be saved. The tail still comes back.
    let mut spill_error: Option<std::io::Error> = None;
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
                kiss_agent::tools::shell::kill_group(&mut child, None);
                break;
            }
        };
        if spill_file.is_none()
            && spill_error.is_none()
            && collected.len() + bytes.len() > MAX_INLINE_BYTES
        {
            match spill(&collected) {
                Ok(file) => spill_file = Some(file),
                Err(error) => spill_error = Some(error),
            }
        }
        if let Some(Err(error)) = spill_file.as_mut().map(|(file, _)| file.write_all(bytes)) {
            if let Some((_, path)) = spill_file.take() {
                let _ = std::fs::remove_file(path);
            }
            spill_error = Some(error);
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
    if let Some(Err(error)) = spill_file.as_mut().map(|(file, _)| file.flush()) {
        if let Some((_, path)) = spill_file.take() {
            let _ = std::fs::remove_file(path);
        }
        spill_error = Some(error);
    }
    let tail = || {
        let start = text.ceil_char_boundary(text.len().saturating_sub(MAX_INLINE_BYTES));
        text[start..].to_string()
    };
    let (output, truncated, full_output_path) = match (spill_file, spill_error) {
        (Some((_, path)), _) => {
            let path = path.display().to_string();
            let notice = format!("\n\n[output truncated. Full output: {path}]");
            (format!("{}{notice}", tail()), true, Some(path))
        }
        (None, Some(error)) => {
            let notice = format!(
                "\n\n[output truncated. Could not save the full output: {error}. Free disk space or make the temporary directory {} writable.]",
                std::env::temp_dir().display()
            );
            (format!("{}{notice}", tail()), true, None)
        }
        (None, None) => (text, false, None),
    };

    Ok(crate::session::BashResult {
        output,
        exit_code,
        cancelled,
        truncated,
        full_output_path,
    })
}

fn spill(bytes: &[u8]) -> std::io::Result<(BufWriter<std::fs::File>, std::path::PathBuf)> {
    let directory = std::env::temp_dir().join("kiss-sdk-bash");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}.log", uuid::Uuid::new_v4()));
    let mut file = BufWriter::new(std::fs::File::create_new(&path)?);
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
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

    #[tokio::test]
    async fn unsaved_full_output_keeps_the_tail() {
        // The temporary directory is read once per process, so a child test
        // process runs the command with a file in its place.
        if std::env::var_os("KISS_TEST_UNUSABLE_TMPDIR").is_none() {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("not-a-directory");
            std::fs::write(&file, "").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "shell::tests::unsaved_full_output_keeps_the_tail",
                ])
                .env("KISS_TEST_UNUSABLE_TMPDIR", "1")
                .env("TMPDIR", file)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{output:?}"
            );
            return;
        }
        let result = run(
            "printf '%070000d' 0",
            Path::new("/"),
            Some("bash"),
            None,
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        assert!(result.truncated);
        assert!(result.full_output_path.is_none());
        assert!(result.output.starts_with("0000"));
        assert!(
            result
                .output
                .contains("[output truncated. Could not save the full output: "),
            "{}",
            &result.output[result.output.len() - 300..]
        );
    }
}
