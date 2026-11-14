//! Provider adapters. Each submodule owns the full round trip for one API
//! dialect: request building from the unified `Context`, stream decoding to
//! unified `AssistantEvent`s, usage extraction, and error normalization.

pub mod anthropic;
mod anthropic_threads;
pub mod bedrock;
pub(crate) mod claude_code;
pub mod cursor;
mod cursor_protocol;
pub mod google;
pub mod openai_compaction;
pub mod openai_completions;
pub mod openai_responses;
pub mod pi_messages;

use crate::event::{AssistantEvent, EventSink};
use crate::json_salvage::{parse_salvage, parse_tool_arguments};
use crate::model::Model;
use crate::types::{AssistantMessage, ContentBlock, Cost, StopReason, ToolCall, Usage};
use crate::types::{Context, Message};

/// The text of a stream that broke or ended early. Session retry treats a
/// message that starts this way as transient, so every adapter uses it.
pub(crate) fn stream_error(detail: impl std::fmt::Display) -> String {
    format!("stream error: {detail}")
}

/// An error followed by each cause in its source chain. A transport error
/// often says only "error sending request"; the cause, such as "connection
/// refused", is what the user and the retry rule need.
pub(crate) fn with_causes(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Read a failed HTTP response into `HTTP <status>: <body>`. A 429 also
/// carries the provider's reset hint as ` (retry-after: Ns)` so account
/// failover can record when the rate-limited account becomes usable again.
pub(crate) async fn http_error(response: reqwest::Response) -> String {
    let status = response.status();
    let hint = (status.as_u16() == 429)
        .then(|| retry_after_seconds(response.headers()))
        .flatten()
        .map(|seconds| format!(" (retry-after: {seconds}s)"))
        .unwrap_or_default();
    let body = response.text().await.unwrap_or_default();
    format!("HTTP {status}: {}{hint}", crate::truncate_err(&body))
}

/// Whether an error text reports HTTP status `code`: in `http_error`'s
/// `HTTP <code>` form, the forms SDKs and proxies print (`status <code>`,
/// `status: <code>`, `status code <code>`, `<code> too many requests`), or a
/// JSON body's `"status"` or `"code"` field.
pub fn mentions_status(error: &str, code: u16) -> bool {
    let text = error.to_ascii_lowercase();
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    [
        format!("http {code}"),
        format!("status {code}"),
        format!("status: {code}"),
        format!("status code {code}"),
        format!("{code} too many requests"),
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || compact.contains(&format!("\"status\":{code}"))
        || compact.contains(&format!("\"code\":{code}"))
}

fn retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
    };
    header("retry-after")
        .or_else(|| {
            // Claude subscription limits report the reset as Unix seconds.
            header("anthropic-ratelimit-unified-reset")
                .map(|reset| reset - chrono::Utc::now().timestamp() as f64)
        })
        .filter(|seconds| *seconds > 0.0)
        .map(|seconds| seconds.ceil() as u64)
}

