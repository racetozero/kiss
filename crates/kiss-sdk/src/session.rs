//! [`Session`]: one embeddable conversation with the KISS coding agent.
//!
//! The most important function here is [`Session::execute`]. It implements
//! every [`Command`] in the protocol, and every other method on `Session` is a
//! thin, typed wrapper around it. The RPC server and the Python and TypeScript
//! bindings call `execute` too, which is why the four surfaces cannot drift.

use crate::events::{agent_settled, bash_execution_update, event_lag, session_event_json};
use crate::options::SessionOptions;
use crate::protocol::{Command, Event, ImageInput, QueueMode, Response, StreamingBehavior};
use kiss_agent::{AgentMessage, BashExecutionMessage};
use kiss_ai::{ContentBlock, Model, ThinkingLevel, UserContent, UserMessage};
use kiss_coding::session_runner::{AgentSession, SessionEvent};
use kiss_coding::settings::QueueMode as SettingsQueueMode;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Anything that can go wrong in a typed SDK call.
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// The agent refused the request. The message is the same text the RPC
    /// protocol would have put in `response.error`.
    #[error("{0}")]
    Command(String),
    /// Session startup failed.
    #[error(transparent)]
    Startup(#[from] anyhow::Error),
}

/// A subscriber to the session's event stream.
///
/// Events are buffered per subscriber. If you consume them more slowly than the
/// agent produces them the oldest are dropped and you receive a single
/// `event_lag` event naming how many you missed; re-read state rather than
/// assume you saw everything.
pub struct EventStream {
    receiver: broadcast::Receiver<Event>,
}

impl EventStream {
    /// Wait for the next event. Returns `None` once the session is closed.
    pub async fn recv(&mut self) -> Option<Event> {
        match self.receiver.recv().await {
            Ok(event) => Some(event),
            // Report the gap rather than hide it; the next `recv` resumes.
            Err(broadcast::error::RecvError::Lagged(skipped)) => Some(Event(event_lag(skipped))),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    /// Take the next event if one is already buffered.
    pub fn try_recv(&mut self) -> Option<Event> {
        match self.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => Some(Event(event_lag(skipped))),
            Err(_) => None,
        }
    }
}

/// Everything a prompt can carry.
#[derive(Debug, Clone, Default)]
pub struct PromptArgs {
    pub message: String,
    pub images: Vec<ImageInput>,
    /// Required when the agent is already streaming.
    pub streaming_behavior: Option<StreamingBehavior>,
}

impl PromptArgs {
    pub fn new(message: impl Into<String>) -> Self {
        PromptArgs {
            message: message.into(),
            ..Default::default()
        }
    }
}

/// The result of a direct shell command (the `bash` command, not a model tool
/// call).
#[derive(Debug, Clone, PartialEq)]
pub struct BashResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
}

impl BashResult {
    fn to_json(&self) -> Value {
        json!({
            "output": self.output,
            "exitCode": self.exit_code,
            "cancelled": self.cancelled,
            "truncated": self.truncated,
            "fullOutputPath": self.full_output_path,
        })
    }
}

/// A convenient snapshot of the session for user interfaces.
#[derive(Debug, Clone)]
pub struct SessionState {
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub is_streaming: bool,
    pub session_file: Option<PathBuf>,
    pub session_id: String,
    pub session_name: Option<String>,
    pub message_count: usize,
    pub tools: Vec<String>,
}

/// One embeddable conversation with the agent.
pub struct Session {
    inner: Arc<AgentSession>,
    events: broadcast::Sender<Event>,
    running: Arc<AtomicBool>,
    /// Serializes prompt runs so `wait_idle` has something to wait on.
    run_lock: Arc<tokio::sync::Mutex<()>>,
    bash_cancel: Mutex<Option<CancellationToken>>,
    cwd: PathBuf,
    closed: AtomicBool,
}

