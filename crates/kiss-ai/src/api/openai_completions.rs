//! OpenAI Chat Completions adapter, covering OpenAI-compatible servers
//! (llama.cpp, vLLM, Ollama, Groq, Cerebras, OpenRouter, DeepSeek, ...)
//! through the compat matrix on the model.

use super::{PartialBuilder, apply_provider_headers, provider_base_url, reasoning_effort};
use crate::event::EventSink;
use crate::model::Model;
use crate::sse::{SseEvent, SseParser};
use crate::stream::{StreamOptions, http_client};
use crate::types::{ContentBlock, Context, Message, StopReason, UserContent};
use futures::StreamExt;
use serde_json::{Value, json};

/// Effective compat flags after URL-based auto-detection.
struct Compat {
    developer_role: bool,
    reasoning_effort: bool,
    usage_in_streaming: bool,
    finish_reason: bool,
    reasoning_content_on_assistant: bool,
    thinking_format: Option<String>,
    max_tokens_field: &'static str,
}

fn detect_compat(model: &Model) -> Compat {
    let url = model.base_url.to_lowercase();
    let is_openai = url.contains("api.openai.com");
    let c = model.compat.clone().unwrap_or_default();
    Compat {
        developer_role: c.supports_developer_role.unwrap_or(is_openai),
        reasoning_effort: c
            .supports_reasoning_effort
            .unwrap_or(is_openai || url.contains("openrouter")),
        usage_in_streaming: c.supports_usage_in_streaming.unwrap_or(true),
        finish_reason: c.supports_finish_reason.unwrap_or(true),
        reasoning_content_on_assistant: c
            .requires_reasoning_content_on_assistant_messages
            .unwrap_or(false),
        thinking_format: c.thinking_format,
        max_tokens_field: match c.max_tokens_field.as_deref() {
            Some("max_tokens") => "max_tokens",
            Some("max_completion_tokens") => "max_completion_tokens",
            _ if is_openai => "max_completion_tokens",
            _ => "max_tokens",
        },
    }
}

