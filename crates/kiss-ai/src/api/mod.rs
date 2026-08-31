//! Provider adapters. Each submodule owns the full round trip for one API
//! dialect: request building from the unified `Context`, stream decoding to
//! unified `AssistantEvent`s, usage extraction, and error normalization.

pub mod anthropic;
pub mod bedrock;
pub(crate) mod claude_code;
pub mod google;
pub mod openai_compaction;
pub mod openai_completions;
pub mod openai_responses;
pub mod pi_messages;

use crate::event::{AssistantEvent, EventSink};
use crate::json_salvage::parse_salvage;
use crate::model::Model;
use crate::types::{AssistantMessage, ContentBlock, Cost, StopReason, ToolCall, Usage};
use crate::types::{Context, Message};

/// Accumulates a streaming assistant message and emits unified events with
/// partial snapshots, mirroring pi's event protocol.
pub struct PartialBuilder {
    pub message: AssistantMessage,
    sink: EventSink,
    /// Raw argument text per tool-call content index.
    tool_args: Vec<(usize, String)>,
    started: bool,
}

impl PartialBuilder {
    pub fn new(model: &Model, sink: EventSink) -> Self {
        PartialBuilder {
            message: AssistantMessage::empty(&model.api, &model.provider, &model.id),
            sink,
            tool_args: Vec::new(),
            started: false,
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

    /// Finalize a tool call: parse accumulated raw args (salvaging truncated
    /// JSON) unless the provider already delivered structured arguments.
    pub fn end_tool_call(&mut self, idx: usize, structured_args: Option<serde_json::Value>) {
        let raw = self
            .tool_args
            .iter()
            .find(|(i, _)| *i == idx)
            .map(|(_, r)| r.clone())
            .unwrap_or_default();
        if let Some(ContentBlock::ToolCall(tc)) = self.message.content.get_mut(idx) {
            tc.arguments = structured_args
                .or_else(|| parse_salvage(&raw))
                .unwrap_or(serde_json::Value::Object(Default::default()));
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

    /// Close any content block that never got its end event (stream cut off).
    pub fn close_open_blocks(&mut self) {
        let open_tools: Vec<usize> = self
            .tool_args
            .iter()
            .filter(|(i, _)| {
                matches!(self.message.content.get(*i), Some(ContentBlock::ToolCall(tc)) if tc.arguments.as_object().is_some_and(|o| o.is_empty()))
            })
            .map(|(i, _)| *i)
            .collect();
        for idx in open_tools {
            self.end_tool_call(idx, None);
        }
    }

    pub fn finish(mut self, stop_reason: StopReason, model: &Model) {
        self.start();
        self.close_open_blocks();
        self.message.stop_reason = stop_reason;
        finalize_cost(&mut self.message.usage, model);
        if matches!(stop_reason, StopReason::Stop)
            && self
                .message
                .content
                .iter()
                .any(|c| matches!(c, ContentBlock::ToolCall(_)))
        {
            self.message.stop_reason = StopReason::ToolUse;
        }
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
        self.message.error_message = Some(error.into());
        finalize_cost(&mut self.message.usage, model);
        self.sink.error(self.message);
    }
}

pub fn finalize_cost(usage: &mut Usage, model: &Model) {
    let per = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    let c = &model.cost;
    let cost = Cost {
        input: per(usage.input, c.input),
        output: per(usage.output, c.output),
        cache_read: per(usage.cache_read, c.cache_read),
        cache_write: per(usage.cache_write, c.cache_write),
        total: 0.0,
    };
    usage.cost = Cost {
        total: cost.input + cost.output + cost.cache_read + cost.cache_write,
        ..cost
    };
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
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
) -> reqwest::RequestBuilder {
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
    use super::*;

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
            context_window: 1,
            max_tokens: 1,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
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
            context_window: 100_000,
            max_tokens: 10_000,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
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
