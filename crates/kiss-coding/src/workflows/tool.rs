//! The `run_workflow` tool.
//!
//! The tool waits for the run to finish and returns its answer, so the model
//! gets the report the way it gets any other tool result. The terminal stays
//! usable throughout, because this is an ordinary async tool: the agent loop
//! awaits it while the terminal keeps drawing and keeps handling keys.

use super::{ApprovalDecision, WorkflowPlan, WorkflowRuntime};
use crate::session_runner::WorkflowTurnStatus;
use kiss_agent::{AgentTool, ExecutionMode, ToolResult, ToolUpdateSink};
use kiss_workflow::Script;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct RunWorkflowTool(Arc<WorkflowRuntime>);

impl RunWorkflowTool {
    pub(crate) fn new(runtime: Arc<WorkflowRuntime>) -> RunWorkflowTool {
        RunWorkflowTool(runtime)
    }
}

#[derive(Deserialize)]
struct RunWorkflowArgs {
    script: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait::async_trait]
impl AgentTool for RunWorkflowTool {
    fn name(&self) -> &str {
        "run_workflow"
    }

    fn description(&self) -> String {
        "Run a dynamic workflow: a script that starts many child agents, collects their answers, \
         and returns one result. Use this instead of working through a large task turn by turn."
            .into()
    }

    /// A workflow can start hundreds of agents. Running it on its own, rather
    /// than beside other tool calls from the same turn, keeps that work
    /// attributable and keeps the progress view showing one thing at a time.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The workflow script, starting with its `export const meta` block."
                },
                "name": {
                    "type": "string",
                    "description": "Short kebab-case name, used if the user saves this workflow."
                },
                "description": {
                    "type": "string",
                    "description": "One sentence saying what the workflow does."
                }
            },
            "required": ["script"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        let args: RunWorkflowArgs = serde_json::from_value(args)?;

        // A script that does not parse comes back as a tool error carrying the
        // rendered diagnostic, so the model can correct it and call again.
        let script = Script::parse(&args.script)
            .map_err(|diagnostic| anyhow::anyhow!("{}", diagnostic.render(&args.script)))?;

        let mut plan = WorkflowPlan::from_script(&script);
        plan.name = args
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(plan.name);
        plan.description = args
            .description
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(plan.description);

        let workflow_name = plan.name.clone();

        if self.0.approve(plan.clone()).await == ApprovalDecision::Cancel {
            self.0
                .emit_outcome(None, workflow_name, WorkflowTurnStatus::Cancelled);
            return Ok(ToolResult::text(
                "The user did not approve this workflow, so nothing ran. Ask what they would \
                 like to change, or carry out the task directly instead.",
            ));
        }

        let record = self.0.prepare(&plan, script, Value::Null)?;
        let outcome = self.0.run(&record, cancel.clone()).await;

        let snapshot = record.snapshot();
        let summary = format!(
            "{} agents across {} phases, {} tokens",
            snapshot.total_agents(),
            snapshot
                .phases
                .iter()
                .filter(|p| !p.agents.is_empty())
                .count(),
            snapshot.tokens,
        );

        match outcome {
            Ok(Value::Null) if snapshot.status == kiss_workflow::RunStatus::Stopped => {
                self.0.emit_outcome(
                    Some(record.id),
                    record.name.clone(),
                    WorkflowTurnStatus::Stopped,
                );
                Ok(ToolResult::text(format!(
                    "The user stopped this workflow after {summary}."
                )))
            }
            Ok(value) => {
                self.0.emit_outcome(
                    Some(record.id),
                    record.name.clone(),
                    WorkflowTurnStatus::Completed,
                );
                Ok(ToolResult::text(format!(
                    "{}\n\n[workflow: {summary}]",
                    super::report_text(value)
                )))
            }
            Err(error) => {
                self.0.emit_outcome(
                    Some(record.id),
                    record.name.clone(),
                    WorkflowTurnStatus::Failed,
                );
                Err(anyhow::anyhow!(
                    "The workflow failed after {summary}.\n{error}"
                ))
            }
        }
    }
}
