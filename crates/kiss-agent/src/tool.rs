//! The tool contract used by the agent runtime.

use kiss_ai::{ContentBlock, ToolDef, Usage};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    /// Text or image content returned to the model.
    pub content: Vec<ContentBlock>,
    /// Structured details for logs / UI rendering (never sent to the model).
    pub details: Value,
    /// Usage from nested LLM work performed by the tool, if any.
    pub usage: Option<Usage>,
    /// Hint that the agent should stop after the current tool batch.
    pub terminate: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        ToolResult {
            content: vec![ContentBlock::text(text)],
            ..Default::default()
        }
    }

    pub fn output_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Sink for streaming partial tool results (e.g. live bash output).
pub type ToolUpdateSink = Arc<dyn Fn(ToolResult) + Send + Sync>;

#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str {
        self.name()
    }
    fn description(&self) -> String;
    /// JSON schema object describing the arguments.
    fn parameters(&self) -> Value;
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }
    /// Normalize raw arguments before schema validation (compat shims).
    fn prepare_arguments(&self, args: Value) -> Value {
        args
    }
    /// Execute. Return Err on failure; the loop converts it into an error
    /// tool result visible to the model.
    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult>;

    fn to_def(&self) -> ToolDef {
        ToolDef {
            name: self.name().to_string(),
            description: self.description(),
            parameters: self.parameters(),
        }
    }
}

pub type DynTool = Arc<dyn AgentTool>;