/// Fluent construction of a [`Session`].
///
/// ```no_run
/// # async fn example() -> anyhow::Result<()> {
/// let session = kiss_sdk::Session::builder()
///     .cwd(".")
///     .tools(["read", "bash"])
///     .build()
///     .await?;
/// session.prompt("List the files here").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct SessionBuilder {
    options: SessionOptions,
}

impl SessionBuilder {
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.options.cwd = cwd.into();
        self
    }

    pub fn model(mut self, pattern: impl Into<String>) -> Self {
        self.options.model = Some(pattern.into());
        self
    }

    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.options.provider = Some(provider.into());
        self
    }

    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.options.api_key = Some(key.into());
        self
    }

    pub fn models_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.models_file = Some(path.into());
        self
    }

    pub fn thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.options.thinking_level = Some(level);
        self
    }

    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn exclude_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.exclude_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn no_tools(mut self, no_tools: bool) -> Self {
        self.options.no_tools = no_tools;
        self
    }

    pub fn custom_tool(mut self, tool: kiss_agent::DynTool) -> Self {
        self.options.custom_tools.push(tool);
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.options.system_prompt = Some(prompt.into());
        self
    }

    pub fn append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.options.append_system_prompt = Some(prompt.into());
        self
    }

    pub fn session(mut self, source: crate::options::SessionSource) -> Self {
        self.options.session = source;
        self
    }

    pub fn session_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.options.session_dir = Some(dir.into());
        self
    }

    pub fn session_name(mut self, name: impl Into<String>) -> Self {
        self.options.session_name = Some(name.into());
        self
    }

    pub fn trust_project_files(mut self, trust: bool) -> Self {
        self.options.trust_project_files = trust;
        self
    }

    pub fn event_capacity(mut self, capacity: usize) -> Self {
        self.options.event_capacity = capacity.max(1);
        self
    }

    pub fn stream_fn(mut self, stream_fn: kiss_agent::StreamFn) -> Self {
        self.options.stream_fn = Some(stream_fn);
        self
    }

    pub fn options(mut self, options: SessionOptions) -> Self {
        self.options = options;
        self
    }

    pub async fn build(self) -> Result<Arc<Session>, SdkError> {
        Session::create(self.options).await
    }
}

impl Session {
    pub fn builder() -> SessionBuilder {
        SessionBuilder::default()
    }

    /// Start a session. This performs disk and (for some model catalogs)
    /// network work, so it is async even though most of the body is not.
    pub async fn create(options: SessionOptions) -> Result<Arc<Session>, SdkError> {
        let (sender, _) = broadcast::channel(options.event_capacity.max(1));
        let forward = sender.clone();
        let sink: kiss_coding::SessionEventSink = Arc::new(move |event: SessionEvent| {
            if let Some(value) = session_event_json(&event) {
                // A send error only means nobody is listening.
                let _ = forward.send(Event(value));
            }
        });
        let built = options.build(sink)?;
        Ok(Arc::new(Session {
            inner: built.session,
            events: sender,
            running: Arc::new(AtomicBool::new(false)),
            run_lock: Arc::new(tokio::sync::Mutex::new(())),
            bash_cancel: Mutex::new(None),
            cwd: options.cwd.clone(),
            closed: AtomicBool::new(false),
        }))
    }

    /// Subscribe to the event stream. Each subscriber gets every event emitted
    /// after it subscribed.
    pub fn events(&self) -> EventStream {
        EventStream {
            receiver: self.events.subscribe(),
        }
    }

    /// The underlying harness session, for operations the SDK does not wrap.
    pub fn agent_session(&self) -> &Arc<AgentSession> {
        &self.inner
    }

    fn emit(&self, value: Value) {
        let _ = self.events.send(Event(value));
    }

    // ----------------------------------------------------------------
    // Typed helpers. Each one is a thin wrapper over `execute`.
    // ----------------------------------------------------------------

    /// Send a prompt and wait for the whole run, including tool calls, retry,
    /// and automatic compaction, to finish.
    pub async fn prompt(self: &Arc<Self>, message: impl Into<String>) -> Result<(), SdkError> {
        self.prompt_with(PromptArgs::new(message)).await
    }

