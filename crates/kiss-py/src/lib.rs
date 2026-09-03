//! Python bindings for the KISS coding agent SDK.
//!
//! Everything here is a thin adapter over `kiss_sdk::Session`. In particular
//! every operation goes through `kiss_sdk::Session::execute`, the same function
//! the Rust SDK, the TypeScript SDK, and RPC mode call, so the Python surface
//! cannot behave differently from the others.
//!
//! Asynchrony: each method returns a Python awaitable backed by a Rust future
//! running on a shared multi-threaded tokio runtime. The Python event loop is
//! never blocked, so a caller can stream events while a prompt runs.

mod convert;

use convert::{Json, json_to_py, py_to_json};
use kiss_sdk::options::{SessionOptions, SessionSource};
use kiss_sdk::protocol::Command;
use pyo3::exceptions::{PyRuntimeError, PyStopAsyncIteration, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3_async_runtimes::tokio::future_into_py;
use std::path::PathBuf;
use std::sync::Arc;

pyo3::create_exception!(_kiss, KissError, pyo3::exceptions::PyException);

fn runtime_error(error: impl std::fmt::Display) -> PyErr {
    KissError::new_err(error.to_string())
}

/// One embeddable conversation with the agent.
#[pyclass(module = "kiss_sdk._kiss")]
pub struct Session {
    inner: Arc<kiss_sdk::Session>,
}

/// An async iterator over session events.
#[pyclass(module = "kiss_sdk._kiss")]
pub struct EventStream {
    inner: Arc<tokio::sync::Mutex<kiss_sdk::EventStream>>,
}

#[pymethods]
impl EventStream {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let mut stream = inner.lock().await;
            match stream.recv().await {
                Some(event) => Ok(Json(event.0)),
                None => Err(PyStopAsyncIteration::new_err(
                    "the session event stream ended",
                )),
            }
        })
    }
}

#[pymethods]
impl Session {
    /// Start a session. Called from Python as `kiss_sdk.Session.create(...)`.
    #[staticmethod]
    #[pyo3(signature = (options))]
    fn create<'py>(py: Python<'py>, options: &Bound<'py, PyDict>) -> PyResult<Bound<'py, PyAny>> {
        let options = options_from_dict(options)?;
        future_into_py(py, async move {
            let session = kiss_sdk::Session::create(options)
                .await
                .map_err(runtime_error)?;
            Ok(Session { inner: session })
        })
    }

