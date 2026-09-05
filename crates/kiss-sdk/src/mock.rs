//! A scripted, in-process language-model server for tests and demos.
//!
//! # Why this exists
//!
//! Proving that an SDK works means proving that a prompt really travels through
//! HTTP, that a streamed reply is really parsed, that a tool really runs, and
//! that events really arrive. Doing that against a real provider needs a paid
//! API key and a network, which no test suite should require.
//!
//! `MockProvider` binds a TCP socket on `127.0.0.1`, speaks just enough
//! HTTP/1.1 to answer the OpenAI "chat completions" request that
//! `crates/kiss-ai/src/api/openai_completions.rs` sends, and streams back a
//! scripted reply as server-sent events. It also writes a `models.json` catalog
//! naming itself, so a session started with `models_file` pointing at that file
//! and `model("mock/mock-1")` will talk to it and nothing else.
//!
//! Every surface — Rust, RPC, Python, TypeScript — uses this same server, so
//! their end-to-end tests are comparing themselves against identical provider
//! behavior.

use serde_json::{Value, json};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One piece of a scripted assistant reply.
#[derive(Debug, Clone)]
pub enum MockTurn {
    /// Emit text, one delta per call.
    Text(String),
    /// Ask the agent to run a tool.
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// What the mock provider replies with, one entry per model call.
///
/// The first request the agent makes consumes `turns[0]`, the second consumes
/// `turns[1]`, and so on. Once the script runs out, the last entry repeats, so
/// a one-entry script is a provider that always says the same thing.
#[derive(Debug, Clone, Default)]
pub struct MockScript {
    pub turns: Vec<Vec<MockTurn>>,
}

impl MockScript {
    /// A provider that answers every request with the same text.
    pub fn text(text: impl Into<String>) -> Self {
        MockScript {
            turns: vec![vec![MockTurn::Text(text.into())]],
        }
    }

    /// Append one model call's worth of output.
    pub fn then(mut self, turn: Vec<MockTurn>) -> Self {
        self.turns.push(turn);
        self
    }
}

/// A running mock provider. Dropping it stops the listener.
pub struct MockProvider {
    port: u16,
    directory: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl MockProvider {
    /// Start the server and write its catalog into `directory`.
    ///
    /// `directory` must already exist; a test normally passes a `TempDir` path.
    pub async fn start(directory: impl AsRef<Path>, script: MockScript) -> io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);

        let call_count = Arc::new(Mutex::new(0usize));
        let accept_requests = requests.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    _ = shutdown_rx.changed() => break,
                };
                let Ok((stream, _)) = accepted else { break };
                let script = script.clone();
                let requests = accept_requests.clone();
                let call_count = call_count.clone();
                tokio::spawn(async move {
                    let index = {
                        let mut count = call_count.lock().unwrap();
                        let index = *count;
                        *count += 1;
                        index
                    };
                    let _ = serve_one(stream, script, index, requests).await;
                });
            }
        });

        let provider = MockProvider {
            port,
            directory,
            requests,
            shutdown,
        };
        provider.write_catalog()?;
        Ok(provider)
    }

    /// The port the server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The base URL a model entry should use.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// Path of the generated `models.json`, for `SessionOptions::models_file`.
    pub fn catalog_path(&self) -> PathBuf {
        self.directory.join("models.json")
    }

    /// Every request body the server received, in order.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn write_catalog(&self) -> io::Result<()> {
        let catalog = json!({
            "providers": {
                "mock": {
                    "name": "Mock",
                    "baseUrl": self.base_url(),
                    "api": "openai-completions",
                    // Declaring a key here is what lets credential resolution
                    // succeed without any file in the user's home directory.
                    "apiKey": "mock-key",
                    "compat": {
                        "supportsUsageInStreaming": false,
                        "supportsDeveloperRole": false,
                        "supportsReasoningEffort": false
                    },
                    "models": [
                        {
                            "id": "mock-1",
                            "name": "Mock 1",
                            "contextWindow": 128000,
                            "maxTokens": 4096
                        }
                    ]
                }
            }
        });
        std::fs::write(
            self.catalog_path(),
            serde_json::to_string_pretty(&catalog).expect("catalog serializes"),
        )
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// Read one HTTP request and stream one scripted answer back.
async fn serve_one(
    mut stream: TcpStream,
    script: MockScript,
    index: usize,
    requests: Arc<Mutex<Vec<Value>>>,
) -> io::Result<()> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 8192];
    // Read until the headers are complete, then until Content-Length is met.
    let (header_end, content_length) = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&buffer[..read]);
        if let Some(position) = find_subslice(&raw, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&raw[..position]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (position + 4, length);
        }
    };
    while raw.len() < header_end + content_length {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
    }
    if let Ok(body) = serde_json::from_slice::<Value>(&raw[header_end..]) {
        requests.lock().unwrap().push(body);
    }

    let turn = script
        .turns
        .get(index)
        .or_else(|| script.turns.last())
        .cloned()
        .unwrap_or_default();

    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\n\
              Content-Type: text/event-stream\r\n\
              Cache-Control: no-cache\r\n\
              Connection: close\r\n\r\n",
        )
        .await?;

    let mut has_tool_call = false;
    let mut content_index = 0usize;
    for piece in &turn {
        match piece {
            MockTurn::Text(text) => {
                // Split into a few deltas so streaming is actually exercised.
                for chunk in split_into_chunks(text, 4) {
                    let frame = json!({
                        "id": "mock",
                        "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"content": chunk}}],
                    });
                    write_event(&mut stream, &frame).await?;
                }
            }
            MockTurn::ToolCall {
                id,
                name,
                arguments,
            } => {
                has_tool_call = true;
                let arguments = arguments.to_string();
                let frame = json!({
                    "id": "mock",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": content_index,
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }]}}],
                });
                write_event(&mut stream, &frame).await?;
                content_index += 1;
            }
        }
    }

    let finish = if has_tool_call { "tool_calls" } else { "stop" };
    let final_frame = json!({
        "id": "mock",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
    });
    write_event(&mut stream, &final_frame).await?;
    stream.write_all(b"data: [DONE]\n\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn write_event(stream: &mut TcpStream, value: &Value) -> io::Result<()> {
    stream
        .write_all(format!("data: {value}\n\n").as_bytes())
        .await?;
    stream.flush().await
}

/// Split text into at most `count` chunks on character boundaries.
fn split_into_chunks(text: &str, count: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let characters: Vec<char> = text.chars().collect();
    let size = characters.len().div_ceil(count.max(1));
    characters
        .chunks(size)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_split_into_several_deltas() {
        let chunks = split_into_chunks("abcdefgh", 4);
        assert_eq!(chunks, ["ab", "cd", "ef", "gh"]);
        assert_eq!(chunks.concat(), "abcdefgh");
    }

    #[test]
    fn short_text_still_produces_one_chunk() {
        assert_eq!(split_into_chunks("a", 4), ["a"]);
        assert_eq!(split_into_chunks("", 4), [""]);
    }
}
