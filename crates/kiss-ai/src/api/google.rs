//! Google Generative AI (Gemini) adapter.

use super::{PartialBuilder, thinking_budget};
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
                "no API key for provider {} (set GEMINI_API_KEY)",
                model.provider
            ),
            false,
            model,
        );
        return;
    };
    let body = build_request(model, context, options);
    let url = match google_url(model) {
        Ok(url) => url,
        Err(error) => {
            builder.fail(error.to_string(), false, model);
            return;
        }
    };
    let mut request = http_client()
        .post(&url)
        .header("content-type", "application/json")
        .json(&body);
    if model.api == "google-vertex" {
        if let Some(token) = api_key.strip_prefix("vertex-oauth:") {
            request = request.bearer_auth(token);
        } else {
            request = request.header("x-goog-api-key", &api_key);
        }
    } else {
        request = request.header("x-goog-api-key", &api_key);
    }
    for (k, v) in &model.headers {
        request = request.header(k, v);
    }

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

    if let Some(idx) = state.text_idx.take() {
        builder.end_text(idx);
    }
    if let Some(idx) = state.thinking_idx.take() {
        builder.end_thinking(idx);
    }
    builder.message.raw_stop_reason = state.finish_reason.clone();
    let stop = match state.finish_reason.as_deref() {
        Some("MAX_TOKENS") => StopReason::Length,
        Some("STOP") | None => StopReason::Stop,
        Some(other) => {
            builder.fail(format!("generation stopped: {other}"), false, model);
            return;
        }
    };
    builder.finish(stop, model);
}

fn google_url(model: &Model) -> anyhow::Result<String> {
    if model.api != "google-vertex" {
        return Ok(format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            model.base_url.trim_end_matches('/'),
            model.id
        ));
    }
    let project = crate::auth::provider_env("google-vertex", "GOOGLE_CLOUD_PROJECT")
        .or_else(|| crate::auth::provider_env("google-vertex", "GCLOUD_PROJECT"))
        .ok_or_else(|| anyhow::anyhow!("Google Vertex needs GOOGLE_CLOUD_PROJECT"))?;
    let location = crate::auth::provider_env("google-vertex", "GOOGLE_CLOUD_LOCATION")
        .ok_or_else(|| anyhow::anyhow!("Google Vertex needs GOOGLE_CLOUD_LOCATION"))?;
    Ok(vertex_url(model, &project, &location))
}

fn vertex_url(model: &Model, project: &str, location: &str) -> String {
    format!(
        "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent?alt=sse",
        model.id
    )
}

#[derive(Default)]
struct DecodeState {
    text_idx: Option<usize>,
    thinking_idx: Option<usize>,
    finish_reason: Option<String>,
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
        return Err(err["message"]
            .as_str()
            .unwrap_or("provider error")
            .to_string());
    }
    if let Some(usage) = chunk.get("usageMetadata") {
        let cached = usage["cachedContentTokenCount"].as_u64().unwrap_or(0);
        let prompt = usage["promptTokenCount"].as_u64().unwrap_or(0);
        builder.message.usage.input = prompt.saturating_sub(cached);
        builder.message.usage.cache_read = cached;
        let thoughts = usage["thoughtsTokenCount"].as_u64().unwrap_or(0);
        builder.message.usage.output =
            usage["candidatesTokenCount"].as_u64().unwrap_or(0) + thoughts;
        if thoughts > 0 {
            builder.message.usage.reasoning = Some(thoughts);
        }
    }
    if let Some(id) = chunk["responseId"].as_str() {
        builder.message.response_id = Some(id.to_string());
    }
    let Some(candidate) = chunk["candidates"].as_array().and_then(|c| c.first()) else {
        return Ok(());
    };
    if let Some(reason) = candidate["finishReason"].as_str() {
        state.finish_reason = Some(reason.to_string());
    }
    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            if let Some(fc) = part.get("functionCall") {
                if let Some(t_idx) = state.thinking_idx.take() {
                    builder.end_thinking(t_idx);
                }
                if let Some(idx) = state.text_idx.take() {
                    builder.end_text(idx);
                }
                let name = fc["name"].as_str().unwrap_or_default().to_string();
                let id = format!("call_{}", uuid_ish());
                let idx = builder.begin_tool_call(id, name);
                if let Some(sig) = part["thoughtSignature"].as_str() {
                    builder.set_tool_thought_signature(idx, sig.to_string());
                }
                builder.end_tool_call(idx, Some(fc["args"].clone()));
            } else if let Some(text) = part["text"].as_str() {
                if part["thought"].as_bool() == Some(true) {
                    let idx = *state
                        .thinking_idx
                        .get_or_insert_with(|| builder.begin_thinking());
                    builder.append_thinking(idx, text);
                    if let Some(sig) = part["thoughtSignature"].as_str() {
                        builder.set_thinking_signature(idx, sig.to_string());
                    }
                } else {
                    if let Some(t_idx) = state.thinking_idx.take() {
                        builder.end_thinking(t_idx);
                    }
                    let idx = *state.text_idx.get_or_insert_with(|| builder.begin_text());
                    builder.append_text(idx, text);
                }
            }
        }
    }
    Ok(())
}

