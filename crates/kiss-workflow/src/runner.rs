//! The boundary between a workflow script and real agents.
//!
//! This crate never starts an agent itself. It hands a request to an
//! [`AgentRunner`] the host supplies. Tests supply a fake runner, so the whole
//! language is exercised without a model.

use serde_json::Value as Json;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

/// Identifies one `agent()` call site by its position in the run.
///
/// Because the interpreter is deterministic, the same script with the same
/// arguments issues the same sequence of calls, so the position is a stable key
/// for [`Journal`] without any bookkeeping in the script itself.
pub type AgentId = u32;

/// One child agent to start.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRequest {
    /// Position of this `agent()` call in the run, counting from zero.
    pub index: AgentId,
    pub prompt: String,
    /// Display name for the progress view. Falls back to the phase and index.
    pub label: Option<String>,
    pub phase: String,
    /// A model pattern for the host to resolve, such as `sonnet` or
    /// `anthropic/claude-sonnet-5`.
    pub model: Option<String>,
    /// A thinking level name, such as `low` or `high`.
    pub effort: Option<String>,
    /// A JSON Schema the answer must satisfy. When set, the call returns parsed
    /// data instead of text.
    pub schema: Option<Json>,
    pub timeout_ms: Option<u64>,
}

/// How one child agent ended.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutcome {
    /// The agent answered. The value is text unless the request carried a
    /// schema, in which case it is the parsed structured answer.
    Done(Json),
    /// The user stopped this agent, or the whole run.
    Stopped,
    /// The agent failed after any retries the host applied.
    Failed(String),
}

impl AgentOutcome {
    /// Only a completed outcome is worth remembering for a resumed run: a
    /// failure must be retried rather than replayed.
    pub(crate) fn is_journalable(&self) -> bool {
        matches!(self, AgentOutcome::Done(_))
    }
}

/// Starts one child agent and waits for its answer.
#[async_trait::async_trait]
pub trait AgentRunner: Send + Sync + 'static {
    async fn run_agent(&self, request: AgentRequest, cancel: CancellationToken) -> AgentOutcome;

    /// Tokens this agent used, for the progress view. Hosts that do not track
    /// usage may leave this at zero.
    fn tokens_used(&self, _index: AgentId) -> u64 {
        0
    }
}

/// Bounds on one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Agents running at once.
    pub max_concurrency: usize,
    /// Agents started over the whole run, which bounds the cost of a runaway
    /// script.
    pub max_agents: u32,
    /// Items one `parallel()` or `pipeline()` call may accept. A longer list is
    /// an error rather than a silent truncation, because dropping part of the
    /// work without saying so is worse than refusing it.
    pub max_fanout: usize,
    /// Interpreter steps, which stops a loop that never terminates.
    pub max_steps: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_concurrency: default_concurrency(),
            max_agents: 1000,
            max_fanout: 4096,
            max_steps: 50_000_000,
        }
    }
}

/// Sixteen at once, or fewer on a machine or container with fewer cores.
fn default_concurrency() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4);
    cores.clamp(2, 16)
}

impl Limits {
    /// Clamp caller-supplied limits into a range the runtime can honor.
    pub fn sanitized(mut self) -> Limits {
        self.max_concurrency = self.max_concurrency.clamp(1, 16);
        self.max_agents = self.max_agents.clamp(1, 1000);
        self.max_fanout = self.max_fanout.clamp(1, 4096);
        self.max_steps = self.max_steps.max(1000);
        self
    }
}

/// Results kept from an earlier run of the same script, so that a stopped run
/// resumes instead of starting over.
///
/// An entry is reused only while the prompt at that position still matches. At
/// the first position whose prompt differs, because the script was edited or an
/// earlier agent answered differently, that agent and every agent after it run
/// again.
#[derive(Debug, Clone, Default)]
pub struct Journal {
    entries: HashMap<AgentId, (String, AgentOutcome)>,
}

impl Journal {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Remember one completed answer for a later deterministic replay.
    ///
    /// Hosts normally obtain a journal from [`crate::Workflow::journal`]. This
    /// constructor is also useful for imported runs and for tests that need a
    /// large completed run without starting real child sessions.
    pub fn record_completed(&mut self, index: AgentId, prompt: &str, value: Json) {
        self.entries
            .insert(index, (prompt.to_string(), AgentOutcome::Done(value)));
    }

    pub(crate) fn record(&mut self, index: AgentId, prompt: &str, outcome: &AgentOutcome) {
        if outcome.is_journalable() {
            self.entries
                .insert(index, (prompt.to_string(), outcome.clone()));
        }
    }

    /// The remembered outcome for this position, when its prompt still matches.
    pub(crate) fn take_matching(&self, index: AgentId, prompt: &str) -> Option<AgentOutcome> {
        let (recorded, outcome) = self.entries.get(&index)?;
        (recorded == prompt).then(|| outcome.clone())
    }

    /// Forget this position and every later one.
    ///
    /// Called when a prompt stops matching: everything after a changed input is
    /// no longer trustworthy, even if it completed.
    pub(crate) fn invalidate_from(&mut self, index: AgentId) {
        self.entries.retain(|recorded, _| *recorded < index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_completed_agents_are_remembered() {
        let mut journal = Journal::default();
        journal.record(0, "a", &AgentOutcome::Done(Json::String("ok".into())));
        journal.record(1, "b", &AgentOutcome::Failed("boom".into()));
        journal.record(2, "c", &AgentOutcome::Stopped);
        assert_eq!(journal.len(), 1);
        assert!(journal.take_matching(1, "b").is_none());
    }

    #[test]
    fn a_host_can_seed_a_completed_result() {
        let mut journal = Journal::default();
        journal.record_completed(4, "audit a.rs", Json::String("ok".into()));
        assert_eq!(
            journal.take_matching(4, "audit a.rs"),
            Some(AgentOutcome::Done(Json::String("ok".into())))
        );
    }

    #[test]
    fn a_changed_prompt_does_not_match_its_remembered_result() {
        let mut journal = Journal::default();
        journal.record(
            0,
            "audit a.rs",
            &AgentOutcome::Done(Json::String("ok".into())),
        );
        assert!(journal.take_matching(0, "audit a.rs").is_some());
        assert!(journal.take_matching(0, "audit b.rs").is_none());
    }

    #[test]
    fn invalidating_a_position_drops_everything_after_it() {
        let mut journal = Journal::default();
        for index in 0..5 {
            journal.record(index, "p", &AgentOutcome::Done(Json::Null));
        }
        journal.invalidate_from(2);
        assert_eq!(journal.len(), 2);
        assert!(journal.take_matching(1, "p").is_some());
        assert!(journal.take_matching(2, "p").is_none());
    }

    #[test]
    fn limits_are_clamped_into_a_range_the_runtime_can_honor() {
        let limits = Limits {
            max_concurrency: 500,
            max_agents: 100_000,
            max_fanout: usize::MAX,
            max_steps: 1,
        }
        .sanitized();
        assert_eq!(limits.max_concurrency, 16);
        assert_eq!(limits.max_agents, 1000);
        assert_eq!(limits.max_fanout, 4096);
        assert_eq!(limits.max_steps, 1000);
    }

    #[test]
    fn the_default_concurrency_stays_within_bounds() {
        let limits = Limits::default();
        assert!((2..=16).contains(&limits.max_concurrency));
    }
}