    /// Run one protocol command and return its response as a dictionary.
    ///
    /// This is the escape hatch: anything the typed methods do not expose can
    /// be reached by building the command dictionary yourself.
    fn execute<'py>(
        &self,
        py: Python<'py>,
        command: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let value = py_to_json(command)?;
        let command: Command = serde_json::from_value(value)
            .map_err(|error| PyValueError::new_err(format!("invalid command: {error}")))?;
        let session = self.inner.clone();
        future_into_py(py, async move {
            let response = session.execute(command).await;
            let value = serde_json::to_value(&response).map_err(runtime_error)?;
            Ok(Json(value))
        })
    }

    /// Subscribe to events. Each call returns an independent stream.
    fn events(&self) -> EventStream {
        EventStream {
            inner: Arc::new(tokio::sync::Mutex::new(self.inner.events())),
        }
    }

    /// Send a prompt and wait for the whole run to finish.
    #[pyo3(signature = (message, images = None, streaming_behavior = None))]
    fn prompt<'py>(
        &self,
        py: Python<'py>,
        message: String,
        images: Option<Bound<'py, PyAny>>,
        streaming_behavior: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let images = match images {
            Some(images) => serde_json::from_value(py_to_json(&images)?)
                .map_err(|error| PyValueError::new_err(format!("invalid images: {error}")))?,
            None => Vec::new(),
        };
        let streaming_behavior = match streaming_behavior.as_deref() {
            None => None,
            Some("steer") => Some(kiss_sdk::StreamingBehavior::Steer),
            Some("followUp") | Some("follow_up") => Some(kiss_sdk::StreamingBehavior::FollowUp),
            Some(other) => {
                return Err(PyValueError::new_err(format!(
                    "streaming_behavior must be 'steer' or 'followUp', got {other:?}"
                )));
            }
        };
        let session = self.inner.clone();
        future_into_py(py, async move {
            session
                .prompt_with(kiss_sdk::PromptArgs {
                    message,
                    images,
                    streaming_behavior,
                })
                .await
                .map_err(runtime_error)
        })
    }

    /// Send a prompt and return as soon as it is accepted or queued.
    #[pyo3(signature = (message, streaming_behavior = None))]
    fn prompt_detached(&self, message: String, streaming_behavior: Option<String>) -> PyResult<()> {
        let behavior = match streaming_behavior.as_deref() {
            None => None,
            Some("steer") => Some(kiss_sdk::StreamingBehavior::Steer),
            Some("followUp") | Some("follow_up") => Some(kiss_sdk::StreamingBehavior::FollowUp),
            Some(other) => {
                return Err(PyValueError::new_err(format!(
                    "streaming_behavior must be 'steer' or 'followUp', got {other:?}"
                )));
            }
        };
        self.inner
            .prompt_detached(kiss_sdk::PromptArgs {
                message,
                images: Vec::new(),
                streaming_behavior: behavior,
            })
            .map_err(runtime_error)
    }

    /// Queue a message for delivery after the current turn's tool calls.
    fn steer(&self, message: String) -> PyResult<()> {
        self.inner.steer(message).map_err(runtime_error)
    }

    /// Queue a message for delivery once the agent stops.
    fn follow_up(&self, message: String) -> PyResult<()> {
        self.inner.follow_up(message).map_err(runtime_error)
    }

    /// Cancel the current run and any direct shell command.
    fn abort(&self) {
        self.inner.abort();
    }

    /// Wait until no prompt run is in flight.
    fn wait_idle<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.inner.clone();
        future_into_py(py, async move {
            session.wait_idle().await;
            Ok(())
        })
    }

    /// Release the event stream and stop any running work.
    fn close(&self) {
        self.inner.close();
    }
}

/// Start the scripted offline provider used by the tests and the demo.
///
/// `directory` receives a `models.json` naming the provider; the returned
/// object keeps the server alive until it is dropped, and exposes that path.
#[cfg(feature = "mock")]
#[pyclass(module = "kiss_sdk._kiss")]
pub struct MockProvider {
    inner: Option<kiss_sdk::mock::MockProvider>,
}

#[cfg(feature = "mock")]
#[pymethods]
impl MockProvider {
    #[staticmethod]
    #[pyo3(signature = (directory, script))]
    fn start<'py>(
        py: Python<'py>,
        directory: String,
        script: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let script = mock_script_from_json(py_to_json(script)?)?;
        future_into_py(py, async move {
            let provider = kiss_sdk::mock::MockProvider::start(&directory, script)
                .await
                .map_err(runtime_error)?;
            Ok(MockProvider {
                inner: Some(provider),
            })
        })
    }

    /// Path of the generated `models.json`.
    #[getter]
    fn catalog_path(&self) -> PyResult<String> {
        self.inner
            .as_ref()
            .map(|provider| provider.catalog_path().display().to_string())
            .ok_or_else(|| PyRuntimeError::new_err("the mock provider was already stopped"))
    }

    /// Every request body the provider received, as dictionaries.
    fn requests<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let requests = self
            .inner
            .as_ref()
            .map(|provider| provider.requests())
            .unwrap_or_default();
        json_to_py(py, &serde_json::Value::Array(requests))
    }

    /// Stop the server.
    fn stop(&mut self) {
        self.inner = None;
    }
}