    /// Send a prompt with images or an explicit queueing behavior, and wait for
    /// the run to finish. When the message was queued rather than started, this
    /// returns as soon as it was queued.
    pub async fn prompt_with(self: &Arc<Self>, args: PromptArgs) -> Result<(), SdkError> {
        let queued = self.accept_prompt(&args)?;
        if queued {
            return Ok(());
        }
        self.clone().run_prompt(args).await;
        Ok(())
    }

    /// Send a prompt and return as soon as it has been accepted or queued. The
    /// run continues in the background; wait for the `agent_settled` event or
    /// call [`Session::wait_idle`].
    pub fn prompt_detached(self: &Arc<Self>, args: PromptArgs) -> Result<(), SdkError> {
        let queued = self.accept_prompt(&args)?;
        if queued {
            return Ok(());
        }
        let session = self.clone();
        tokio::spawn(async move { session.clone().run_prompt(args).await });
        Ok(())
    }

    /// Decide whether a prompt starts a run or joins a queue.
    ///
    /// Returns `Ok(true)` when the message was queued and no run should start.
    fn accept_prompt(self: &Arc<Self>, args: &PromptArgs) -> Result<bool, SdkError> {
        if args.message.trim().is_empty() && args.images.is_empty() {
            return Err(SdkError::Command("prompt message is empty".into()));
        }
        let busy = self.running.load(Ordering::SeqCst) || self.inner.is_running();
        if busy {
            return match args.streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    self.inner.queue_steering(user_message(args));
                    Ok(true)
                }
                Some(StreamingBehavior::FollowUp) => {
                    self.inner.queue_follow_up(user_message(args));
                    Ok(true)
                }
                None => Err(SdkError::Command(
                    "agent is streaming: pass streamingBehavior \"steer\" or \"followUp\"".into(),
                )),
            };
        }
        self.running.store(true, Ordering::SeqCst);
        Ok(false)
    }

    async fn run_prompt(self: Arc<Self>, args: PromptArgs) {
        let guard = self.run_lock.clone().lock_owned().await;
        let message = user_message(&args);
        let mode = self.inner.prompt_mode_for(&args.message);
        self.inner.prompt_with_mode(vec![message], mode).await;
        self.running.store(false, Ordering::SeqCst);
        drop(guard);
        self.emit(agent_settled());
    }

    /// Queue a message for delivery after the current turn's tool calls.
    pub fn steer(&self, message: impl Into<String>) -> Result<(), SdkError> {
        let text = message.into();
        if text.trim().is_empty() {
            return Err(SdkError::Command("steering message is empty".into()));
        }
        self.inner.queue_steering(AgentMessage::user(text));
        Ok(())
    }

    /// Queue a message for delivery once the agent stops.
    pub fn follow_up(&self, message: impl Into<String>) -> Result<(), SdkError> {
        let text = message.into();
        if text.trim().is_empty() {
            return Err(SdkError::Command("follow-up message is empty".into()));
        }
        self.inner.queue_follow_up(AgentMessage::user(text));
        Ok(())
    }

    /// Cancel the current run and any running direct shell command.
    pub fn abort(&self) {
        self.inner.abort();
        if let Some(cancel) = self.bash_cancel.lock().unwrap().take() {
            cancel.cancel();
        }
    }

    /// Resolve once no prompt run is in flight.
    pub async fn wait_idle(&self) {
        let _guard = self.run_lock.lock().await;
        while self.inner.is_running() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// A snapshot of model, thinking level, session identity, and tools.
    pub fn state(&self) -> SessionState {
        let manager = self.inner.manager.lock().unwrap();
        SessionState {
            model: Some(self.inner.model()),
            thinking_level: self.inner.thinking_level(),
            is_streaming: self.inner.is_running() || self.running.load(Ordering::SeqCst),
            session_file: manager.session_file().map(Path::to_path_buf),
            session_id: manager.session_id().to_string(),
            session_name: manager.session_name(),
            message_count: manager.build_session_context().messages.len(),
            tools: self.inner.available_tool_names(),
        }
    }

    /// The active conversation, after compaction and branching are applied.
    pub fn messages(&self) -> Vec<AgentMessage> {
        self.inner
            .manager
            .lock()
            .unwrap()
            .build_session_context()
            .messages
    }

    /// Replace old history with a model-written summary.
    pub async fn compact(
        self: &Arc<Self>,
        custom_instructions: Option<String>,
    ) -> Result<(), SdkError> {
        self.inner.compact(custom_instructions, false).await;
        Ok(())
    }

    /// Choose a model by provider and identifier.
    pub fn set_model(&self, provider: &str, model_id: &str) -> Result<Model, SdkError> {
        let (model, _) = self
            .inner
            .registry
            .resolve(model_id, Some(provider))
            .ok_or_else(|| SdkError::Command(format!("model not found: {provider}/{model_id}")))?;
        self.inner.set_model(model.clone());
        Ok(model)
    }

    /// Choose a reasoning effort. Levels the model does not support are
    /// rejected rather than silently downgraded.
    pub fn set_thinking_level(&self, level: ThinkingLevel) -> Result<(), SdkError> {
        let model = self.inner.model();
        if !model.supported_thinking_levels().contains(&level) {
            return Err(SdkError::Command(format!(
                "model {}/{} does not support thinking level {}",
                model.provider,
                model.id,
                level.as_str()
            )));
        }
        self.inner.set_thinking_level(level);
        Ok(())
    }

    /// Run a shell command directly and record it in the conversation.
    ///
    /// The output becomes part of the context sent with the *next* prompt, the
    /// same as `!` in the terminal interface.
    pub async fn bash(self: &Arc<Self>, command: &str) -> Result<BashResult, SdkError> {
        self.bash_with_id(command, None).await
    }

    async fn bash_with_id(
        self: &Arc<Self>,
        command: &str,
        request_id: Option<String>,
    ) -> Result<BashResult, SdkError> {
        let cancel = CancellationToken::new();
        *self.bash_cancel.lock().unwrap() = Some(cancel.clone());
        let settings = self.inner.settings();
        let session = self.clone();
        let id_for_updates = request_id.clone();
        let on_update = Arc::new(move |delta: String| {
            session.emit(bash_execution_update(id_for_updates.as_deref(), &delta));
        });
        let result = crate::shell::run(
            command,
            &self.cwd,
            settings.shell_path.as_deref(),
            settings.shell_command_prefix.as_deref(),
            cancel.clone(),
            on_update,
        )
        .await
        .map_err(|error| SdkError::Command(format!("{error:#}")))?;
        *self.bash_cancel.lock().unwrap() = None;

        let message = AgentMessage::BashExecution(BashExecutionMessage {
            command: command.to_string(),
            output: result.output.clone(),
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result.full_output_path.clone(),
            exclude_from_context: false,
            timestamp: kiss_ai::now_ms(),
        });
        let _ = self.inner.manager.lock().unwrap().append_message(message);
        Ok(result)
    }

    /// Stop a running direct shell command.
    pub fn abort_bash(&self) {
        if let Some(cancel) = self.bash_cancel.lock().unwrap().take() {
            cancel.cancel();
        }
    }

    /// Release the event channel. Subscribers see their streams end.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.abort();
    }

    // ----------------------------------------------------------------
    // The dispatcher.
    // ----------------------------------------------------------------

    /// Execute one protocol command.
    ///
    /// This never panics and never returns `Err`: every outcome, including
    /// invalid arguments, is encoded in the [`Response`]. A binding that let a
    /// panic escape would abort the host process, so all fallible work is
    /// converted here.
    pub async fn execute(self: &Arc<Self>, command: Command) -> Response {
        let name = command.name();
        match self.execute_inner(command).await {
            Ok(Some(data)) => Response::ok_data(name, data),
            Ok(None) => Response::ok(name),
            Err(error) => Response::err(name, error),
        }
    }

    async fn execute_inner(
        self: &Arc<Self>,
        command: Command,
    ) -> Result<Option<Value>, SdkErrorText> {
        match command {
            Command::Prompt {
                message,
                images,
                streaming_behavior,
            } => {
                self.prompt_detached(PromptArgs {
                    message,
                    images,
                    streaming_behavior,
                })?;
                Ok(None)
            }
            Command::Steer { message, images } => {
                let args = PromptArgs {
                    message,
                    images,
                    streaming_behavior: None,
                };
                self.inner.queue_steering(user_message(&args));
                Ok(None)
            }
            Command::FollowUp { message, images } => {
                let args = PromptArgs {
                    message,
                    images,
                    streaming_behavior: None,
                };
                self.inner.queue_follow_up(user_message(&args));
                Ok(None)
            }
            Command::Abort {} => {
                self.abort();
                self.wait_idle().await;
                Ok(None)
            }
            Command::ClearQueue {} => {
                let reclaimed: Vec<String> = self
                    .inner
                    .reclaim_queued()
                    .iter()
                    .map(message_text)
                    .collect();
                // The harness returns one flat list; report it under both keys
                // so a client can restore the text without guessing.
                Ok(Some(json!({"messages": reclaimed})))
            }
            Command::NewSession {} => {
                let manager = kiss_coding::SessionManager::in_memory(&self.cwd);
                self.inner.replace_manager(manager);
                Ok(Some(json!({"cancelled": false})))
            }
            Command::GetState {} => {
                let state = self.state();
                let settings = self.inner.settings();
                Ok(Some(json!({
                    "model": state.model,
                    "thinkingLevel": state.thinking_level,
                    "isStreaming": state.is_streaming,
                    "sessionFile": state.session_file,
                    "sessionId": state.session_id,
                    "sessionName": state.session_name,
                    "messageCount": state.message_count,
                    "tools": state.tools,
                    "steeringMode": queue_mode_name(settings.steering_mode),
                    "followUpMode": queue_mode_name(settings.follow_up_mode),
                    "autoCompactionEnabled": settings.compaction.enabled,
                    "autoRetryEnabled": settings.retry.enabled,
                })))
            }
            Command::GetMessages {} => Ok(Some(json!({"messages": self.messages()}))),
            Command::GetEntries { since } => {
                let manager = self.inner.manager.lock().unwrap();
                let entries = manager.entries();
                let selected: Vec<&kiss_coding::SessionEntry> = match &since {
                    None => entries.iter().collect(),
                    Some(cursor) => {
                        let position = entries
                            .iter()
                            .position(|entry| entry.id() == cursor)
                            .ok_or_else(|| {
                                SdkErrorText(format!("no entry matches cursor '{cursor}'"))
                            })?;
                        entries[position + 1..].iter().collect()
                    }
                };
                Ok(Some(json!({
                    "entries": selected,
                    "leafId": manager.leaf_id(),
                })))
            }
            Command::GetTree {} => {
                let manager = self.inner.manager.lock().unwrap();
                Ok(Some(json!({
                    "tree": build_tree(&manager, None),
                    "leafId": manager.leaf_id(),
                })))
            }
            Command::GetLastAssistantText {} => {
                let text = self
                    .messages()
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        AgentMessage::Assistant(assistant) => {
                            let text = assistant.text();
                            (!text.is_empty()).then_some(text)
                        }
                        _ => None,
                    });
                Ok(Some(json!({"text": text})))
            }
            Command::GetSessionStats {} => Ok(Some(self.session_stats())),
            Command::SetSessionName { name } => {
                self.inner
                    .manager
                    .lock()
                    .unwrap()
                    .append_session_info(&name)
                    .map_err(|error| SdkErrorText(format!("{error:#}")))?;
                Ok(None)
            }
            Command::SetModel { provider, model_id } => {
                let model = self.set_model(&provider, &model_id)?;
                Ok(Some(serde_json::to_value(model).unwrap_or(Value::Null)))
            }
            Command::GetAvailableModels { search } => {
                let needle = search.unwrap_or_default().to_lowercase();
                let models: Vec<&Model> = self
                    .inner
                    .registry
                    .all()
                    .iter()
                    .filter(|model| {
                        needle.is_empty()
                            || format!("{}/{}", model.provider, model.id)
                                .to_lowercase()
                                .contains(&needle)
                    })
                    .collect();
                Ok(Some(json!({"models": models})))
            }
            Command::SetThinkingLevel { level } => {
                let parsed = ThinkingLevel::parse(&level)
                    .ok_or_else(|| SdkErrorText(format!("unknown thinking level '{level}'")))?;
                self.set_thinking_level(parsed)?;
                Ok(None)
            }
            Command::GetAvailableThinkingLevels {} => {
                let levels: Vec<&str> = self
                    .inner
                    .model()
                    .supported_thinking_levels()
                    .iter()
                    .map(|level| level.as_str())
                    .collect();
                Ok(Some(json!({"levels": levels})))
            }
            Command::SetSteeringMode { mode } => {
                let mut settings = self.inner.settings();
                settings.steering_mode = settings_queue_mode(mode);
                self.inner.update_settings(settings);
                Ok(None)
            }
            Command::SetFollowUpMode { mode } => {
                let mut settings = self.inner.settings();
                settings.follow_up_mode = settings_queue_mode(mode);
                self.inner.update_settings(settings);
                Ok(None)
            }
            Command::Compact {
                custom_instructions,
            } => {
                self.inner.compact(custom_instructions, false).await;
                let (used, window) = self.inner.context_usage();
                Ok(Some(json!({
                    "contextTokens": used,
                    "contextWindow": window,
                })))
            }
            Command::SetAutoCompaction { enabled } => {
                let mut settings = self.inner.settings();
                settings.compaction.enabled = enabled;
                self.inner.update_settings(settings);
                Ok(None)
            }
            Command::SetAutoRetry { enabled } => {
                let mut settings = self.inner.settings();
                settings.retry.enabled = enabled;
                self.inner.update_settings(settings);
                Ok(None)
            }
            Command::Bash { command } => {
                let result = self.bash_with_id(&command, None).await?;
                Ok(Some(result.to_json()))
            }
            Command::AbortBash {} => {
                self.abort_bash();
                Ok(None)
            }
            Command::GetTools {} => {
                let tools: Vec<Value> = self
                    .inner
                    .available_tool_names()
                    .into_iter()
                    .map(|name| json!({"name": name}))
                    .collect();
                Ok(Some(json!({"tools": tools})))
            }
            Command::ExportHtml { output_path } => Err(SdkErrorText(format!(
                "export_html is not implemented in this build{}",
                output_path
                    .map(|path| format!(" (requested {path})"))
                    .unwrap_or_default()
            ))),
            Command::SwitchSession { session_path } => {
                let manager =
                    kiss_coding::SessionManager::open(std::path::Path::new(&session_path))
                        .map_err(|error| SdkErrorText(format!("{error:#}")))?;
                self.inner.replace_manager(manager);
                Ok(Some(json!({"cancelled": false})))
            }
            Command::Fork { entry_id } => {
                let outcome = self
                    .inner
                    .navigate_tree(&entry_id, false, None, CancellationToken::new())
                    .await
                    .map_err(|error| SdkErrorText(format!("{error:#}")))?;
                Ok(Some(json!({
                    "text": outcome.editor_text,
                    "cancelled": outcome.cancelled,
                })))
            }
            Command::GetForkMessages {} => {
                let manager = self.inner.manager.lock().unwrap();
                let messages: Vec<Value> = manager
                    .branch_entries(manager.leaf_id())
                    .into_iter()
                    .filter_map(|entry| match entry {
                        kiss_coding::SessionEntry::Message {
                            base,
                            message: AgentMessage::User(user),
                            ..
                        } => Some(json!({
                            "entryId": base.id,
                            "text": user.content.as_text(),
                        })),
                        _ => None,
                    })
                    .collect();
                Ok(Some(json!({"messages": messages})))
            }
            Command::Ping {} => Ok(Some(json!({"pong": true}))),
        }
    }

    fn session_stats(&self) -> Value {
        let messages = self.messages();
        let mut user = 0usize;
        let mut assistant = 0usize;
        let mut tool_calls = 0usize;
        let mut tool_results = 0usize;
        for message in &messages {
            match message {
                AgentMessage::User(_) => user += 1,
                AgentMessage::Assistant(a) => {
                    assistant += 1;
                    tool_calls += a.tool_calls().count();
                }
                AgentMessage::ToolResult(_) => tool_results += 1,
                _ => {}
            }
        }
        let totals = self.inner.totals();
        let (used, window) = self.inner.context_usage();
        let manager = self.inner.manager.lock().unwrap();
        json!({
            "sessionFile": manager.session_file(),
            "sessionId": manager.session_id(),
            "userMessages": user,
            "assistantMessages": assistant,
            "toolCalls": tool_calls,
            "toolResults": tool_results,
            "totalMessages": messages.len(),
            "tokens": {
                "input": totals.input,
                "output": totals.output,
                "cacheRead": totals.cache_read,
                "cacheWrite": totals.cache_write,
                "total": totals.total_tokens,
            },
            "cost": totals.cost.total,
            "contextUsage": {
                "tokens": used,
                "contextWindow": window,
                "percent": used.saturating_mul(100).checked_div(window).unwrap_or(0),
            },
        })
    }
}