/// Maps a tool call id onto Anthropic/Bedrock's `^[a-zA-Z0-9_-]+$` (max 64)
/// pattern. Ids from other providers (e.g. OpenAI Responses `call_x|fc_y`)
/// otherwise get rejected after a mid-session model switch.
pub(crate) fn anthropic_tool_id(id: &str) -> String {
    id.chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Accumulates a streaming assistant message and emits unified events with
/// partial snapshots, mirroring pi's event protocol.
pub struct PartialBuilder {
    pub message: AssistantMessage,
    sink: EventSink,
    /// Raw argument text per tool-call content index.
    /// Tool calls still receiving arguments: content index and raw JSON.
    tool_args: Vec<(usize, String)>,
    started: bool,
    cost_multiplier: f64,
}

impl PartialBuilder {
    pub fn new(model: &Model, sink: EventSink) -> Self {
        PartialBuilder {
            message: AssistantMessage::empty(&model.api, &model.provider, &model.id),
            sink,
            tool_args: Vec::new(),
            started: false,
            cost_multiplier: 1.0,
        }
    }

    pub fn start(&mut self) {
        if !self.started {
            self.started = true;
            self.sink.send(AssistantEvent::Start {
                partial: self.message.clone(),
            });
        }
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Use the reported tier when present, otherwise the requested tier.
    pub fn set_service_tier(&mut self, tier: &str) {
        if matches!(self.message.provider.as_str(), "openai" | "openai-codex") {
            self.cost_multiplier = match tier {
                "flex" => 0.5,
                "priority" | "fast" if self.message.model == "gpt-5.5" => 2.5,
                "priority" | "fast" => 2.0,
                _ => 1.0,
            };
        }
    }

    fn finalize_usage(&mut self, model: &Model) {
        finalize_cost(&mut self.message.usage, model);
        let cost = &mut self.message.usage.cost;
        cost.input *= self.cost_multiplier;
        cost.output *= self.cost_multiplier;
        cost.cache_read *= self.cost_multiplier;
        cost.cache_write *= self.cost_multiplier;
        cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    }

    pub fn begin_text(&mut self) -> usize {
        self.start();
        self.message.content.push(ContentBlock::text(""));
        let idx = self.message.content.len() - 1;
        self.sink
            .send(AssistantEvent::TextStart { content_index: idx });
        idx
    }

    pub fn append_text(&mut self, idx: usize, delta: &str) {
        if let Some(ContentBlock::Text { text, .. }) = self.message.content.get_mut(idx) {
            text.push_str(delta);
        }
        self.sink.send(AssistantEvent::TextDelta {
            content_index: idx,
            delta: delta.to_string(),
        });
    }

    pub fn end_text(&mut self, idx: usize) {
        let content = match self.message.content.get(idx) {
            Some(ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        };
        self.sink.send(AssistantEvent::TextEnd {
            content_index: idx,
            content,
        });
    }

    pub fn set_text_signature(&mut self, idx: usize, signature: String) {
        if let Some(ContentBlock::Text { text_signature, .. }) = self.message.content.get_mut(idx) {
            *text_signature = Some(signature);
        }
    }

    pub fn begin_thinking(&mut self) -> usize {
        self.start();
        self.message.content.push(ContentBlock::Thinking {
            thinking: String::new(),
            thinking_signature: None,
            redacted: false,
        });
        let idx = self.message.content.len() - 1;
        self.sink
            .send(AssistantEvent::ThinkingStart { content_index: idx });
        idx
    }

    pub fn append_thinking(&mut self, idx: usize, delta: &str) {
        if let Some(ContentBlock::Thinking { thinking, .. }) = self.message.content.get_mut(idx) {
            thinking.push_str(delta);
        }
        self.sink.send(AssistantEvent::ThinkingDelta {
            content_index: idx,
            delta: delta.to_string(),
        });
    }

    pub fn set_thinking_signature(&mut self, idx: usize, signature: String) {
        if let Some(ContentBlock::Thinking {
            thinking_signature, ..
        }) = self.message.content.get_mut(idx)
        {
            *thinking_signature = Some(signature);
        }
    }

    pub fn end_thinking(&mut self, idx: usize) {
        let content = match self.message.content.get(idx) {
            Some(ContentBlock::Thinking { thinking, .. }) => thinking.clone(),
            _ => String::new(),
        };
        self.sink.send(AssistantEvent::ThinkingEnd {
            content_index: idx,
            content,
        });
    }

    pub fn begin_tool_call(&mut self, id: String, name: String) -> usize {
        self.start();
        self.message.content.push(ContentBlock::ToolCall(ToolCall {
            id,
            name,
            arguments: serde_json::Value::Object(Default::default()),
            thought_signature: None,
            arguments_error: None,
        }));
        let idx = self.message.content.len() - 1;
        self.tool_args.push((idx, String::new()));
        let tool_call = match self.message.content.get(idx) {
            Some(ContentBlock::ToolCall(tool_call)) => tool_call.clone(),
            _ => unreachable!("the new content block is a tool call"),
        };
        self.sink.send(AssistantEvent::ToolCallStart {
            content_index: idx,
            tool_call,
        });
        idx
    }

    pub fn append_tool_args(&mut self, idx: usize, delta: &str) {
        if let Some((_, raw)) = self.tool_args.iter_mut().find(|(i, _)| *i == idx) {
            raw.push_str(delta);
        }
        self.sink.send(AssistantEvent::ToolCallDelta {
            content_index: idx,
            delta: delta.to_string(),
        });
    }

    /// Finalize a tool call: parse the complete raw arguments unless the
    /// provider already delivered structured arguments.
    pub fn end_tool_call(&mut self, idx: usize, structured_args: Option<serde_json::Value>) {
        self.finalize_tool_call(idx, structured_args, true);
    }

    /// `complete` is false for a call cut off by the end of the stream. Its
    /// truncated JSON is salvaged for display; the agent loop never runs it.
    fn finalize_tool_call(
        &mut self,
        idx: usize,
        structured_args: Option<serde_json::Value>,
        complete: bool,
    ) {
        let Some(position) = self.tool_args.iter().position(|(i, _)| *i == idx) else {
            // Already finished: a provider can send a second end event.
            return;
        };
        let (_, raw) = self.tool_args.remove(position);
        if let Some(ContentBlock::ToolCall(tc)) = self.message.content.get_mut(idx) {
            let empty = || serde_json::Value::Object(Default::default());
            tc.arguments = match structured_args {
                Some(arguments) => arguments,
                None if complete => parse_tool_arguments(&raw).unwrap_or_else(|error| {
                    tc.arguments_error = Some(error);
                    empty()
                }),
                None => parse_salvage(&raw).unwrap_or_else(empty),
            };
            let tool_call = tc.clone();
            self.sink.send(AssistantEvent::ToolCallEnd {
                content_index: idx,
                tool_call,
            });
        }
    }

    pub fn set_tool_thought_signature(&mut self, idx: usize, signature: String) {
        if let Some(ContentBlock::ToolCall(tc)) = self.message.content.get_mut(idx) {
            tc.thought_signature = Some(signature);
        }
    }

    /// Close any tool call that never got its end event (stream cut off).
    pub fn close_open_blocks(&mut self) {
        let open_tools: Vec<usize> = self.tool_args.iter().map(|(i, _)| *i).collect();
        for idx in open_tools {
            self.finalize_tool_call(idx, None, false);
        }
    }

    pub fn finish(self, stop_reason: StopReason, model: &Model) {
        self.finish_with(stop_reason, model, |_| {});
    }

    /// [`Self::finish`], showing the final message to `inspect` before the
    /// stream ends.
    pub fn finish_with(
        mut self,
        stop_reason: StopReason,
        model: &Model,
        inspect: impl FnOnce(&AssistantMessage),
    ) {
        self.start();
        self.close_open_blocks();
        self.message.stop_reason = stop_reason;
        self.finalize_usage(model);
        if matches!(stop_reason, StopReason::Stop)
            && self
                .message
                .content
                .iter()
                .any(|c| matches!(c, ContentBlock::ToolCall(_)))
        {
            self.message.stop_reason = StopReason::ToolUse;
        }
        inspect(&self.message);
        self.sink.done(self.message);
    }

    pub fn fail(mut self, error: impl Into<String>, aborted: bool, model: &Model) {
        self.start();
        self.close_open_blocks();
        self.message.stop_reason = if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        let mut error = error.into();
        if model.provider == "openai" && error.contains("subscription_sharing_usage_limit_exceeded")
        {
            error.push_str("\nCheck your ChatGPT usage: https://chatgpt.com/settings/usage");
        }
        self.message.error_message = Some(error);
        self.finalize_usage(model);
        self.sink.error(self.message);
    }
}

/// Use OpenRouter's billed cost as the usage total when its accounting is
/// complete. With the user's own upstream key (BYOK), OpenRouter's charge
/// does not include the upstream inference cost, so the total adds it.
pub(crate) fn apply_openrouter_cost(usage: &mut Usage, reported: &serde_json::Value) {
    let valid = |value: &serde_json::Value| {
        value
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
    };
    let Some(cost) = valid(&reported["cost"]) else {
        return;
    };
    let total = match reported["is_byok"].as_bool() {
        Some(false) => cost,
        Some(true) => {
            let details = &reported["cost_details"];
            let upstream = match details.get("upstream_inference_cost") {
                Some(serde_json::Value::Null) => valid(&details["upstream_inference_prompt_cost"])
                    .zip(valid(&details["upstream_inference_completions_cost"]))
                    .map(|(prompt, completion)| prompt + completion),
                Some(value) => valid(value),
                None => None,
            };
            let Some(upstream) = upstream else {
                return;
            };
            cost + upstream
        }
        None => return,
    };
    usage.cost.total = total;
    usage.cost.total_from_provider = true;
}

pub fn finalize_cost(usage: &mut Usage, model: &Model) {
    let per = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    let c = &model.cost;
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let tier = c
        .tiers
        .iter()
        .filter(|tier| input_tokens > tier.input_tokens_above)
        .max_by_key(|tier| tier.input_tokens_above);
    let cost = Cost {
        input: per(usage.input, tier.map_or(c.input, |tier| tier.input)),
        output: per(usage.output, tier.map_or(c.output, |tier| tier.output)),
        cache_read: per(
            usage.cache_read,
            tier.map_or(c.cache_read, |tier| tier.cache_read),
        ),
        cache_write: per(
            usage.cache_write.saturating_sub(usage.cache_write_1h),
            tier.map_or(c.cache_write, |tier| tier.cache_write),
        ) + per(
            usage.cache_write_1h,
            tier.map_or(c.input, |tier| tier.input) * 2.0,
        ),
        total: 0.0,
        total_from_provider: false,
    };
    // A billed total from the provider replaces the catalog estimate.
    let reported = usage.cost.total_from_provider.then_some(usage.cost.total);
    usage.cost = Cost {
        total: reported.unwrap_or(cost.input + cost.output + cost.cache_read + cost.cache_write),
        total_from_provider: reported.is_some(),
        ..cost
    };
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
}

/// Estimated input tokens of a request's messages, as Pi's
/// `estimateContextTokens`: the usage of the newest assistant reply that
/// still describes the prefix, plus about 3.5 characters per token for the
/// messages after it. A message newer than a reply (such as a compaction
/// summary inserted before it) means that reply's usage no longer applies.
fn estimate_context_tokens(messages: &[crate::types::Message]) -> u64 {
    use crate::types::{ContentBlock, Message, StopReason, UserContent};
    let chars = |blocks: &[ContentBlock]| -> usize {
        blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.len(),
                ContentBlock::Thinking { thinking, .. } => thinking.len(),
                ContentBlock::ToolCall(call) => call.name.len() + call.arguments.to_string().len(),
                ContentBlock::Image { .. } => 4800,
            })
            .sum()
    };
    let estimate = |message: &Message| -> u64 {
        let chars = match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => text.len(),
                UserContent::Blocks(blocks) => chars(blocks),
            },
            Message::Assistant(assistant) => chars(&assistant.content),
            Message::ToolResult(result) => chars(&result.content),
        };
        (chars as f64 / 3.5).ceil() as u64
    };
    let usage_tokens = |usage: &crate::types::Usage| {
        if usage.total_tokens > 0 {
            usage.total_tokens
        } else {
            usage.input + usage.output + usage.cache_read + usage.cache_write
        }
    };
    let mut latest_prefix = i64::MIN;
    let mut last_usage = None;
    for (index, message) in messages.iter().enumerate() {
        let timestamp = match message {
            Message::User(user) => user.timestamp,
            Message::Assistant(assistant) => assistant.timestamp,
            Message::ToolResult(result) => result.timestamp,
        };
        if let Message::Assistant(assistant) = message
            && assistant.timestamp >= latest_prefix
            && !matches!(
                assistant.stop_reason,
                StopReason::Aborted | StopReason::Error
            )
            && usage_tokens(&assistant.usage) > 0
        {
            last_usage = Some((index, usage_tokens(&assistant.usage)));
        }
        latest_prefix = latest_prefix.max(timestamp);
    }
    match last_usage {
        Some((index, tokens)) => tokens + messages[index + 1..].iter().map(estimate).sum::<u64>(),
        None => messages.iter().map(estimate).sum(),
    }
}

