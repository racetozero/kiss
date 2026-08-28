//! Unified message model shared by every provider adapter.
//!
//! Serde renames keep the JSON wire shape compatible with pi's session
//! format v3 (camelCase fields, `role`/`type` tag strings).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Milliseconds since the Unix epoch.
pub type TimestampMs = i64;

pub fn now_ms() -> TimestampMs {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    #[serde(rename_all = "camelCase")]
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    #[serde(rename_all = "camelCase")]
    Image {
        /// Base64-encoded image bytes.
        data: String,
        mime_type: String,
    },
    #[serde(rename = "toolCall", rename_all = "camelCase")]
    ToolCall(ToolCall),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            text_signature: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: Cost,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        if let Some(r) = other.reasoning {
            *self.reasoning.get_or_insert(0) += r;
        }
        self.total_tokens += other.total_tokens;
        self.cost.input += other.cost.input;
        self.cost.output += other.cost.output;
        self.cost.cache_read += other.cost.cache_read;
        self.cost.cache_write += other.cost.cache_write;
        self.cost.total += other.cost.total;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Pending,
    #[default]
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

/// User message content: either a bare string or typed blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl UserContent {
    pub fn as_text(&self) -> String {
        match self {
            UserContent::Text(t) => t.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    pub timestamp: TimestampMs,
}

impl AssistantMessage {
    pub fn empty(api: &str, provider: &str, model: &str) -> Self {
        AssistantMessage {
            content: Vec::new(),
            api: api.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            error_message: None,
            raw_stop_reason: None,
            timestamp: now_ms(),
        }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|c| match c {
            ContentBlock::ToolCall(tc) => Some(tc),
            _ => None,
        })
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub is_error: bool,
    pub timestamp: TimestampMs,
}

/// A message the LLM providers understand, tagged by `role`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

/// Tool definition surfaced to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON schema object for the arguments.
    pub parameters: Value,
}

/// Everything a provider request needs besides the model.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub system_prompt: Option<String>,
    /// Provider-native prefix for OpenAI Responses requests. This can contain
    /// opaque server-compaction items that are not common chat messages.
    pub openai_responses_input: Option<Vec<Value>>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}

/// Requested reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => ThinkingLevel::Off,
            "minimal" => ThinkingLevel::Minimal,
            "low" => ThinkingLevel::Low,
            "medium" => ThinkingLevel::Medium,
            "high" => ThinkingLevel::High,
            "xhigh" => ThinkingLevel::Xhigh,
            "max" => ThinkingLevel::Max,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_json_shape_matches_pi() {
        let msg = Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_1".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text("hi")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 123,
        });
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "toolResult");
        assert_eq!(v["toolCallId"], "call_1");
        assert_eq!(v["content"][0]["type"], "text");

        let tc = ContentBlock::ToolCall(ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "x"}),
            thought_signature: None,
        });
        let v = serde_json::to_value(&tc).unwrap();
        assert_eq!(v["type"], "toolCall");
    }

    #[test]
    fn stop_reason_camel_case() {
        assert_eq!(
            serde_json::to_value(StopReason::ToolUse).unwrap(),
            "toolUse"
        );
    }
}
