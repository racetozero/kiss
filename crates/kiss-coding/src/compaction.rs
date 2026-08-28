//! Compaction: summarize older context when the window fills, keeping a
//! recent tail. Also branch summarization support for /tree navigation.

use kiss_agent::{AgentMessage, convert_to_llm};
use kiss_ai::{ContentBlock, Message, Model, StreamOptions, ThinkingLevel, UserContent};
use serde_json::json;

const TOOL_RESULT_SERIALIZE_CAP: usize = 2000;

/// chars/4 heuristic, matching pi's estimator for messages without usage.
pub fn estimate_tokens_text(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

pub fn estimate_message_tokens(message: &AgentMessage) -> u64 {
    let text = match message {
        AgentMessage::User(u) => u.content.as_text(),
        AgentMessage::Assistant(a) => a
            .content
            .iter()
            .map(|c| match c {
                ContentBlock::Text { text, .. } => text.len(),
                ContentBlock::Thinking { thinking, .. } => thinking.len(),
                ContentBlock::ToolCall(tc) => tc.arguments.to_string().len() + tc.name.len(),
                ContentBlock::Image { .. } => 1600,
            })
            .sum::<usize>()
            .to_string(),
        AgentMessage::ToolResult(t) => t
            .content
            .iter()
            .map(|c| match c {
                ContentBlock::Text { text, .. } => text.len(),
                _ => 1600,
            })
            .sum::<usize>()
            .to_string(),
        AgentMessage::BashExecution(b) => (b.command.len() + b.output.len()).to_string(),
        AgentMessage::Custom(c) => c.content.as_text(),
        AgentMessage::BranchSummary(b) => b.summary.clone(),
        AgentMessage::CompactionSummary(c) => c.summary.clone(),
    };
    match message {
        AgentMessage::Assistant(_) | AgentMessage::ToolResult(_) => text
            .parse::<u64>()
            .map(|chars| chars.div_ceil(4))
            .unwrap_or_else(|_| estimate_tokens_text(&text)),
        _ => estimate_tokens_text(&text),
    }
}

/// Estimated context size: prefer the last assistant usage (input+output+
/// cache) and add estimates for everything after it.
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> u64 {
    let last_assistant = messages
        .iter()
        .rposition(|m| matches!(m, AgentMessage::Assistant(_)));
    match last_assistant {
        Some(pos) => {
            let AgentMessage::Assistant(a) = &messages[pos] else {
                unreachable!()
            };
            let base = a.usage.input + a.usage.output + a.usage.cache_read + a.usage.cache_write;
            let tail: u64 = messages[pos + 1..]
                .iter()
                .map(estimate_message_tokens)
                .sum();
            base + tail
        }
        None => messages.iter().map(estimate_message_tokens).sum(),
    }
}

/// Serialize a conversation to labeled text for the summary prompt; tool
/// results are capped so summarization stays cheap.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            Message::User(u) => {
                out.push_str(&format!("[User]: {}\n", u.content.as_text()));
            }
            Message::Assistant(a) => {
                let mut tool_calls: Vec<String> = Vec::new();
                for block in &a.content {
                    match block {
                        ContentBlock::Thinking { thinking, .. } => {
                            if !thinking.is_empty() {
                                out.push_str(&format!("[Assistant thinking]: {thinking}\n"));
                            }
                        }
                        ContentBlock::Text { text, .. } => {
                            if !text.is_empty() {
                                out.push_str(&format!("[Assistant]: {text}\n"));
                            }
                        }
                        ContentBlock::ToolCall(tc) => {
                            let args = tc
                                .arguments
                                .as_object()
                                .map(|o| {
                                    o.iter()
                                        .map(|(k, v)| format!("{k}={v}"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_else(|| tc.arguments.to_string());
                            tool_calls.push(format!("{}({args})", tc.name));
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !tool_calls.is_empty() {
                    out.push_str(&format!(
                        "[Assistant tool calls]: {}\n",
                        tool_calls.join("; ")
                    ));
                }
            }
            Message::ToolResult(t) => {
                let text: String = t
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let capped = if text.chars().count() > TOOL_RESULT_SERIALIZE_CAP {
                    let kept: String = text.chars().take(TOOL_RESULT_SERIALIZE_CAP).collect();
                    let dropped = text.chars().count() - TOOL_RESULT_SERIALIZE_CAP;
                    format!("{kept}\n[... {dropped} characters truncated]")
                } else {
                    text
                };
                out.push_str(&format!("[Tool result]: {capped}\n"));
            }
        }
    }
    out
}

/// Extract read/modified file paths from tool calls in the messages.
pub fn extract_file_ops(messages: &[AgentMessage]) -> (Vec<String>, Vec<String>) {
    let mut read: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    for message in messages {
        let AgentMessage::Assistant(a) = message else {
            continue;
        };
        for tc in a.tool_calls() {
            let Some(path) = tc.arguments["path"].as_str() else {
                continue;
            };
            match tc.name.as_str() {
                "read" => read.push(path.to_string()),
                "edit" | "write" => modified.push(path.to_string()),
                _ => {}
            }
        }
    }
    read.sort();
    read.dedup();
    modified.sort();
    modified.dedup();
    (read, modified)
}

pub const SUMMARY_FORMAT: &str = "## Goal\n[What the user is trying to accomplish]\n\n## Constraints & Preferences\n- [Requirements mentioned by user]\n\n## Progress\n### Done\n- [x] [Completed tasks]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues, if any]\n\n## Key Decisions\n- **[Decision]**: [Rationale]\n\n## Next Steps\n1. [What should happen next]\n\n## Critical Context\n- [Data needed to continue]";

/// Cut-point decision for a compaction pass.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPlan {
    /// Messages to summarize (complete turns before the cut).
    pub to_summarize: Vec<AgentMessage>,
    /// Messages kept verbatim after the cut (the retained tail).
    pub kept: Vec<AgentMessage>,
    /// Split-turn prefix that must be summarized separately.
    pub turn_prefix: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
}

fn is_valid_cut(message: &AgentMessage) -> bool {
    // Never cut at tool results: they must stay with their tool call.
    matches!(
        message,
        AgentMessage::User(_)
            | AgentMessage::Assistant(_)
            | AgentMessage::BashExecution(_)
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary(_)
    )
}

fn is_turn_start(message: &AgentMessage) -> bool {
    matches!(
        message,
        AgentMessage::User(_) | AgentMessage::Custom(_) | AgentMessage::BashExecution(_)
    )
}

/// Choose the cut point: walk back from the newest message accumulating
/// estimates until `keep_recent_tokens`, then snap to a turn boundary.
pub fn plan_compaction(messages: &[AgentMessage], keep_recent_tokens: u64) -> CompactionPlan {
    let tokens_before = estimate_context_tokens(messages);
    let mut budget = 0u64;
    let mut cut = messages.len();
    for (i, message) in messages.iter().enumerate().rev() {
        budget += estimate_message_tokens(message);
        if budget > keep_recent_tokens {
            break;
        }
        if is_valid_cut(message) {
            cut = i;
        }
    }
    if cut == messages.len() && !messages.is_empty() {
        // Keep at least the final message when everything is over budget.
        cut = messages.len() - 1;
        while cut > 0 && !is_valid_cut(&messages[cut]) {
            cut -= 1;
        }
    }

    // Snap to a turn start when possible; otherwise this is a split turn.
    let turn_start = messages[..cut].iter().rposition(is_turn_start);
    match turn_start {
        Some(_) if is_turn_start(&messages[cut]) => CompactionPlan {
            to_summarize: messages[..cut].to_vec(),
            kept: messages[cut..].to_vec(),
            turn_prefix: Vec::new(),
            is_split_turn: false,
            tokens_before,
        },
        Some(start) => CompactionPlan {
            to_summarize: messages[..start].to_vec(),
            kept: messages[cut..].to_vec(),
            turn_prefix: messages[start..cut].to_vec(),
            is_split_turn: true,
            tokens_before,
        },
        None => CompactionPlan {
            to_summarize: Vec::new(),
            kept: messages[cut..].to_vec(),
            turn_prefix: messages[..cut].to_vec(),
            is_split_turn: cut > 0,
            tokens_before,
        },
    }
}

/// Whether auto-compaction should trigger.
pub fn should_compact(context_tokens: u64, context_window: u64, reserve_tokens: u64) -> bool {
    context_tokens > context_window.saturating_sub(reserve_tokens)
}

pub struct SummaryOutcome {
    pub summary: String,
    pub usage: Option<kiss_ai::Usage>,
}

/// Generate a structured summary with the LLM. One-off prompt: fresh session
/// id and no cache writes wanted, so it's a plain request.
pub async fn generate_summary(
    model: &Model,
    api_key: Option<String>,
    conversation_text: &str,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<SummaryOutcome> {
    let mut prompt = String::from(
        "Summarize the conversation below so a coding agent can seamlessly continue the work. Use exactly this structure:\n\n",
    );
    prompt.push_str(SUMMARY_FORMAT);
    prompt.push_str("\n\nAlso include, at the end, a <read-files> block listing files that were read and a <modified-files> block listing files that were changed, one path per line, when known.");
    if let Some(prev) = previous_summary {
        prompt.push_str("\n\nA previous summary of earlier context exists; fold it in:\n\n");
        prompt.push_str(prev);
    }
    if let Some(custom) = custom_instructions {
        prompt.push_str("\n\nAdditional focus requested by the user: ");
        prompt.push_str(custom);
    }
    prompt.push_str("\n\nConversation:\n\n");
    prompt.push_str(conversation_text);

    let context = kiss_ai::Context {
        system_prompt: None,
        openai_responses_input: None,
        messages: vec![Message::User(kiss_ai::UserMessage {
            content: UserContent::Text(prompt),
            timestamp: kiss_ai::now_ms(),
        })],
        tools: vec![],
    };
    let options = StreamOptions {
        api_key,
        reasoning: ThinkingLevel::Off,
        cancel,
        ..Default::default()
    };
    let message = kiss_ai::stream_simple(model, &context, &options)
        .result()
        .await;
    if message.stop_reason == kiss_ai::StopReason::Error {
        anyhow::bail!(
            "summary generation failed: {}",
            message.error_message.unwrap_or_default()
        );
    }
    Ok(SummaryOutcome {
        summary: message.text(),
        usage: Some(message.usage),
    })
}

/// Details payload stored on compaction/branch-summary entries.
pub fn file_ops_details(read: &[String], modified: &[String]) -> serde_json::Value {
    json!({"readFiles": read, "modifiedFiles": modified})
}

/// Serialize agent messages for summarization (convert then serialize).
pub fn serialize_agent_messages(messages: &[AgentMessage]) -> String {
    serialize_conversation(&convert_to_llm(messages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::{AssistantMessage, StopReason, ToolCall, ToolResultMessage};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(text)
    }

    fn assistant(text: &str) -> AgentMessage {
        let mut m = AssistantMessage::empty("fake", "fake", "fake");
        m.content.push(ContentBlock::text(text));
        m.stop_reason = StopReason::Stop;
        AgentMessage::Assistant(m)
    }

    fn tool_result(text: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text(text)],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn cut_at_turn_boundary_never_tool_result() {
        // Two turns; small keep budget keeps only the last turn.
        let messages = vec![
            user(&"a".repeat(400)),
            assistant(&"b".repeat(400)),
            tool_result(&"c".repeat(400)),
            user(&"d".repeat(400)),
            assistant(&"e".repeat(400)),
        ];
        let plan = plan_compaction(&messages, 250);
        assert!(!plan.is_split_turn);
        assert!(matches!(plan.kept.first().unwrap(), AgentMessage::User(_)));
        assert_eq!(plan.to_summarize.len(), 3);
    }

    #[test]
    fn split_turn_detected() {
        // One huge turn: user + many assistant/tool pairs.
        let messages = vec![
            user("start"),
            assistant(&"x".repeat(2000)),
            tool_result(&"y".repeat(2000)),
            assistant(&"z".repeat(2000)),
        ];
        let plan = plan_compaction(&messages, 600);
        assert!(plan.is_split_turn);
        assert!(plan.to_summarize.is_empty());
        assert!(!plan.turn_prefix.is_empty());
        // Kept must not start with a tool result.
        assert!(is_valid_cut(plan.kept.first().unwrap()));
    }

    #[test]
    fn serialization_labels_and_caps() {
        let big = "L".repeat(3000);
        let messages = vec![user("do it"), assistant("on it"), tool_result(&big)];
        let text = serialize_agent_messages(&messages);
        assert!(text.contains("[User]: do it"));
        assert!(text.contains("[Assistant]: on it"));
        assert!(text.contains("characters truncated"));
    }

    #[test]
    fn file_ops_extraction() {
        let mut a = AssistantMessage::empty("f", "f", "f");
        a.content.push(ContentBlock::ToolCall(ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: json!({"path": "src/a.rs"}),
            thought_signature: None,
        }));
        a.content.push(ContentBlock::ToolCall(ToolCall {
            id: "2".into(),
            name: "edit".into(),
            arguments: json!({"path": "src/b.rs"}),
            thought_signature: None,
        }));
        let (read, modified) = extract_file_ops(&[AgentMessage::Assistant(a)]);
        assert_eq!(read, vec!["src/a.rs"]);
        assert_eq!(modified, vec!["src/b.rs"]);
    }

    #[test]
    fn threshold() {
        assert!(should_compact(190_000, 200_000, 16_384));
        assert!(!should_compact(100_000, 200_000, 16_384));
    }
}