/// The output limit for a request: `max_tokens` fit into the context window
/// with room for the input being counted higher than estimated, as Pi's
/// `clampMaxTokensToContext`. OpenRouter counts input with its own
/// tokenizer, up to about 12% above the provider's count, and rejects input
/// plus `max_tokens` above the window. The margin is the larger of 4096
/// tokens and 15% of the input while that leaves at least 32768 tokens of
/// output. Otherwise only 4096 tokens are kept free and output is capped at
/// 32768.
pub fn request_max_tokens(
    model: &Model,
    context: &Context,
    options: &crate::stream::StreamOptions,
) -> u64 {
    const SAFETY_TOKENS: i64 = 4096;
    const SAFETY_RATIO: f64 = 0.15;
    const MIN_OUTPUT_WITH_RATIO_MARGIN: i64 = 32768;
    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens).max(1);
    if model.context_window == 0 {
        return max_tokens;
    }
    let window = model.context_window as i64;
    let input = estimate_context_tokens(&context.messages) as i64;
    let margin = SAFETY_TOKENS.max((input as f64 * SAFETY_RATIO).ceil() as i64);
    let mut room = window - input - margin;
    if room < MIN_OUTPUT_WITH_RATIO_MARGIN {
        room = MIN_OUTPUT_WITH_RATIO_MARGIN.min(window - input - SAFETY_TOKENS);
    }
    max_tokens.min(room.max(1) as u64)
}

