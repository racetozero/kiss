//! The `run_workflow` tool.
//!
//! The tool waits for the run to finish and returns its answer, so the model
//! gets the report the way it gets any other tool result. The terminal stays
//! usable throughout, because this is an ordinary async tool: the agent loop
//! awaits it while the terminal keeps drawing and keeps handling keys.

use super::{ApprovalDecision, WorkflowPlan, WorkflowRuntime};
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

        let plan = WorkflowPlan {
            name: args
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| script.meta().name.clone()),
            description: args
                .description
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| script.meta().description.clone()),
            phases: script.declared_phases().to_vec(),
            estimated_agents: script.estimated_agents(),
            source: Arc::from(script.source()),
        };

        if self.0.approve(plan).await == ApprovalDecision::Cancel {
            return Ok(ToolResult::text(
                "The user did not approve this workflow, so nothing ran. Ask what they would \
                 like to change, or carry out the task directly instead.",
            ));
        }

        let record = self.0.prepare(script, Value::Null, Default::default())?;

        // Stopping the turn stops the run, so an interrupted turn does not
        // leave child agents working in the background.
        let stopping = record.clone();
        let guard_cancel = cancel.clone();
        let guard = tokio::spawn(async move {
            guard_cancel.cancelled().await;
            stopping.stop();
        });
        let outcome = self.0.run(&record).await;
        guard.abort();

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
                Ok(ToolResult::text(format!(
                    "The user stopped this workflow after {summary}. Completed results are kept, \
                     so relaunching the same script reuses them."
                )))
            }
            Ok(value) => {
                let report = match value {
                    Value::String(text) => text,
                    Value::Null => "The workflow returned nothing.".to_string(),
                    other => serde_json::to_string_pretty(&other)?,
                };
                Ok(ToolResult::text(format!(
                    "{report}\n\n[workflow: {summary}]"
                )))
            }
            Err(error) => Err(anyhow::anyhow!(
                "The workflow failed after {summary}.\n{error}"
            )),
        }
    }
}
