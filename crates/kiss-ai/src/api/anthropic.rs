//! Anthropic Messages API adapter.

use super::{PartialBuilder, apply_provider_headers, provider_base_url, thinking_budget};
use crate::event::EventSink;
use crate::model::Model;
use crate::sse::{SseEvent, SseParser};
use crate::stream::{StreamOptions, http_client};
use crate::types::{ContentBlock, Context, Message, StopReason, UserContent};
use futures::StreamExt;
use serde_json::{Value, json};

pub async fn stream(model: &Model, context: &Context, options: &StreamOptions, sink: EventSink) {
    let mut builder = PartialBuilder::new(model, sink);
    let Some(credential) = options.credential.as_ref() else {
        builder.fail(
            format!(
                "no API key for provider {} (set ANTHROPIC_API_KEY or use /login)",
                model.provider
            ),
            false,
            model,
        );
        return;
    };
    let api_key = credential.value().to_string();

    let body = build_request(model, context, options);
    if model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_mid_convo_effort)
        .unwrap_or(false)
    {
        builder.message.provider_thinking_level = Some(adaptive_effort(model, options.reasoning));
    }
    // A workload identity federation token is an OAuth bearer token, but
    // not a Claude subscription token.
    let (api_key, federated) =
        match api_key.strip_prefix(crate::auth::anthropic_federation::TOKEN_PREFIX) {
            Some(token) => (token.to_string(), true),
            None => (api_key, false),
        };
    let oauth = model.provider == "anthropic"
        && !federated
        && crate::auth::is_oauth_access_token(&model.provider, &api_key);
    let bearer = federated || crate::auth::is_bearer_access_token(&model.provider, &api_key);
    let base_url = provider_base_url(model, &api_key);
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let mut request = http_client()
        .post(&url)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json");
    if oauth {
        let serialized = match super::claude_code::serialize_request(
            body,
            context,
            options.session_id.as_deref(),
        ) {
            Ok(serialized) => serialized,
            Err(error) => {
                builder.fail(
                    format!("could not build Anthropic OAuth request: {error}"),
                    false,
                    model,
                );
                return;
            }
        };
        request = request
            .bearer_auth(&api_key)
            .header("accept", "application/json")
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header(
                "anthropic-beta",
                beta_features(model, true, options.fast_mode).join(","),
            )
            .header(
                "user-agent",
                format!(
                    "claude-cli/{} (external, {})",
                    super::claude_code::VERSION,
                    super::claude_code::ENTRYPOINT
                ),
            )
            .header("x-app", "cli")
            .header("x-client-request-id", uuid::Uuid::new_v4().to_string())
            .body(serialized);
        if let Some(session_id) = &options.session_id {
            request = request.header("x-claude-code-session-id", session_id);
        }
    } else {
        request = request.json(&body);
        let mut beta_features = beta_features(model, false, options.fast_mode);
        if federated {
            beta_features.insert(0, "oauth-2025-04-20");
        }
        if !beta_features.is_empty() {
            request = request.header("anthropic-beta", beta_features.join(","));
        }
    }
    request = if oauth {
        request
    } else if bearer || model.provider == "github-copilot" {
        request.bearer_auth(&api_key)
    } else {
        request.header("x-api-key", &api_key)
    };
    for (k, v) in &model.headers {
        request = request.header(k, v);
    }
    request = apply_provider_headers(request, model, context, options);

    let response = tokio::select! {
        r = request.send() => r,
        _ = options.cancel.cancelled() => {
            builder.fail("Request aborted", true, model);
            return;
        }
    };
    let response = match response {
        Ok(r) => r,
        Err(e) => {
            builder.fail(
                format!("request failed: {}", super::with_causes(&e)),
                false,
                model,
            );
            return;
        }
    };
    if !response.status().is_success() {
        builder.fail(super::http_error(response).await, false, model);
        return;
    }

    let mut parser = SseParser::new();
    let mut events: Vec<SseEvent> = Vec::new();
    let mut body_stream = response.bytes_stream();
    let mut state = DecodeState {
        openrouter: model.provider == "openrouter" || model.base_url.contains("openrouter.ai"),
        ..Default::default()
    };

    loop {
        let chunk = tokio::select! {
            c = body_stream.next() => c,
            _ = options.cancel.cancelled() => {
                builder.fail("Request aborted", true, model);
                return;
            }
        };
        match chunk {
            Some(Ok(bytes)) => {
                parser.feed(&bytes, &mut events);
                for event in events.drain(..) {
                    match handle_event(&event, &mut builder, &mut state) {
                        Flow::Continue => {}
                        Flow::Done => {
                            builder.finish(state.stop_reason, model);
                            return;
                        }
                        Flow::Error(msg) => {
                            builder.fail(msg, false, model);
                            return;
                        }
                    }
                }
            }
            Some(Err(e)) => {
                builder.fail(super::stream_error(super::with_causes(&e)), false, model);
                return;
            }
            None => break,
        }
    }
    // A stream may end without the blank line after its last event.
    parser.finish(&mut events);
    for event in events.drain(..) {
        match handle_event(&event, &mut builder, &mut state) {
            Flow::Continue => {}
            Flow::Done => {
                builder.finish(state.stop_reason, model);
                return;
            }
            Flow::Error(msg) => {
                builder.fail(msg, false, model);
                return;
            }
        }
    }
    // Some compatible endpoints end after the final message_delta without
    // message_stop. Without even a stop reason the reply was cut off, and
    // finishing would hand the agent a half-written tool call.
    if builder.message.raw_stop_reason.is_some() {
        builder.finish(state.stop_reason, model);
    } else {
        builder.fail(
            super::stream_error("the response ended before the provider sent a stop reason"),
            false,
            model,
        );
    }
}

