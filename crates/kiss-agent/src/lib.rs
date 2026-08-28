//! kiss-agent: the agent runtime — turn loop, events, tool contract, and
//! the four harness-core tools (read, bash, edit, write).

pub mod agent_loop;
pub mod config;
pub mod events;
pub mod message;
pub mod tool;
pub mod tools;
pub mod validate;

pub use agent_loop::{EventSink, run_agent_loop, run_agent_loop_continue};
pub use config::{AgentContext, AgentLoopConfig, BeforeToolCallResult, TurnInfo, TurnUpdate};
pub use events::AgentEvent;
pub use message::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomMessage, convert_to_llm,
};
pub use tool::{AgentTool, DynTool, ExecutionMode, ToolResult, ToolUpdateSink};
