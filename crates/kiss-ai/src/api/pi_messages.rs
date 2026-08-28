//! Pi Messages adapter used by Radius and compatible gateways.

use super::PartialBuilder;
use crate::event::EventSink;
use crate::model::Model;
use crate::sse::{SseEvent, SseParser};
use crate::stream::{StreamOptions, http_client};
use crate::types::{ContentBlock, Context, StopReason, ToolCall, Usage};
use futures::StreamExt as _;
use serde_json::{Value, json};

pub async fn stream(model: &Model, context: &Context, options: &StreamOptions, sink: EventSink) {
    let mut builder = PartialBuilder::new(model, sink);
    let Some(api_key) = options.api_key.as_ref() else {
        builder.fail(
            format!("no API key for provider {}", model.provider),
            false,
            model,
        );
        return;
    };
    let url = format!("{}/messages", model.base_url.trim_end_matches('/'));
    let body = json!({
        "model": model.id,
        "context": {
            "systemPrompt": context.system_prompt,
            "messages": context.messages,
            "tools": context.tools,
        },
        "options": {
            "temperature": options.temperature,
            "maxTokens": options.max_tokens,
            "reasoning": options.reasoning.as_str(),
            "sessionId": options.session_id,
        }
    });
    let mut request = http_client()
        .post(url)
        .bearer_auth(api_key)
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .json(&body);
    for (name, value) in &model.headers {
        request = request.header(name, value);
    }
    let response = tokio::select! {
        response = request.send() => response,
        _ = options.cancel.cancelled() => {
            builder.fail("Request aborted", true, model);
            return;
        }
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            builder.fail(format!("request failed: {error}"), false, model);
            return;
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        builder.fail(
            format!("HTTP {status}: {}", crate::truncate_err(&body)),
            false,
            model,
        );
        return;
    }

    let mut parser = SseParser::new();
    let mut parsed = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = options.cancel.cancelled() => {
                builder.fail("Request aborted", true, model);
                return;
            }
        };
        match chunk {
            Some(Ok(bytes)) => {
                parser.feed(&bytes, &mut parsed);
                for event in parsed.drain(..) {
                    if let Some(terminal) = handle_event(&event, &mut builder) {
                        finish_terminal(terminal, builder, model);
                        return;
                    }
                }
            }
            Some(Err(error)) => {
                builder.fail(format!("stream failed: {error}"), false, model);
                return;
            }
            None => break,
        }
    }
    parser.finish(&mut parsed);
    for event in parsed.drain(..) {
        if let Some(terminal) = handle_event(&event, &mut builder) {
            finish_terminal(terminal, builder, model);
            return;
        }
    }
    builder.fail(
        "gateway stream ended without a terminal event",
        false,
        model,
    );
}

enum Terminal {
    Done(StopReason),
    Error { message: String, aborted: bool },
}

