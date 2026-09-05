//! Browser-local KISS agent kernel.
//!
//! Unlike `kiss-wasm`, this crate does not use RPC or WebSockets. The real KISS
//! agent/tool loop and conversation history live inside WebAssembly. JavaScript
//! supplies only explicit model and tool capabilities.

mod events;
mod host;
mod tool;
mod types;

use js_sys::{Function, Promise, Uint8Array};
use kiss_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, DynTool, EventSink, StreamFn,
};
use kiss_ai::{AssistantMessage, Model, StopReason, UserMessage};
use serde::Serialize;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use types::{
    AgentOptions, AgentStateView, Checkpoint, MAX_CHECKPOINT_BYTES, MAX_HISTORY_BYTES,
    MAX_PROMPT_BYTES, MAX_QUEUED_MESSAGES, MAX_TOOL_COUNT, MAX_TOOL_SCHEMA_BYTES, ModelInput,
    PromptInput, PromptResult, ToolDefinitionInput, validate_model, validate_options,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

struct AgentState {
    model: Model,
    system_prompt: String,
    thinking_level: kiss_ai::ThinkingLevel,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    max_turns: usize,
    max_history_messages: usize,
    messages: Vec<AgentMessage>,
    tools: Vec<DynTool>,
    tool_names: HashSet<String>,
    steering: Arc<Mutex<Vec<AgentMessage>>>,
    follow_up: Arc<Mutex<Vec<AgentMessage>>>,
    active: Option<CancellationToken>,
    closed: bool,
}

impl AgentState {
    fn view(&self) -> AgentStateView {
        let mut tools = self.tool_names.iter().cloned().collect::<Vec<_>>();
        tools.sort();
        AgentStateView {
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            is_streaming: self.active.is_some(),
            closed: self.closed,
            message_count: self.messages.len(),
            steering_count: self.steering.lock().map_or(0, |queue| queue.len()),
            follow_up_count: self.follow_up.lock().map_or(0, |queue| queue.len()),
            tools,
        }
    }
}

struct EventRegistration(Option<u32>);

impl Drop for EventRegistration {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            host::remove_event(id);
        }
    }
}

/// One in-browser KISS conversation and agent loop.
#[wasm_bindgen]
pub struct KissAgent {
    inner: Rc<RefCell<AgentState>>,
    model_callback_id: u32,
    model_registered: Cell<bool>,
}

#[wasm_bindgen]
impl KissAgent {
    /// Create an agent around an explicit host model provider.
    #[wasm_bindgen(js_name = create)]
    pub fn create(options: JsValue, model_provider: Function) -> Result<KissAgent, JsValue> {
        let options: AgentOptions = serde_wasm_bindgen::from_value(options)
            .map_err(|error| js_error(format!("KISS_INVALID_OPTIONS: {error}")))?;
        validate_options(&options).map_err(js_error)?;

        let messages = restore_checkpoint(options.checkpoint.as_deref())?;
        let max_history_messages = options.max_history_messages;
        let messages = trim_history(messages, max_history_messages);
        let model: Model = options.model.into();
        let thinking_level = model.map_thinking_level(options.thinking_level);
        let model_callback_id = host::register_model(model_provider);
        Ok(KissAgent {
            inner: Rc::new(RefCell::new(AgentState {
                model,
                system_prompt: options.system_prompt,
                thinking_level,
                temperature: options.temperature,
                max_tokens: options.max_tokens,
                max_turns: options.max_turns,
                max_history_messages,
                messages,
                tools: Vec::new(),
                tool_names: HashSet::new(),
                steering: Arc::new(Mutex::new(Vec::new())),
                follow_up: Arc::new(Mutex::new(Vec::new())),
                active: None,
                closed: false,
            })),
            model_callback_id,
            model_registered: Cell::new(true),
        })
    }

    /// Register an explicitly host-provided tool capability.
    #[wasm_bindgen(js_name = registerTool)]
    pub fn register_tool(&self, definition: JsValue, execute: Function) -> Result<(), JsValue> {
        let definition: ToolDefinitionInput = serde_wasm_bindgen::from_value(definition)
            .map_err(|error| js_error(format!("KISS_INVALID_TOOL: {error}")))?;
        validate_tool(&definition).map_err(js_error)?;

        let mut state = self.inner.borrow_mut();
        if state.closed {
            return Err(js_error("KISS_CLOSED: agent is closed"));
        }
        if state.active.is_some() {
            return Err(js_error("KISS_BUSY: tools cannot change during a prompt"));
        }
        if state.tools.len() >= MAX_TOOL_COUNT {
            return Err(js_error(format!(
                "KISS_LIMIT: at most {MAX_TOOL_COUNT} tools may be registered"
            )));
        }
        if !state.tool_names.insert(definition.name.clone()) {
            return Err(js_error(format!(
                "KISS_INVALID_TOOL: duplicate tool {}",
                definition.name
            )));
        }

        let callback_id = host::register_tool(execute);
        state
            .tools
            .push(Arc::new(tool::HostTool::new(callback_id, definition)));
        Ok(())
    }

