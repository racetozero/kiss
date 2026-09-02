//! JSON event-stream mode: every session event as one JSON line on stdout.
//! Streaming message_update events omit the cumulative partial snapshot.

use crate::args::Args;
use crate::setup::build_startup;
use anyhow::Result;
use kiss_agent::AgentEvent;
use kiss_ai::AssistantEvent;
use kiss_coding::session_runner::{SessionEvent, WorkflowTurnStatus};
use serde_json::{Value, json};
use std::io::Read;
use std::sync::Arc;

fn assistant_event_json(event: &AssistantEvent) -> Value {
    // Delta-only wire form: no cumulative partial snapshots.
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
        AssistantEvent::ToolCallStart { content_index, .. } => {
            json!({"type": "toolcall_start", "contentIndex": content_index})
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

pub fn event_json(event: &SessionEvent) -> Option<Value> {
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
                json!({"type": "message_update", "assistantMessageEvent": assistant_event_json(assistant_event.as_ref())})
            }
            AgentEvent::MessageEnd { message } => {
                json!({"type": "message_end", "message": message})
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                json!({"type": "tool_execution_start", "toolCallId": tool_call_id, "toolName": tool_name, "args": args})
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial,
            } => json!({
                "type": "tool_execution_update", "toolCallId": tool_call_id, "toolName": tool_name,
                "args": args, "partialResult": {"content": partial.content, "details": partial.details},
            }),
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => json!({
                "type": "tool_execution_end", "toolCallId": tool_call_id, "toolName": tool_name,
                "result": {"content": result.content, "details": result.details}, "isError": is_error,
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
            "type": "compaction_end", "summary": summary, "tokensBefore": tokens_before, "error": error,
        }),
        SessionEvent::Retry {
            attempt,
            max,
            delay_ms,
            error,
        } => json!({
            "type": "retry", "attempt": attempt, "max": max, "delayMs": delay_ms, "error": error,
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

pub async fn run(args: &Args) -> Result<i32> {
    let sink = Arc::new(move |event: SessionEvent| {
        if let Some(value) = event_json(&event) {
            println!("{value}");
        }
    });
    let startup = build_startup(args, false, sink).await?;

    let mut prompt = startup.initial_message.clone().unwrap_or_default();
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        if !piped.trim().is_empty() {
            prompt = if prompt.is_empty() {
                piped.trim().to_string()
            } else {
                format!("{}\n\n{prompt}", piped.trim())
            };
        }
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("no prompt provided");
    }
    let prompt_mode = startup.session.prompt_mode_for(&prompt);
    startup
        .session
        .prompt_with_mode(vec![kiss_agent::AgentMessage::user(prompt)], prompt_mode)
        .await;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_outcomes_have_a_controller_owned_json_event() {
        let event = event_json(&SessionEvent::WorkflowOutcome {
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
}
