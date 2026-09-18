use anyhow::{Context as _, bail};
use kiss_agent::AgentMessage;
use kiss_ai::{ContentBlock, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const KEEP_THRESHOLD: f64 = 0.5;
const TRUNCATE_HEAD_CHARS: usize = 300;

struct Interaction {
    id: String,
    tool: String,
}

#[derive(Serialize)]
struct JevRequest {
    state: Value,
    model: &'static str,
    questions: BTreeMap<String, NoulQuestion>,
}

#[derive(Serialize)]
struct NoulQuestion {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: String,
}

#[derive(Deserialize)]
struct JevResponse {
    answers: BTreeMap<String, NoulAnswer>,
    usage: JevUsage,
}

#[derive(Deserialize)]
struct NoulAnswer {
    #[serde(rename = "type")]
    kind: String,
    noul: f64,
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct JevUsage {
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JevStats {
    pub eligible: usize,
    pub kept: usize,
    pub truncated: usize,
    pub dropped: usize,
    pub chars_before: usize,
    pub chars_after: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub(crate) struct JevCompaction {
    pub messages: Vec<AgentMessage>,
    pub stats: JevStats,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Keep,
    Truncate,
    Drop,
}

pub(crate) async fn compact(
    messages: &[AgentMessage],
    pinned_start: usize,
    api_key: &str,
    cancel: CancellationToken,
) -> anyhow::Result<JevCompaction> {
    let interactions = collect_interactions(messages, pinned_start);
    if interactions.is_empty() {
        bail!("Jev found no older tool interactions to compact");
    }
    let request = build_request(messages, &interactions);
    let send = reqwest::Client::new()
        .post(ENDPOINT)
        .timeout(Duration::from_secs(15))
        .bearer_auth(api_key)
        .json(&request)
        .send();
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("Jev compaction cancelled"),
        response = send => response.context("send Jev compaction request")?,
    };
    let status = response.status();
    let body = tokio::select! {
        _ = cancel.cancelled() => bail!("Jev compaction cancelled"),
        body = response.text() => body.context("read Jev compaction response")?,
    };
    if !status.is_success() {
        bail!("Jev request failed with {status}: {}", body.trim());
    }
    let (decisions, usage) = parse_response(&body, interactions.len())?;
    Ok(apply_decisions(messages, &interactions, &decisions, usage))
}

fn collect_interactions(messages: &[AgentMessage], pinned_start: usize) -> Vec<Interaction> {
    let result_ids: HashSet<&str> = messages[..pinned_start.min(messages.len())]
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    let mut seen = HashSet::new();
    messages[..pinned_start.min(messages.len())]
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant| assistant.tool_calls())
        .filter(|call| result_ids.contains(call.id.as_str()) && seen.insert(call.id.clone()))
        .map(|call| Interaction {
            id: call.id.clone(),
            tool: call.name.clone(),
        })
        .collect()
}

fn build_request(messages: &[AgentMessage], interactions: &[Interaction]) -> JevRequest {
    let mut goals: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::User(user) => Some(user.content.as_text()),
            _ => None,
        })
        .rev()
        .take(3)
        .collect();
    goals.reverse();
    let state = json!({
        "goal": goals,
        "messages": messages.iter().map(state_message).collect::<Vec<_>>(),
    });
    let mut questions = BTreeMap::new();
    for (index, interaction) in interactions.iter().enumerate() {
        questions.insert(
            format!("call_{index}"),
            NoulQuestion {
                kind: "noul",
                instructions: format!(
                    "Does knowing that the {} tool was called, including its input, still matter for completing the current task?",
                    interaction.tool
                ),
            },
        );
        questions.insert(
            format!("result_{index}"),
            NoulQuestion {
                kind: "noul",
                instructions: format!(
                    "Is the exact result body from this {} tool call still needed, and not cheaply reproducible?",
                    interaction.tool
                ),
            },
        );
    }
    JevRequest {
        state,
        model: "jev-latest",
        questions,
    }
}

