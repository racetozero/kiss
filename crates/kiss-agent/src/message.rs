//! Session-level message union: provider messages plus harness roles.

use kiss_ai::{
    AssistantMessage, ContentBlock, Message, TimestampMs, ToolResultMessage, UserContent,
    UserMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// True for `!!` commands whose output stays out of the LLM context.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude_from_context: bool,
    pub timestamp: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: UserContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub timestamp: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    /// Entry id we branched away from.
    pub from_id: String,
    pub timestamp: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    pub timestamp: TimestampMs,
}

/// Every message the harness stores or renders, tagged by `role` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum AgentMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    BashExecution(BashExecutionMessage),
    Custom(CustomMessage),
    BranchSummary(BranchSummaryMessage),
    CompactionSummary(CompactionSummaryMessage),
}

impl AgentMessage {
    pub fn role(&self) -> &'static str {
        match self {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolResult",
            AgentMessage::BashExecution(_) => "bashExecution",
            AgentMessage::Custom(_) => "custom",
            AgentMessage::BranchSummary(_) => "branchSummary",
            AgentMessage::CompactionSummary(_) => "compactionSummary",
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        AgentMessage::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: kiss_ai::now_ms(),
        })
    }
}

impl From<Message> for AgentMessage {
    fn from(m: Message) -> Self {
        match m {
            Message::User(u) => AgentMessage::User(u),
            Message::Assistant(a) => AgentMessage::Assistant(a),
            Message::ToolResult(t) => AgentMessage::ToolResult(t),
        }
    }
}

/// Default conversion of harness messages to provider messages, matching
/// pi's `convertToLlm`: harness roles become user messages with labeled
/// text, UI-only content is dropped.
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            AgentMessage::User(u) => out.push(Message::User(u.clone())),
            AgentMessage::Assistant(a) => out.push(Message::Assistant(a.clone())),
            AgentMessage::ToolResult(t) => out.push(Message::ToolResult(t.clone())),
            AgentMessage::BashExecution(b) => {
                if b.exclude_from_context {
                    continue;
                }
                let mut text = format!("Ran shell command: {}\nOutput:\n{}", b.command, b.output);
                if let Some(code) = b.exit_code
                    && code != 0
                {
                    text.push_str(&format!("\nExit code: {code}"));
                }
                out.push(Message::User(UserMessage {
                    content: UserContent::Text(text),
                    timestamp: b.timestamp,
                }));
            }
            AgentMessage::Custom(c) => out.push(Message::User(UserMessage {
                content: c.content.clone(),
                timestamp: c.timestamp,
            })),
            AgentMessage::BranchSummary(b) => out.push(Message::User(UserMessage {
                content: UserContent::Text(format!(
                    "Summary of an abandoned conversation branch:\n\n{}",
                    b.summary
                )),
                timestamp: b.timestamp,
            })),
            AgentMessage::CompactionSummary(c) => out.push(Message::User(UserMessage {
                content: UserContent::Text(format!(
                    "The conversation history before this point was compacted into this summary:\n\n{}",
                    c.summary
                )),
                timestamp: c.timestamp,
            })),
        }
    }
    // Providers reject empty-content user turns; drop them defensively.
    out.retain(|m| match m {
        Message::User(u) => !u.content.as_text().trim().is_empty() || matches!(&u.content, UserContent::Blocks(b) if b.iter().any(|c| matches!(c, ContentBlock::Image { .. }))),
        _ => true,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_tags_match_pi() {
        let m = AgentMessage::BashExecution(BashExecutionMessage {
            command: "ls".into(),
            output: "a".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            exclude_from_context: false,
            timestamp: 1,
        });
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "bashExecution");
        assert_eq!(v["exitCode"], 0);

        let m = AgentMessage::CompactionSummary(CompactionSummaryMessage {
            summary: "s".into(),
            tokens_before: 5,
            timestamp: 1,
        });
        assert_eq!(
            serde_json::to_value(&m).unwrap()["role"],
            "compactionSummary"
        );
    }

    #[test]
    fn excluded_bash_dropped_from_llm() {
        let messages = vec![
            AgentMessage::user("hi"),
            AgentMessage::BashExecution(BashExecutionMessage {
                command: "secret".into(),
                output: "x".into(),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
                full_output_path: None,
                exclude_from_context: true,
                timestamp: 1,
            }),
        ];
        assert_eq!(convert_to_llm(&messages).len(), 1);
    }
}