fn handle_event(event: &SseEvent, builder: &mut PartialBuilder) -> Option<Terminal> {
    let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
        return None;
    };
    let index = || data["contentIndex"].as_u64().unwrap_or_default() as usize;
    match data["type"].as_str().unwrap_or_default() {
        "start" => builder.start(),
        "text_start" => {
            let actual = builder.begin_text();
            debug_assert_eq!(actual, index());
        }
        "text_delta" => {
            if let Some(delta) = data["delta"].as_str() {
                builder.append_text(index(), delta);
            }
        }
        "text_end" => {
            if let Some(signature) = data["contentSignature"].as_str() {
                builder.set_text_signature(index(), signature.to_string());
            }
            builder.end_text(index());
        }
        "thinking_start" => {
            let actual = builder.begin_thinking();
            debug_assert_eq!(actual, index());
        }
        "thinking_delta" => {
            if let Some(delta) = data["delta"].as_str() {
                builder.append_thinking(index(), delta);
            }
        }
        "thinking_end" => {
            if let Some(signature) = data["contentSignature"].as_str() {
                builder.set_thinking_signature(index(), signature.to_string());
            }
            if let Some(ContentBlock::Thinking { redacted, .. }) =
                builder.message.content.get_mut(index())
            {
                *redacted = data["redacted"].as_bool().unwrap_or(false);
            }
            builder.end_thinking(index());
        }
        "toolcall_start" => {
            let actual = builder.begin_tool_call(
                data["id"].as_str().unwrap_or_default().to_string(),
                data["toolName"].as_str().unwrap_or_default().to_string(),
            );
            debug_assert_eq!(actual, index());
        }
        "toolcall_delta" => {
            if let Some(delta) = data["delta"].as_str() {
                builder.append_tool_args(index(), delta);
            }
        }
        "toolcall_end" => {
            let tool_call = serde_json::from_value::<ToolCall>(data["toolCall"].clone()).ok();
            builder.end_tool_call(index(), tool_call.map(|call| call.arguments));
        }
        "done" => {
            apply_terminal_fields(&data, builder);
            return Some(Terminal::Done(parse_stop_reason(&data["reason"])));
        }
        "error" => {
            apply_terminal_fields(&data, builder);
            let aborted = data["reason"] == "aborted";
            return Some(Terminal::Error {
                message: data["errorMessage"]
                    .as_str()
                    .unwrap_or("gateway request failed")
                    .to_string(),
                aborted,
            });
        }
        _ => {}
    }
    None
}

fn finish_terminal(terminal: Terminal, builder: PartialBuilder, model: &Model) {
    match terminal {
        Terminal::Done(reason) => builder.finish(reason, model),
        Terminal::Error { message, aborted } => builder.fail(message, aborted, model),
    }
}

fn apply_terminal_fields(data: &Value, builder: &mut PartialBuilder) {
    if let Ok(usage) = serde_json::from_value::<Usage>(data["usage"].clone()) {
        builder.message.usage = usage;
    }
    if let Some(response_id) = data["responseId"].as_str() {
        builder.message.response_id = Some(response_id.to_string());
    }
}

fn parse_stop_reason(value: &Value) -> StopReason {
    match value.as_str() {
        Some("length") => StopReason::Length,
        Some("toolUse") => StopReason::ToolUse,
        _ => StopReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, UserContent, UserMessage};
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn sends_pi_messages_payload_and_decodes_gateway_events() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..count]);
                let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or_default();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            let events = [
                json!({"type":"start"}),
                json!({"type":"text_start","contentIndex":0}),
                json!({"type":"text_delta","contentIndex":0,"delta":"hello"}),
                json!({"type":"text_end","contentIndex":0,"content":"hello"}),
                json!({
                    "type":"done",
                    "reason":"stop",
                    "responseId":"radius-response",
                    "usage":{
                        "input":4,"output":1,"cacheRead":0,"cacheWrite":0,
                        "totalTokens":5,
                        "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}
                    }
                }),
            ];
            let body = events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });

        let model = Model {
            id: "radius-model".into(),
            name: "Radius model".into(),
            api: "pi-messages".into(),
            provider: "radius".into(),
            base_url: format!("http://{address}"),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            context_window: 1000,
            max_tokens: 100,
            compat: None,
            headers: BTreeMap::new(),
        };
        let context = Context {
            system_prompt: Some("system".into()),
            openai_responses_input: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("hi".into()),
                timestamp: 1,
            })],
            tools: vec![],
        };
        let output = crate::stream_simple(
            &model,
            &context,
            &StreamOptions {
                api_key: Some("radius-key".into()),
                ..Default::default()
            },
        )
        .result()
        .await;
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert_eq!(output.text(), "hello");
        assert_eq!(output.response_id.as_deref(), Some("radius-response"));
        assert_eq!(output.usage.input, 4);

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /messages HTTP/1.1"));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer radius-key")
        );
        let payload: Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(payload["model"], "radius-model");
        assert_eq!(payload["context"]["systemPrompt"], "system");
        assert_eq!(payload["context"]["messages"][0]["role"], "user");
    }
}
