//! Events emitted by the agent loop for UIs and persistence.

use crate::message::AgentMessage;
use crate::tool::ToolResult;
use kiss_ai::{AssistantEvent, ToolResultMessage};
use serde_json::Value;

#[derive(Debug, Clone)]
// Event payloads are consumed immediately and the harness boxes AgentEvent at
// its queue boundary. Keeping direct payloads makes the public match surface
// simpler without retaining a large queue of enum values.
#[allow(clippy::large_enum_variant)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    /// Streaming assistant updates only.
    MessageUpdate {
        assistant_event: Box<AssistantEvent>,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial: ToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: ToolResult,
        is_error: bool,
    },
}