fn state_message(message: &AgentMessage) -> Value {
    match message {
        AgentMessage::User(user) => json!({"role": "user", "text": user.content.as_text()}),
        AgentMessage::Assistant(assistant) => json!({
            "role": "assistant",
            "content": assistant.content.iter().map(|block| match block {
                ContentBlock::Text { text, .. } => json!({"type": "text", "text": text}),
                ContentBlock::Thinking { thinking, .. } => json!({"type": "thinking", "text": thinking}),
                ContentBlock::Image { .. } => json!({"type": "image", "content": "omitted"}),
                ContentBlock::ToolCall(call) => json!({
                    "type": "tool_call", "id": call.id, "tool": call.name, "input": call.arguments
                }),
            }).collect::<Vec<_>>()
        }),
        AgentMessage::ToolResult(result) => json!({
            "role": "tool_result",
            "toolCallId": result.tool_call_id,
            "tool": result.tool_name,
            "isError": result.is_error,
            "characters": text_chars(&result.content),
            "content": "omitted"
        }),
        AgentMessage::BashExecution(bash) => json!({
            "role": "bash", "command": bash.command, "outputCharacters": bash.output.chars().count(),
            "exitCode": bash.exit_code, "cancelled": bash.cancelled
        }),
        AgentMessage::Custom(custom) => {
            json!({"role": "custom", "text": custom.content.as_text()})
        }
        AgentMessage::BranchSummary(summary) => {
            json!({"role": "branch_summary", "text": summary.summary})
        }
        AgentMessage::CompactionSummary(summary) => {
            json!({"role": "compaction_summary", "text": summary.summary})
        }
    }
}

fn parse_response(body: &str, count: usize) -> anyhow::Result<(Vec<Decision>, JevUsage)> {
    let response: JevResponse = serde_json::from_str(body).context("parse Jev response")?;
    let probability = |key: &str| -> anyhow::Result<f64> {
        let answer = response
            .answers
            .get(key)
            .with_context(|| format!("Jev response is missing {key}"))?;
        if answer.kind != "noul" || !answer.noul.is_finite() || !(0.0..=1.0).contains(&answer.noul)
        {
            bail!("Jev returned an invalid answer for {key}");
        }
        Ok(answer.noul)
    };
    let mut decisions = Vec::with_capacity(count);
    for index in 0..count {
        let call = probability(&format!("call_{index}"))?;
        let result = probability(&format!("result_{index}"))?;
        decisions.push(if result >= KEEP_THRESHOLD {
            Decision::Keep
        } else if call >= KEEP_THRESHOLD {
            Decision::Truncate
        } else {
            Decision::Drop
        });
    }
    Ok((decisions, response.usage))
}

fn apply_decisions(
    messages: &[AgentMessage],
    interactions: &[Interaction],
    decisions: &[Decision],
    usage: JevUsage,
) -> JevCompaction {
    let by_id: HashMap<&str, Decision> = interactions
        .iter()
        .zip(decisions)
        .map(|(interaction, decision)| (interaction.id.as_str(), *decision))
        .collect();
    let chars_before = serialized_len(messages);
    let mut filtered = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            AgentMessage::Assistant(assistant) => {
                let mut assistant = assistant.clone();
                assistant.content.retain(|block| {
                    !matches!(block, ContentBlock::ToolCall(call) if by_id.get(call.id.as_str()) == Some(&Decision::Drop))
                });
                if !assistant.content.is_empty() {
                    filtered.push(AgentMessage::Assistant(assistant));
                }
            }
            AgentMessage::ToolResult(result) => match by_id.get(result.tool_call_id.as_str()) {
                Some(Decision::Drop) => {}
                Some(Decision::Truncate) => {
                    filtered.push(AgentMessage::ToolResult(truncate_result(result)))
                }
                _ => filtered.push(message.clone()),
            },
            _ => filtered.push(message.clone()),
        }
    }
    let chars_after = serialized_len(&filtered);
    JevCompaction {
        messages: filtered,
        stats: JevStats {
            eligible: interactions.len(),
            kept: decisions.iter().filter(|d| **d == Decision::Keep).count(),
            truncated: decisions
                .iter()
                .filter(|d| **d == Decision::Truncate)
                .count(),
            dropped: decisions.iter().filter(|d| **d == Decision::Drop).count(),
            chars_before,
            chars_after,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        },
    }
}

