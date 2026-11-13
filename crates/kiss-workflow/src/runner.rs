//! The boundary between a workflow script and real agents.
//!
//! This crate never starts an agent itself. It hands a request to an
//! [`AgentRunner`] the host supplies. Tests supply a fake runner, so the whole
//! language is exercised without a model.

use serde_json::Value as Json;
use tokio_util::sync::CancellationToken;

/// Identifies one `agent()` call site by its position in the run.
///
/// Because the interpreter is deterministic, the same script with the same
/// arguments issues the same sequence of calls.
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

/// The most child agents running at once that the runtime allows.
const MAX_CONCURRENCY: usize = 16;
/// The most child agents one run may start, and the default.
const MAX_AGENTS: u32 = 1000;
/// The largest list one `parallel` or `pipeline` call may fan out over.
const MAX_FANOUT: usize = 4096;

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_concurrency: default_concurrency(),
            max_agents: MAX_AGENTS,
            max_fanout: MAX_FANOUT,
            max_steps: 50_000_000,
        }
    }
}

/// The most at once, or fewer on a machine or container with fewer cores.
fn default_concurrency() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4);
    cores.clamp(2, MAX_CONCURRENCY)
}

impl Limits {
    /// Clamp caller-supplied limits into a range the runtime can honor.
    pub fn sanitized(mut self) -> Limits {
        self.max_concurrency = self.max_concurrency.clamp(1, MAX_CONCURRENCY);
        self.max_agents = self.max_agents.clamp(1, MAX_AGENTS);
        self.max_fanout = self.max_fanout.clamp(1, MAX_FANOUT);
        self.max_steps = self.max_steps.max(1000);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
