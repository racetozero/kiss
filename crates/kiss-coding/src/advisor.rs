//! The advisor: a second model, often from another provider, that the agent
//! consults through a tool when it needs stronger judgment.

use crate::compaction::serialize_conversation_capped;
use crate::session_runner::AgentSession;
use anyhow::Context as _;
use kiss_agent::{AgentMessage, AgentTool, ToolResult, ToolUpdateSink};
use serde_json::{Value, json};
use std::sync::Weak;
use tokio_util::sync::CancellationToken;

pub const ADVISOR_TOOL_NAME: &str = "advisor";

/// Advisor calls allowed per user prompt when `advisorMaxUses` is not set.
pub const DEFAULT_ADVISOR_MAX_USES: u32 = 3;

/// The advisor's answer, without its thinking. The advice itself asks for at
/// most 700 words; this leaves room so a long answer is not cut off.
const ADVICE_TOKENS: u64 = 2_048;

/// Told to the working model while an advisor is set.
pub(crate) fn advisor_system_prompt(max_uses: u32) -> String {
    format!(
        "{ADVISOR_SYSTEM_PROMPT}\n- You can call it at most {max_uses} times for each user request. Save the calls for the moments that need them.\n- The advice starts with PLAN, CORRECTION, or STOP. After STOP, stop working and ask the user what the advice says to ask."
    )
}

const ADVISOR_SYSTEM_PROMPT: &str = "Advisor:\n- The advisor tool sends your whole conversation to a stronger reviewer model and returns its guidance. It takes no arguments.\n- Call it alone, not in the same turn as other tool calls: it sees only the results of earlier turns.\n- On a task with more than a few steps, call it after orientation (reading files, seeing what is there) and before you commit to an approach.\n- Call it when you are stuck: errors recur, an approach does not converge, or results do not fit.\n- Call it when you believe a long task is complete, after you saved the result.\n- Give the advice serious weight. If your evidence contradicts it, say so in one more call instead of switching silently.\n- Do not call it for short tasks whose next step is clear.";

/// The advisor model's own instructions.
const ADVISOR_INSTRUCTIONS: &str = "You advise a coding agent. You receive the agent's instructions and a transcript of its session. The transcript ends where the agent asked for your advice. You cannot run tools, and the user does not see your answer; the agent acts on it.

Start your answer with exactly one of these words on its own line:
PLAN - the agent is on track or has not started. Give the steps to take, what to check first, and the risks.
CORRECTION - the agent made a mistake or is going the wrong way. Say what is wrong, why, and what to do instead.
STOP - the agent must stop and ask the user, because the task is unclear, unsafe, or not possible as stated. Say why and what to ask.

Then give the guidance in at most 700 words. Name files, functions, and commands. Say plainly when the agent is wrong. Do not repeat the transcript.";

/// Tool results are cut to this many characters in the advisor transcript.
/// The advisor needs more evidence than a compaction summary does.
const TOOL_RESULT_CAP: usize = 8_000;

const OMITTED: &str = "\n[... earlier transcript omitted ...]\n";

/// The advisor's prompt: the agent's instructions and its conversation as
/// labeled text. A transcript longer than `max_chars` keeps the first user
/// message and the most recent part.
pub(crate) fn advisor_prompt(
    system_prompt: &str,
    messages: &[AgentMessage],
    max_chars: usize,
) -> String {
    let instructions = truncate_chars(system_prompt, max_chars / 4);
    let budget = max_chars.saturating_sub(instructions.len());
    let llm_messages = kiss_agent::convert_to_llm(messages);
    let mut transcript = serialize_conversation_capped(&llm_messages, TOOL_RESULT_CAP);
    if transcript.len() > budget {
        let first_user = llm_messages
            .iter()
            .position(|message| matches!(message, kiss_ai::Message::User(_)))
            .map(|index| {
                serialize_conversation_capped(&llm_messages[index..=index], TOOL_RESULT_CAP)
            })
            .unwrap_or_default();
        let head = truncate_chars(&first_user, budget / 4);
        let tail_len = budget.saturating_sub(head.len() + OMITTED.len());
        let mut start = transcript.len() - tail_len.min(transcript.len());
        while !transcript.is_char_boundary(start) {
            start += 1;
        }
        // Start the tail at a whole line.
        if let Some(newline) = transcript[start..].find('\n') {
            start += newline + 1;
        }
        transcript = format!("{head}{OMITTED}{}", &transcript[start..]);
    }
    format!(
        "<agent_instructions>\n{instructions}\n</agent_instructions>\n\n<transcript>\n{transcript}</transcript>\n\nThe agent called the advisor at the end of this transcript. Give your advice."
    )
}

