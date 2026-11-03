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
    // Providers reject empty-content user turns. Drop them defensively.
    out.retain(|m| match m {
        Message::User(u) => !u.content.as_text().trim().is_empty() || matches!(&u.content, UserContent::Blocks(b) if b.iter().any(|c| matches!(c, ContentBlock::Image { .. }))),
        _ => true,
    });
    // A stopped stream or process can leave calls without results in saved
    // history. Close each batch before the next turn, without running tools.
    let mut messages = out.into_iter().peekable();
    let mut out = Vec::new();
    while let Some(message) = messages.next() {
        let Message::Assistant(assistant) = &message else {
            out.push(message);
            continue;
        };
        let mut pending: Vec<_> = assistant.tool_calls().cloned().collect();
        let timestamp = assistant.timestamp;
        out.push(message);
        while matches!(messages.peek(), Some(Message::ToolResult(_))) {
            if let Some(Message::ToolResult(result)) = messages.next() {
                pending.retain(|call| call.id != result.tool_call_id);
                out.push(Message::ToolResult(result));
            }
        }
        for call in pending {
            out.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: call.id,
                tool_name: call.name,
                content: vec![ContentBlock::text(
                    "The tool call has no saved result because the previous turn was interrupted. Its execution status is unknown. Check the current state before you repeat the call.",
                )],
                details: None,
                usage: None,
                is_error: true,
                timestamp,
            }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_tool_history_gets_results_before_the_next_turn() {
        let mut assistant = AssistantMessage::empty("openai-responses", "openai", "test");
        assistant.stop_reason = kiss_ai::StopReason::Aborted;
        assistant.content = (0..3)
            .map(|index| {
                ContentBlock::ToolCall(kiss_ai::ToolCall {
                    id: format!("call_{index}|fc_{index}"),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo test"}),
                    thought_signature: None,
                    arguments_error: None,
                })
            })
            .collect();
        let completed = ToolResultMessage {
            tool_call_id: "call_1|fc_1".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text("test")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 1,
        };
        for next_turn in [vec![], vec![AgentMessage::user("continue")]] {
            let mut history = vec![
                AgentMessage::Assistant(assistant.clone()),
                AgentMessage::ToolResult(completed.clone()),
            ];
            history.extend(next_turn.clone());
            let converted = convert_to_llm(&history);
            let results: Vec<_> = converted
                .iter()
                .filter_map(|message| match message {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                })
                .collect();
            assert_eq!(results.len(), 3);
            assert_eq!(results[0], &completed);
            for (result, id) in results[1..].iter().zip(["call_0|fc_0", "call_2|fc_2"]) {
                assert_eq!(result.tool_call_id, id);
                assert!(result.is_error);
            }
            assert_eq!(converted.len(), 4 + next_turn.len());
            if !next_turn.is_empty() {
                assert!(matches!(converted.last(), Some(Message::User(_))));
            }
            let repaired: Vec<_> = converted.into_iter().map(AgentMessage::from).collect();
            assert_eq!(convert_to_llm(&repaired).len(), repaired.len());
            assert_eq!(history.len(), 2 + next_turn.len());
        }
    }

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
