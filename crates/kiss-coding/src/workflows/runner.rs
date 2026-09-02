//! Running one workflow agent as a real KISS child session.

use crate::child_turn;
use crate::session_runner::AgentSession;
use crate::subagents::ForkTurns;
use kiss_workflow::{AgentId, AgentOutcome, AgentRequest, AgentRunner};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Starts each workflow agent as a fresh child session of the parent.
///
/// Workflow agents get their own concurrency budget rather than sharing the
/// four permits the `spawn_agent` runtime uses: a fan-out wants up to sixteen
/// at once and would otherwise starve behind hand-started children.
pub(crate) struct SessionAgentRunner {
    parent: Weak<AgentSession>,
    permits: Arc<Semaphore>,
    tokens: Mutex<HashMap<AgentId, u64>>,
}

impl SessionAgentRunner {
    pub(crate) fn new(parent: Weak<AgentSession>, concurrency: usize) -> SessionAgentRunner {
        SessionAgentRunner {
            parent,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            tokens: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl AgentRunner for SessionAgentRunner {
    async fn run_agent(&self, request: AgentRequest, cancel: CancellationToken) -> AgentOutcome {
        if cancel.is_cancelled() {
            return AgentOutcome::Stopped;
        }
        let Some(parent) = self.parent.upgrade() else {
            return AgentOutcome::Failed("the parent session has closed".into());
        };
        let _permit = match self.permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return AgentOutcome::Stopped,
        };

        let task_name = task_name_for(&request);
        let canonical_path = format!("/root/{task_name}");
        let child = match parent.create_subagent_session(
            &task_name,
            &canonical_path,
            // A workflow agent gets exactly the task the script wrote. Copying
            // the parent conversation would defeat the point of keeping
            // intermediate results out of that conversation.
            ForkTurns::None,
            request.model.as_deref(),
            request.effort.as_deref(),
        ) {
            Ok(child) => child,
            Err(error) => return AgentOutcome::Failed(format!("{error:#}")),
        };

        let prompt = match &request.schema {
            Some(schema) => format!("{}\n\n{}", request.prompt, schema_instruction(schema)),
            None => request.prompt.clone(),
        };

        let outcome = child_turn::run_child_turn(
            &self.parent,
            &child,
            prompt,
            Some(cancel.clone()),
            request.timeout_ms,
        )
        .await;
        if let Ok(mut tokens) = self.tokens.lock() {
            *tokens.entry(request.index).or_default() += outcome.usage.total_tokens;
        }

        if cancel.is_cancelled() {
            return AgentOutcome::Stopped;
        }
        match outcome.status {
            crate::subagents::AgentStatus::Completed => {
                let text = outcome.result.unwrap_or_default();
                match &request.schema {
                    Some(schema) => match parse_structured(&text, schema) {
                        Ok(value) => AgentOutcome::Done(value),
                        // A schema miss is recoverable: the interpreter retries
                        // it when the script asked for retries.
                        Err(reason) => AgentOutcome::Failed(reason),
                    },
                    None => AgentOutcome::Done(Value::String(text)),
                }
            }
            crate::subagents::AgentStatus::Interrupted => AgentOutcome::Stopped,
            _ => AgentOutcome::Failed(outcome.error.unwrap_or_else(|| "the agent failed".into())),
        }
    }

    fn tokens_used(&self, index: AgentId) -> u64 {
        self.tokens
            .lock()
            .ok()
            .and_then(|tokens| tokens.get(&index).copied())
            .unwrap_or(0)
    }
}

/// A task name for the child session, within the name rules `spawn_agent` uses.
fn task_name_for(request: &AgentRequest) -> String {
    let mut name = String::from("wf_");
    for character in request.phase.chars() {
        if name.len() >= 48 {
            break;
        }
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }
    if !name.ends_with('_') {
        name.push('_');
    }
    name.push_str(&request.index.to_string());
    name
}

/// Tell the agent to answer with JSON only, and show it the shape.
fn schema_instruction(schema: &Value) -> String {
    let shape = serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
    format!(
        "Reply with JSON only, matching this JSON Schema. Do not add any text before or after \
         it, and do not wrap it in a code fence.\n\n{shape}"
    )
}

/// Read the agent's answer as JSON and check it against the schema.
fn parse_structured(text: &str, schema: &Value) -> Result<Value, String> {
    let candidate = strip_code_fence(text.trim());
    let value: Value = serde_json::from_str(candidate)
        .map_err(|error| format!("the agent's answer was not valid JSON: {error}"))?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("the schema in this script is not valid: {error}"))?;
    if let Err(error) = validator.validate(&value) {
        return Err(format!(
            "the agent's answer did not match the requested schema: {error}"
        ));
    }
    Ok(value)
}

/// Remove a Markdown code fence, which models add even when asked not to.
fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    // Drop an optional language tag on the opening line.
    let rest = match rest.split_once('\n') {
        Some((_language, body)) => body,
        None => return text,
    };
    rest.trim_end()
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(phase: &str, index: AgentId) -> AgentRequest {
        AgentRequest {
            index,
            prompt: "check it".into(),
            label: None,
            phase: phase.into(),
            model: None,
            effort: None,
            schema: None,
            timeout_ms: None,
        }
    }