#[derive(Default)]
struct DecodeState {
    /// Provider content index -> our builder content index.
    index_map: std::collections::HashMap<u64, (usize, BlockKind)>,
    stop_reason: StopReason,
    /// OpenRouter reports its billed cost in `usage`.
    openrouter: bool,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum BlockKind {
    #[default]
    Text,
    Thinking,
    ToolUse,
}

enum Flow {
    Continue,
    Done,
    Error(String),
}

fn handle_event(event: &SseEvent, builder: &mut PartialBuilder, state: &mut DecodeState) -> Flow {
    let data: Value = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(_) => return Flow::Continue,
    };
    let event_type = event
        .event
        .as_deref()
        .or_else(|| data["type"].as_str())
        .unwrap_or("");
    match event_type {
        "message_start" => {
            builder.start();
            let usage = &data["message"]["usage"];
            builder.message.usage.input = usage["input_tokens"].as_u64().unwrap_or(0);
            builder.message.usage.cache_read =
                usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            builder.message.usage.cache_write =
                usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            builder.message.usage.cache_write_1h =
                usage["cache_creation"]["ephemeral_1h_input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
            builder.message.usage.cache_read_available = usage
                .get("cache_read_input_tokens")
                .or_else(|| usage.get("cache_creation_input_tokens"))
                .is_some();
            if let Some(id) = data["message"]["id"].as_str() {
                builder.message.response_id = Some(id.to_string());
            }
            if let Some(m) = data["message"]["model"].as_str()
                && m != builder.message.model
            {
                builder.message.response_model = Some(m.to_string());
            }
            Flow::Continue
        }
        "content_block_start" => {
            let provider_idx = data["index"].as_u64().unwrap_or(0);
            let block = &data["content_block"];
            match block["type"].as_str().unwrap_or("") {
                "text" => {
                    let idx = builder.begin_text();
                    state.index_map.insert(provider_idx, (idx, BlockKind::Text));
                }
                "thinking" => {
                    let idx = builder.begin_thinking();
                    state
                        .index_map
                        .insert(provider_idx, (idx, BlockKind::Thinking));
                }
                "redacted_thinking" => {
                    let idx = builder.begin_thinking();
                    if let Some(payload) = block["data"].as_str() {
                        builder.set_thinking_signature(idx, payload.to_string());
                    }
                    if let Some(ContentBlock::Thinking { redacted, .. }) =
                        builder.message.content.get_mut(idx)
                    {
                        *redacted = true;
                    }
                    state
                        .index_map
                        .insert(provider_idx, (idx, BlockKind::Thinking));
                }
                "tool_use" => {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    let idx = builder.begin_tool_call(id, name);
                    state
                        .index_map
                        .insert(provider_idx, (idx, BlockKind::ToolUse));
                }
                _ => {}
            }
            Flow::Continue
        }
        "content_block_delta" => {
            let provider_idx = data["index"].as_u64().unwrap_or(0);
            let Some(&(idx, _)) = state.index_map.get(&provider_idx) else {
                return Flow::Continue;
            };
            let delta = &data["delta"];
            match delta["type"].as_str().unwrap_or("") {
                "text_delta" => {
                    if let Some(t) = delta["text"].as_str() {
                        builder.append_text(idx, t);
                    }
                }
                "thinking_delta" => {
                    if let Some(t) = delta["thinking"].as_str() {
                        builder.append_thinking(idx, t);
                    }
                }
                "signature_delta" => {
                    if let Some(s) = delta["signature"].as_str() {
                        builder.set_thinking_signature(idx, s.to_string());
                    }
                }
                "input_json_delta" => {
                    if let Some(j) = delta["partial_json"].as_str() {
                        builder.append_tool_args(idx, j);
                    }
                }
                _ => {}
            }
            Flow::Continue
        }
        "content_block_stop" => {
            let provider_idx = data["index"].as_u64().unwrap_or(0);
            if let Some(&(idx, kind)) = state.index_map.get(&provider_idx) {
                match kind {
                    BlockKind::Text => builder.end_text(idx),
                    BlockKind::Thinking => builder.end_thinking(idx),
                    BlockKind::ToolUse => builder.end_tool_call(idx, None),
                }
            }
            Flow::Continue
        }
        "message_delta" => {
            if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                builder.message.raw_stop_reason = Some(reason.to_string());
                state.stop_reason = match reason {
                    "end_turn" | "stop_sequence" => StopReason::Stop,
                    "max_tokens" => StopReason::Length,
                    "tool_use" => StopReason::ToolUse,
                    _ => StopReason::Stop,
                };
            }
            if let Some(out) = data["usage"]["output_tokens"].as_u64() {
                builder.message.usage.output = out;
            }
            if let Some(input) = data["usage"]["input_tokens"].as_u64() {
                builder.message.usage.input = input;
            }
            if let Some(cache_read) = data["usage"]["cache_read_input_tokens"].as_u64() {
                builder.message.usage.cache_read = cache_read;
                builder.message.usage.cache_read_available = true;
            }
            if let Some(cache_write) = data["usage"]["cache_creation_input_tokens"].as_u64() {
                builder.message.usage.cache_write = cache_write;
                builder.message.usage.cache_read_available = true;
            }
            if let Some(cache_write_1h) =
                data["usage"]["cache_creation"]["ephemeral_1h_input_tokens"].as_u64()
            {
                builder.message.usage.cache_write_1h = cache_write_1h;
            }
            if state.openrouter {
                super::apply_openrouter_cost(&mut builder.message.usage, &data["usage"]);
            }
            // A refusal is not a finished answer. Report it as an error, as
            // the Chat Completions adapter does for content_filter.
            if let Some(reason @ ("refusal" | "sensitive")) =
                builder.message.raw_stop_reason.as_deref()
            {
                return Flow::Error(format!(
                    "Provider stop_reason: {reason}. The model declined to answer this request"
                ));
            }
            Flow::Continue
        }
        "message_stop" => Flow::Done,
        "error" => {
            let msg = data["error"]["message"]
                .as_str()
                .unwrap_or("provider error");
            Flow::Error(match data["error"]["type"].as_str() {
                Some(kind) => format!("{kind}: {msg}"),
                None => msg.to_string(),
            })
        }
        _ => Flow::Continue,
    }
}

