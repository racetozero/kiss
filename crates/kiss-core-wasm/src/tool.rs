use crate::host;
use crate::types::{ToolDefinitionInput, ToolExecutionMode};
use kiss_agent::{AgentTool, ExecutionMode, ToolResult, ToolUpdateSink};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub struct HostTool {
    callback_id: u32,
    definition: ToolDefinitionInput,
}

impl HostTool {
    pub fn new(callback_id: u32, definition: ToolDefinitionInput) -> Self {
        Self {
            callback_id,
            definition,
        }
    }
}

impl Drop for HostTool {
    fn drop(&mut self) {
        host::remove_tool(self.callback_id);
    }
}

#[async_trait::async_trait]
impl AgentTool for HostTool {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn label(&self) -> &str {
        if self.definition.label.is_empty() {
            &self.definition.name
        } else {
            &self.definition.label
        }
    }

    fn description(&self) -> String {
        self.definition.description.clone()
    }

    fn parameters(&self) -> Value {
        self.definition.parameters.clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        match self.definition.execution_mode {
            ToolExecutionMode::Sequential => ExecutionMode::Sequential,
            ToolExecutionMode::Parallel => ExecutionMode::Parallel,
        }
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        host::launch_tool(
            self.callback_id,
            tool_call_id.to_string(),
            args,
            cancel,
            on_update,
        )
        .await
        .map_err(|_| anyhow::anyhow!("KISS_CLOSED: tool task ended without a result"))?
        .map_err(anyhow::Error::msg)
    }
}
