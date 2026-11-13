//! Dynamic workflows: many child agents orchestrated by a script the model
//! writes.
//!
//! A workflow is built on the same child sessions `spawn_agent` uses, so it is
//! available only when subagents are on. The difference is where the plan
//! lives: with `spawn_agent` the model decides what to start next on every
//! turn, and every result lands in its context. With a workflow the plan is a
//! script, and only the final answer comes back.

mod prompt;
mod runner;
mod store;
mod tool;

pub use prompt::{authoring_prompt, workflow_trigger};
pub use store::{SaveLocation, SavedWorkflow, discover, save};

// Re-exported so the terminal renders workflow state without depending on
// `kiss-workflow` directly.
pub use kiss_workflow::{
    AgentId, AgentOutcome, AgentSnapshot, AgentStatus, PhaseSnapshot, RunSnapshot, RunStatus,
    Script,
};

use crate::session_runner::{AgentSession, WorkflowTurnStatus};
use kiss_agent::DynTool;
use kiss_workflow::{Limits, Workflow};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Identifies one run within a session.
pub type RunId = u64;

/// How many finished runs to keep, so `/workflows` can still show and save a
/// run the user is only now getting back to.
const MAX_RETAINED_RUNS: usize = 20;

/// One run, with the script that produced it.
pub struct RunRecord {
    pub id: RunId,
    pub name: String,
    pub description: String,
    /// The script text, kept so the user can read it and save it afterward.
    pub source: Arc<str>,
    workflow: Arc<Workflow>,
}

impl RunRecord {
    pub fn snapshot(&self) -> Arc<RunSnapshot> {
        self.workflow.snapshot()
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.workflow.subscribe()
    }

    pub fn pause(&self) {
        self.workflow.pause();
    }

    pub fn resume(&self) {
        self.workflow.resume();
    }

    pub fn is_paused(&self) -> bool {
        self.workflow.is_paused()
    }

    pub fn stop(&self) {
        self.workflow.stop();
    }

    pub fn stop_agent(&self, agent: kiss_workflow::AgentId) {
        self.workflow.stop_agent(agent);
    }

    pub fn restart_agent(&self, agent: kiss_workflow::AgentId) {
        self.workflow.restart_agent(agent);
    }
}

/// The text a finished run reports: a string as is, anything else as JSON.
pub fn report_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Null => "The workflow returned nothing.".to_string(),
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
    }
}

/// A run as listed in `/workflows`.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub id: RunId,
    pub name: String,
    pub description: String,
    pub status: RunStatus,
    pub total_agents: usize,
    pub finished_agents: usize,
    pub tokens: u64,
    pub elapsed: Duration,
}

/// What the user decided when asked to approve a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Cancel,
}

/// What the user is shown before a run starts.
#[derive(Debug, Clone)]
pub struct WorkflowPlan {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    /// `None` when the agent count depends on data the run has not fetched yet.
    pub estimated_agents: Option<u32>,
    pub source: Arc<str>,
}

impl WorkflowPlan {
    pub fn from_script(script: &Script) -> WorkflowPlan {
        WorkflowPlan {
            name: script.meta().name.clone(),
            description: script.meta().description.clone(),
            phases: script.declared_phases().to_vec(),
            estimated_agents: script.estimated_agents(),
            source: Arc::from(script.source()),
        }
    }

    /// The agent count as shown to the user.
    ///
    /// A count is only stated when the script fixes it. Otherwise this says so,
    /// because a number in an approval prompt has to be one the user can rely
    /// on.
    pub fn agent_estimate(&self) -> String {
        match self.estimated_agents {
            Some(1) => "1 agent".into(),
            Some(count) => format!("{count} agents"),
            None => "an unbounded number of agents".into(),
        }
    }
}

/// Asks the user to approve a run. Interactive mode installs one. Other modes
/// leave it unset and runs start without asking, since nothing can answer.
pub type WorkflowApprover = Arc<
    dyn Fn(WorkflowPlan) -> futures::future::BoxFuture<'static, ApprovalDecision> + Send + Sync,
>;

/// The workflow runs belonging to one session.
pub struct WorkflowRuntime {
    parent: Weak<AgentSession>,
    runs: Mutex<Vec<Arc<RunRecord>>>,
    next_id: AtomicU64,
}

impl WorkflowRuntime {
    pub(crate) fn new(parent: Weak<AgentSession>) -> Arc<WorkflowRuntime> {
        Arc::new(WorkflowRuntime {
            parent,
            runs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        })
    }

    /// The tool the model calls to run a workflow it has written.
    pub(crate) fn tool(self: &Arc<Self>) -> DynTool {
        Arc::new(tool::RunWorkflowTool::new(self.clone()))
    }

    pub(crate) fn emit_outcome(
        &self,
        run: Option<RunId>,
        name: String,
        status: WorkflowTurnStatus,
    ) {
        if let Some(parent) = self.parent.upgrade() {
            parent.emit_workflow_outcome(run, name, status);
        }
    }

