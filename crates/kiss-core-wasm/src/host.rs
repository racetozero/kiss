use crate::events::agent_event_json;
use crate::types::{
    HostModelEvent, HostModelResponse, MAX_MODEL_RESPONSE_BYTES, MAX_TOOL_RESULT_BYTES,
    ModelRequest, ToolInvocationContext, ToolResultContent, ToolResultInput,
};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{Either, select};
use js_sys::{Function, Promise};
use kiss_agent::{AgentEvent, ToolResult, ToolUpdateSink};
use kiss_ai::{
    AssistantEvent, AssistantMessage, ContentBlock, Context, EventStream, Model, StopReason,
    StreamOptions,
};
use serde::Serialize;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use tokio_util::sync::CancellationToken;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::AbortController;

thread_local! {
    static CALLBACKS: RefCell<CallbackRegistry> = RefCell::new(CallbackRegistry::default());
}

#[derive(Default)]
struct CallbackRegistry {
    next_id: u32,
    models: HashMap<u32, Function>,
    tools: HashMap<u32, Function>,
    events: HashMap<u32, Function>,
}

impl CallbackRegistry {
    fn id(&mut self) -> u32 {
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        self.next_id
    }
}

pub fn register_model(callback: Function) -> u32 {
    CALLBACKS.with_borrow_mut(|callbacks| {
        let id = callbacks.id();
        callbacks.models.insert(id, callback);
        id
    })
}

pub fn remove_model(id: u32) {
    CALLBACKS.with_borrow_mut(|callbacks| {
        callbacks.models.remove(&id);
    });
}

pub fn register_tool(callback: Function) -> u32 {
    CALLBACKS.with_borrow_mut(|callbacks| {
        let id = callbacks.id();
        callbacks.tools.insert(id, callback);
        id
    })
}

pub fn remove_tool(id: u32) {
    CALLBACKS.with_borrow_mut(|callbacks| {
        callbacks.tools.remove(&id);
    });
}

pub fn register_event(callback: Function) -> u32 {
    CALLBACKS.with_borrow_mut(|callbacks| {
        let id = callbacks.id();
        callbacks.events.insert(id, callback);
        id
    })
}

pub fn remove_event(id: u32) {
    CALLBACKS.with_borrow_mut(|callbacks| {
        callbacks.events.remove(&id);
    });
}

pub fn emit_agent_event(callback_id: Option<u32>, event: &AgentEvent) {
    let Some(callback_id) = callback_id else {
        return;
    };
    let callback = CALLBACKS.with_borrow(|callbacks| callbacks.events.get(&callback_id).cloned());
    if let Some(callback) = callback
        && let Ok(value) = to_js(&agent_event_json(event))
        && let Err(error) = callback.call1(&JsValue::NULL, &value)
    {
        web_sys::console::error_2(&JsValue::from_str("KISS event callback failed"), &error);
    }
}

pub fn emit_settled(callback_id: Option<u32>) {
    let Some(callback_id) = callback_id else {
        return;
    };
    let callback = CALLBACKS.with_borrow(|callbacks| callbacks.events.get(&callback_id).cloned());
    if let Some(callback) = callback
        && let Ok(value) = to_js(&serde_json::json!({"type": "agent_settled"}))
    {
        let _ = callback.call1(&JsValue::NULL, &value);
    }
}

pub fn model_stream(
    callback_id: u32,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> EventStream {
    let (sink, stream) = EventStream::channel();
    let partial = AssistantMessage::empty(&model.api, &model.provider, &model.id);
    sink.send(AssistantEvent::Start {
        partial: partial.clone(),
    });

    let request = ModelRequest {
        model: model.clone(),
        context: context.into(),
        reasoning: options.reasoning,
        temperature: options.temperature,
        max_tokens: options.max_tokens,
    };
    let cancel = options.cancel.clone();
    let sink_for_events = sink.clone();
    let model = model.clone();
    spawn_local(async move {
        match invoke_model(callback_id, request, sink_for_events, cancel).await {
            Ok(response) => {
                let stop_reason = response.stop_reason.unwrap_or_else(|| {
                    if response
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::ToolCall(_)))
                    {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    }
                });
                let message = AssistantMessage {
                    content: response.content,
                    api: model.api,
                    provider: model.provider,
                    model: model.id,
                    response_model: response.response_model,
                    response_id: response.response_id,
                    usage: response.usage,
                    stop_reason,
                    error_message: response.error_message,
                    raw_stop_reason: response.raw_stop_reason,
                    timestamp: kiss_ai::now_ms(),
                };
                if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                    sink.error(message);
                } else {
                    sink.done(message);
                }
            }
            Err(HostFailure::Cancelled) => {
                let mut message = partial;
                message.stop_reason = StopReason::Aborted;
                message.error_message = Some("Operation aborted".into());
                sink.error(message);
            }
            Err(HostFailure::Failed(error)) => {
                let mut message = partial;
                message.stop_reason = StopReason::Error;
                message.error_message = Some(error);
                sink.error(message);
            }
        }
    });
    stream
}