    #[test]
    fn task_names_follow_the_subagent_naming_rules() {
        // `spawn_agent` accepts 1 to 64 lower-case letters, digits, and
        // underscores, and a workflow child must not be the thing that breaks
        // that rule.
        for phase in ["Discover", "Audit files!", "Report / summary", ""] {
            let name = task_name_for(&request(phase, 7));
            assert!(!name.is_empty() && name.len() <= 64, "{name}");
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
                "{name}"
            );
            assert!(name.ends_with('7'), "{name}");
        }
    }

    #[test]
    fn a_very_long_phase_title_still_yields_a_short_name() {
        let name = task_name_for(&request(&"verification".repeat(20), 3));
        assert!(name.len() <= 64, "{name} is {} bytes", name.len());
    }

    #[test]
    fn each_agent_gets_its_own_task_name() {
        assert_ne!(
            task_name_for(&request("Audit", 1)),
            task_name_for(&request("Audit", 2))
        );
    }

    #[test]
    fn a_fenced_answer_is_unwrapped_before_parsing() {
        let schema = json!({"type": "object", "required": ["files"]});
        let fenced = "```json\n{\"files\": [\"a.rs\"]}\n```";
        let value = parse_structured(fenced, &schema).expect("the fence is stripped");
        assert_eq!(value, json!({"files": ["a.rs"]}));
    }

    #[test]
    fn a_plain_json_answer_parses() {
        let schema = json!({"type": "object"});
        assert_eq!(
            parse_structured("  {\"ok\": true}  ", &schema).unwrap(),
            json!({"ok": true})
        );
    }

    #[test]
    fn an_answer_that_misses_the_schema_is_a_recoverable_failure() {
        let schema = json!({
            "type": "object",
            "required": ["files"],
            "properties": {"files": {"type": "array"}},
        });
        let error = parse_structured("{\"other\": 1}", &schema).unwrap_err();
        assert!(error.contains("did not match the requested schema"));

        let error = parse_structured("not json at all", &schema).unwrap_err();
        assert!(error.contains("not valid JSON"));
    }

    #[test]
    fn the_schema_instruction_shows_the_shape_and_forbids_a_fence() {
        let instruction = schema_instruction(&json!({"type": "object"}));
        assert!(instruction.contains("JSON only"));
        assert!(instruction.contains("code fence"));
        assert!(instruction.contains("\"type\": \"object\""));
    }

    #[test]
    fn text_without_a_fence_is_left_alone() {
        assert_eq!(strip_code_fence("{\"a\": 1}"), "{\"a\": 1}");
        // An unterminated fence is left as-is so the JSON error names the real
        // problem.
        assert_eq!(strip_code_fence("```json\n{"), "```json\n{");
    }
}