const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

fn supports_mid_convo_effort(model: &Model) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_mid_convo_effort)
        .unwrap_or(false)
}

fn adaptive_effort(model: &Model, level: crate::ThinkingLevel) -> String {
    if let Some(Some(mapped)) = model.thinking_level_map.get(level.as_str()) {
        return mapped.clone();
    }
    match level {
        crate::ThinkingLevel::Minimal | crate::ThinkingLevel::Low => "low",
        crate::ThinkingLevel::Medium => "medium",
        crate::ThinkingLevel::High => "high",
        crate::ThinkingLevel::Xhigh => "xhigh",
        crate::ThinkingLevel::Max => "max",
        crate::ThinkingLevel::Off => "high",
    }
    .into()
}

fn beta_features(model: &Model, oauth: bool, fast_mode: bool) -> Vec<&'static str> {
    let mut features = if oauth {
        vec!["claude-code-20250219", "oauth-2025-04-20"]
    } else {
        Vec::new()
    };
    if supports_mid_convo_effort(model) {
        features.extend([
            MID_CONVERSATION_OUTPUT_CONFIG_BETA,
            THINKING_BINDING_CONTROLS_BETA,
        ]);
    }
    if fast_mode && model.supports_fast_mode() {
        features.push(FAST_MODE_BETA);
    }
    features
}

