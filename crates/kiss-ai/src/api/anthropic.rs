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
    let Some(api_key) = options.api_key.clone() else {
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

    let body = build_request(model, context, options);
    let oauth = model.provider == "anthropic"
        && crate::auth::is_oauth_access_token(&model.provider, &api_key);
    let bearer = crate::auth::is_bearer_access_token(&model.provider, &api_key);
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
            .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
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
    request = apply_provider_headers(request, model, context);

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
            builder.fail(format!("request failed: {e}"), false, model);
            return;
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        builder.fail(
            format!("HTTP {status}: {}", crate::truncate_err(&text)),
            false,
            model,
        );
        return;
    }

    let mut parser = SseParser::new();
    let mut events: Vec<SseEvent> = Vec::new();
    let mut body_stream = response.bytes_stream();
    let mut state = DecodeState::default();

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
                builder.fail(format!("stream error: {e}"), false, model);
                return;
            }
            None => break,
        }
    }
    // Stream ended without message_stop.
    builder.finish(state.stop_reason, model);
}

#[derive(Default)]
struct DecodeState {
    /// Provider content index -> our builder content index.
    index_map: std::collections::HashMap<u64, (usize, BlockKind)>,
    stop_reason: StopReason,
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
                    let name = super::claude_code::local_tool_name(
                        block["name"].as_str().unwrap_or_default(),
                    );
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
            let Some(&(idx, kind)) = state.index_map.get(&provider_idx) else {
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
            let _ = kind;
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
            Flow::Continue
        }
        "message_stop" => Flow::Done,
        "error" => {
            let msg = data["error"]["message"]
                .as_str()
                .unwrap_or("provider error")
                .to_string();
            Flow::Error(msg)
        }
        _ => Flow::Continue,
    }
}

fn build_request(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "model": model.id,
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
        "stream": true,
    });

    if let Some(system) = &context.system_prompt {
        // Cache breakpoint on the system prompt: stable prefix across turns.
        body["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if model.reasoning
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
    for message in &context.messages {
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
                            }
                        }
                        ContentBlock::ToolCall(tc) => {
                            content.push(json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": tc.arguments,
                            }));
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !content.is_empty() {
                    messages.push(json!({"role": "assistant", "content": content}));
                }
            }
            Message::ToolResult(result) => {
                let inner: Vec<Value> = result.content.iter().filter_map(user_block).collect();
                let tool_result = json!({
                    "type": "tool_result",
                    "tool_use_id": result.tool_call_id,
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

    // Cache breakpoint on the final message so the conversation prefix caches.
    if let Some(last) = messages.last_mut()
        && let Some(parts) = last["content"].as_array_mut()
        && let Some(last_part) = parts.last_mut()
    {
        last_part["cache_control"] = json!({"type": "ephemeral"});
    }

    body["messages"] = Value::Array(messages);
    body
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