/// Thinking-level token budgets (Anthropic + Google token-budget style).
pub fn thinking_budget(level: crate::types::ThinkingLevel, max_tokens: u64) -> Option<u64> {
    use crate::types::ThinkingLevel::*;
    let budget: u64 = match level {
        Off => return None,
        Minimal => 1024,
        Low => 4096,
        Medium => 10_240,
        High => 32_768,
        Xhigh => 65_536,
        Max => 131_072,
    };
    Some(budget.min(max_tokens.saturating_sub(1024).max(1024)))
}

/// Reasoning-effort string for effort-based providers (OpenAI style).
pub fn reasoning_effort(level: crate::types::ThinkingLevel) -> Option<&'static str> {
    use crate::types::ThinkingLevel::*;
    match level {
        Off => None,
        Minimal => Some("minimal"),
        Low => Some("low"),
        Medium => Some("medium"),
        High => Some("high"),
        Xhigh | Max => Some("high"),
    }
}

pub fn provider_base_url(model: &Model, api_key: &str) -> String {
    if model.provider == "github-copilot"
        && let Some(proxy_host) = api_key
            .split(';')
            .find_map(|part| part.strip_prefix("proxy-ep="))
    {
        return format!("https://{}", proxy_host.replacen("proxy.", "api.", 1));
    }
    model.base_url.clone()
}