fn build_request(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "model": model.id,
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
        "stream": true,
    });
    if options.fast_mode && model.supports_fast_mode() {
        body["speed"] = json!("fast");
    }

    if let Some(system) = &context.system_prompt {
        // Cache breakpoint on the system prompt: stable prefix across turns.
        body["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    let compat = model.compat.as_ref();
    let managed_effort = supports_mid_convo_effort(model);
    let adaptive = compat
        .and_then(|compat| compat.force_adaptive_thinking)
        .unwrap_or(false);
    if let Some(t) = options.temperature
        && options.reasoning == crate::ThinkingLevel::Off
        && !managed_effort
        && compat
            .and_then(|compat| compat.supports_temperature)
            .unwrap_or(true)
    {
        body["temperature"] = json!(t);
    }
    if managed_effort {
        body["thinking"] = json!({
            "type": "adaptive",
            "display": "summarized",
            "block_binding": {"prefix_mismatch_behavior": "drop_block"},
        });
        // Effort markers set the level of each reply. The top-level effort is
        // part of the cached prompt, so it stays fixed. Organizations can cap
        // effort and reject a higher top-level value, so it is "low".
        body["output_config"] = json!({"effort": "low"});
    } else if model.reasoning && options.reasoning != crate::ThinkingLevel::Off && adaptive {
        body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        body["output_config"] = json!({"effort": adaptive_effort(model, options.reasoning)});
    } else if model.reasoning
        && let Some(budget) = thinking_budget(
            options.reasoning,
            options.max_tokens.unwrap_or(model.max_tokens),
        )
    {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
    if !context.tools.is_empty() {
        body["tools"] = Value::Array(
            context
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect(),
        );
    }
    if let Some(session) = &options.session_id {
        body["metadata"] = json!({"user_id": session});
    }

    let mut messages: Vec<Value> = Vec::new();
    // Anthropic rejects images in error tool results. Send them after the
    // turn's tool results, in the same user message.
    let mut error_images: Vec<Value> = Vec::new();
    for message in &context.messages {
        if !matches!(message, Message::ToolResult(_)) && !error_images.is_empty() {
            append_error_images(&mut messages, &mut error_images);
        }
        match message {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(t) => json!([{ "type": "text", "text": t }]),
                    UserContent::Blocks(blocks) => {
                        Value::Array(blocks.iter().filter_map(user_block).collect())
                    }
                };
                messages.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant(assistant) => {
                let mut content: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if !text.is_empty() {
                                content.push(json!({"type": "text", "text": text}));
                            }
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if *redacted {
                                if let Some(sig) = thinking_signature {
                                    content.push(json!({"type": "redacted_thinking", "data": sig}));
                                }
                            } else if let Some(sig) = thinking_signature {
                                content.push(json!({"type": "thinking", "thinking": thinking, "signature": sig}));
                            } else if compat
                                .and_then(|compat| compat.allow_empty_signature)
                                .unwrap_or(false)
                            {
                                content.push(json!({"type": "thinking", "thinking": thinking, "signature": ""}));
                            }
                        }
                        ContentBlock::ToolCall(tc) => {
                            content.push(json!({
                                "type": "tool_use",
                                "id": super::anthropic_tool_id(&tc.id),
                                "name": tc.name,
                                "input": tc.arguments,
                            }));
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !content.is_empty() {
                    if managed_effort
                        && assistant.api == "anthropic-messages"
                        && assistant.provider == model.provider
                        && let Some(effort) = assistant.provider_thinking_level.as_deref()
                        && matches!(effort, "low" | "medium" | "high" | "xhigh" | "max")
                    {
                        messages.push(json!({
                            "role": "system",
                            "content": [],
                            "output_config": {"effort": effort},
                        }));
                    }
                    messages.push(json!({"role": "assistant", "content": content}));
                }
            }
            Message::ToolResult(result) => {
                let has_images = result
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image { .. }));
                let inner: Vec<Value> = if result.is_error && has_images {
                    error_images.push(json!({
                        "type": "text",
                        "text": format!(
                            "Images from the error result of tool call {} ({}):",
                            result.tool_call_id, result.tool_name
                        ),
                    }));
                    let mut inner = Vec::new();
                    for block in &result.content {
                        if matches!(block, ContentBlock::Image { .. }) {
                            error_images.extend(user_block(block));
                            if inner.last().is_none_or(|last: &Value| {
                                last["text"] != ERROR_TOOL_IMAGE_PLACEHOLDER
                            }) {
                                inner.push(
                                    json!({"type": "text", "text": ERROR_TOOL_IMAGE_PLACEHOLDER}),
                                );
                            }
                        } else {
                            inner.extend(user_block(block));
                        }
                    }
                    inner
                } else {
                    result.content.iter().filter_map(user_block).collect()
                };
                let tool_result = json!({
                    "type": "tool_result",
                    "tool_use_id": super::anthropic_tool_id(&result.tool_call_id),
                    "content": inner,
                    "is_error": result.is_error,
                });
                // Merge consecutive tool results into one user message.
                if let Some(last) = messages.last_mut()
                    && last["role"] == "user"
                    && last["content"]
                        .as_array()
                        .is_some_and(|a| a.iter().all(|c| c["type"] == "tool_result"))
                {
                    last["content"].as_array_mut().unwrap().push(tool_result);
                    continue;
                }
                messages.push(json!({"role": "user", "content": [tool_result]}));
            }
        }
    }
    append_error_images(&mut messages, &mut error_images);

    // Cache breakpoint on the final message so the conversation prefix caches.
    if let Some(last) = messages.last_mut()
        && let Some(parts) = last["content"].as_array_mut()
        && let Some(last_part) = parts.last_mut()
    {
        last_part["cache_control"] = json!({"type": "ephemeral"});
    }

    if managed_effort {
        messages.push(json!({
            "role": "system",
            "content": [],
            "output_config": {"effort": adaptive_effort(model, options.reasoning)},
        }));
    }

    body["messages"] = Value::Array(messages);
    body
}