pub async fn stream(model: &Model, context: &Context, options: &StreamOptions, sink: EventSink) {
    let mut builder = PartialBuilder::new(model, sink);
    let Some(api_key) = options.api_key.clone() else {
        builder.fail(
            format!("no API key for provider {}", model.provider),
            false,
            model,
        );
        return;
    };
    let compat = detect_compat(model);
    let body = build_request(model, context, options, &compat);
    let base_url = provider_base_url(model, &api_key);
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut request = http_client()
        .post(&url)
        .bearer_auth(&api_key)
        .header("content-type", "application/json")
        .json(&body);
    for (k, v) in &model.headers {
        request = request.header(k, v);
    }
    request = apply_provider_headers(request, model, context);

    let response = tokio::select! {
        r = request.send() => r,
        _ = options.cancel.cancelled() => { builder.fail("Request aborted", true, model); return; }
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
    builder.start();

    loop {
        let chunk = tokio::select! {
            c = body_stream.next() => c,
            _ = options.cancel.cancelled() => { builder.fail("Request aborted", true, model); return; }
        };
        match chunk {
            Some(Ok(bytes)) => {
                parser.feed(&bytes, &mut events);
                for event in events.drain(..) {
                    if event.data == "[DONE]" {
                        finish(builder, state, model, compat.finish_reason);
                        return;
                    }
                    if let Err(msg) = handle_chunk(&event.data, &mut builder, &mut state) {
                        builder.fail(msg, false, model);
                        return;
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
    finish(builder, state, model, compat.finish_reason);
}

#[derive(Default)]
struct DecodeState {
    text_idx: Option<usize>,
    thinking_idx: Option<usize>,
    reasoning_details: Vec<Value>,
    /// Provider tool-call index -> builder content index.
    tool_map: std::collections::HashMap<u64, usize>,
    finish_reason: Option<String>,
}

fn is_reasoning_detail(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("reasoning.text") => object.get("text").is_some_and(Value::is_string),
        Some("reasoning.summary") => object.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => object.get("data").is_some_and(Value::is_string),
        _ => false,
    }
}

fn fill_missing_reasoning_fields(
    target: &mut serde_json::Map<String, Value>,
    source: &serde_json::Map<String, Value>,
) {
    for field in ["id", "format", "index"] {
        let missing = target.get(field).is_none_or(|value| {
            value.is_null() || (field == "format" && value.as_str().is_some_and(str::is_empty))
        });
        if missing && let Some(value) = source.get(field) {
            target.insert(field.into(), value.clone());
        }
    }
}

fn append_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    if !is_reasoning_detail(detail) {
        return;
    }
    let detail_type = detail["type"].as_str().unwrap_or_default();
    if let (Some(last), Some(source)) = (details.last_mut(), detail.as_object())
        && last["type"].as_str() == Some(detail_type)
        && matches!(detail_type, "reasoning.text" | "reasoning.summary")
    {
        let field = if detail_type == "reasoning.text" {
            "text"
        } else {
            "summary"
        };
        if let Some(target) = last.as_object_mut() {
            let suffix = source
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut merged = target
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            merged.push_str(suffix);
            target.insert(field.into(), Value::String(merged));
            if detail_type == "reasoning.text"
                && target
                    .get("signature")
                    .is_none_or(|value| value.is_null() || value.as_str() == Some(""))
                && let Some(signature) = source.get("signature")
            {
                target.insert("signature".into(), signature.clone());
            }
            fill_missing_reasoning_fields(target, source);
            return;
        }
    }
    details.push(detail.clone());
}

fn finish_thinking(builder: &mut PartialBuilder, state: &mut DecodeState) {
    let Some(idx) = state.thinking_idx.take() else {
        return;
    };
    if !state.reasoning_details.is_empty()
        && let Ok(signature) = serde_json::to_string(&state.reasoning_details)
    {
        builder.set_thinking_signature(idx, signature);
    }
    builder.end_thinking(idx);
}

fn parse_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let details: Vec<Value> = serde_json::from_str(signature?).ok()?;
    (!details.is_empty() && details.iter().all(is_reasoning_detail)).then_some(details)
}

fn finish(
    mut builder: PartialBuilder,
    mut state: DecodeState,
    model: &Model,
    finish_reason_required: bool,
) {
    if let Some(idx) = state.text_idx {
        builder.end_text(idx);
    }
    finish_thinking(&mut builder, &mut state);
    let mut tool_indexes: Vec<usize> = state.tool_map.values().copied().collect();
    tool_indexes.sort_unstable();
    for idx in tool_indexes {
        builder.end_tool_call(idx, None);
    }
    let has_tools = !state.tool_map.is_empty();
    builder.message.raw_stop_reason = state.finish_reason.clone();
    if finish_reason_required && state.finish_reason.is_none() {
        builder.fail("Stream ended without finish_reason", false, model);
        return;
    }
    let stop = match state.finish_reason.as_deref() {
        Some("length") => StopReason::Length,
        Some("tool_calls") => StopReason::ToolUse,
        Some(_) => StopReason::Stop,
        // Some servers omit finish_reason; infer from content.
        None if has_tools => StopReason::ToolUse,
        None => StopReason::Stop,
    };
    builder.finish(stop, model);
}

fn handle_chunk(
    data: &str,
    builder: &mut PartialBuilder,
    state: &mut DecodeState,
) -> Result<(), String> {
    let chunk: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if let Some(err) = chunk.get("error") {
        let msg = err["message"].as_str().unwrap_or("provider error");
        return Err(msg.to_string());
    }
    if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
        let cached = usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
        builder.message.usage.input = prompt.saturating_sub(cached);
        builder.message.usage.cache_read = cached;
        builder.message.usage.output = usage["completion_tokens"].as_u64().unwrap_or(0);
        if let Some(r) = usage["completion_tokens_details"]["reasoning_tokens"].as_u64() {
            builder.message.usage.reasoning = Some(r);
        }
    }
    if let Some(id) = chunk["id"].as_str() {
        builder.message.response_id = Some(id.to_string());
    }
    if let Some(m) = chunk["model"].as_str()
        && m != builder.message.model
    {
        builder.message.response_model = Some(m.to_string());
    }
    let Some(choice) = chunk["choices"].as_array().and_then(|c| c.first()) else {
        return Ok(());
    };
    if let Some(reason) = choice["finish_reason"].as_str() {
        state.finish_reason = Some(reason.to_string());
    }
    let delta = &choice["delta"];

    // DeepSeek-style reasoning stream.
    if let Some(reasoning) = delta["reasoning_content"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        let idx = *state
            .thinking_idx
            .get_or_insert_with(|| builder.begin_thinking());
        builder.set_thinking_signature(idx, "reasoning_content".into());
        builder.append_thinking(idx, reasoning);
    }
    if let Some(details) = delta["reasoning_details"].as_array() {
        if details.iter().any(is_reasoning_detail) {
            state
                .thinking_idx
                .get_or_insert_with(|| builder.begin_thinking());
        }
        for detail in details {
            append_reasoning_detail(&mut state.reasoning_details, detail);
        }
    }
    if let Some(text) = delta["content"].as_str().filter(|s| !s.is_empty()) {
        finish_thinking(builder, state);
        let idx = *state.text_idx.get_or_insert_with(|| builder.begin_text());
        builder.append_text(idx, text);
    }
    if let Some(tool_calls) = delta["tool_calls"].as_array() {
        finish_thinking(builder, state);
        for tc in tool_calls {
            let provider_idx = tc["index"].as_u64().unwrap_or(0);
            let idx = match state.tool_map.get(&provider_idx) {
                Some(&i) => i,
                None => {
                    let id = tc["id"].as_str().unwrap_or_default().to_string();
                    let name = tc["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let i = builder.begin_tool_call(id, name);
                    state.tool_map.insert(provider_idx, i);
                    i
                }
            };
            if let Some(args) = tc["function"]["arguments"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                builder.append_tool_args(idx, args);
            }
        }
    }
    Ok(())
}

fn build_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &Compat,
) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &context.system_prompt {
        let role = if compat.developer_role && model.reasoning {
            "developer"
        } else {
            "system"
        };
        messages.push(json!({"role": role, "content": system}));
    }
    for message in &context.messages {
        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(t) => messages.push(json!({"role": "user", "content": t})),
                UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text, .. } => {
                                Some(json!({"type": "text", "text": text}))
                            }
                            ContentBlock::Image { data, mime_type } => Some(json!({
                                "type": "image_url",
                                "image_url": {"url": format!("data:{mime_type};base64,{data}")},
                            })),
                            _ => None,
                        })
                        .collect();
                    messages.push(json!({"role": "user", "content": parts}));
                }
            },
            Message::Assistant(assistant) => {
                let text = assistant.text();
                let thinking: Vec<(&str, Option<&str>)> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            ..
                        } => Some((thinking.as_str(), thinking_signature.as_deref())),
                        _ => None,
                    })
                    .collect();
                let reasoning_details = thinking
                    .iter()
                    .find_map(|(_, signature)| parse_reasoning_details(*signature));
                let tool_calls: Vec<Value> = assistant
                    .tool_calls()
                    .map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".into()),
                            },
                        })
                    })
                    .collect();
                let mut m = json!({"role": "assistant"});
                m["content"] = if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text)
                };
                if !tool_calls.is_empty() {
                    m["tool_calls"] = Value::Array(tool_calls);
                }
                if let Some(details) = reasoning_details {
                    m["reasoning_details"] = Value::Array(details);
                } else if let Some(field) = thinking.iter().find_map(|(_, signature)| {
                    signature.filter(|field| {
                        matches!(*field, "reasoning" | "reasoning_content" | "reasoning_text")
                    })
                }) {
                    let reasoning = thinking
                        .iter()
                        .map(|(text, _)| *text)
                        .filter(|text| !text.trim().is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !reasoning.is_empty() {
                        m[field] = Value::String(reasoning);
                    }
                }
                if compat.reasoning_content_on_assistant && m.get("reasoning_content").is_none() {
                    m["reasoning_content"] = Value::String(String::new());
                }
                messages.push(m);
            }
            Message::ToolResult(result) => {
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        ContentBlock::Image { .. } => Some("[image attached]"),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": result.tool_call_id,
                    "content": text,
                }));
            }
        }
    }

    let mut body = json!({
        "model": model.id,
        "messages": messages,
        "stream": true,
    });
    if compat.usage_in_streaming {
        body["stream_options"] = json!({"include_usage": true});
    }
    body[compat.max_tokens_field] = json!(options.max_tokens.unwrap_or(model.max_tokens));
    if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if model.reasoning {
        let mapped_effort = model
            .thinking_level_map
            .get(options.reasoning.as_str())
            .map_or_else(
                || reasoning_effort(options.reasoning).map(str::to_string),
                Clone::clone,
            );
        match compat.thinking_format.as_deref() {
            Some("openrouter") => {
                if let Some(effort) = mapped_effort {
                    body["reasoning"] = json!({"effort": effort});
                }
            }
            Some("deepseek") => {
                if options.reasoning != crate::ThinkingLevel::Off {
                    body["thinking"] = json!({"type": "enabled"});
                } else if model.thinking_level_map.get("off") != Some(&None) {
                    body["thinking"] = json!({"type": "disabled"});
                }
                if compat.reasoning_effort
                    && let Some(effort) = mapped_effort
                {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            _ if compat.reasoning_effort => {
                if let Some(effort) = mapped_effort {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            _ => {}
        }
    }
    if !context.tools.is_empty() {
        body["tools"] = Value::Array(
            context
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        },
                    })
                })
                .collect(),
        );
    }
    if let Some(choice) = &options.tool_choice {
        body["tool_choice"] = choice.openai_chat_value();
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OpenAICompat;

    fn model_with_compat(compat: OpenAICompat) -> Model {
        Model {
            id: "model".into(),
            name: "Model".into(),
            api: "openai-completions".into(),
            provider: "custom".into(),
            base_url: "http://localhost/v1".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            context_window: 1_000,
            max_tokens: 100,
            compat: Some(compat),
            thinking_level_map: Default::default(),
            headers: Default::default(),
        }
    }

    #[test]
    fn finish_reason_requirement_can_be_overridden() {
        let required = detect_compat(&model_with_compat(OpenAICompat::default()));
        assert!(required.finish_reason);

        let lenient = detect_compat(&model_with_compat(OpenAICompat {
            supports_finish_reason: Some(false),
            ..Default::default()
        }));
        assert!(!lenient.finish_reason);
    }

    #[test]
    fn explicit_tool_choice_is_sent_without_tool_definitions() {
        let model = model_with_compat(OpenAICompat::default());
        let context = Context {
            system_prompt: None,
            openai_responses_input: None,
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let options = StreamOptions {
            tool_choice: Some(crate::ToolChoice::Function("read".into())),
            ..Default::default()
        };
        let body = build_request(&model, &context, &options, &detect_compat(&model));
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["function"]["name"], "read");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn reasoning_detail_deltas_merge_and_replay() {
        let model = model_with_compat(OpenAICompat::default());
        let (sink, _stream) = crate::EventStream::channel();
        let mut builder = PartialBuilder::new(&model, sink);
        let mut state = DecodeState::default();
        for delta in [
            json!({"reasoning_details":[{"type":"reasoning.text","text":"The","index":0}]}),
            json!({"reasoning_details":[{"type":"reasoning.text","text":" answer","signature":"sig","format":"v1","index":0}]}),
            json!({"reasoning_details":[{"type":"reasoning.summary","summary":"Short","index":0}]}),
            json!({"reasoning_details":[{"type":"reasoning.summary","summary":" summary","format":"v1","index":0}]}),
            json!({"reasoning_details":[{"type":"reasoning.encrypted","data":"opaque","id":"r1"}]}),
        ] {
            handle_chunk(
                &json!({"choices":[{"delta":delta}]}).to_string(),
                &mut builder,
                &mut state,
            )
            .unwrap();
        }
        finish_thinking(&mut builder, &mut state);
        let ContentBlock::Thinking {
            thinking_signature: Some(signature),
            ..
        } = &builder.message.content[0]
        else {
            panic!("expected signed thinking block");
        };
        let details: Value = serde_json::from_str(signature).unwrap();
        assert_eq!(details[0]["text"], "The answer");
        assert_eq!(details[0]["signature"], "sig");
        assert_eq!(details[0]["format"], "v1");
        assert_eq!(details[1]["summary"], "Short summary");
        assert_eq!(details[2]["data"], "opaque");

        let context = Context {
            system_prompt: None,
            openai_responses_input: None,
            messages: vec![Message::Assistant(builder.message)],
            tools: Vec::new(),
        };
        let body = build_request(
            &model,
            &context,
            &StreamOptions::default(),
            &detect_compat(&model),
        );
        assert_eq!(body["messages"][0]["reasoning_details"], details);
    }

    #[test]
    fn openrouter_uses_nested_mapped_reasoning_effort() {
        let mut model = model_with_compat(OpenAICompat {
            thinking_format: Some("openrouter".into()),
            ..Default::default()
        });
        model.reasoning = true;
        model
            .thinking_level_map
            .insert("off".into(), Some("none".into()));
        model
            .thinking_level_map
            .insert("high".into(), Some("high".into()));
        let context = Context {
            system_prompt: None,
            openai_responses_input: None,
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let high = build_request(
            &model,
            &context,
            &StreamOptions {
                reasoning: crate::ThinkingLevel::High,
                ..Default::default()
            },
            &detect_compat(&model),
        );
        assert_eq!(high["reasoning"]["effort"], "high");
        assert!(high.get("reasoning_effort").is_none());

        let off = build_request(
            &model,
            &context,
            &StreamOptions::default(),
            &detect_compat(&model),
        );
        assert_eq!(off["reasoning"]["effort"], "none");

        model.thinking_level_map.insert("off".into(), None);
        let mandatory = build_request(
            &model,
            &context,
            &StreamOptions::default(),
            &detect_compat(&model),
        );
        assert!(mandatory.get("reasoning").is_none());
    }
}
