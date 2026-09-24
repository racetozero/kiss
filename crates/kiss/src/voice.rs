//! Local microphone capture and transcription. No audio leaves this machine.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use tempfile::TempDir;

pub struct Recording {
    child: Option<Child>,
    dir: TempDir,
    model: PathBuf,
    whisper: PathBuf,
}

pub fn check_setup() -> Result<PathBuf> {
    let model = std::env::var_os("KISS_VOICE_MODEL")
        .map(PathBuf::from)
        .context("set KISS_VOICE_MODEL to a local whisper.cpp GGML model before using /voice")?;
    if !model.is_file() {
        bail!(
            "voice model not found: {} (set KISS_VOICE_MODEL to a whisper.cpp GGML model)",
            model.display()
        );
    }
    if Command::new("whisper-cli")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        bail!("whisper-cli not found in PATH; install whisper.cpp and set KISS_VOICE_MODEL");
    }
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        bail!("ffmpeg not found in PATH; install ffmpeg for microphone capture");
    }
    Ok(model)
}

impl Recording {
    pub fn start() -> Result<Self> {
        Self::start_with("ffmpeg", "whisper-cli", check_setup()?)
    }

    fn start_with(ffmpeg: &str, whisper: &str, model: PathBuf) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let (format, default_input) = microphone();
        let input = std::env::var("KISS_VOICE_INPUT").unwrap_or_else(|_| default_input.into());
        let child = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                format,
                "-i",
                &input,
                "-ac",
                "1",
                "-ar",
                "16000",
            ])
            .arg(dir.path().join("speech.wav"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("ffmpeg not found; install ffmpeg for microphone capture")?;
        Ok(Self {
            child: Some(child),
            dir,
            model,
            whisper: whisper.into(),
        })
    }

    pub fn finish(mut self, language: &str) -> Result<String> {
        // ffmpeg accepts q on stdin, closes the WAV header and exits cleanly.
        let mut child = self.child.take().unwrap();
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"q\n");
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "microphone capture failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let wav = self.dir.path().join("speech.wav");
        if !wav.is_file() || wav.metadata()?.len() <= 44 {
            bail!("no audio recorded; check microphone permissions and KISS_VOICE_INPUT");
        }
        let output = Command::new(&self.whisper)
            .arg("-m")
            .arg(&self.model)
            .arg("-f")
            .arg(&wav)
            .args(["-l", language, "-nt", "-otxt", "-of"])
            .arg(self.dir.path().join("transcript"))
            .output()
            .context("could not start whisper-cli")?;
        if !output.status.success() {
            bail!(
                "transcription failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(
            std::fs::read_to_string(self.dir.path().join("transcript.txt"))?
                .trim()
                .to_owned(),
        )
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn microphone() -> (&'static str, &'static str) {
    #[cfg(target_os = "macos")]
    {
        ("avfoundation", ":0")
    }
    #[cfg(target_os = "linux")]
    {
        ("pulse", "default")
    }
    #[cfg(target_os = "windows")]
    {
        ("dshow", "audio=default")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        ("pulse", "default")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &TempDir, name: &str, body: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path.to_str().unwrap().into()
    }

    #[test]
    fn records_transcribes_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg = script(
            &dir,
            "ffmpeg",
            "for last; do :; done\nread stop\n[ \"$stop\" = q ] || exit 1\nprintf '%050d' 1 > \"$last\"",
        );
        let whisper = script(
            &dir,
            "whisper",
            "for arg; do prev=$last; last=$arg; done\n[ \"$prev\" = -of ] || exit 1\nprintf 'hello world\\n' > \"$last.txt\"",
        );
        let model = dir.path().join("model.bin");
        std::fs::write(&model, b"fake").unwrap();
        let recording = Recording::start_with(&ffmpeg, &whisper, model).unwrap();
        let wav_dir = recording.dir.path().to_owned();
        assert_eq!(recording.finish("en").unwrap(), "hello world");
        assert!(!wav_dir.exists());
    }

    #[test]
    fn capture_error_is_reported_without_transcribing() {
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg = script(&dir, "ffmpeg", "echo 'device unavailable' >&2\nexit 1");
        let recording =
            Recording::start_with(&ffmpeg, "/nonexistent/whisper", dir.path().join("model"))
                .unwrap();
        assert!(
            recording
                .finish("en")
                .unwrap_err()
                .to_string()
                .contains("device unavailable")
        );
    }
}
