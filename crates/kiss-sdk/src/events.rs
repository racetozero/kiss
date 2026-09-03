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

use kiss_agent::AgentEvent;
use kiss_ai::AssistantEvent;
use kiss_coding::session_runner::{SessionEvent, WorkflowTurnStatus};
use serde_json::{Value, json};

/// Encode one streaming assistant delta.
pub fn assistant_event_json(event: &AssistantEvent) -> Value {
    match event {
        AssistantEvent::Start { .. } => json!({"type": "start"}),
        AssistantEvent::TextStart { content_index, .. } => {
            json!({"type": "text_start", "contentIndex": content_index})
        }
        AssistantEvent::TextDelta {
            content_index,
            delta,
            ..
        } => {
            json!({"type": "text_delta", "contentIndex": content_index, "delta": delta})
        }
        AssistantEvent::TextEnd {
            content_index,
            content,
            ..
        } => {
            json!({"type": "text_end", "contentIndex": content_index, "content": content})
        }
        AssistantEvent::ThinkingStart { content_index, .. } => {
            json!({"type": "thinking_start", "contentIndex": content_index})
        }
        AssistantEvent::ThinkingDelta {
            content_index,
            delta,
            ..
        } => {
            json!({"type": "thinking_delta", "contentIndex": content_index, "delta": delta})
        }
        AssistantEvent::ThinkingEnd {
            content_index,
            content,
            ..
        } => {
            json!({"type": "thinking_end", "contentIndex": content_index, "content": content})
        }
        AssistantEvent::ToolCallStart {
            content_index,
            tool_call,
        } => {
            json!({
                "type": "toolcall_start", "contentIndex": content_index,
                "id": tool_call.id, "toolName": tool_call.name,
            })
        }
        AssistantEvent::ToolCallDelta {
            content_index,
            delta,
            ..
        } => {
            json!({"type": "toolcall_delta", "contentIndex": content_index, "delta": delta})
        }
        AssistantEvent::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => {
            json!({"type": "toolcall_end", "contentIndex": content_index, "toolCall": tool_call})
        }
        AssistantEvent::Done { reason, message } => {
            json!({"type": "done", "reason": reason, "message": message})
        }
        AssistantEvent::Error { reason, message } => {
            json!({"type": "error", "reason": reason, "error": message})
        }
    }
}

/// Encode one harness event, or `None` when it has no public form.
pub fn session_event_json(event: &SessionEvent) -> Option<Value> {
    Some(match event {
        SessionEvent::Agent(agent_event) => match agent_event.as_ref() {
            AgentEvent::AgentStart => json!({"type": "agent_start"}),
            AgentEvent::AgentEnd { messages } => json!({"type": "agent_end", "messages": messages}),
            AgentEvent::TurnStart => json!({"type": "turn_start"}),
            AgentEvent::TurnEnd {
                message,
                tool_results,
            } => {
                json!({"type": "turn_end", "message": message, "toolResults": tool_results})
            }
            AgentEvent::MessageStart { message } => {
                json!({"type": "message_start", "message": message})
            }
            AgentEvent::MessageUpdate {
                assistant_event, ..
            } => {
                json!({
                    "type": "message_update",
                    "assistantMessageEvent": assistant_event_json(assistant_event.as_ref()),
                })
            }
            AgentEvent::MessageEnd { message } => {
                json!({"type": "message_end", "message": message})
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                json!({
                    "type": "tool_execution_start", "toolCallId": tool_call_id,
                    "toolName": tool_name, "args": args,
                })
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial,
            } => json!({
                "type": "tool_execution_update", "toolCallId": tool_call_id, "toolName": tool_name,
                "args": args,
                "partialResult": {"content": partial.content, "details": partial.details},
            }),
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => json!({
                "type": "tool_execution_end", "toolCallId": tool_call_id, "toolName": tool_name,
                "result": {"content": result.content, "details": result.details},
                "isError": is_error,
            }),
        },
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
    })
}

/// Emitted by the SDK once a prompt run has fully settled: no retry, no
/// automatic compaction, and no queued message remains. A client uses it to
/// know the agent is idle again.
pub fn agent_settled() -> Value {
    json!({"type": "agent_settled"})
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
    fn a_tool_call_start_names_the_tool_so_clients_can_show_it_immediately() {
        let event = assistant_event_json(&AssistantEvent::ToolCallStart {
            content_index: 1,
            tool_call: kiss_ai::ToolCall {
                id: "call_1".into(),
                name: "write".into(),
                arguments: json!({}),
                thought_signature: None,
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