    /// Add a user message after the active model turn and before its next turn.
    pub fn steer(&self, input: JsValue) -> Result<(), JsValue> {
        self.queue_message(input, true)
    }

    /// Add a user message after the active run would otherwise settle.
    #[wasm_bindgen(js_name = followUp)]
    pub fn follow_up(&self, input: JsValue) -> Result<(), JsValue> {
        self.queue_message(input, false)
    }

    /// Replace the selected model while idle; the host provider is unchanged.
    #[wasm_bindgen(js_name = setModel)]
    pub fn set_model(&self, model: JsValue) -> Result<(), JsValue> {
        let model: ModelInput = serde_wasm_bindgen::from_value(model)
            .map_err(|error| js_error(format!("KISS_INVALID_OPTIONS: {error}")))?;
        validate_model(&model).map_err(js_error)?;
        let mut state = self.inner.borrow_mut();
        ensure_mutable(&state)?;
        state.model = model.into();
        state.thinking_level = state.model.map_thinking_level(state.thinking_level);
        Ok(())
    }

    /// Change reasoning effort while idle.
    #[wasm_bindgen(js_name = setThinkingLevel)]
    pub fn set_thinking_level(&self, level: JsValue) -> Result<(), JsValue> {
        let level = serde_wasm_bindgen::from_value(level)
            .map_err(|error| js_error(format!("KISS_INVALID_OPTIONS: {error}")))?;
        let mut state = self.inner.borrow_mut();
        ensure_mutable(&state)?;
        state.thinking_level = state.model.map_thinking_level(level);
        Ok(())
    }

    /// Return the retained KISS messages.
    pub fn messages(&self) -> Result<JsValue, JsValue> {
        to_js(&self.inner.borrow().messages)
    }

    /// Clear conversation history while retaining model and tools.
    #[wasm_bindgen(js_name = clearHistory)]
    pub fn clear_history(&self) -> Result<(), JsValue> {
        let mut state = self.inner.borrow_mut();
        ensure_mutable(&state)?;
        state.messages.clear();
        Ok(())
    }

    /// Run a prompt through the in-WASM model/tool loop.
    pub fn prompt(&self, input: JsValue, on_event: Option<Function>) -> Promise {
        let content = match parse_prompt(input) {
            Ok(content) => content,
            Err(error) => return Promise::reject(&error),
        };

        let cancel = CancellationToken::new();
        let snapshot = {
            let mut state = self.inner.borrow_mut();
            if state.closed {
                return Promise::reject(&js_error("KISS_CLOSED: agent is closed"));
            }
            if state.active.is_some() {
                return Promise::reject(&js_error("KISS_BUSY: a prompt is already running"));
            }
            state.active = Some(cancel.clone());
            (
                state.model.clone(),
                state.system_prompt.clone(),
                state.thinking_level,
                state.temperature,
                state.max_tokens,
                state.max_turns,
                state.messages.clone(),
                state.tools.clone(),
                state.steering.clone(),
                state.follow_up.clone(),
            )
        };

        let event_id = on_event.map(host::register_event);
        let registration = EventRegistration(event_id);
        let inner = self.inner.clone();
        let model_callback_id = self.model_callback_id;
        future_to_promise(async move {
            let _registration = registration;
            let (
                model,
                system_prompt,
                thinking_level,
                temperature,
                max_tokens,
                max_turns,
                messages,
                tools,
                steering,
                follow_up,
            ) = snapshot;
            let stream_fn: StreamFn = Arc::new(move |model, context, options| {
                host::model_stream(model_callback_id, model, context, options)
            });
            let mut config = AgentLoopConfig::with_stream(model.clone(), stream_fn);
            config.thinking_level = thinking_level;
            config.temperature = temperature;
            config.max_tokens = max_tokens;
            let turns = Arc::new(AtomicUsize::new(0));
            config.should_stop_after_turn = Some(Arc::new(move |_| {
                let count = turns.fetch_add(1, Ordering::Relaxed) + 1;
                Box::pin(async move { count >= max_turns })
            }));
            let steering_for_loop = steering.clone();
            config.get_steering_messages = Some(Arc::new(move || {
                let messages = steering_for_loop
                    .lock()
                    .map(|mut queue| std::mem::take(&mut *queue))
                    .unwrap_or_default();
                Box::pin(async move { messages })
            }));
            let follow_up_for_loop = follow_up.clone();
            config.get_follow_up_messages = Some(Arc::new(move || {
                let messages = follow_up_for_loop
                    .lock()
                    .map(|mut queue| std::mem::take(&mut *queue))
                    .unwrap_or_default();
                Box::pin(async move { messages })
            }));

            let context = AgentContext {
                system_prompt,
                openai_responses_input: None,
                messages,
                tools,
            };
            let prompt = AgentMessage::User(UserMessage {
                content,
                timestamp: kiss_ai::now_ms(),
            });
            let emit: EventSink = Arc::new(move |event: AgentEvent| {
                host::emit_agent_event(event_id, &event);
            });
            let new_messages =
                kiss_agent::run_agent_loop(vec![prompt], context, config, cancel.clone(), emit)
                    .await;

            let assistant = new_messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    AgentMessage::Assistant(message) => Some(message.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| missing_assistant(&model));
            let mut usage = kiss_ai::Usage::default();
            for message in &new_messages {
                if let AgentMessage::Assistant(message) = message {
                    usage.add(&message.usage);
                }
            }

            let state_view = {
                let mut state = inner.borrow_mut();
                state.active = None;
                if let Ok(mut queue) = state.steering.lock() {
                    queue.clear();
                }
                if let Ok(mut queue) = state.follow_up.lock() {
                    queue.clear();
                }
                if !state.closed {
                    state.messages.extend(new_messages.iter().cloned());
                    state.messages = trim_history(
                        std::mem::take(&mut state.messages),
                        state.max_history_messages,
                    );
                }
                state.view()
            };
            host::emit_settled(event_id);
            to_js(&PromptResult {
                text: assistant.text(),
                stop_reason: assistant.stop_reason,
                messages: new_messages,
                usage,
                state: state_view,
            })
        })
    }