    /// Ask the user to approve a run.
    ///
    /// This is a cost gate rather than a permission gate: one run can start
    /// hundreds of child agents, and the user should see that before it is
    /// spent. Modes with no way to answer, such as `-p`, start the run.
    pub async fn approve(&self, plan: WorkflowPlan) -> ApprovalDecision {
        let Some(parent) = self.parent.upgrade() else {
            return ApprovalDecision::Cancel;
        };
        if !parent.settings().workflows.confirm {
            return ApprovalDecision::Approve;
        }
        match parent.workflow_approver() {
            Some(approver) => approver(plan).await,
            None => ApprovalDecision::Approve,
        }
    }

    /// Prepare a run and add it to the list, without starting it.
    ///
    /// Registering before running is what lets the progress view find the run
    /// as soon as it is approved, rather than after its first agent answers.
    pub fn prepare(
        &self,
        plan: &WorkflowPlan,
        script: Script,
        args: Value,
    ) -> anyhow::Result<Arc<RunRecord>> {
        let parent = self
            .parent
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the parent session has closed"))?;
        let limits = Limits::default();
        let runner = Arc::new(runner::SessionAgentRunner::new(
            self.parent.clone(),
            limits.max_concurrency,
        ));
        let cwd = parent
            .manager
            .lock()
            .map(|manager| manager.cwd().display().to_string())
            .unwrap_or_default();

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let name = plan.name.clone();
        let description = plan.description.clone();
        let source: Arc<str> = Arc::from(script.source());
        let workflow = Arc::new(Workflow::new(script, args, cwd, runner, limits));
        let record = Arc::new(RunRecord {
            id,
            name,
            description,
            source,
            workflow,
        });

        // Forward the workflow's watch channel into the normal session event
        // stream. The terminal then sleeps until state changes instead of
        // polling every frame.
        let mut updates = record.subscribe();
        let parent_for_updates = self.parent.clone();
        let record_for_updates = record.clone();
        tokio::spawn(async move {
            while updates.changed().await.is_ok() {
                let version = *updates.borrow_and_update();
                let Some(parent) = parent_for_updates.upgrade() else {
                    break;
                };
                parent.emit_workflow(id, version);
                if record_for_updates.snapshot().status.is_finished() {
                    break;
                }
            }
        });

        let mut runs = self
            .runs
            .lock()
            .map_err(|_| anyhow::anyhow!("the workflow list is unavailable"))?;
        runs.push(record.clone());
        // Keep the newest runs, but never drop one that is still working.
        while runs.len() > MAX_RETAINED_RUNS {
            let Some(position) = runs
                .iter()
                .position(|run| run.snapshot().status.is_finished())
            else {
                break;
            };
            runs.remove(position);
        }
        Ok(record)
    }

    /// Run a prepared workflow to completion.
    /// Run a prepared workflow until it ends. Cancelling `stop` stops it,
    /// so an interrupted turn or command leaves no child agents working.
    pub async fn run(
        &self,
        record: &Arc<RunRecord>,
        stop: CancellationToken,
    ) -> Result<Value, String> {
        let stopping = record.clone();
        let guard = tokio::spawn(async move {
            stop.cancelled().await;
            stopping.stop();
        });
        let outcome = record
            .workflow
            .run()
            .await
            .map_err(|error| error.to_string());
        guard.abort();
        outcome
    }

    pub fn get(&self, id: RunId) -> Option<Arc<RunRecord>> {
        self.runs
            .lock()
            .ok()?
            .iter()
            .find(|run| run.id == id)
            .cloned()
    }

    /// Every run in this session, oldest first.
    pub fn summaries(&self) -> Vec<RunSummary> {
        let Ok(runs) = self.runs.lock() else {
            return Vec::new();
        };
        runs.iter()
            .map(|run| {
                let snapshot = run.snapshot();
                RunSummary {
                    id: run.id,
                    name: run.name.clone(),
                    description: run.description.clone(),
                    status: snapshot.status,
                    total_agents: snapshot.total_agents(),
                    finished_agents: snapshot.finished_agents(),
                    tokens: snapshot.tokens,
                    elapsed: snapshot.elapsed,
                }
            })
            .collect()
    }

    /// The run still working, if any.
    pub fn active(&self) -> Option<Arc<RunRecord>> {
        let runs = self.runs.lock().ok()?;
        runs.iter()
            .rev()
            .find(|run| !run.snapshot().status.is_finished())
            .cloned()
    }

    /// The most recent run, working or finished.
    pub fn latest(&self) -> Option<Arc<RunRecord>> {
        self.runs.lock().ok()?.last().cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.lock().map(|runs| runs.is_empty()).unwrap_or(true)
    }

    /// Stop every run still working. Called when the session closes or when the
    /// setting is turned off.
    pub(crate) fn stop_all(&self) {
        let Ok(runs) = self.runs.lock() else {
            return;
        };
        for run in runs.iter() {
            if !run.snapshot().status.is_finished() {
                run.stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_estimate_never_guesses_a_number_it_does_not_know() {
        let plan = |estimated_agents| WorkflowPlan {
            name: "x".into(),
            description: "y".into(),
            phases: Vec::new(),
            estimated_agents,
            source: Arc::from(""),
        };
        assert_eq!(plan(Some(1)).agent_estimate(), "1 agent");
        assert_eq!(plan(Some(12)).agent_estimate(), "12 agents");
        assert_eq!(plan(None).agent_estimate(), "an unbounded number of agents");
    }
}
