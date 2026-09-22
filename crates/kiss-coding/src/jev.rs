use anyhow::{Context as _, bail};
use kiss_agent::AgentMessage;
use kiss_ai::{ContentBlock, Model, ThinkingLevel, ToolResultMessage, UserContent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const KEEP_THRESHOLD: f64 = 0.5;
const TRUNCATE_HEAD_CHARS: usize = 300;
const MAX_TARGET_CHARS: usize = 200;
const MAX_TASK_CHARS: usize = 4_000;
const MAX_PROGRESS_ITEMS: usize = 6;
const MAX_PROGRESS_CHARS: usize = 1_200;
const MAX_TOOL_INTERACTIONS: usize = 6;
const MAX_TOOL_INPUT_CHARS: usize = 1_000;
const MAX_TOOL_OUTPUT_CHARS: usize = 2_000;

struct Interaction {
    id: String,
    tool: String,
}

#[derive(Serialize)]
struct JevRequest<Q> {
    state: Value,
    model: &'static str,
    questions: BTreeMap<String, Q>,
}

#[derive(Serialize)]
struct NoulQuestion {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: String,
}

#[derive(Serialize)]
struct ChoiceQuestion {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'static str,
    criteria: BTreeMap<&'static str, &'static str>,
}

#[derive(Deserialize)]
struct JevResponse {
    answers: BTreeMap<String, JevAnswer>,
    usage: JevUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum JevAnswer {
    #[serde(rename = "noul")]
    Noul { noul: f64 },
    #[serde(rename = "choice")]
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
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

pub(crate) struct ReasoningSelection {
    pub level: ThinkingLevel,
    pub generations: u8,
}

#[derive(Default)]
pub(crate) struct ReasoningLease {
    level: Option<ThinkingLevel>,
    remaining: u8,
}

impl ReasoningLease {
    pub fn consume_generation(&mut self) {
        if self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.level = None;
            }
        }
    }

    pub fn current(&self) -> Option<ThinkingLevel> {
        self.level
    }

    pub fn install(&mut self, selection: &ReasoningSelection) {
        self.level = Some(selection.level);
        self.remaining = selection.generations;
    }

    pub fn clear(&mut self) {
        self.level = None;
        self.remaining = 0;
    }
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
    let send = http_client()
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

pub(crate) async fn select_reasoning(
    messages: &[AgentMessage],
    queued: &[AgentMessage],
    model: &Model,
    api_key: &str,
    cancel: CancellationToken,
) -> anyhow::Result<ReasoningSelection> {
    let supported = model.supported_thinking_levels();
    if supported.is_empty() {
        bail!(
            "Jev dynamic reasoning was selected for {}/{}, but the model has no supported reasoning efforts; choose a reasoning model with supported efforts or set Dynamic reasoning to fixed",
            model.provider,
            model.id
        );
    }
    let request = build_reasoning_request(messages, queued, model, &supported);
    let send = http_client()
        .post(ENDPOINT)
        .timeout(Duration::from_secs(2))
        .bearer_auth(api_key)
        .json(&request)
        .send();
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("Jev reasoning selection was cancelled; retry the task or set Dynamic reasoning to fixed"),
        response = send => response.context("send Jev reasoning selection request")?,
    };
    let status = response.status();
    let body = tokio::select! {
        _ = cancel.cancelled() => bail!("Jev reasoning selection was cancelled; retry the task or set Dynamic reasoning to fixed"),
        body = response.text() => body.context("read Jev reasoning selection response")?,
    };
    if !status.is_success() {
        bail!(
            "Jev reasoning selection failed with {status}: {}; retry with valid TypeSafe credentials or set Dynamic reasoning to fixed",
            body.trim()
        );
    }
    parse_reasoning_response(&body, &supported)
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
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

fn build_request(
    messages: &[AgentMessage],
    interactions: &[Interaction],
) -> JevRequest<NoulQuestion> {
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

fn build_reasoning_request(
    messages: &[AgentMessage],
    queued: &[AgentMessage],
    model: &Model,
    supported: &[ThinkingLevel],
) -> JevRequest<ChoiceQuestion> {
    let task = queued
        .iter()
        .rev()
        .chain(messages.iter().rev())
        .find_map(|message| match message {
            AgentMessage::User(user) => Some(user_content_excerpt(&user.content, MAX_TASK_CHARS)),
            _ => None,
        })
        .unwrap_or_default();

    let mut progress = messages
        .iter()
        .rev()
        .filter_map(|message| match message {
            AgentMessage::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant| assistant.content.iter().rev())
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(json!({
                "kind": "progress",
                "text": bounded_text(text, MAX_PROGRESS_CHARS),
            })),
            ContentBlock::Thinking { thinking, .. } => Some(json!({
                "kind": "reasoning_summary",
                "text": bounded_text(thinking, MAX_PROGRESS_CHARS),
            })),
            _ => None,
        })
        .take(MAX_PROGRESS_ITEMS)
        .collect::<Vec<_>>();
    progress.reverse();

    let mut tools = Vec::with_capacity(MAX_TOOL_INTERACTIONS);
    for result in messages.iter().rev().filter_map(|message| match message {
        AgentMessage::ToolResult(result) => Some(result),
        _ => None,
    }) {
        let call = messages.iter().rev().find_map(|message| match message {
            AgentMessage::Assistant(assistant) => assistant
                .tool_calls()
                .find(|call| call.id == result.tool_call_id),
            _ => None,
        });
        let Some(call) = call else {
            continue;
        };
        tools.push(json!({
            "tool": call.name,
            "input": bounded_text(&call.arguments.to_string(), MAX_TOOL_INPUT_CHARS),
            "output": content_excerpt(&result.content, MAX_TOOL_OUTPUT_CHARS),
            "failed": result.is_error,
        }));
        if tools.len() == MAX_TOOL_INTERACTIONS {
            break;
        }
    }
    tools.reverse();

    let effort_criteria = supported
        .iter()
        .map(|level| {
            (
                level.as_str(),
                match level {
                    ThinkingLevel::Off => "No reasoning for a direct answer or trivial step",
                    ThinkingLevel::Minimal => "Very light reasoning for a simple task",
                    ThinkingLevel::Low => {
                        "Routine, clear, or mechanical work with little uncertainty"
                    }
                    ThinkingLevel::Medium => "Normal coding work that needs some analysis",
                    ThinkingLevel::High => {
                        "Complex work, ambiguity, debugging, or important tradeoffs"
                    }
                    ThinkingLevel::Xhigh => {
                        "The agent is stuck or the next step needs deep reasoning"
                    }
                    ThinkingLevel::Max => {
                        "Repeated failure or an exceptionally difficult high-stakes step"
                    }
                },
            )
        })
        .collect();
    let questions = BTreeMap::from([
        (
            "effort".into(),
            ChoiceQuestion {
                kind: "choice",
                instructions: "Which supported reasoning effort should the active model use for the next generation?",
                criteria: effort_criteria,
            },
        ),
        (
            "generations".into(),
            ChoiceQuestion {
                kind: "choice",
                instructions: "For how many future model generations should this effort remain useful before reassessment?",
                criteria: BTreeMap::from([
                    (
                        "1",
                        "Reassess after the next generation because the phase is changing",
                    ),
                    ("2", "The near-term phase is stable for two generations"),
                    ("5", "The current phase is stable for several generations"),
                    ("10", "The work is repetitive and likely to stay stable"),
                ]),
            },
        ),
    ]);
    JevRequest {
        state: json!({
            "target": {
                "provider": bounded_text(&model.provider, MAX_TARGET_CHARS),
                "id": bounded_text(&model.id, MAX_TARGET_CHARS),
                "name": bounded_text(model.display_name(), MAX_TARGET_CHARS),
            },
            "task": task,
            "progress": progress,
            "tools": tools,
        }),
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
        let JevAnswer::Noul { noul } = answer else {
            bail!("Jev returned the wrong answer type for {key}; expected noul");
        };
        if !noul.is_finite() || !(0.0..=1.0).contains(noul) {
            bail!("Jev returned an invalid probability for {key}; expected a number from 0 to 1");
        }
        Ok(*noul)
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

fn parse_reasoning_response(
    body: &str,
    supported: &[ThinkingLevel],
) -> anyhow::Result<ReasoningSelection> {
    let response: JevResponse = serde_json::from_str(body)
        .context("parse Jev reasoning selection response; expected System One Choice JSON")?;
    let choice = |key: &str, allowed: &[&str]| -> anyhow::Result<&str> {
        let answer = response.answers.get(key).with_context(|| {
            format!(
                "Jev reasoning response is missing {key}; expected effort and generations choices"
            )
        })?;
        let JevAnswer::Choice {
            choice,
            probabilities,
            confidence,
        } = answer
        else {
            bail!("Jev returned the wrong answer type for {key}; expected choice");
        };
        if !allowed.contains(&choice.as_str()) {
            bail!(
                "Jev selected invalid {key} value '{choice}'; expected one of {}",
                allowed.join(", ")
            );
        }
        if !confidence.is_finite() || !(0.0..=1.0).contains(confidence) {
            bail!("Jev returned invalid confidence for {key}; expected a number from 0 to 1");
        }
        if probabilities.len() != allowed.len()
            || allowed.iter().any(|option| {
                probabilities
                    .get(*option)
                    .is_none_or(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            })
            || (probabilities.values().sum::<f64>() - 1.0).abs() > 0.01
        {
            bail!(
                "Jev returned invalid probabilities for {key}; expected every allowed option with values that sum to 1"
            );
        }
        Ok(choice)
    };
    let allowed = supported
        .iter()
        .map(ThinkingLevel::as_str)
        .collect::<Vec<_>>();
    let level =
        ThinkingLevel::parse(choice("effort", &allowed)?).context("parse validated Jev effort")?;
    let generations = choice("generations", &["1", "2", "5", "10"])?
        .parse()
        .context("parse validated Jev generation lease")?;
    Ok(ReasoningSelection { level, generations })
}

fn bounded_text(text: &str, max_chars: usize) -> String {
    let Some((end, _)) = text.char_indices().nth(max_chars) else {
        return text.to_string();
    };
    format!("{}…", &text[..end])
}

fn user_content_excerpt(content: &UserContent, max_chars: usize) -> String {
    match content {
        UserContent::Text(text) => bounded_text(text, max_chars),
        UserContent::Blocks(blocks) => content_excerpt(blocks, max_chars),
    }
}

fn content_excerpt(content: &[ContentBlock], max_chars: usize) -> String {
    let mut excerpt = String::new();
    let mut remaining = max_chars;
    for text in content.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(text),
        _ => None,
    }) {
        if !excerpt.is_empty() && remaining > 0 {
            excerpt.push('\n');
            remaining -= 1;
        }
        let kept = text.chars().take(remaining).collect::<String>();
        remaining -= kept.chars().count();
        excerpt.push_str(&kept);
        if remaining == 0 {
            excerpt.push('…');
            break;
        }
    }
    excerpt
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
    use kiss_ai::{AssistantMessage, Registry, StopReason, ToolCall};

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

    fn reasoning_response(effort: &str, generations: &str, efforts: &[&str]) -> String {
        let probability = 1.0 / efforts.len() as f64;
        let probabilities = efforts
            .iter()
            .map(|effort| ((*effort).to_string(), probability))
            .collect::<BTreeMap<_, _>>();
        json!({
            "answers": {
                "effort": {
                    "type": "choice",
                    "choice": effort,
                    "confidence": 0.5,
                    "probabilities": probabilities
                },
                "generations": {
                    "type": "choice",
                    "choice": generations,
                    "confidence": 0.5,
                    "probabilities": {"1": 0.25, "2": 0.25, "5": 0.25, "10": 0.25}
                }
            },
            "usage": {"input_tokens": 12, "output_tokens": 3}
        })
        .to_string()
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

    #[test]
    fn reasoning_request_is_bounded_and_uses_choice_questions() {
        let registry = Registry::from_builtin();
        let (mut model, _) = registry.resolve("openai/gpt-6-astra", None).unwrap();
        model.name = "界".repeat(MAX_TARGET_CHARS + 10);
        let supported = model.supported_thinking_levels();
        let mut messages = vec![AgentMessage::user("old task")];
        for index in 0..8 {
            let id = format!("call_{index}");
            let mut message = AssistantMessage::empty("test", "test", "test");
            message.content.push(ContentBlock::Thinking {
                thinking: format!("summary {index} {}", "界".repeat(2_000)),
                thinking_signature: None,
                redacted: false,
            });
            message.content.push(ContentBlock::ToolCall(ToolCall {
                id: id.clone(),
                name: "read".into(),
                arguments: json!({"path": format!("{id}-{}", "界".repeat(2_000))}),
                thought_signature: None,
            }));
            message.stop_reason = StopReason::ToolUse;
            messages.push(AgentMessage::Assistant(message));
            messages.push(result(
                &id,
                &format!("output {index} {}", "界".repeat(3_000)),
            ));
        }
        let request = build_reasoning_request(
            &messages,
            &[AgentMessage::user(format!(
                "new queued task {}",
                "界".repeat(5_000)
            ))],
            &model,
            &supported,
        );
        let value = serde_json::to_value(request).unwrap();

        assert_eq!(value["model"], "jev-latest");
        assert_eq!(value["state"]["target"]["provider"], "openai");
        assert_eq!(value["state"]["target"]["id"], "gpt-6-astra");
        assert!(
            value["state"]["target"]["name"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= MAX_TARGET_CHARS + 1
        );
        assert_eq!(value["questions"]["effort"]["type"], "choice");
        assert_eq!(
            value["questions"]["effort"]["criteria"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["high", "low", "max", "medium", "xhigh"]
        );
        assert_eq!(value["questions"]["generations"]["type"], "choice");
        assert_eq!(
            value["questions"]["generations"]["criteria"]
                .as_object()
                .unwrap()
                .len(),
            4
        );
        assert!(
            value["state"]["task"]
                .as_str()
                .unwrap()
                .starts_with("new queued task")
        );
        assert!(value["state"]["task"].as_str().unwrap().chars().count() <= MAX_TASK_CHARS + 1);
        assert_eq!(value["state"]["progress"].as_array().unwrap().len(), 6);
        let tools = value["state"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 6);
        assert!(tools[0]["input"].as_str().unwrap().contains("call_2"));
        assert!(tools[5]["input"].as_str().unwrap().contains("call_7"));
        assert!(tools.iter().all(
            |tool| tool["input"].as_str().unwrap().chars().count() <= MAX_TOOL_INPUT_CHARS + 1
        ));
        assert!(
            tools
                .iter()
                .all(|tool| tool["output"].as_str().unwrap().chars().count()
                    <= MAX_TOOL_OUTPUT_CHARS + 1)
        );
    }

    #[test]
    fn reasoning_response_accepts_only_documented_choices() {
        let supported = [
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ];
        let efforts = ["low", "medium", "high", "xhigh", "max"];
        let selection =
            parse_reasoning_response(&reasoning_response("xhigh", "5", &efforts), &supported)
                .unwrap();
        assert_eq!(selection.level, ThinkingLevel::Xhigh);
        assert_eq!(selection.generations, 5);

        assert!(
            parse_reasoning_response(&reasoning_response("minimal", "5", &efforts), &supported)
                .is_err()
        );
        assert!(
            parse_reasoning_response(&reasoning_response("high", "3", &efforts), &supported)
                .is_err()
        );
        let wrong_type = reasoning_response("high", "5", &efforts)
            .replace("\"type\":\"choice\"", "\"type\":\"noul\",\"noul\":0.5");
        assert!(parse_reasoning_response(&wrong_type, &supported).is_err());
    }

    #[test]
    fn reasoning_lease_counts_generations_and_clears_early() {
        for generations in [1, 2, 5, 10] {
            let selection = ReasoningSelection {
                level: ThinkingLevel::High,
                generations,
            };
            let mut lease = ReasoningLease::default();
            lease.install(&selection);
            for _ in 1..generations {
                assert_eq!(lease.current(), Some(ThinkingLevel::High));
                lease.consume_generation();
            }
            assert_eq!(lease.current(), Some(ThinkingLevel::High));
            lease.consume_generation();
            assert_eq!(lease.current(), None);
        }

        let mut lease = ReasoningLease::default();
        lease.install(&ReasoningSelection {
            level: ThinkingLevel::Max,
            generations: 10,
        });
        lease.clear();
        assert_eq!(lease.current(), None);
    }

    #[test]
    fn reasoning_choices_match_the_active_model() {
        let registry = Registry::from_builtin();
        let (model, _) = registry.resolve("openai/gpt-5.4", None).unwrap();
        let supported = model.supported_thinking_levels();
        let request = build_reasoning_request(&[], &[], &model, &supported);
        let value = serde_json::to_value(request).unwrap();
        let efforts = value["questions"]["effort"]["criteria"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(efforts, ["high", "low", "medium", "off", "xhigh"]);

        let selection =
            parse_reasoning_response(&reasoning_response("off", "2", &efforts), &supported)
                .unwrap();
        assert_eq!(selection.level, ThinkingLevel::Off);
        assert!(
            parse_reasoning_response(&reasoning_response("minimal", "2", &efforts), &supported,)
                .is_err()
        );
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_reasoning_router() {
        let selection = ReasoningSelection {
            level: ThinkingLevel::Low,
            generations: 10,
        };
        let mut active = ReasoningLease::default();
        active.install(&selection);
        kiss_bench::measure_pair(
            ("jev_reasoning_disabled", "jev_reasoning_active_lease"),
            21,
            1_000_000,
            ("no_request", "no_request_reuse_10_generations"),
            || {
                let mut lease = ReasoningLease::default();
                lease.clear();
                lease.current()
            },
            || {
                if active.current().is_none() {
                    active.install(&selection);
                }
                let level = active.current();
                active.consume_generation();
                level
            },
        );
    }
}