    /// Abort the active model/tool callback and agent loop.
    pub fn abort(&self) {
        let state = self.inner.borrow();
        if let Some(cancel) = state.active.clone() {
            cancel.cancel();
        }
        if let Ok(mut queue) = state.steering.lock() {
            queue.clear();
        }
        if let Ok(mut queue) = state.follow_up.lock() {
            queue.clear();
        }
    }

    /// Return an immediate snapshot of browser-agent state.
    pub fn state(&self) -> Result<JsValue, JsValue> {
        to_js(&self.inner.borrow().view())
    }

    /// Serialize bounded, versioned conversation history.
    pub fn checkpoint(&self) -> Result<Uint8Array, JsValue> {
        let state = self.inner.borrow();
        if state.active.is_some() {
            return Err(js_error("KISS_BUSY: checkpoint requires an idle agent"));
        }
        let bytes = serde_json::to_vec(&Checkpoint {
            version: 1,
            messages: state.messages.clone(),
        })
        .map_err(|error| js_error(format!("KISS_CHECKPOINT: {error}")))?;
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(js_error(format!(
                "KISS_LIMIT: checkpoint exceeds {MAX_CHECKPOINT_BYTES} bytes"
            )));
        }
        Ok(Uint8Array::from(bytes.as_slice()))
    }

    /// Idempotently abort and release every host capability.
    pub fn close(&self) {
        let mut state = self.inner.borrow_mut();
        if state.closed {
            return;
        }
        state.closed = true;
        if let Some(cancel) = state.active.take() {
            cancel.cancel();
        }
        state.tools.clear();
        state.tool_names.clear();
        if let Ok(mut queue) = state.steering.lock() {
            queue.clear();
        }
        if let Ok(mut queue) = state.follow_up.lock() {
            queue.clear();
        }
        drop(state);
        self.release_model();
    }
}

impl KissAgent {
    fn queue_message(&self, input: JsValue, steering: bool) -> Result<(), JsValue> {
        let content = parse_prompt(input)?;
        let state = self.inner.borrow();
        if state.closed {
            return Err(js_error("KISS_CLOSED: agent is closed"));
        }
        if state.active.is_none() {
            return Err(js_error(
                "KISS_IDLE: messages can only be queued during a prompt",
            ));
        }
        let queue = if steering {
            &state.steering
        } else {
            &state.follow_up
        };
        let mut queue = queue
            .lock()
            .map_err(|_| js_error("KISS_INTERNAL: message queue is unavailable"))?;
        if queue.len() >= MAX_QUEUED_MESSAGES {
            return Err(js_error(format!(
                "KISS_LIMIT: at most {MAX_QUEUED_MESSAGES} messages may be queued"
            )));
        }
        queue.push(AgentMessage::User(UserMessage {
            content,
            timestamp: kiss_ai::now_ms(),
        }));
        Ok(())
    }

