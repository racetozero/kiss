//! N-API binding for Node.js, Bun, and Deno.
//!
//! JavaScript engines parse JSON in highly optimized native code, so payloads
//! cross this boundary as strings and are parsed exactly once by the small
//! TypeScript wrapper. This avoids one N-API call per field on every streaming
//! token. Every operation still reaches the same `kiss_sdk::Session::execute`
//! dispatcher used by Rust, Python, and RPC mode.

use kiss_sdk::{Command, SessionOptions, SessionSource};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

fn napi_error(error: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, error.to_string())
}

/// One native session. The TypeScript `Session` class wraps this low-level API.
#[napi]
pub struct NativeSession {
    inner: Arc<kiss_sdk::Session>,
}

#[napi]
impl NativeSession {
    /// Start a session from a JSON-encoded options object.
    #[napi(factory)]
    pub async fn create(options_json: String) -> Result<Self> {
        let value: Value = serde_json::from_str(&options_json).map_err(napi_error)?;
        let options = options_from_json(value)?;
        let inner = kiss_sdk::Session::create(options)
            .await
            .map_err(napi_error)?;
        Ok(NativeSession { inner })
    }

    /// Run one protocol command and return its JSON-encoded response.
    #[napi]
    pub async fn execute_json(&self, command_json: String) -> Result<String> {
        let command: Command = serde_json::from_str(&command_json).map_err(napi_error)?;
        let response = self.inner.execute(command).await;
        serde_json::to_string(&response).map_err(napi_error)
    }

    /// Send a prompt and wait for the entire agent run.
    #[napi]
    pub async fn prompt(&self, message: String) -> Result<()> {
        self.inner.prompt(message).await.map_err(napi_error)
    }

    /// Send a prompt and return once accepted; events continue in the background.
    #[napi]
    pub fn prompt_detached(&self, message: String) -> Result<()> {
        self.inner
            .prompt_detached(kiss_sdk::PromptArgs::new(message))
            .map_err(napi_error)
    }

    /// Create an independent event subscription.
    #[napi]
    pub fn events(&self) -> NativeEventStream {
        NativeEventStream {
            inner: Arc::new(tokio::sync::Mutex::new(self.inner.events())),
        }
    }

    #[napi]
    pub fn abort(&self) {
        self.inner.abort();
    }

    #[napi]
    pub async fn wait_idle(&self) {
        self.inner.wait_idle().await;
    }

    #[napi]
    pub fn close(&self) {
        self.inner.close();
    }
}

/// Low-level event receiver. TypeScript wraps `nextJson()` as an AsyncIterator.
#[napi]
pub struct NativeEventStream {
    inner: Arc<tokio::sync::Mutex<kiss_sdk::EventStream>>,
}

#[napi]
impl NativeEventStream {
    /// The next JSON event, or null once the session closes.
    #[napi]
    pub async fn next_json(&self) -> Option<String> {
        let mut events = self.inner.lock().await;
        events.recv().await.map(|event| event.to_line())
    }
}

/// The shared scripted provider, exposed so every language binding's
/// end-to-end test exercises byte-identical HTTP/SSE behavior.
#[cfg(feature = "mock")]
#[napi]
pub struct MockProvider {
    inner: Option<kiss_sdk::mock::MockProvider>,
}

#[cfg(feature = "mock")]
#[napi]
impl MockProvider {
    #[napi(factory)]
    pub async fn start(directory: String, script_json: String) -> Result<Self> {
        let value: Value = serde_json::from_str(&script_json).map_err(napi_error)?;
        let script = mock_script_from_json(value)?;
        let provider = kiss_sdk::mock::MockProvider::start(directory, script)
            .await
            .map_err(napi_error)?;
        Ok(MockProvider {
            inner: Some(provider),
        })
    }

    #[napi(getter)]
    pub fn catalog_path(&self) -> Result<String> {
        self.inner
            .as_ref()
            .map(|provider| provider.catalog_path().display().to_string())
            .ok_or_else(|| napi_error("the mock provider was already stopped"))
    }

    #[napi]
    pub fn requests_json(&self) -> String {
        let requests = self
            .inner
            .as_ref()
            .map(|provider| provider.requests())
            .unwrap_or_default();
        Value::Array(requests).to_string()
    }

    #[napi]
    pub fn stop(&mut self) {
        self.inner = None;
    }
}

fn options_from_json(value: Value) -> Result<SessionOptions> {
    let object = value
        .as_object()
        .ok_or_else(|| napi_error("session options must be an object"))?;
    let string = |name: &str| object.get(name).and_then(Value::as_str).map(str::to_string);
    let strings = |name: &str| -> Result<Option<Vec<String>>> {
        object
            .get(name)
            .filter(|value| !value.is_null())
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|error| napi_error(format!("invalid {name}: {error}")))
            })
            .transpose()
    };

    let session = match string("session").as_deref() {
        None | Some("in-memory") => SessionSource::InMemory,
        Some("create") => SessionSource::Create,
        Some("continue") => SessionSource::ContinueRecent,
        Some(path) if path.starts_with("open:") => {
            SessionSource::Open(PathBuf::from(&path["open:".len()..]))
        }
        Some(path) if path.starts_with("fork:") => {
            SessionSource::Fork(PathBuf::from(&path["fork:".len()..]))
        }
        Some(other) => return Err(napi_error(format!("invalid session source {other:?}"))),
    };

    let thinking_level = string("thinkingLevel")
        .map(|level| {
            kiss_sdk::ThinkingLevel::parse(&level)
                .ok_or_else(|| napi_error(format!("unknown thinking level {level:?}")))
        })
        .transpose()?;

    Ok(SessionOptions {
        cwd: string("cwd")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        model: string("model"),
        provider: string("provider"),
        api_key: string("apiKey"),
        models_file: string("modelsFile").map(PathBuf::from),
        thinking_level,
        tools: strings("tools")?,
        exclude_tools: strings("excludeTools")?.unwrap_or_default(),
        no_tools: object
            .get("noTools")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        system_prompt: string("systemPrompt"),
        append_system_prompt: string("appendSystemPrompt"),
        session,
        session_dir: string("sessionDir").map(PathBuf::from),
        session_name: string("sessionName"),
        trust_project_files: object
            .get("trustProjectFiles")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        no_context_files: object
            .get("noContextFiles")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        event_capacity: object
            .get("eventCapacity")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(1024),
        ..Default::default()
    })
}

#[cfg(feature = "mock")]
fn mock_script_from_json(value: Value) -> Result<kiss_sdk::mock::MockScript> {
    use kiss_sdk::mock::{MockScript, MockTurn};
    let turns = value
        .as_array()
        .ok_or_else(|| napi_error("the script must be an array of turns"))?;
    let mut script = MockScript::default();
    for turn in turns {
        let pieces = turn
            .as_array()
            .ok_or_else(|| napi_error("each turn must be an array"))?;
        let mut built = Vec::new();
        for piece in pieces {
            if let Some(text) = piece.get("text").and_then(Value::as_str) {
                built.push(MockTurn::Text(text.to_string()));
            } else if let Some(call) = piece.get("toolCall") {
                built.push(MockTurn::ToolCall {
                    id: call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_1")
                        .to_string(),
                    name: call
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| napi_error("a toolCall needs a name"))?
                        .to_string(),
                    arguments: call.get("arguments").cloned().unwrap_or_else(|| json!({})),
                });
            } else {
                return Err(napi_error("each piece needs text or toolCall"));
            }
        }
        script.turns.push(built);
    }
    Ok(script)
}