async fn invoke_model(
    callback_id: u32,
    request: ModelRequest,
    sink: kiss_ai::EventSink,
    cancel: CancellationToken,
) -> Result<HostModelResponse, HostFailure> {
    let callback = CALLBACKS
        .with_borrow(|callbacks| callbacks.models.get(&callback_id).cloned())
        .ok_or_else(|| HostFailure::Failed("KISS_CLOSED: model provider was released".into()))?;
    let request = to_js(&request).map_err(HostFailure::Failed)?;
    let controller = AbortController::new().map_err(|error| {
        HostFailure::Failed(js_error("could not create AbortController", error))
    })?;
    let event_sink = sink.clone();
    let streamed_bytes = Rc::new(Cell::new(0usize));
    let stream_overflowed = Rc::new(Cell::new(false));
    let callback_bytes = streamed_bytes.clone();
    let callback_overflowed = stream_overflowed.clone();
    let callback_controller = controller.clone();
    let emit = Closure::<dyn FnMut(JsValue)>::new(move |value: JsValue| {
        match serde_wasm_bindgen::from_value::<HostModelEvent>(value) {
            Ok(event) => {
                let bytes = serde_json::to_vec(&event).map_or(0, |value| value.len());
                let total = callback_bytes.get().saturating_add(bytes);
                callback_bytes.set(total);
                if total > MAX_MODEL_RESPONSE_BYTES {
                    callback_overflowed.set(true);
                    callback_controller.abort();
                    return;
                }
                event_sink.send(event.into());
            }
            Err(error) => web_sys::console::error_1(&JsValue::from_str(&format!(
                "invalid KISS model stream event: {error}"
            ))),
        }
    });
    let returned = callback
        .call3(
            &JsValue::NULL,
            &request,
            emit.as_ref().unchecked_ref(),
            controller.signal().as_ref(),
        )
        .map_err(|error| HostFailure::Failed(js_error("model provider threw", error)))?;

    let promise = JsFuture::from(Promise::resolve(&returned)).fuse();
    let cancelled = cancel.cancelled_owned().fuse();
    futures::pin_mut!(promise, cancelled);
    let value = match select(promise, cancelled).await {
        Either::Left((result, _)) => result
            .map_err(|error| HostFailure::Failed(js_error("model provider rejected", error)))?,
        Either::Right(((), _)) => {
            controller.abort();
            return Err(HostFailure::Cancelled);
        }
    };
    drop(emit);
    if stream_overflowed.get() {
        return Err(HostFailure::Failed(format!(
            "KISS_LIMIT: model stream exceeds {MAX_MODEL_RESPONSE_BYTES} bytes"
        )));
    }
    let response: HostModelResponse = serde_wasm_bindgen::from_value(value)
        .map_err(|error| HostFailure::Failed(format!("KISS_HOST_RESPONSE: {error}")))?;
    let response_bytes = serde_json::to_vec(&response)
        .map_err(|error| HostFailure::Failed(format!("KISS_HOST_RESPONSE: {error}")))?;
    if response_bytes.len() > MAX_MODEL_RESPONSE_BYTES {
        return Err(HostFailure::Failed(format!(
            "KISS_LIMIT: model response exceeds {MAX_MODEL_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(response)
}

impl From<HostModelEvent> for AssistantEvent {
    fn from(value: HostModelEvent) -> Self {
        match value {
            HostModelEvent::TextStart { content_index } => Self::TextStart { content_index },
            HostModelEvent::TextDelta {
                content_index,
                delta,
            } => Self::TextDelta {
                content_index,
                delta,
            },
            HostModelEvent::TextEnd {
                content_index,
                content,
            } => Self::TextEnd {
                content_index,
                content,
            },
            HostModelEvent::ThinkingStart { content_index } => {
                Self::ThinkingStart { content_index }
            }
            HostModelEvent::ThinkingDelta {
                content_index,
                delta,
            } => Self::ThinkingDelta {
                content_index,
                delta,
            },
            HostModelEvent::ThinkingEnd {
                content_index,
                content,
            } => Self::ThinkingEnd {
                content_index,
                content,
            },
            HostModelEvent::ToolcallStart {
                content_index,
                tool_call,
            } => Self::ToolCallStart {
                content_index,
                tool_call,
            },
            HostModelEvent::ToolcallDelta {
                content_index,
                delta,
            } => Self::ToolCallDelta {
                content_index,
                delta,
            },
            HostModelEvent::ToolcallEnd {
                content_index,
                tool_call,
            } => Self::ToolCallEnd {
                content_index,
                tool_call,
            },
        }
    }
}

pub fn launch_tool(
    callback_id: u32,
    tool_call_id: String,
    args: serde_json::Value,
    cancel: CancellationToken,
    on_update: Option<ToolUpdateSink>,
) -> oneshot::Receiver<Result<ToolResult, String>> {
    let (sender, receiver) = oneshot::channel();
    spawn_local(async move {
        let result = invoke_tool(callback_id, tool_call_id, args, cancel, on_update).await;
        let _ = sender.send(result);
    });
    receiver
}

async fn invoke_tool(
    callback_id: u32,
    tool_call_id: String,
    args: serde_json::Value,
    cancel: CancellationToken,
    on_update: Option<ToolUpdateSink>,
) -> Result<ToolResult, String> {
    let callback = CALLBACKS
        .with_borrow(|callbacks| callbacks.tools.get(&callback_id).cloned())
        .ok_or_else(|| "KISS_CLOSED: tool callback was released".to_string())?;
    let args = to_js(&args)?;
    let context = to_js(&ToolInvocationContext { tool_call_id })?;
    let controller = AbortController::new()
        .map_err(|error| js_error("could not create AbortController", error))?;
    js_sys::Reflect::set(&context, &"signal".into(), controller.signal().as_ref())
        .map_err(|error| js_error("could not attach tool AbortSignal", error))?;

    let update = Closure::<dyn FnMut(JsValue)>::new(move |value: JsValue| {
        let Some(on_update) = &on_update else {
            return;
        };
        match parse_tool_result(value) {
            Ok(result) => on_update(result),
            Err(error) => web_sys::console::error_1(&JsValue::from_str(&format!(
                "invalid KISS tool update: {error}"
            ))),
        }
    });
    js_sys::Reflect::set(
        &context,
        &"onUpdate".into(),
        update.as_ref().unchecked_ref(),
    )
    .map_err(|error| js_error("could not attach tool update callback", error))?;

    let returned = callback
        .call2(&JsValue::NULL, &args, &context)
        .map_err(|error| js_error("tool callback threw", error))?;
    let promise = JsFuture::from(Promise::resolve(&returned)).fuse();
    let cancelled = cancel.cancelled_owned().fuse();
    futures::pin_mut!(promise, cancelled);
    let value = match select(promise, cancelled).await {
        Either::Left((result, _)) => {
            result.map_err(|error| js_error("tool callback rejected", error))?
        }
        Either::Right(((), _)) => {
            controller.abort();
            return Err("Operation aborted".into());
        }
    };
    drop(update);
    parse_tool_result(value)
}

fn parse_tool_result(value: JsValue) -> Result<ToolResult, String> {
    let input = if let Some(text) = value.as_string() {
        ToolResultInput::Text(text)
    } else {
        serde_wasm_bindgen::from_value(value)
            .map_err(|error| format!("KISS_HOST_RESPONSE: {error}"))?
    };
    let result = match input {
        ToolResultInput::Text(text) => ToolResult::text(text),
        ToolResultInput::Object(object) => ToolResult {
            content: match object.content {
                ToolResultContent::Text(text) => vec![ContentBlock::text(text)],
                ToolResultContent::Blocks(blocks) => blocks,
                ToolResultContent::Empty => Vec::new(),
            },
            details: object.details,
            usage: None,
            terminate: object.terminate,
        },
    };
    let bytes = serde_json::to_vec(&serde_json::json!({
        "content": result.content,
        "details": result.details,
    }))
    .map_err(|error| format!("KISS_HOST_RESPONSE: {error}"))?;
    if bytes.len() > MAX_TOOL_RESULT_BYTES {
        return Err(format!(
            "KISS_LIMIT: tool result exceeds {MAX_TOOL_RESULT_BYTES} bytes"
        ));
    }
    Ok(result)
}

#[derive(Debug)]
enum HostFailure {
    Cancelled,
    Failed(String),
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, String> {
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|error| error.to_string())
}

fn js_error(prefix: &str, value: JsValue) -> String {
    let detail = if let Some(error) = value.dyn_ref::<js_sys::Error>() {
        error.message().into()
    } else {
        value.as_string().unwrap_or_default()
    };
    if detail.is_empty() {
        return prefix.to_string();
    }
    const MAX_ERROR_CHARS: usize = 8 * 1024;
    let detail = detail.chars().take(MAX_ERROR_CHARS).collect::<String>();
    format!("{prefix}: {detail}")
}
