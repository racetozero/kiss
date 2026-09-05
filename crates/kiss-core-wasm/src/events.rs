use kiss_agent::AgentEvent;
use kiss_ai::AssistantEvent;
use serde_json::{Value, json};

pub fn assistant_event_json(event: &AssistantEvent) -> Value {
    match event {
        AssistantEvent::Start { .. } => json!({"type": "start"}),
        AssistantEvent::TextStart { content_index } => {
            json!({"type": "text_start", "contentIndex": content_index})
        }
        AssistantEvent::TextDelta {
            content_index,
            delta,
        } => json!({"type": "text_delta", "contentIndex": content_index, "delta": delta}),
        AssistantEvent::TextEnd {
            content_index,
            content,
        } => json!({"type": "text_end", "contentIndex": content_index, "content": content}),
        AssistantEvent::ThinkingStart { content_index } => {
            json!({"type": "thinking_start", "contentIndex": content_index})
        }
        AssistantEvent::ThinkingDelta {
            content_index,
            delta,
        } => json!({"type": "thinking_delta", "contentIndex": content_index, "delta": delta}),
        AssistantEvent::ThinkingEnd {
            content_index,
            content,
        } => json!({"type": "thinking_end", "contentIndex": content_index, "content": content}),
        AssistantEvent::ToolCallStart {
            content_index,
            tool_call,
        } => json!({
            "type": "toolcall_start", "contentIndex": content_index,
            "id": tool_call.id, "toolName": tool_call.name,
        }),
        AssistantEvent::ToolCallDelta {
            content_index,
            delta,
        } => json!({"type": "toolcall_delta", "contentIndex": content_index, "delta": delta}),
        AssistantEvent::ToolCallEnd {
            content_index,
            tool_call,
        } => json!({
            "type": "toolcall_end", "contentIndex": content_index, "toolCall": tool_call,
        }),
        AssistantEvent::Done { reason, message } => {
            json!({"type": "done", "reason": reason, "message": message})
        }
        AssistantEvent::Error { reason, message } => {
            json!({"type": "error", "reason": reason, "error": message})
        }
    }
}

pub fn agent_event_json(event: &AgentEvent) -> Value {
    match event {
        AgentEvent::AgentStart => json!({"type": "agent_start"}),
        AgentEvent::AgentEnd { messages } => json!({"type": "agent_end", "messages": messages}),
        AgentEvent::TurnStart => json!({"type": "turn_start"}),
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => json!({"type": "turn_end", "message": message, "toolResults": tool_results}),
        AgentEvent::MessageStart { message } => {
            json!({"type": "message_start", "message": message})
        }
        AgentEvent::MessageUpdate { assistant_event } => json!({
            "type": "message_update",
            "assistantMessageEvent": assistant_event_json(assistant_event),
        }),
        AgentEvent::MessageEnd { message } => {
            json!({"type": "message_end", "message": message})
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => json!({
            "type": "tool_execution_start", "toolCallId": tool_call_id,
            "toolName": tool_name, "args": args,
        }),
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial,
        } => json!({
            "type": "tool_execution_update", "toolCallId": tool_call_id,
            "toolName": tool_name, "args": args,
            "partialResult": {"content": partial.content, "details": partial.details},
        }),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => json!({
            "type": "tool_execution_end", "toolCallId": tool_call_id,
            "toolName": tool_name,
            "result": {"content": result.content, "details": result.details},
            "isError": is_error,
        }),
    }
}
