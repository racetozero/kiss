//! One microphone stream (16 kHz mono PCM16) feeding local or opt-in cloud STT.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdout};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Local,
    Deepgram,
    Elevenlabs,
}

impl Backend {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            "deepgram" => Some(Self::Deepgram),
            "elevenlabs" => Some(Self::Elevenlabs),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Deepgram => "deepgram",
            Self::Elevenlabs => "elevenlabs",
        }
    }

    fn key(self) -> Option<&'static str> {
        match self {
            Self::Local => None,
            Self::Deepgram => Some("DEEPGRAM_API_KEY"),
            Self::Elevenlabs => Some("ELEVENLABS_API_KEY"),
        }
    }
}

pub enum Event {
    Preview(String),
    Finished(Result<String, String>),
}

enum Control {
    Stop,
    Cancel,
}

pub struct Recording {
    control: Option<mpsc::UnboundedSender<Control>>,
}

impl Recording {
    pub fn stop(&self) {
        if let Some(tx) = &self.control {
            let _ = tx.send(Control::Stop);
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(tx) = &self.control {
            let _ = tx.send(Control::Cancel);
        }
    }
}

pub fn check_setup(backend: Backend) -> Result<()> {
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        bail!("ffmpeg not found in PATH; install ffmpeg for microphone capture");
    }
    if let Some(key) = backend.key() {
        if std::env::var(key).map_or(true, |value| value.trim().is_empty()) {
            bail!(
                "set {key} before selecting {} (microphone audio will be sent to this provider)",
                backend.name()
            );
        }
    } else {
        let model = model_path()?;
        if !model.is_file() {
            bail!("voice model not found: {}", model.display());
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
    }
    Ok(())
}

pub fn start(
    backend: Backend,
    language: String,
) -> Result<(Recording, mpsc::UnboundedReceiver<Event>)> {
    check_setup(backend)?;
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let result = run(backend, &language, control_rx, &event_tx).await;
        let _ = event_tx.send(Event::Finished(
            result.map_err(|error| format!("{error:#}")),
        ));
    });
    Ok((
        Recording {
            control: Some(control_tx),
        },
        event_rx,
    ))
}

async fn run(
    backend: Backend,
    language: &str,
    mut control: mpsc::UnboundedReceiver<Control>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<String> {
    let mut socket = if backend == Backend::Local {
        None
    } else {
        Some(connect(backend, language).await?)
    };
    let (mut child, mut audio) = microphone()?;
    let mut pcm = Vec::new();
    let mut pending = Vec::new();
    let mut captured_bytes = 0usize;
    let mut finalized = String::new();
    let mut provisional = String::new();
    let mut buffer = [0u8; 3200];
    loop {
        tokio::select! {
            biased;
            command = control.recv() => match command {
                Some(Control::Cancel) | None => return Ok(String::new()),
                Some(Control::Stop) => break,
            },
            read = audio.read(&mut buffer) => {
                let n = read.context("reading microphone audio")?;
                if n == 0 { bail!("microphone capture ended: {} (check microphone permissions and KISS_VOICE_INPUT)", child.wait().await?); }
                captured_bytes += n;
                if captured_bytes > 16000 * 2 * 120 { bail!("voice recordings are limited to two minutes"); }
                if let Some(socket) = &mut socket {
                    pending.extend_from_slice(&buffer[..n]);
                    while pending.len() >= 3200 {
                        socket.send(audio_message(backend, &pending[..3200])).await.context("sending microphone audio")?;
                        pending.drain(..3200);
                    }
                } else {
                    pcm.extend_from_slice(&buffer[..n]);
                }
            },
            message = async { socket.as_mut().unwrap().next().await }, if socket.is_some() => {
                let message = message.context("speech service disconnected")??;
                process_message(backend, &message, &mut finalized, &mut provisional, events)?;
            }
        }
    }
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"q\n").await;
        }
        // Drain buffered audio (including the last fragment) before finalizing the provider.
        let drain = async {
            loop {
                let n = audio.read(&mut buffer).await?;
                if n == 0 {
                    break;
                }
                if let Some(socket) = &mut socket {
                    pending.extend_from_slice(&buffer[..n]);
                    while pending.len() >= 3200 {
                        socket
                            .send(audio_message(backend, &pending[..3200]))
                            .await?;
                        pending.drain(..3200);
                    }
                } else {
                    pcm.extend_from_slice(&buffer[..n]);
                }
            }
            Result::<()>::Ok(())
        };
        tokio::time::timeout(Duration::from_secs(3), drain)
            .await
            .context("microphone did not stop")??;
        let status = child.wait().await?;
        if !status.success() {
            bail!("microphone capture failed ({status}); check permissions and KISS_VOICE_INPUT");
        }
    }
    if let Some(mut socket) = socket {
        if !pending.is_empty() {
            socket.send(audio_message(backend, &pending)).await?;
        }
        socket.send(finish_message(backend)).await?;
        // Cloud services may send several final segments after the stop signal.
        let finish = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                // After a final arrives, allow a short grace period for another segment.
                let wait = if finalized.is_empty() {
                    remaining
                } else {
                    remaining.min(Duration::from_millis(800))
                };
                let message = match tokio::time::timeout(wait, socket.next()).await {
                    Ok(Some(message)) => message?,
                    Ok(None) | Err(_) => break,
                };
                process_message(backend, &message, &mut finalized, &mut provisional, events)?;
                if message.is_close() {
                    break;
                }
            }
            Result::<()>::Ok(())
        };
        // A quiet provider need not keep the microphone or editor waiting indefinitely.
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(3), finish) => {
                if let Ok(result) = result { result?; }
            }
            command = control.recv() => {
                if matches!(command, Some(Control::Cancel) | None) { return Ok(String::new()); }
            }
        }
        let _ = socket.close(None).await;
        if finalized.is_empty() && !provisional.is_empty() {
            bail!("speech service did not finalize the transcript");
        }
        Ok(finalized)
    } else {
        if pcm.len() <= 320 {
            bail!("no audio recorded; check microphone permissions and KISS_VOICE_INPUT");
        }
        let language = language.to_owned();
        tokio::task::spawn_blocking(move || transcribe_local(&pcm, &language)).await?
    }
}