/// A plain error message. `execute_inner` uses it so every failure path becomes
/// a `Response::err` rather than an escaping error type.
struct SdkErrorText(String);

impl std::fmt::Display for SdkErrorText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<SdkError> for SdkErrorText {
    fn from(error: SdkError) -> Self {
        SdkErrorText(error.to_string())
    }
}

fn queue_mode_name(mode: SettingsQueueMode) -> &'static str {
    match mode {
        SettingsQueueMode::All => "all",
        SettingsQueueMode::OneAtATime => "one-at-a-time",
    }
}

fn settings_queue_mode(mode: QueueMode) -> SettingsQueueMode {
    match mode {
        QueueMode::All => SettingsQueueMode::All,
        QueueMode::OneAtATime => SettingsQueueMode::OneAtATime,
    }
}

fn message_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(user) => user.content.as_text(),
        other => other.role().to_string(),
    }
}

/// Build the user message a prompt turns into, attaching any images.
fn user_message(args: &PromptArgs) -> AgentMessage {
    if args.images.is_empty() {
        return AgentMessage::user(args.message.clone());
    }
    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(args.images.len() + 1);
    if !args.message.is_empty() {
        blocks.push(ContentBlock::text(args.message.clone()));
    }
    for image in &args.images {
        blocks.push(ContentBlock::Image {
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
        });
    }
    AgentMessage::User(UserMessage {
        content: UserContent::Blocks(blocks),
        timestamp: kiss_ai::now_ms(),
    })
}

/// Recursively build `{entry, children}` nodes for `get_tree`.
fn build_tree(manager: &kiss_coding::SessionManager, parent: Option<&str>) -> Vec<Value> {
    manager
        .children(parent)
        .into_iter()
        .map(|entry| {
            let id = entry.id().to_string();
            json!({
                "entry": entry,
                "label": manager.label_of(&id),
                "children": build_tree(manager, Some(&id)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_mode_names_match_the_settings_file() {
        assert_eq!(
            queue_mode_name(SettingsQueueMode::OneAtATime),
            "one-at-a-time"
        );
        assert_eq!(queue_mode_name(SettingsQueueMode::All), "all");
    }

    #[test]
    fn images_become_content_blocks_alongside_the_text() {
        let args = PromptArgs {
            message: "look".into(),
            images: vec![ImageInput {
                kind: "image".into(),
                data: "AA==".into(),
                mime_type: "image/png".into(),
            }],
            streaming_behavior: None,
        };
        match user_message(&args) {
            AgentMessage::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(blocks[1], ContentBlock::Image { .. }));
            }
            other => panic!("expected block content, got {other:?}"),
        }
    }
}