/// Translate the JSON script description into the Rust script type.
///
/// The description is a list of turns; each turn is a list of pieces, and each
/// piece is either `{"text": "..."}` or
/// `{"toolCall": {"id": ..., "name": ..., "arguments": {...}}}`.
#[cfg(feature = "mock")]
fn mock_script_from_json(value: serde_json::Value) -> PyResult<kiss_sdk::mock::MockScript> {
    use kiss_sdk::mock::{MockScript, MockTurn};
    let turns = value
        .as_array()
        .ok_or_else(|| PyValueError::new_err("the script must be a list of turns"))?;
    let mut script = MockScript::default();
    for turn in turns {
        let pieces = turn
            .as_array()
            .ok_or_else(|| PyValueError::new_err("each turn must be a list of pieces"))?;
        let mut built = Vec::new();
        for piece in pieces {
            if let Some(text) = piece.get("text").and_then(|value| value.as_str()) {
                built.push(MockTurn::Text(text.to_string()));
            } else if let Some(call) = piece.get("toolCall") {
                built.push(MockTurn::ToolCall {
                    id: call
                        .get("id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("call_1")
                        .to_string(),
                    name: call
                        .get("name")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| PyValueError::new_err("a toolCall needs a name"))?
                        .to_string(),
                    arguments: call
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                });
            } else {
                return Err(PyValueError::new_err(
                    "each piece must have a 'text' or 'toolCall' key",
                ));
            }
        }
        script.turns.push(built);
    }
    Ok(script)
}

/// Build session options from the keyword arguments the Python wrapper passes.
fn options_from_dict(options: &Bound<'_, PyDict>) -> PyResult<SessionOptions> {
    let mut built = SessionOptions::default();

    if let Some(value) = get_optional_string(options, "cwd")? {
        built.cwd = PathBuf::from(value);
    }
    built.model = get_optional_string(options, "model")?;
    built.provider = get_optional_string(options, "provider")?;
    built.api_key = get_optional_string(options, "api_key")?;
    built.models_file = get_optional_string(options, "models_file")?.map(PathBuf::from);
    built.system_prompt = get_optional_string(options, "system_prompt")?;
    built.append_system_prompt = get_optional_string(options, "append_system_prompt")?;
    built.session_dir = get_optional_string(options, "session_dir")?.map(PathBuf::from);
    built.session_name = get_optional_string(options, "session_name")?;

    if let Some(level) = get_optional_string(options, "thinking_level")? {
        built.thinking_level =
            Some(kiss_sdk::ThinkingLevel::parse(&level).ok_or_else(|| {
                PyValueError::new_err(format!("unknown thinking level {level:?}"))
            })?);
    }
    if let Some(value) = options.get_item("tools")?
        && !value.is_none()
    {
        built.tools = Some(value.extract()?);
    }
    if let Some(value) = options.get_item("exclude_tools")?
        && !value.is_none()
    {
        built.exclude_tools = value.extract()?;
    }
    if let Some(value) = options.get_item("no_tools")?
        && !value.is_none()
    {
        built.no_tools = value.extract()?;
    }
    if let Some(value) = options.get_item("trust_project_files")?
        && !value.is_none()
    {
        built.trust_project_files = value.extract()?;
    }
    if let Some(value) = options.get_item("no_context_files")?
        && !value.is_none()
    {
        built.no_context_files = value.extract()?;
    }
    if let Some(value) = options.get_item("event_capacity")?
        && !value.is_none()
    {
        built.event_capacity = value.extract()?;
    }

    built.session = match get_optional_string(options, "session")?.as_deref() {
        None | Some("in-memory") => SessionSource::InMemory,
        Some("create") => SessionSource::Create,
        Some("continue") => SessionSource::ContinueRecent,
        Some(path) if path.starts_with("open:") => {
            SessionSource::Open(PathBuf::from(&path["open:".len()..]))
        }
        Some(path) if path.starts_with("fork:") => {
            SessionSource::Fork(PathBuf::from(&path["fork:".len()..]))
        }
        Some(other) => {
            return Err(PyValueError::new_err(format!(
                "session must be 'in-memory', 'create', 'continue', 'open:<path>', or \
                 'fork:<path>', got {other:?}"
            )));
        }
    };

    Ok(built)
}

fn get_optional_string(options: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<String>> {
    match options.get_item(key)? {
        Some(value) if !value.is_none() => Ok(Some(value.extract()?)),
        _ => Ok(None),
    }
}

#[pymodule]
fn _kiss(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("KissError", module.py().get_type::<KissError>())?;
    module.add_class::<Session>()?;
    module.add_class::<EventStream>()?;
    #[cfg(feature = "mock")]
    module.add_class::<MockProvider>()?;
    Ok(())
}
