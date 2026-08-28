//! OpenAI Responses remote compaction v2.
//!
//! This keeps a portable local summary in the coding layer and returns the
//! opaque provider history that compatible OpenAI turns can replay.

use super::{finalize_cost, provider_base_url};
use crate::api::openai_responses::{build_request, response_url};
use crate::auth::openai_codex::decode_jwt_account_id;
use crate::model::Model;
use crate::sse::{SseEvent, SseParser};
use crate::stream::{StreamOptions, http_client};
use crate::types::{Context, Usage};
use anyhow::{Context as _, Result};
use futures::StreamExt;
use serde_json::{Map, Value, json};

const REMOTE_COMPACTION_FEATURE: &str = "remote_compaction_v2";
const RETAINED_USER_TOKEN_BUDGET: usize = 20_000;

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteCompactionResult {
    pub replacement_history: Vec<Value>,
    pub usage: Option<Usage>,
}

pub fn model_key(model: &Model) -> String {
    format!("{}:{}:{}", model.provider, model.api, model.id)
}

pub fn supports_remote_compaction(model: &Model) -> bool {
    match model.api.as_str() {
        "openai-responses" if model.provider == "openai" => official_host(model, "api.openai.com"),
        "openai-codex-responses" if model.provider == "openai-codex" => {
            official_host(model, "chatgpt.com")
        }
        _ => false,
    }
}

fn official_host(model: &Model, expected: &str) -> bool {
    if model.base_url.trim().is_empty() {
        return true;
    }
    url::Url::parse(&model.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == expected)
}

pub fn build_remote_compaction_details(model: &Model, result: &RemoteCompactionResult) -> Value {
    let mut remote = Map::from_iter([
        ("version".into(), json!(2)),
        ("provider".into(), json!("openai-responses-compaction")),
        ("implementation".into(), json!("responses_compaction_v2")),
        ("modelKey".into(), json!(model_key(model))),
        (
            "replacementHistory".into(),
            Value::Array(result.replacement_history.clone()),
        ),
    ]);
    if let Some(usage) = result.usage {
        remote.insert(
            "usage".into(),
            serde_json::to_value(usage).unwrap_or_default(),
        );
    }
    json!({"remoteCompaction": remote})
}

pub async fn compact(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<RemoteCompactionResult> {
    if !supports_remote_compaction(model) {
        anyhow::bail!("remote compaction is not supported for this model");
    }
    let api_key = options
        .api_key
        .as_deref()
        .context("OpenAI remote compaction needs an API key")?;
    let mut endpoint_model = model.clone();
    endpoint_model.base_url = provider_base_url(model, api_key);
    let url = response_url(&endpoint_model)?;
    compact_at_url(model, context, options, &url).await
}

async fn compact_at_url(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    url: &str,
) -> Result<RemoteCompactionResult> {
    let api_key = options
        .api_key
        .as_deref()
        .context("OpenAI remote compaction needs an API key")?;
    let body = build_remote_request(model, context, options);

    let mut request = http_client()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("x-codex-beta-features", beta_features(model))
        .bearer_auth(api_key)
        .json(&body);
    for (name, value) in &model.headers {
        if !name.eq_ignore_ascii_case("x-codex-beta-features") {
            request = request.header(name, value);
        }
    }
    request = request.header("x-codex-installation-id", codex_installation_id());
    if let Some(session_id) = options.session_id.as_deref() {
        request = request
            .header("session_id", session_id)
            .header("x-codex-window-id", format!("{session_id}:0"));
    }
    if model.api == "openai-codex-responses" {
        let account_id = decode_jwt_account_id(api_key)
            .context("OpenAI Codex access token has no account ID")?;
        request = request
            .header("chatgpt-account-id", account_id)
            .header("originator", "kiss")
            .header("user-agent", concat!("kiss/", env!("CARGO_PKG_VERSION")))
            .header("OpenAI-Beta", "responses=experimental");
        if let Some(session_id) = options.session_id.as_deref() {
            request = request
                .header("session-id", session_id)
                .header("x-client-request-id", session_id);
        }
    }

    let response = tokio::select! {
        response = request.send() => response?,
        _ = options.cancel.cancelled() => anyhow::bail!("OpenAI remote compaction was cancelled"),
    };
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "OpenAI remote compaction failed ({status}): {}",
            crate::truncate_err(&text)
        );
    }

    let input = body["input"].as_array().cloned().unwrap_or_default();
    let input = &input[..input.len().saturating_sub(1)];
    let mut parser = SseParser::new();
    let mut events = Vec::new();
    let mut compaction_items = Vec::new();
    let mut usage = None;
    let mut completed = false;
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = options.cancel.cancelled() => anyhow::bail!("OpenAI remote compaction was cancelled"),
        };
        let Some(chunk) = chunk else { break };
        parser.feed(&chunk?, &mut events);
        for event in events.drain(..) {
            parse_remote_event(&event, &mut compaction_items, &mut usage, &mut completed)?;
        }
    }
    parser.finish(&mut events);
    for event in events.drain(..) {
        parse_remote_event(&event, &mut compaction_items, &mut usage, &mut completed)?;
    }
    if !completed {
        anyhow::bail!("OpenAI remote compaction stream ended before response.completed");
    }
    if compaction_items.len() != 1 {
        anyhow::bail!(
            "OpenAI remote compaction expected exactly one compaction item, got {}",
            compaction_items.len()
        );
    }
    let compaction_item = compaction_items.pop().unwrap();
    Ok(RemoteCompactionResult {
        replacement_history: build_replacement_history(input, compaction_item)?,
        usage: usage.map(|value| parse_usage(model, &value)),
    })
}