const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image attached after the tool results)";

fn append_error_images(messages: &mut [Value], error_images: &mut Vec<Value>) {
    if let Some(content) = messages
        .last_mut()
        .and_then(|last| last["content"].as_array_mut())
    {
        content.append(error_images);
    }
    error_images.clear();
}

fn user_block(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text, .. } => Some(json!({"type": "text", "text": text})),
        ContentBlock::Image { data, mime_type } => Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mime_type, "data": data},
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OpenAICompat;
    use crate::types::{AssistantMessage, UserMessage};
    use std::collections::BTreeMap;

    fn model(compat: OpenAICompat) -> Model {
        Model {
            id: "claude-test".into(),
            name: "Claude Test".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 1_000_000,
            max_tokens: 128_000,
            compat: Some(compat),
            thinking_level_map: BTreeMap::new(),
            headers: BTreeMap::new(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        }
    }

    #[test]
    fn message_delta_updates_final_cache_usage() {
        let model = model(OpenAICompat::default());
        let (sink, _stream) = crate::EventStream::channel();
        let mut builder = PartialBuilder::new(&model, sink);
        let mut state = DecodeState::default();

        handle_event(
            &SseEvent {
                event: Some("message_start".into()),
                data: json!({
                    "message": {
                        "usage": {
                            "input_tokens": 10,
                            "cache_read_input_tokens": 20,
                            "cache_creation_input_tokens": 30,
                            "cache_creation": {"ephemeral_1h_input_tokens": 12}
                        }
                    }
                })
                .to_string(),
            },
            &mut builder,
            &mut state,
        );
        handle_event(
            &SseEvent {
                event: Some("message_delta".into()),
                data: json!({
                    "usage": {
                        "input_tokens": 12,
                        "cache_read_input_tokens": 40,
                        "cache_creation_input_tokens": 5,
                        "cache_creation": {"ephemeral_1h_input_tokens": 2},
                        "output_tokens": 7
                    }
                })
                .to_string(),
            },
            &mut builder,
            &mut state,
        );

        assert_eq!(builder.message.usage.input, 12);
        assert_eq!(builder.message.usage.cache_read, 40);
        assert_eq!(builder.message.usage.cache_write, 5);
        assert_eq!(builder.message.usage.cache_write_1h, 2);
        assert_eq!(builder.message.usage.output, 7);
        assert!(builder.message.usage.cache_read_available);
    }

    #[test]
    fn a_refusal_ends_the_stream_as_an_error() {
        let model = model(OpenAICompat::default());
        let (sink, _stream) = crate::EventStream::channel();
        let mut builder = PartialBuilder::new(&model, sink);
        let flow = handle_event(
            &SseEvent {
                event: Some("message_delta".into()),
                data: json!({"delta": {"stop_reason": "refusal"}}).to_string(),
            },
            &mut builder,
            &mut DecodeState::default(),
        );
        assert!(matches!(flow, Flow::Error(message) if message.contains("refusal")));
    }

    #[tokio::test]
    async fn federated_token_uses_bearer_auth_and_the_oauth_beta() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        crate::stream::ensure_tls_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 8192];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..read]);
            }
            let body = "{\"type\":\"error\",\"error\":{\"message\":\"stop\"}}";
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).to_ascii_lowercase()
        });
        let mut model = model(OpenAICompat::default());
        model.base_url = format!("http://{address}");
        let options = StreamOptions {
            credential: Some(crate::ResolvedCredential::bearer(format!(
                "{}workload-token",
                crate::auth::anthropic_federation::TOKEN_PREFIX
            ))),
            ..Default::default()
        };
        let _ = crate::stream_simple(&model, &Context::default(), &options)
            .result()
            .await;
        let head = server.await.unwrap();
        assert!(
            head.contains("authorization: bearer workload-token"),
            "{head}"
        );
        assert!(head.contains("anthropic-beta: oauth-2025-04-20"), "{head}");
        assert!(!head.contains("x-api-key"), "{head}");
        assert!(!head.contains("claude-cli"), "{head}");
    }

    #[test]
    fn error_tool_result_images_follow_the_tool_results() {
        let tool_result = |id: &str, is_error: bool| {
            Message::ToolResult(crate::types::ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "read".into(),
                content: vec![
                    ContentBlock::Text {
                        text: "failed".into(),
                        text_signature: None,
                    },
                    ContentBlock::Image {
                        data: "aW1n".into(),
                        mime_type: "image/png".into(),
                    },
                ],
                details: None,
                usage: None,
                is_error,
                timestamp: 0,
            })
        };
        let context = Context {
            messages: vec![tool_result("a", true), tool_result("b", false)],
            ..Default::default()
        };
        let body = build_request(
            &model(OpenAICompat::default()),
            &context,
            &StreamOptions::default(),
        );
        let content = body["messages"][0]["content"].as_array().unwrap();
        let types: Vec<&str> = content
            .iter()
            .map(|part| part["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["tool_result", "tool_result", "text", "image"]);
        assert_eq!(
            content[0]["content"][1]["text"],
            ERROR_TOOL_IMAGE_PLACEHOLDER
        );
        assert_eq!(content[1]["content"][1]["type"], "image");
    }

    #[test]
    fn foreign_tool_ids_are_sanitized() {
        let id = "call_abc|fc_123";
        let mut previous = AssistantMessage::empty("openai-codex-responses", "openai-codex", "gpt");
        previous
            .content
            .push(ContentBlock::ToolCall(crate::types::ToolCall {
                id: id.into(),
                name: "bash".into(),
                arguments: json!({}),
                thought_signature: None,
                arguments_error: None,
            }));
        let body = build_request(
            &model(OpenAICompat::default()),
            &Context {
                messages: vec![
                    Message::Assistant(previous),
                    Message::ToolResult(crate::types::ToolResultMessage {
                        tool_call_id: id.into(),
                        tool_name: "bash".into(),
                        content: vec![ContentBlock::text("ok")],
                        details: None,
                        usage: None,
                        is_error: false,
                        timestamp: 0,
                    }),
                ],
                ..Default::default()
            },
            &StreamOptions::default(),
        );
        assert_eq!(body["messages"][0]["content"][0]["id"], "call_abc_fc_123");
        assert_eq!(
            body["messages"][1]["content"][0]["tool_use_id"],
            "call_abc_fc_123"
        );
    }

    #[test]
    fn empty_signature_compat_replays_unsigned_thinking() {
        let model = model(OpenAICompat {
            allow_empty_signature: Some(true),
            ..Default::default()
        });
        let mut previous = AssistantMessage::empty("anthropic-messages", "relay", "model");
        previous.content.push(ContentBlock::Thinking {
            thinking: "reasoning".into(),
            thinking_signature: None,
            redacted: false,
        });
        let body = build_request(
            &model,
            &Context {
                messages: vec![Message::Assistant(previous)],
                ..Default::default()
            },
            &StreamOptions::default(),
        );
        assert_eq!(body["messages"][0]["content"][0]["signature"], "");
    }

    #[test]
    fn managed_effort_replays_historical_and_active_levels() {
        let model = model(OpenAICompat {
            supports_mid_convo_effort: Some(true),
            force_adaptive_thinking: Some(true),
            ..Default::default()
        });
        let mut previous =
            AssistantMessage::empty("anthropic-messages", "anthropic", "claude-test");
        previous.content.push(ContentBlock::text("answer"));
        previous.provider_thinking_level = Some("low".into());
        let context = Context {
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("first".into()),
                    timestamp: 1,
                }),
                Message::Assistant(previous),
                Message::User(UserMessage {
                    content: UserContent::Text("next".into()),
                    timestamp: 2,
                }),
            ],
            ..Default::default()
        };
        let options = StreamOptions {
            reasoning: crate::ThinkingLevel::Xhigh,
            ..Default::default()
        };
        let body = build_request(&model, &context, &options);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(
            body["thinking"]["block_binding"]["prefix_mismatch_behavior"],
            "drop_block"
        );
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["messages"][1]["role"], "system");
        assert_eq!(body["messages"][1]["output_config"]["effort"], "low");
        assert_eq!(
            body["messages"][3]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(body["messages"][4]["role"], "system");
        assert_eq!(body["messages"][4]["output_config"]["effort"], "xhigh");
        assert!(beta_features(&model, false, false).contains(&MID_CONVERSATION_OUTPUT_CONFIG_BETA));
        assert!(beta_features(&model, false, false).contains(&THINKING_BINDING_CONTROLS_BETA));
    }

    #[test]
    fn adaptive_and_budget_models_keep_distinct_request_shapes() {
        let adaptive = model(OpenAICompat {
            force_adaptive_thinking: Some(true),
            supports_temperature: Some(false),
            ..Default::default()
        });
        let options = StreamOptions {
            reasoning: crate::ThinkingLevel::Max,
            temperature: Some(0.5),
            ..Default::default()
        };
        let body = build_request(&adaptive, &Context::default(), &options);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "max");
        assert!(body.get("temperature").is_none());

        let budget = model(OpenAICompat::default());
        let body = build_request(&budget, &Context::default(), &options);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body["thinking"]["budget_tokens"].as_u64().is_some());
    }

    #[test]
    fn fast_mode_adds_speed_and_beta_for_supported_models() {
        let mut model = model(OpenAICompat::default());
        model.id = "claude-opus-4-6".into();
        let normal = build_request(&model, &Context::default(), &StreamOptions::default());
        assert!(normal.get("speed").is_none());
        assert!(!beta_features(&model, false, false).contains(&FAST_MODE_BETA));

        let fast = build_request(
            &model,
            &Context::default(),
            &StreamOptions {
                fast_mode: true,
                ..Default::default()
            },
        );
        assert_eq!(fast["speed"], "fast");
        assert!(beta_features(&model, false, true).contains(&FAST_MODE_BETA));
    }
}
