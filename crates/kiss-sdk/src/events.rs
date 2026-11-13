//! Turning internal harness events into the JSON every SDK surface publishes.
//!
//! There is exactly one encoder so the `--mode json` stream, the `--mode rpc`
//! stream, the Rust event channel, and the Python and TypeScript event
//! iterators all show a caller the same object for the same occurrence.
//!
//! Streaming `message_update` events carry only the delta, never a cumulative
//! snapshot of the partial message. A client that wants a live partial message
//! assembles it from `message_start` plus the deltas, keyed by `contentIndex`,
//! and treats `message_end.message` as authoritative. Sending the snapshot on
//! every token would multiply the bytes on the wire by the length of the reply.

pub use kiss_agent::event_json::{agent_event_json, agent_settled, assistant_event_json};
use kiss_coding::session_runner::{SessionEvent, WorkflowTurnStatus};
use serde_json::{Value, json};

/// Encode one harness event, or `None` when it has no public form.
pub fn session_event_json(event: &SessionEvent) -> Option<Value> {
    Some(match event {
        // The SDK emits agent_settled after awaiting the whole prompt itself.
        SessionEvent::PromptEnd { .. } | SessionEvent::SaveFailed { .. } => return None,
        SessionEvent::Agent(agent_event) => agent_event_json(agent_event),
        SessionEvent::QueueUpdate {
            steering,
            follow_up,
        } => {
            json!({"type": "queue_update", "steering": steering, "followUp": follow_up})
        }
        SessionEvent::CompactionStart { auto } => json!({"type": "compaction_start", "auto": auto}),
        SessionEvent::CompactionEnd {
            summary,
            tokens_before,
            error,
        } => json!({
            "type": "compaction_end", "summary": summary,
            "tokensBefore": tokens_before, "error": error,
        }),
        SessionEvent::Retry {
            attempt,
            max,
            delay_ms,
            error,
        } => json!({
            "type": "retry", "attempt": attempt, "max": max,
            "delayMs": delay_ms, "error": error,
        }),
        SessionEvent::ModelChanged { provider, model_id } => {
            json!({"type": "model_changed", "provider": provider, "modelId": model_id})
        }
        SessionEvent::ReasoningEffortChanged {
            decision_model,
            level,
            generations,
        } => json!({
            "type": "reasoning_effort_changed",
            "decisionModel": decision_model,
            "level": level.as_str(),
            "generations": generations,
        }),
        SessionEvent::ReasoningEffortFallback { level, reason } => json!({
            "type": "reasoning_effort_fallback",
            "level": level.as_str(),
            "reason": reason,
        }),
        SessionEvent::Workflow { run, version } => {
            json!({"type": "workflow_progress", "run": run, "version": version})
        }
        SessionEvent::WorkflowOutcome { run, name, status } => {
            let status = match status {
                WorkflowTurnStatus::Cancelled => "cancelled",
                WorkflowTurnStatus::Completed => "completed",
                WorkflowTurnStatus::Failed => "failed",
                WorkflowTurnStatus::Stopped => "stopped",
            };
            json!({"type": "workflow_outcome", "run": run, "name": name, "status": status})
        }
        SessionEvent::Iterative { job, version } => {
            json!({"type": "iterative_progress", "job": job, "version": version})
        }
        SessionEvent::AccountSwitched(switch) => json!({
            "type": "account_switched", "provider": switch.provider, "from": switch.from,
            "to": switch.to, "index": switch.index, "total": switch.total,
            "retryAfterSecs": switch.retry_after_secs,
        }),
    })
}

/// Emitted while a direct `bash` *command* streams output. `id` is the
/// correlation id of the originating request, so a client can tell two
/// concurrent commands apart. This is distinct from `tool_execution_update`,
/// which reports a shell command the *model* asked for.
pub fn bash_execution_update(id: Option<&str>, delta: &str) -> Value {
    match id {
        Some(id) => json!({"type": "bash_execution_update", "id": id, "delta": delta}),
        None => json!({"type": "bash_execution_update", "delta": delta}),
    }
}

/// Emitted when a subscriber consumed events too slowly and the buffer
/// overwrote some. The client should re-read state rather than assume it saw
/// everything.
pub fn event_lag(skipped: u64) -> Value {
    json!({"type": "event_lag", "skipped": skipped})
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::AssistantEvent;

    #[test]
    fn workflow_outcomes_have_a_controller_owned_json_event() {
        let event = session_event_json(&SessionEvent::WorkflowOutcome {
            run: None,
            name: "audit".into(),
            status: WorkflowTurnStatus::Cancelled,
        })
        .expect("workflow outcome event");
        assert_eq!(
            event,
            json!({
                "type": "workflow_outcome",
                "run": null,
                "name": "audit",
                "status": "cancelled"
            })
        );
    }

    #[test]
    fn reasoning_effort_changes_have_a_public_json_event() {
        let event = session_event_json(&SessionEvent::ReasoningEffortChanged {
            decision_model: "d1-ultra · Liquid".into(),
            level: kiss_ai::ThinkingLevel::Xhigh,
            generations: 2,
        })
        .unwrap();
        assert_eq!(
            event,
            json!({
                "type": "reasoning_effort_changed",
                "decisionModel": "d1-ultra · Liquid",
                "level": "xhigh",
                "generations": 2
            })
        );
    }

    #[test]
    fn reasoning_fallback_reports_the_saved_effort_and_cause() {
        let event = session_event_json(&SessionEvent::ReasoningEffortFallback {
            level: kiss_ai::ThinkingLevel::Medium,
            reason: "request timed out".into(),
        })
        .unwrap();
        assert_eq!(
            event,
            json!({
                "type": "reasoning_effort_fallback",
                "level": "medium",
                "reason": "request timed out"
            })
        );
    }

    #[test]
    fn a_tool_call_start_names_the_tool_so_clients_can_show_it_immediately() {
        let event = assistant_event_json(&AssistantEvent::ToolCallStart {
            content_index: 1,
            tool_call: kiss_ai::ToolCall {
                id: "call_1".into(),
                name: "write".into(),
                arguments: json!({}),
                thought_signature: None,
                arguments_error: None,
            },
        });
        assert_eq!(event["type"], "toolcall_start");
        assert_eq!(event["id"], "call_1");
        assert_eq!(event["toolName"], "write");
    }

    #[test]
    fn bash_updates_carry_their_request_id() {
        assert_eq!(
            bash_execution_update(Some("req-1"), "hello"),
            json!({"type": "bash_execution_update", "id": "req-1", "delta": "hello"})
        );
    }
}