fn codex_installation_id() -> String {
    let root = std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")));
    if let Some(path) = root.map(|root| root.join("installation_id"))
        && let Ok(value) = std::fs::read_to_string(path)
        && let Ok(id) = uuid::Uuid::parse_str(value.trim())
    {
        return id.to_string();
    }
    static FALLBACK_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FALLBACK_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

fn build_remote_request(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = build_request(model, context, options);
    let input = body["input"].as_array_mut().expect("Responses input array");
    input.push(json!({"type": "compaction_trigger"}));
    body["stream"] = json!(true);
    body["store"] = json!(false);
    body["parallel_tool_calls"] = json!(true);
    body["tool_choice"] = json!("auto");
    body["include"] = json!(["reasoning.encrypted_content"]);
    if body.get("tools").is_none() {
        body["tools"] = json!([]);
    }
    if let Some(object) = body.as_object_mut() {
        object.remove("max_output_tokens");
        object.remove("temperature");
    }
    body
}

fn beta_features(model: &Model) -> String {
    let configured = model
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-codex-beta-features"))
        .map(|(_, value)| value.as_str())
        .unwrap_or_default();
    let mut features = configured
        .split(',')
        .map(str::trim)
        .filter(|feature| !feature.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !features
        .iter()
        .any(|feature| feature == REMOTE_COMPACTION_FEATURE)
    {
        features.push(REMOTE_COMPACTION_FEATURE.into());
    }
    features.join(",")
}

fn parse_remote_event(
    event: &SseEvent,
    compaction_items: &mut Vec<Value>,
    usage: &mut Option<Value>,
    completed: &mut bool,
) -> Result<()> {
    if event.data == "[DONE]" || event.data.trim().is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(&event.data)
        .with_context(|| "parse OpenAI remote compaction SSE event")?;
    match value["type"].as_str().unwrap_or_default() {
        "error" => anyhow::bail!(
            "OpenAI remote compaction failed: {}",
            value["message"]
                .as_str()
                .unwrap_or("unknown Responses error")
        ),
        "response.failed" => anyhow::bail!(
            "OpenAI remote compaction failed: {}",
            value["response"]["error"]["message"]
                .as_str()
                .unwrap_or("Responses request failed")
        ),
        "response.output_item.done" if value["item"]["type"] == "compaction" => {
            compaction_items.push(value["item"].clone());
        }
        "response.completed" => {
            *completed = true;
            if !value["response"]["usage"].is_null() {
                *usage = Some(value["response"]["usage"].clone());
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_usage(model: &Model, value: &Value) -> Usage {
    let input_tokens = value["input_tokens"].as_u64().unwrap_or(0);
    let cached = value["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    let cache_write = value["input_tokens_details"]["cache_creation_tokens"]
        .as_u64()
        .or_else(|| value["input_tokens_details"]["cache_write_tokens"].as_u64())
        .unwrap_or(0);
    let mut usage = Usage {
        input: input_tokens
            .saturating_sub(cached)
            .saturating_sub(cache_write),
        output: value["output_tokens"].as_u64().unwrap_or(0),
        cache_read: cached,
        cache_write,
        reasoning: value["output_tokens_details"]["reasoning_tokens"].as_u64(),
        ..Default::default()
    };
    finalize_cost(&mut usage, model);
    usage
}

fn build_replacement_history(input: &[Value], compaction_item: Value) -> Result<Vec<Value>> {
    if compaction_item["type"] != "compaction" {
        anyhow::bail!("OpenAI remote compaction did not return a compaction item");
    }
    let users = input
        .iter()
        .filter(|item| {
            item["type"] == "message"
                && item["role"] == "user"
                && item["content"]
                    .as_array()
                    .is_some_and(|content| !content.is_empty())
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut retained = truncate_user_messages(&users, RETAINED_USER_TOKEN_BUDGET);
    retained.push(compaction_item);
    Ok(retained)
}

fn truncate_user_messages(items: &[Value], max_tokens: usize) -> Vec<Value> {
    let mut remaining = max_tokens;
    let mut retained = Vec::new();
    for item in items.iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = approximate_message_tokens(item);
        if tokens <= remaining {
            retained.push(item.clone());
            remaining -= tokens;
        } else if let Some(item) = truncate_message(item, remaining * 4) {
            retained.push(item);
            remaining = 0;
        }
    }
    retained.reverse();
    retained
}

fn approximate_message_tokens(item: &Value) -> usize {
    let chars = item["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .map(str::len)
        .sum::<usize>();
    chars.div_ceil(4).max(1)
}

fn truncate_message(item: &Value, max_chars: usize) -> Option<Value> {
    let mut item = item.clone();
    let content = item["content"].as_array_mut()?;
    let mut remaining = max_chars;
    content.retain_mut(|part| {
        if part["type"] == "input_image" {
            return true;
        }
        let Some(text) = part["text"].as_str() else {
            return false;
        };
        if remaining == 0 {
            return false;
        }
        let kept = text.chars().take(remaining).collect::<String>();
        remaining = remaining.saturating_sub(kept.chars().count());
        part["text"] = Value::String(kept);
        !part["text"].as_str().unwrap_or_default().is_empty()
    });
    (!content.is_empty()).then_some(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelCost, ThinkingLevel, ToolDef, UserContent, UserMessage};
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    fn model(provider: &str, api: &str, base_url: &str) -> Model {
        Model {
            id: "gpt-test".into(),
            name: "GPT test".into(),
            api: api.into(),
            provider: provider.into(),
            base_url: base_url.into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.5,
                cache_write: 1.5,
            },
            context_window: 100_000,
            max_tokens: 1_000,
            compat: None,
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn only_official_openai_responses_models_are_supported() {
        assert!(supports_remote_compaction(&model(
            "openai",
            "openai-responses",
            "https://api.openai.com/v1"
        )));
        assert!(supports_remote_compaction(&model(
            "openai-codex",
            "openai-codex-responses",
            "https://chatgpt.com/backend-api"
        )));
        assert!(!supports_remote_compaction(&model(
            "custom",
            "openai-responses",
            "https://example.com/v1"
        )));
        assert!(!supports_remote_compaction(&model(
            "anthropic",
            "anthropic-messages",
            "https://api.anthropic.com"
        )));
    }

    #[test]
    fn compaction_request_mirrors_normal_shape_and_adds_trigger() {
        let context = Context {
            system_prompt: Some("system".into()),
            openai_responses_input: Some(vec![json!({
                "type": "compaction",
                "encrypted_content": "old"
            })]),
            messages: vec![crate::Message::User(UserMessage {
                content: UserContent::Text("continue".into()),
                timestamp: 1,
            })],
            tools: vec![ToolDef {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: json!({"type": "object"}),
            }],
        };
        let options = StreamOptions {
            reasoning: ThinkingLevel::High,
            session_id: Some("session-1".into()),
            ..Default::default()
        };
        let body = build_remote_request(
            &model("openai", "openai-responses", "https://api.openai.com/v1"),
            &context,
            &options,
        );
        assert_eq!(body["input"][0]["type"], "compaction");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][2]["type"], "compaction_trigger");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["store"], false);
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn parser_requires_one_item_and_completion() {
        let mut items = Vec::new();
        let mut usage = None;
        let mut completed = false;
        parse_remote_event(
            &SseEvent {
                event: None,
                data: json!({
                    "type": "response.output_item.done",
                    "item": {"type": "compaction", "encrypted_content": "opaque"}
                })
                .to_string(),
            },
            &mut items,
            &mut usage,
            &mut completed,
        )
        .unwrap();
        parse_remote_event(
            &SseEvent {
                event: None,
                data: json!({
                    "type": "response.completed",
                    "response": {"usage": {"input_tokens": 10, "output_tokens": 2}}
                })
                .to_string(),
            },
            &mut items,
            &mut usage,
            &mut completed,
        )
        .unwrap();
        assert!(completed);
        assert_eq!(items.len(), 1);
        assert_eq!(usage.unwrap()["input_tokens"], 10);
    }

    #[test]
    fn replacement_history_keeps_recent_users_and_compaction() {
        let old = "x".repeat(60_000);
        let recent = "y".repeat(60_000);
        let input = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":old}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":recent}]}),
        ];
        let history = build_replacement_history(
            &input,
            json!({"type":"compaction","encrypted_content":"opaque"}),
        )
        .unwrap();
        assert_eq!(history.last().unwrap()["type"], "compaction");
        assert_eq!(history.len(), 3);
        let retained_chars = history[..2]
            .iter()
            .flat_map(|item| item["content"].as_array().unwrap())
            .filter_map(|part| part["text"].as_str())
            .map(str::len)
            .sum::<usize>();
        assert_eq!(retained_chars, RETAINED_USER_TOKEN_BUDGET * 4);
    }

    #[test]
    fn details_use_reference_wire_shape() {
        let model = model("openai", "openai-responses", "https://api.openai.com/v1");
        let details = build_remote_compaction_details(
            &model,
            &RemoteCompactionResult {
                replacement_history: vec![json!({"type":"compaction"})],
                usage: None,
            },
        );
        assert_eq!(details["remoteCompaction"]["version"], 2);
        assert_eq!(
            details["remoteCompaction"]["implementation"],
            "responses_compaction_v2"
        );
        assert_eq!(
            details["remoteCompaction"]["modelKey"],
            "openai:openai-responses:gpt-test"
        );
    }

    #[tokio::test]
    async fn remote_client_sends_trigger_and_parses_stream() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let output = json!({
                "type": "response.output_item.done",
                "item": {"type": "compaction", "encrypted_content": "opaque"}
            });
            let completed = json!({
                "type": "response.completed",
                "response": {
                    "usage": {
                        "input_tokens": 20,
                        "input_tokens_details": {"cached_tokens": 5},
                        "output_tokens": 3,
                        "output_tokens_details": {"reasoning_tokens": 2}
                    }
                }
            });
            let body = format!("data: {output}\n\ndata: {completed}\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });

        let model = model("openai", "openai-responses", "https://api.openai.com/v1");
        let context = Context {
            system_prompt: Some("system".into()),
            openai_responses_input: None,
            messages: vec![crate::Message::User(UserMessage {
                content: UserContent::Text("remember this".into()),
                timestamp: 1,
            })],
            tools: vec![],
        };
        let options = StreamOptions {
            api_key: Some("secret".into()),
            session_id: Some("session-1".into()),
            ..Default::default()
        };
        let result = compact_at_url(
            &model,
            &context,
            &options,
            &format!("http://{address}/v1/responses"),
        )
        .await
        .unwrap();
        let request = server.await.unwrap();
        let header_end = request
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap();
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer secret"));
        assert!(headers.contains("x-codex-beta-features: remote_compaction_v2"));
        assert!(headers.contains("session_id: session-1"));
        let body: Value = serde_json::from_slice(&request[header_end + 4..]).unwrap();
        assert_eq!(
            body["input"].as_array().unwrap().last().unwrap()["type"],
            "compaction_trigger"
        );
        assert_eq!(body["store"], false);
        assert_eq!(
            result.replacement_history.last().unwrap()["type"],
            "compaction"
        );
        let usage = result.usage.unwrap();
        assert_eq!(usage.input, 15);
        assert_eq!(usage.cache_read, 5);
        assert_eq!(usage.output, 3);
        assert_eq!(usage.reasoning, Some(2));
    }
}