pub fn apply_provider_headers(
    mut request: reqwest::RequestBuilder,
    model: &Model,
    context: &Context,
    options: &crate::StreamOptions,
) -> reqwest::RequestBuilder {
    if let Some(session_id) = options.session_id.as_deref() {
        if matches!(model.provider.as_str(), "opencode" | "opencode-go") {
            request = request.header("x-opencode-session", session_id);
        }
        let compat = model.compat.as_ref();
        let openrouter = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
        let format = compat
            .and_then(|compat| compat.session_affinity_format.as_deref())
            .or(openrouter.then_some("openrouter"));
        if matches!(
            model.api.as_str(),
            "openai-responses" | "azure-openai-responses"
        ) {
            match format {
                Some("openrouter") => request = request.header("x-session-id", session_id),
                Some("openai") => {
                    request = request
                        .header("session-id", session_id)
                        .header("x-client-request-id", session_id);
                }
                _ => request = request.header("x-client-request-id", session_id),
            }
        } else if compat
            .and_then(|compat| compat.send_session_affinity_headers)
            .unwrap_or(openrouter)
        {
            match format {
                Some("openrouter") => request = request.header("x-session-id", session_id),
                Some("openai") => {
                    request = request
                        .header("session_id", session_id)
                        .header("x-client-request-id", session_id)
                        .header("x-session-affinity", session_id);
                }
                Some("openai-nosession") => {
                    request = request
                        .header("x-client-request-id", session_id)
                        .header("x-session-affinity", session_id);
                }
                _ => request = request.header("x-session-affinity", session_id),
            }
        }
    }
    if model.provider != "github-copilot" {
        return request;
    }
    let initiator = if matches!(context.messages.last(), Some(Message::User(_))) {
        "user"
    } else {
        "agent"
    };
    request = request
        .header("x-initiator", initiator)
        .header("openai-intent", "conversation-edits");
    let has_images = context.messages.iter().any(|message| match message {
        Message::User(user) => match &user.content {
            crate::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            crate::types::UserContent::Text(_) => false,
        },
        Message::ToolResult(result) => result
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. })),
        Message::Assistant(_) => false,
    });
    if has_images {
        request = request.header("copilot-vision-request", "true");
    }
    request
}

#[cfg(test)]
mod provider_header_tests {
    #[test]
    fn an_error_shows_each_cause_once() {
        let error = anyhow::anyhow!("connection refused").context("error sending request");
        assert_eq!(
            super::with_causes(error.as_ref()),
            "error sending request: connection refused"
        );
    }

