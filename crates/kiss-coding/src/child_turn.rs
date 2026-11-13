//! Shared execution for one child-session turn.

use crate::session_runner::AgentSession;
use crate::subagents::{AgentStatus, turn_outcome};
use kiss_agent::AgentMessage;
use kiss_ai::Usage;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(crate) struct ChildTurnOutcome {
    pub status: AgentStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub usage: Usage,
}

/// Run one child turn, account for its cost, and read its final answer.
///
/// If a cancel token or time limit is present, the child is asked to abort.
/// The prompt future is still awaited so its session stays consistent.
pub(crate) async fn run_child_turn(
    parent: &Weak<AgentSession>,
    child: &Arc<AgentSession>,
    prompt: String,
    cancel: Option<CancellationToken>,
    timeout_ms: Option<u64>,
) -> ChildTurnOutcome {
    let stopped = async {
        match (cancel, timeout_ms) {
            (Some(cancel), Some(ms)) => {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                }
            }
            (Some(cancel), None) => cancel.cancelled().await,
            (None, Some(ms)) => tokio::time::sleep(Duration::from_millis(ms)).await,
            (None, None) => std::future::pending().await,
        }
    };

    let parent_session = parent
        .upgrade()
        .map(|parent| parent.manager.lock().unwrap().session_id().to_string());
    let usage_before = child.totals();
    let run = child.prompt(vec![AgentMessage::user(prompt)]);
    tokio::pin!(run);
    tokio::select! {
        _ = &mut run => {}
        _ = stopped => {
            child.abort();
            run.await;
        }
    }
    let usage = child.totals().saturating_sub(&usage_before);
    if let (Some(parent), Some(session_id)) = (parent.upgrade(), parent_session) {
        let model = child.model();
        parent.record_usage(&session_id, "subagent", &model.provider, &model.id, usage);
    }
    let (status, result, error) = turn_outcome(child);
    ChildTurnOutcome {
        status,
        result,
        error,
        usage,
    }
}