    fn release_model(&self) {
        if self.model_registered.replace(false) {
            host::remove_model(self.model_callback_id);
        }
    }
}

impl Drop for KissAgent {
    fn drop(&mut self) {
        if let Ok(mut state) = self.inner.try_borrow_mut() {
            if let Some(cancel) = state.active.take() {
                cancel.cancel();
            }
            state.closed = true;
            state.tools.clear();
            state.tool_names.clear();
        }
        self.release_model();
    }
}

fn ensure_mutable(state: &AgentState) -> Result<(), JsValue> {
    if state.closed {
        Err(js_error("KISS_CLOSED: agent is closed"))
    } else if state.active.is_some() {
        Err(js_error(
            "KISS_BUSY: agent configuration cannot change during a prompt",
        ))
    } else {
        Ok(())
    }
}

fn parse_prompt(input: JsValue) -> Result<kiss_ai::UserContent, JsValue> {
    let input: PromptInput = serde_wasm_bindgen::from_value(input)
        .map_err(|error| js_error(format!("KISS_INVALID_PROMPT: {error}")))?;
    let content = input.into_content();
    let prompt_bytes = serde_json::to_vec(&content)
        .map_err(|error| js_error(format!("KISS_INVALID_PROMPT: {error}")))?
        .len();
    if prompt_bytes > MAX_PROMPT_BYTES {
        return Err(js_error(format!(
            "KISS_LIMIT: prompt exceeds {MAX_PROMPT_BYTES} bytes"
        )));
    }
    Ok(content)
}

fn restore_checkpoint(bytes: Option<&[u8]>) -> Result<Vec<AgentMessage>, JsValue> {
    let Some(bytes) = bytes else {
        return Ok(Vec::new());
    };
    if bytes.len() > MAX_CHECKPOINT_BYTES {
        return Err(js_error(format!(
            "KISS_LIMIT: checkpoint exceeds {MAX_CHECKPOINT_BYTES} bytes"
        )));
    }
    let checkpoint: Checkpoint = serde_json::from_slice(bytes)
        .map_err(|error| js_error(format!("KISS_CHECKPOINT: {error}")))?;
    if checkpoint.version != 1 {
        return Err(js_error(format!(
            "KISS_CHECKPOINT_VERSION: unsupported version {}",
            checkpoint.version
        )));
    }
    Ok(checkpoint.messages)
}

fn trim_history(messages: Vec<AgentMessage>, max: usize) -> Vec<AgentMessage> {
    let mut kept = Vec::with_capacity(messages.len().min(max));
    let mut bytes = 32usize;
    for message in messages.into_iter().rev().take(max) {
        let message_bytes =
            serde_json::to_vec(&message).map_or(MAX_HISTORY_BYTES, |value| value.len());
        if bytes.saturating_add(message_bytes) > MAX_HISTORY_BYTES {
            break;
        }
        bytes += message_bytes;
        kept.push(message);
    }
    kept.reverse();
    kept
}

fn validate_tool(definition: &ToolDefinitionInput) -> Result<(), String> {
    if definition.name.is_empty()
        || definition.name.len() > 128
        || !definition
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(
            "KISS_INVALID_TOOL: name must be 1-128 ASCII letters, numbers, '_', '-', or '.'".into(),
        );
    }
    if definition.description.len() > 64 * 1024 {
        return Err("KISS_LIMIT: tool description exceeds 65536 bytes".into());
    }
    let schema = serde_json::to_vec(&definition.parameters)
        .map_err(|error| format!("KISS_INVALID_TOOL: {error}"))?;
    if schema.len() > MAX_TOOL_SCHEMA_BYTES {
        return Err(format!(
            "KISS_LIMIT: tool schema exceeds {MAX_TOOL_SCHEMA_BYTES} bytes"
        ));
    }
    if !definition.parameters.is_object() {
        return Err("KISS_INVALID_TOOL: parameters must be a JSON Schema object".into());
    }
    Ok(())
}

fn missing_assistant(model: &Model) -> AssistantMessage {
    let mut assistant = AssistantMessage::empty(&model.api, &model.provider, &model.id);
    assistant.stop_reason = StopReason::Error;
    assistant.error_message = Some("agent ended without an assistant message".into());
    assistant
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|error| js_error(error.to_string()))
}

fn js_error(message: impl AsRef<str>) -> JsValue {
    js_sys::Error::new(message.as_ref()).into()
}