    use super::*;

    #[test]
    fn rate_limit_reset_headers_become_retry_hints() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_seconds(&headers), None);
        headers.insert("retry-after", "42".parse().unwrap());
        assert_eq!(retry_after_seconds(&headers), Some(42));
        headers.clear();
        let reset = chrono::Utc::now().timestamp() + 600;
        headers.insert(
            "anthropic-ratelimit-unified-reset",
            reset.to_string().parse().unwrap(),
        );
        let seconds = retry_after_seconds(&headers).unwrap();
        assert!((598..=601).contains(&seconds), "{seconds}");
    }

    #[tokio::test]
    async fn a_finished_tool_call_without_arguments_ends_once() {
        let model: Model = serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
            "baseUrl": "https://example.com", "reasoning": false, "input": ["text"],
            "contextWindow": 1000, "maxTokens": 100,
            "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0}
        }))
        .unwrap();
        let (sink, mut stream) = crate::EventStream::channel();
        let mut builder = PartialBuilder::new(&model, sink);
        let idx = builder.begin_tool_call("call_1".into(), "now".into());
        builder.append_tool_args(idx, "{}");
        builder.end_tool_call(idx, None);
        builder.finish(StopReason::ToolUse, &model);
        let mut ends = 0;
        while let Some(event) = stream.next().await {
            ends += usize::from(matches!(event, AssistantEvent::ToolCallEnd { .. }));
        }
        assert_eq!(ends, 1);
    }

    #[test]
    fn copilot_token_selects_its_account_endpoint() {
        let mut model = Model {
            id: "gpt".into(),
            name: String::new(),
            api: "openai-responses".into(),
            provider: "github-copilot".into(),
            base_url: "https://api.individual.githubcopilot.com".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 1,
            max_tokens: 1,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        };
        assert_eq!(
            provider_base_url(
                &model,
                "tid=1;proxy-ep=proxy.business.githubcopilot.com;exp=2"
            ),
            "https://api.business.githubcopilot.com"
        );
        model.provider = "openai".into();
        assert_eq!(provider_base_url(&model, "secret"), model.base_url);
    }

    #[test]
    fn cost_uses_the_highest_matching_input_tier() {
        let mut model = Model {
            id: "tiered".into(),
            name: String::new(),
            api: "openai-responses".into(),
            provider: "test".into(),
            base_url: String::new(),
            reasoning: false,
            input: vec!["text".into()],
            cost: crate::ModelCost {
                input: 1.0,
                output: 2.0,
                tiers: vec![crate::ModelCostTier {
                    input_tokens_above: 100,
                    input: 10.0,
                    output: 20.0,
                    ..Default::default()
                }],
                ..Default::default()
            },
            prompt_cache: None,
            context_window: 1,
            max_tokens: 1,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        };
        let mut usage = Usage {
            input: 101,
            output: 10,
            ..Default::default()
        };
        finalize_cost(&mut usage, &model);
        assert_eq!(usage.cost.input, 0.00101);
        assert_eq!(usage.cost.output, 0.0002);

        model.cost.tiers.push(crate::ModelCostTier {
            input_tokens_above: 1_000,
            input: 100.0,
            output: 200.0,
            ..Default::default()
        });
        usage.input = 1_001;
        finalize_cost(&mut usage, &model);
        assert_eq!(usage.cost.input, 0.1001);
        assert_eq!(usage.cost.output, 0.002);
    }

    #[test]
    fn provider_session_headers_follow_catalog_compatibility() {
        let model = Model {
            id: "model".into(),
            name: String::new(),
            api: "openai-completions".into(),
            provider: "opencode".into(),
            base_url: "https://opencode.ai/v1".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 1,
            max_tokens: 1,
            compat: Some(crate::OpenAICompat {
                send_session_affinity_headers: Some(true),
                session_affinity_format: Some("openrouter".into()),
                ..Default::default()
            }),
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        };
        let request = apply_provider_headers(
            crate::stream::http_client().get("https://example.invalid"),
            &model,
            &Context::default(),
            &crate::StreamOptions {
                session_id: Some("session-one".into()),
                ..Default::default()
            },
        )
        .build()
        .unwrap();
        assert_eq!(request.headers()["x-opencode-session"], "session-one");
        assert_eq!(request.headers()["x-session-id"], "session-one");
    }

    #[test]
    fn responses_send_client_request_id_without_opt_in() {
        let model = Model {
            id: "model".into(),
            name: String::new(),
            api: "openai-responses".into(),
            provider: "provider".into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 1,
            max_tokens: 1,
            compat: Some(crate::OpenAICompat {
                session_affinity_format: Some("openai-nosession".into()),
                ..Default::default()
            }),
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        };
        let request = apply_provider_headers(
            crate::stream::http_client().get("https://example.invalid"),
            &model,
            &Context::default(),
            &crate::StreamOptions {
                session_id: Some("session-one".into()),
                ..Default::default()
            },
        )
        .build()
        .unwrap();
        assert_eq!(request.headers()["x-client-request-id"], "session-one");
        assert!(!request.headers().contains_key("session_id"));
        assert!(!request.headers().contains_key("x-session-affinity"));
    }

    #[test]
    fn output_limit_leaves_a_margin_for_input_counted_higher() {
        let mut model = Model {
            id: "model".into(),
            name: String::new(),
            api: "openai-completions".into(),
            provider: "openrouter".into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 100_000,
            max_tokens: 64_000,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        };
        let context = |input: u64| {
            let mut reply = AssistantMessage::empty("openai-completions", "openrouter", "model");
            reply.usage.input = input;
            reply.timestamp = 1;
            let user = crate::types::UserMessage {
                content: crate::types::UserContent::Text("x".repeat(35)),
                timestamp: 2,
            };
            Context {
                messages: vec![Message::Assistant(reply), Message::User(user)],
                ..Default::default()
            }
        };
        let options = crate::StreamOptions::default();
        // 15% of 50,010 input tokens stays free.
        assert_eq!(
            request_max_tokens(&model, &context(50_000), &options),
            42_488
        );
        // Near a full window only 4096 tokens stay free.
        assert_eq!(
            request_max_tokens(&model, &context(90_000), &options),
            5_894
        );
        assert_eq!(request_max_tokens(&model, &context(0), &options), 64_000);
        model.context_window = 0;
        assert_eq!(
            request_max_tokens(&model, &context(90_000), &options),
            64_000
        );
    }

    #[test]
    fn responses_openai_affinity_sends_the_documented_session_id_header() {
        let model = Model {
            id: "model".into(),
            name: String::new(),
            api: "openai-responses".into(),
            provider: "provider".into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 1,
            max_tokens: 1,
            compat: Some(crate::OpenAICompat {
                session_affinity_format: Some("openai".into()),
                ..Default::default()
            }),
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        };
        let request = apply_provider_headers(
            crate::stream::http_client().get("https://example.invalid"),
            &model,
            &Context::default(),
            &crate::StreamOptions {
                session_id: Some("session-one".into()),
                ..Default::default()
            },
        )
        .build()
        .unwrap();
        assert_eq!(request.headers()["session-id"], "session-one");
        assert!(!request.headers().contains_key("session_id"));
    }

    #[test]
    fn one_hour_cache_writes_use_twice_the_input_rate() {
        let model = Model {
            id: "bedrock".into(),
            name: String::new(),
            api: "bedrock-converse-stream".into(),
            provider: "amazon-bedrock".into(),
            base_url: String::new(),
            reasoning: false,
            input: vec!["text".into()],
            cost: crate::ModelCost {
                input: 2.0,
                cache_write: 2.5,
                ..Default::default()
            },
            prompt_cache: None,
            context_window: 1,
            max_tokens: 1,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        };
        let mut usage = Usage {
            cache_write: 1_000_000,
            cache_write_1h: 400_000,
            ..Default::default()
        };
        finalize_cost(&mut usage, &model);
        assert_eq!(usage.cost.cache_write, 3.1);
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_stream_delta_snapshots() {
        let model = Model {
            id: "benchmark".into(),
            name: "benchmark".into(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url: "https://example.invalid".into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 100_000,
            max_tokens: 10_000,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        };
        kiss_bench::measure(
            "stream_text_40k_2000",
            11,
            1,
            "2000_twenty_byte_deltas",
            || {
                let (sink, _stream) = crate::EventStream::channel();
                let mut builder = PartialBuilder::new(&model, sink);
                let index = builder.begin_text();
                for _ in 0..2_000 {
                    builder.append_text(index, "01234567890123456789");
                }
                builder.message.text().len()
            },
        );
    }
}