fn process_message(
    backend: Backend,
    message: &Message,
    finalized: &mut String,
    provisional: &mut String,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let Message::Text(text) = message else {
        return Ok(());
    };
    let value: Value = serde_json::from_str(text)?;
    let kind = value
        .get("type")
        .or_else(|| value.get("message_type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let (text, final_segment) = match backend {
        Backend::Deepgram if kind == "Results" => (
            value
                .pointer("/channel/alternatives/0/transcript")
                .and_then(Value::as_str)
                .unwrap_or(""),
            value["is_final"].as_bool().unwrap_or(false),
        ),
        Backend::Elevenlabs if kind == "committed_transcript" || kind == "partial_transcript" => (
            value["text"].as_str().unwrap_or(""),
            kind == "committed_transcript",
        ),
        _ => {
            if kind.ends_with("error") || kind == "Error" || kind == "error" {
                bail!(
                    "speech service: {}",
                    value
                        .get("error")
                        .or_else(|| value.get("message"))
                        .unwrap_or(&value)
                );
            }
            return Ok(());
        }
    };
    if final_segment {
        if !text.trim().is_empty() {
            if !finalized.is_empty() {
                finalized.push(' ');
            }
            finalized.push_str(text.trim());
        }
        provisional.clear();
    } else {
        *provisional = text.into();
    }
    let preview = if provisional.is_empty() {
        finalized.clone()
    } else if finalized.is_empty() {
        provisional.clone()
    } else {
        format!("{finalized} {provisional}")
    };
    let _ = events.send(Event::Preview(preview));
    Ok(())
}

fn audio_message(backend: Backend, pcm: &[u8]) -> Message {
    match backend {
        Backend::Deepgram => Message::Binary(pcm.to_vec().into()),
        Backend::Elevenlabs => Message::Text(json!({"message_type":"input_audio_chunk", "audio_base_64":base64::engine::general_purpose::STANDARD.encode(pcm)}).to_string().into()),
        Backend::Local => unreachable!(),
    }
}

fn finish_message(backend: Backend) -> Message {
    match backend {
        Backend::Deepgram => Message::Text(json!({"type":"Finalize"}).to_string().into()),
        Backend::Elevenlabs => Message::Text(
            json!({"message_type":"input_audio_chunk", "audio_base_64":"", "commit":true})
                .to_string()
                .into(),
        ),
        Backend::Local => unreachable!(),
    }
}

async fn connect(
    backend: Backend,
    language: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let url = match backend {
        Backend::Deepgram => {
            let mut url = url::Url::parse("wss://api.deepgram.com/v1/listen")?;
            url.query_pairs_mut()
                .append_pair("model", "nova-3")
                .append_pair("encoding", "linear16")
                .append_pair("sample_rate", "16000")
                .append_pair("interim_results", "true")
                .append_pair("smart_format", "true");
            url.query_pairs_mut().append_pair(
                "language",
                if language == "auto" {
                    "multi"
                } else {
                    language
                },
            );
            url.to_string()
        }
        Backend::Elevenlabs => {
            let mut url = url::Url::parse("wss://api.elevenlabs.io/v1/speech-to-text/realtime")?;
            url.query_pairs_mut()
                .append_pair("model_id", "scribe_v2_realtime")
                .append_pair("audio_format", "pcm_16000")
                .append_pair("commit_strategy", "manual");
            if language != "auto" {
                url.query_pairs_mut().append_pair("language_code", language);
            }
            url.to_string()
        }
        Backend::Local => unreachable!(),
    };
    let key = std::env::var(backend.key().unwrap())?;
    let mut request = url.into_client_request()?;
    let (header, value) = match backend {
        Backend::Deepgram => ("Authorization", format!("Token {key}")),
        Backend::Elevenlabs => ("xi-api-key", key),
        Backend::Local => unreachable!(),
    };
    request
        .headers_mut()
        .insert(header, HeaderValue::from_str(&value)?);
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .context("speech service connection timed out")??;
    Ok(socket)
}

fn microphone() -> Result<(Child, ChildStdout)> {
    let (format, default_input) = microphone_device();
    let input = std::env::var("KISS_VOICE_INPUT").unwrap_or_else(|_| default_input.into());
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            format,
            "-i",
            &input,
            "-ac",
            "1",
            "-ar",
            "16000",
            "-f",
            "s16le",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("could not start microphone capture")?;
    let stdout = child.stdout.take().unwrap();
    Ok((child, stdout))
}

fn model_path() -> Result<PathBuf> {
    std::env::var_os("KISS_VOICE_MODEL")
        .map(PathBuf::from)
        .context("set KISS_VOICE_MODEL to a local whisper.cpp GGML model")
}

fn wav_bytes(pcm: &[u8]) -> Result<Vec<u8>> {
    let size = u32::try_from(pcm.len())?;
    let mut bytes = Vec::with_capacity(pcm.len() + 44);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(size + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(&32000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(pcm);
    Ok(bytes)
}

fn transcribe_local(pcm: &[u8], language: &str) -> Result<String> {
    let dir = tempfile::tempdir()?;
    let wav = dir.path().join("speech.wav");
    std::fs::write(&wav, wav_bytes(pcm)?)?;
    let output = Command::new("whisper-cli")
        .arg("-m")
        .arg(model_path()?)
        .arg("-f")
        .arg(&wav)
        .args(["-l", language, "-nt", "-otxt", "-of"])
        .arg(dir.path().join("transcript"))
        .output()
        .context("could not start whisper-cli")?;
    if !output.status.success() {
        bail!(
            "transcription failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(std::fs::read_to_string(dir.path().join("transcript.txt"))?
        .trim()
        .into())
}

fn microphone_device() -> (&'static str, &'static str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backend_choice_and_cloud_wire_formats() {
        assert_eq!(Backend::parse("local"), Some(Backend::Local));
        assert_eq!(Backend::parse("deepgram"), Some(Backend::Deepgram));
        assert_eq!(Backend::parse("elevenlabs"), Some(Backend::Elevenlabs));
        assert_eq!(Backend::parse("other"), None);
        assert_eq!(
            audio_message(Backend::Deepgram, b"\x00\x01")
                .into_data()
                .as_ref(),
            b"\x00\x01"
        );
        let message = audio_message(Backend::Elevenlabs, b"\x00\x01").to_string();
        let value: Value = serde_json::from_str(&message).unwrap();
        assert_eq!(value["audio_base_64"], "AAE=");
        assert_eq!(value["message_type"], "input_audio_chunk");
        assert_eq!(
            serde_json::from_str::<Value>(&finish_message(Backend::Deepgram).to_string()).unwrap()
                ["type"],
            "Finalize"
        );
    }

    #[test]
    fn provider_messages_keep_only_final_text() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut final_text = String::new();
        let mut partial = String::new();
        for (backend, text) in [
            (
                Backend::Deepgram,
                r#"{"type":"Results","is_final":false,"channel":{"alternatives":[{"transcript":"hel"}]}}"#,
            ),
            (
                Backend::Deepgram,
                r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"hello"}]}}"#,
            ),
            (
                Backend::Elevenlabs,
                r#"{"message_type":"partial_transcript","text":"wor"}"#,
            ),
            (
                Backend::Elevenlabs,
                r#"{"message_type":"committed_transcript","text":"world"}"#,
            ),
        ] {
            process_message(
                backend,
                &Message::Text(text.into()),
                &mut final_text,
                &mut partial,
                &tx,
            )
            .unwrap();
        }
        assert_eq!(final_text, "hello world");
        assert_eq!(partial, "");
        assert_eq!(
            rx.try_recv().ok().and_then(|event| match event {
                Event::Preview(s) => Some(s),
                _ => None,
            }),
            Some("hel".into())
        );
        assert!(
            finish_message(Backend::Elevenlabs)
                .to_string()
                .contains("\"commit\":true")
        );
    }
    #[test]
    fn wav_has_pcm_header() {
        let mut pcm = vec![0u8; 32000];
        pcm[0] = 1;
        let wav = wav_bytes(&pcm).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[44..], pcm);
        let message = audio_message(Backend::Deepgram, &pcm);
        assert_eq!(message.into_data().len(), 32000);
    }
}