fn uuid_ish() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:x}{:08x}", chrono::Utc::now().timestamp_micros(), nanos)
}

fn build_request(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    for message in &context.messages {
        match message {
            Message::User(user) => {
                let parts = match &user.content {
                    UserContent::Text(t) => json!([{ "text": t }]),
                    UserContent::Blocks(blocks) => Value::Array(
                        blocks
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text, .. } => Some(json!({"text": text})),
                                ContentBlock::Image { data, mime_type } => Some(json!({
                                    "inlineData": {"mimeType": mime_type, "data": data},
                                })),
                                _ => None,
                            })
                            .collect(),
                    ),
                };
                contents.push(json!({"role": "user", "parts": parts}));
            }
            Message::Assistant(assistant) => {
                let mut parts: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if !text.is_empty() {
                                parts.push(json!({"text": text}));
                            }
                        }
                        ContentBlock::ToolCall(tc) => {
                            let mut part = json!({
                                "functionCall": {"name": tc.name, "args": tc.arguments},
                            });
                            if let Some(sig) = &tc.thought_signature {
                                part["thoughtSignature"] = json!(sig);
                            }
                            parts.push(part);
                        }
                        // Thinking text is not replayed; signatures ride on tool calls.
                        ContentBlock::Thinking { .. } | ContentBlock::Image { .. } => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Message::ToolResult(result) => {
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": result.tool_name,
                            "response": {"output": text},
                        },
                    }],
                }));
            }
        }
    }

    let mut generation_config = json!({
        "maxOutputTokens": options.max_tokens.unwrap_or(model.max_tokens),
    });
    if let Some(t) = options.temperature {
        generation_config["temperature"] = json!(t);
    }
    if model.reasoning
        && let Some(budget) = thinking_budget(
            options.reasoning,
            options.max_tokens.unwrap_or(model.max_tokens),
        )
    {
        generation_config["thinkingConfig"] = json!({
            "thinkingBudget": budget,
            "includeThoughts": true,
        });
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": generation_config,
    });
    if let Some(system) = &context.system_prompt {
        body["systemInstruction"] = json!({"parts": [{"text": system}]});
    }
    if !context.tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": context
                .tools
                .iter()
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": sanitize_schema(&t.parameters),
                }))
                .collect::<Vec<Value>>(),
        }]);
    }
    body
}

/// Gemini rejects some JSON-schema keywords; strip the unsupported ones.
fn sanitize_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if matches!(k.as_str(), "additionalProperties" | "$schema" | "default") {
                    continue;
                }
                out.insert(k.clone(), sanitize_schema(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_schema).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod vertex_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn builds_vertex_stream_url() {
        let model = Model {
            id: "gemini-test".into(),
            name: String::new(),
            api: "google-vertex".into(),
            provider: "google-vertex".into(),
            base_url: "https://{location}-aiplatform.googleapis.com".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            context_window: 100,
            max_tokens: 10,
            compat: None,
            thinking_level_map: BTreeMap::new(),
            headers: BTreeMap::new(),
        };
        assert_eq!(
            vertex_url(&model, "project-one", "us-central1"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/project-one/locations/us-central1/publishers/google/models/gemini-test:streamGenerateContent?alt=sse"
        );
    }
}