fn truncate_chars(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(crate) struct AdvisorTool(pub(crate) Weak<AgentSession>);

#[async_trait::async_trait]
impl AgentTool for AdvisorTool {
    fn name(&self) -> &str {
        ADVISOR_TOOL_NAME
    }

    fn description(&self) -> String {
        "Ask a stronger advisor model for guidance on a hard decision, an ambiguous failure, or a problem you are not solving. Your whole conversation is sent automatically, so this tool takes no arguments.".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        let session = self
            .0
            .upgrade()
            .context("the session that owns the advisor tool is closed")?;
        let (model, thinking) = session.advisor().context(
            "No advisor model is set. Run /advisor to choose one, or set advisorModel in ~/.kiss/agent/settings.json.",
        )?;
        let max_uses = session.advisor_max_uses();
        let call = session
            .advisor_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if call > max_uses {
            anyhow::bail!(
                "The advisor limit of {max_uses} calls for this user request is reached. Continue with the advice you have, or ask the user. The user can raise the limit with advisorMaxUses in ~/.kiss/agent/settings.json."
            );
        }
        let messages = session
            .manager
            .lock()
            .unwrap()
            .build_session_context()
            .messages;
        // About half the advisor's window at three characters per token, the
        // density of code and JSON. The rest is left for the answer.
        let max_chars = match model.context_window {
            0 => 300_000,
            window => {
                usize::try_from(window)
                    .unwrap_or(usize::MAX / 2)
                    .saturating_mul(3)
                    / 2
            }
        };
        let prompt = advisor_prompt(&session.base_system_prompt(), &messages, max_chars);
        let label = format!("{}/{}", model.provider, model.id);
        // Room for the thinking budget plus a short answer, so an advisor
        // that is asked for a short plan cannot write a long one.
        let budget = kiss_ai::api::thinking_budget(thinking, u64::MAX).unwrap_or(0);
        let max_tokens = match model.max_tokens {
            0 => ADVICE_TOKENS + budget,
            limit => (ADVICE_TOKENS + budget).min(limit),
        };
        let response = session
            .run_ephemeral(
                "advisor",
                (model.clone(), thinking),
                ADVISOR_INSTRUCTIONS.into(),
                prompt,
                Vec::new(),
                Some(max_tokens),
                cancel,
            )
            .await
            .with_context(|| {
                format!(
                    "The advisor {label} did not answer. Continue without it, or ask the user to run /login {} or to choose another advisor with /advisor.",
                    model.provider
                )
            })?;
        Ok(ToolResult {
            content: vec![kiss_ai::ContentBlock::text(format!(
                "{}\n\n[Advisor call {call} of {max_uses} for this user request.]",
                response.text
            ))],
            details: json!({"model": label, "thinking": thinking.as_str(), "call": call, "maxUses": max_uses}),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_transcript_keeps_the_task_and_the_latest_turns() {
        let mut messages = vec![AgentMessage::user("THE TASK")];
        for index in 0..200 {
            messages.push(AgentMessage::user(format!(
                "turn {index} {}",
                "x".repeat(100)
            )));
        }
        let prompt = advisor_prompt("instructions", &messages, 4_000);

        assert!(prompt.len() < 4_400, "{}", prompt.len());
        assert!(prompt.contains("[User]: THE TASK"));
        assert!(prompt.contains("earlier transcript omitted"));
        assert!(prompt.contains("turn 199"));
        assert!(!prompt.contains("turn 100 "));
        assert!(prompt.contains("<agent_instructions>\ninstructions\n"));
    }

    #[test]
    fn a_short_transcript_is_sent_whole() {
        let messages = vec![
            AgentMessage::user("fix the bug"),
            AgentMessage::user("ü".repeat(10)),
        ];
        let prompt = advisor_prompt("instructions", &messages, 100_000);
        assert!(!prompt.contains("omitted"));
        assert!(prompt.contains("[User]: fix the bug\n[User]: üüüüüüüüüü\n</transcript>"));
    }
}