fn truncate_result(result: &ToolResultMessage) -> ToolResultMessage {
    let total = text_chars(&result.content);
    if total <= TRUNCATE_HEAD_CHARS {
        return result.clone();
    }
    let mut remaining = TRUNCATE_HEAD_CHARS;
    let mut content = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text { text, .. } if remaining > 0 => {
                let kept = text.chars().take(remaining).collect::<String>();
                remaining = remaining.saturating_sub(kept.chars().count());
                if !kept.is_empty() {
                    content.push(ContentBlock::text(kept));
                }
            }
            ContentBlock::Text { .. } => {}
            other => content.push(other.clone()),
        }
    }
    content.push(ContentBlock::text(format!(
        "\n[... {} characters omitted by Jev compaction]",
        total - TRUNCATE_HEAD_CHARS
    )));
    ToolResultMessage {
        content,
        ..result.clone()
    }
}

fn text_chars(content: &[ContentBlock]) -> usize {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.chars().count()),
            _ => None,
        })
        .sum()
}

fn serialized_len(messages: &[AgentMessage]) -> usize {
    serde_json::to_string(messages).map_or(0, |text| text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::{AssistantMessage, StopReason, ToolCall};

    fn assistant(calls: &[(&str, &str)], text: Option<&str>) -> AgentMessage {
        let mut message = AssistantMessage::empty("test", "test", "test");
        if let Some(text) = text {
            message.content.push(ContentBlock::text(text));
        }
        for (id, name) in calls {
            message.content.push(ContentBlock::ToolCall(ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                arguments: json!({"path": format!("{id}.rs")}),
                thought_signature: None,
            }));
        }
        message.stop_reason = StopReason::ToolUse;
        AgentMessage::Assistant(message)
    }

    fn result(id: &str, text: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text(text)],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 1,
        })
    }

    #[test]
    fn request_contains_inputs_but_omits_result_bodies() {
        let messages = vec![
            AgentMessage::user("fix it"),
            assistant(&[("old", "read")], None),
            result("old", "secret result body"),
        ];
        let interactions = collect_interactions(&messages, messages.len());
        let value = serde_json::to_value(build_request(&messages, &interactions)).unwrap();
        let text = value.to_string();
        assert_eq!(value["model"], "jev-latest");
        assert_eq!(value["questions"].as_object().unwrap().len(), 2);
        assert!(text.contains("old.rs"));
        assert!(!text.contains("secret result body"));
    }

    #[test]
    fn decisions_keep_truncate_drop_and_pin_recent_pairs() {
        let long = "x".repeat(500);
        let messages = vec![
            AgentMessage::user("keep prose exactly"),
            assistant(
                &[("keep", "read"), ("shorten", "read"), ("drop", "read")],
                Some("assistant prose"),
            ),
            result("keep", "exact"),
            result("shorten", &long),
            result("drop", "gone"),
            assistant(&[("unmatched", "read")], None),
            result("orphan", "orphan exact"),
            assistant(&[("recent", "read")], None),
            result("recent", "recent exact"),
        ];
        let interactions = collect_interactions(&messages, 7);
        assert_eq!(interactions.len(), 3);
        let output = apply_decisions(
            &messages,
            &interactions,
            &[Decision::Keep, Decision::Truncate, Decision::Drop],
            JevUsage::default(),
        );
        assert_eq!(output.stats.kept, 1);
        assert_eq!(output.stats.truncated, 1);
        assert_eq!(output.stats.dropped, 1);
        assert!(matches!(
            &output.messages[0],
            AgentMessage::User(user) if user.content.as_text() == "keep prose exactly"
        ));
        let serialized = serde_json::to_string(&output.messages).unwrap();
        assert!(serialized.contains("assistant prose"));
        assert!(serialized.contains("exact"));
        assert!(serialized.contains(&"x".repeat(300)));
        assert!(!serialized.contains(&"x".repeat(301)));
        assert!(!serialized.contains("gone"));
        assert!(!serialized.contains("\"id\":\"drop\""));
        assert!(serialized.contains("\"id\":\"unmatched\""));
        assert!(serialized.contains("orphan exact"));
        assert!(serialized.contains("recent exact"));
        assert!(serialized.contains("\"id\":\"recent\""));
    }

    #[test]
    fn malformed_answers_are_rejected() {
        let missing = r#"{"answers":{},"usage":{"input_tokens":1,"output_tokens":1}}"#;
        assert!(parse_response(missing, 1).is_err());
        let invalid = r#"{"answers":{"call_0":{"type":"noul","noul":2},"result_0":{"type":"noul","noul":0}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
        assert!(parse_response(invalid, 1).is_err());
    }
}
