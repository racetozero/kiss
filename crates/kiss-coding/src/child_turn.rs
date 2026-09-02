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
    let guard = if cancel.is_some() || timeout_ms.is_some() {
        let guard_child = child.clone();
        Some(tokio::spawn(async move {
            match (cancel, timeout_ms) {
                (Some(cancel), Some(ms)) => {
                    tokio::select! {
                        _ = cancel.cancelled() => {}
                        _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                    }
                }
                (Some(cancel), None) => cancel.cancelled().await,
                (None, Some(ms)) => tokio::time::sleep(Duration::from_millis(ms)).await,
                (None, None) => return,
            }
            guard_child.abort();
        }))
    } else {
        None
    };

    let usage_before = child.totals();
    child.prompt(vec![AgentMessage::user(prompt)]).await;
    if let Some(guard) = guard {
        guard.abort();
    }
    let usage = usage_delta(child.totals(), usage_before);
    if let Some(parent) = parent.upgrade() {
        parent.record_subagent_usage(usage);
    }
    let (status, result, error) = turn_outcome(child);
    ChildTurnOutcome {
        status,
        result,
        error,
        usage,
    }
}

fn usage_delta(after: Usage, before: Usage) -> Usage {
    Usage {
        input: after.input.saturating_sub(before.input),
        output: after.output.saturating_sub(before.output),
        cache_read: after.cache_read.saturating_sub(before.cache_read),
        cache_write: after.cache_write.saturating_sub(before.cache_write),
        reasoning: after
            .reasoning
            .map(|after| after.saturating_sub(before.reasoning.unwrap_or_default())),
        total_tokens: after.total_tokens.saturating_sub(before.total_tokens),
        cost: kiss_ai::Cost {
            input: (after.cost.input - before.cost.input).max(0.0),
            output: (after.cost.output - before.cost.output).max(0.0),
            cache_read: (after.cost.cache_read - before.cost.cache_read).max(0.0),
            cache_write: (after.cost.cache_write - before.cost.cache_write).max(0.0),
            total: (after.cost.total - before.cost.total).max(0.0),
        },
    }
}
