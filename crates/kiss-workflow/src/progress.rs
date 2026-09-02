//! Live run state and the snapshots a user interface renders.
//!
//! State changes bump a version counter published on a `watch` channel. A
//! viewer subscribes and redraws only when the version moves, so nothing polls.
//! Snapshots are built at most once per change and shared behind an `Arc`.

use crate::runner::AgentId;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Stopped,
}

impl RunStatus {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Stopped
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Paused => "paused",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Stopped,
    /// Replayed from a previous run rather than started again.
    Reused,
}

impl AgentStatus {
    pub fn is_finished(self) -> bool {
        !matches!(self, AgentStatus::Queued | AgentStatus::Running)
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Queued => "queued",
            AgentStatus::Running => "running",
            AgentStatus::Completed => "completed",
            AgentStatus::Failed => "failed",
            AgentStatus::Stopped => "stopped",
            AgentStatus::Reused => "reused",
        }
    }
}

/// One agent as shown in the progress view.
#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    pub id: AgentId,
    pub label: String,
    pub status: AgentStatus,
    pub tokens: u64,
    pub elapsed: Duration,
    pub prompt: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// One phase, with its agents.
#[derive(Debug, Clone)]
pub struct PhaseSnapshot {
    pub title: String,
    pub agents: Vec<AgentSnapshot>,
    pub tokens: u64,
}

impl PhaseSnapshot {
    pub fn finished_agents(&self) -> usize {
        self.agents
            .iter()
            .filter(|agent| agent.status.is_finished())
            .count()
    }
}

/// The whole run as shown in the progress view.
#[derive(Debug, Clone)]
pub struct RunSnapshot {
    pub status: RunStatus,
    pub elapsed: Duration,
    pub log: Vec<String>,
    pub phases: Vec<PhaseSnapshot>,
    pub tokens: u64,
    pub error: Option<String>,
}

impl RunSnapshot {
    pub fn total_agents(&self) -> usize {
        self.phases.iter().map(|phase| phase.agents.len()).sum()
    }

    pub fn finished_agents(&self) -> usize {
        self.phases.iter().map(PhaseSnapshot::finished_agents).sum()
    }

    /// The phase with work in flight, or the last one that has any agents.
    pub fn active_phase(&self) -> Option<&PhaseSnapshot> {
        self.phases
            .iter()
            .find(|phase| phase.agents.iter().any(|agent| !agent.status.is_finished()))
            .or_else(|| {
                self.phases
                    .iter()
                    .rev()
                    .find(|phase| !phase.agents.is_empty())
            })
    }
}

struct AgentRecord {
    id: AgentId,
    label: String,
    status: AgentStatus,
    tokens: u64,
    started: Option<Instant>,
    elapsed: Duration,
    prompt: String,
    result: Option<String>,
    error: Option<String>,
    cancel: CancellationToken,
}

struct PhaseRecord {
    title: String,
    agents: Vec<AgentRecord>,
}

struct Inner {
    status: RunStatus,
    started: Instant,
    finished: Option<Instant>,
    log: Vec<String>,
    phases: Vec<PhaseRecord>,
    current_phase: usize,
    error: Option<String>,
}

/// The live state of one run.
pub(crate) struct RunState {
    inner: Mutex<Inner>,
    version: watch::Sender<u64>,
    /// The snapshot for the current version, built on first request.
    cached: Mutex<Option<(u64, Arc<RunSnapshot>)>>,
    paused: AtomicBool,
    resumed: Notify,
    stop: CancellationToken,
}

/// The phase used before a script calls `phase()`.
pub(crate) const DEFAULT_PHASE: &str = "Workflow";

impl RunState {
    pub(crate) fn new(declared_phases: &[String]) -> Arc<RunState> {
        let mut phases: Vec<PhaseRecord> = declared_phases
            .iter()
            .map(|title| PhaseRecord {
                title: title.clone(),
                agents: Vec::new(),
            })
            .collect();
        if phases.is_empty() {
            phases.push(PhaseRecord {
                title: DEFAULT_PHASE.to_string(),
                agents: Vec::new(),
            });
        }
        let (version, _) = watch::channel(0);
        Arc::new(RunState {
            inner: Mutex::new(Inner {
                status: RunStatus::Running,
                started: Instant::now(),
                finished: None,
                log: Vec::new(),
                phases,
                current_phase: 0,
                error: None,
            }),
            version,
            cached: Mutex::new(None),
            paused: AtomicBool::new(false),
            resumed: Notify::new(),
            stop: CancellationToken::new(),
        })
    }

    pub(crate) fn stop_token(&self) -> CancellationToken {
        self.stop.clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    fn changed(&self) {
        if let Ok(mut cached) = self.cached.lock() {
            *cached = None;
        }
        self.version
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Select the phase later agents belong to, adding it when it is new.
    pub(crate) fn set_phase(&self, title: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        match inner.phases.iter().position(|phase| phase.title == title) {
            Some(index) => inner.current_phase = index,
            None => {
                inner.phases.push(PhaseRecord {
                    title: title.to_string(),
                    agents: Vec::new(),
                });
                inner.current_phase = inner.phases.len() - 1;
            }
        }
        drop(inner);
        self.changed();
    }

    pub(crate) fn current_phase_title(&self) -> String {
        let Ok(inner) = self.inner.lock() else {
            return DEFAULT_PHASE.to_string();
        };
        inner
            .phases
            .get(inner.current_phase)
            .map(|phase| phase.title.clone())
            .unwrap_or_else(|| DEFAULT_PHASE.to_string())
    }

    pub(crate) fn log(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.log.push(message);
        }
        self.changed();
    }

    /// Register an agent before it starts, returning the token that stops it.
    pub(crate) fn register_agent(
        &self,
        id: AgentId,
        label: String,
        prompt: String,
    ) -> CancellationToken {
        let cancel = self.stop.child_token();
        if let Ok(mut inner) = self.inner.lock() {
            let phase = inner.current_phase;
            if let Some(phase) = inner.phases.get_mut(phase) {
                phase.agents.push(AgentRecord {
                    id,
                    label,
                    status: AgentStatus::Queued,
                    tokens: 0,
                    started: None,
                    elapsed: Duration::ZERO,
                    prompt,
                    result: None,
                    error: None,
                    cancel: cancel.clone(),
                });
            }
        }
        self.changed();
        cancel
    }

    pub(crate) fn agent_started(&self, id: AgentId) {
        self.with_agent(id, |agent| {
            agent.status = AgentStatus::Running;
            agent.started = Some(Instant::now());
        });
    }

    pub(crate) fn agent_finished(
        &self,
        id: AgentId,
        status: AgentStatus,
        result: Option<String>,
        error: Option<String>,
        tokens: u64,
    ) {
        self.with_agent(id, |agent| {
            agent.status = status;
            agent.result = result;
            agent.error = error;
            agent.tokens = tokens;
            if let Some(started) = agent.started {
                agent.elapsed = started.elapsed();
            }
        });
    }

    fn with_agent(&self, id: AgentId, apply: impl FnOnce(&mut AgentRecord)) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(agent) = inner
                .phases
                .iter_mut()
                .flat_map(|phase| phase.agents.iter_mut())
                .find(|agent| agent.id == id)
        {
            apply(agent);
        }
        self.changed();
    }

    pub(crate) fn finish(&self, status: RunStatus, error: Option<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.status = status;
            inner.error = error;
            inner.finished = Some(Instant::now());
        }
        self.changed();
    }

    // ----- user controls ----------------------------------------------------

    pub fn pause(&self) {
        if self.paused.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut inner) = self.inner.lock()
            && inner.status == RunStatus::Running
        {
            inner.status = RunStatus::Paused;
        }
        self.changed();
    }

    pub fn resume(&self) {
        if !self.paused.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut inner) = self.inner.lock()
            && inner.status == RunStatus::Paused
        {
            inner.status = RunStatus::Running;
        }
        self.resumed.notify_waiters();
        self.changed();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Wait here while the run is paused. Returns false when the run was
    /// stopped while waiting.
    pub(crate) async fn wait_while_paused(&self) -> bool {
        while self.paused.load(Ordering::SeqCst) {
            if self.stop.is_cancelled() {
                return false;
            }
            let resumed = self.resumed.notified();
            // Re-check after arming the wait, so a resume that lands in between
            // is not missed.
            if !self.paused.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                _ = resumed => {}
                _ = self.stop.cancelled() => return false,
            }
        }
        !self.stop.is_cancelled()
    }

    pub fn stop(&self) {
        self.stop.cancel();
        // A paused run must wake so that it can observe the stop.
        self.resume();
        self.changed();
    }

    pub fn stop_agent(&self, id: AgentId) {
        if let Ok(inner) = self.inner.lock()
            && let Some(agent) = inner
                .phases
                .iter()
                .flat_map(|phase| phase.agents.iter())
                .find(|agent| agent.id == id)
        {
            agent.cancel.cancel();
        }
        self.changed();
    }

    // ----- snapshots --------------------------------------------------------

    pub fn snapshot(&self) -> Arc<RunSnapshot> {
        let version = *self.version.borrow();
        if let Ok(cached) = self.cached.lock()
            && let Some((cached_version, snapshot)) = cached.as_ref()
            && *cached_version == version
        {
            return snapshot.clone();
        }
        let snapshot = Arc::new(self.build_snapshot());
        if let Ok(mut cached) = self.cached.lock() {
            *cached = Some((version, snapshot.clone()));
        }
        snapshot
    }

    fn build_snapshot(&self) -> RunSnapshot {
        let Ok(inner) = self.inner.lock() else {
            return RunSnapshot {
                status: RunStatus::Failed,
                elapsed: Duration::ZERO,
                log: Vec::new(),
                phases: Vec::new(),
                tokens: 0,
                error: Some("the run state was poisoned".into()),
            };
        };
        let mut total_tokens = 0;
        let phases = inner
            .phases
            .iter()
            .map(|phase| {
                let mut phase_tokens = 0;
                let agents = phase
                    .agents
                    .iter()
                    .map(|agent| {
                        phase_tokens += agent.tokens;
                        AgentSnapshot {
                            id: agent.id,
                            label: agent.label.clone(),
                            status: agent.status,
                            tokens: agent.tokens,
                            elapsed: match (agent.status.is_finished(), agent.started) {
                                (false, Some(started)) => started.elapsed(),
                                _ => agent.elapsed,
                            },
                            prompt: agent.prompt.clone(),
                            result: agent.result.clone(),
                            error: agent.error.clone(),
                        }
                    })
                    .collect();
                total_tokens += phase_tokens;
                PhaseSnapshot {
                    title: phase.title.clone(),
                    agents,
                    tokens: phase_tokens,
                }
            })
            .collect();
        RunSnapshot {
            status: inner.status,
            elapsed: inner
                .finished
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(inner.started),
            log: inner.log.clone(),
            phases,
            tokens: total_tokens,
            error: inner.error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_phases_appear_before_any_agent_runs() {
        let state = RunState::new(&["Discover".into(), "Audit".into()]);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.phases.len(), 2);
        assert_eq!(snapshot.phases[0].title, "Discover");
        assert_eq!(snapshot.total_agents(), 0);
    }

    #[test]
    fn a_script_without_phases_gets_one_default_phase() {
        let state = RunState::new(&[]);
        assert_eq!(state.snapshot().phases[0].title, DEFAULT_PHASE);
    }

    #[test]
    fn an_undeclared_phase_is_added_when_it_is_first_used() {
        let state = RunState::new(&["Discover".into()]);
        state.set_phase("Verify");
        state.register_agent(0, "check".into(), "prompt".into());
        let snapshot = state.snapshot();
        assert_eq!(snapshot.phases.len(), 2);
        assert_eq!(snapshot.phases[1].title, "Verify");
        assert_eq!(snapshot.phases[1].agents.len(), 1);
    }

    #[test]
    fn snapshots_are_shared_until_the_state_changes() {
        let state = RunState::new(&[]);
        let first = state.snapshot();
        let second = state.snapshot();
        assert!(Arc::ptr_eq(&first, &second));

        state.register_agent(0, "one".into(), "prompt".into());
        let third = state.snapshot();
        assert!(!Arc::ptr_eq(&first, &third));
    }

    #[test]
    fn the_version_moves_on_every_change_so_viewers_never_poll() {
        let state = RunState::new(&[]);
        let mut versions = state.subscribe();
        assert_eq!(*versions.borrow_and_update(), 0);
        state.log("started".into());
        assert!(versions.has_changed().unwrap_or(false));
    }

    #[test]
    fn finished_counts_ignore_agents_still_working() {
        let state = RunState::new(&[]);
        state.register_agent(0, "one".into(), "p".into());
        state.register_agent(1, "two".into(), "p".into());
        state.agent_started(0);
        state.agent_finished(0, AgentStatus::Completed, Some("ok".into()), None, 12);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.total_agents(), 2);
        assert_eq!(snapshot.finished_agents(), 1);
        assert_eq!(snapshot.tokens, 12);
    }

    #[tokio::test]
    async fn a_paused_run_continues_after_resume() {
        let state = RunState::new(&[]);
        state.pause();
        assert!(state.is_paused());
        assert_eq!(state.snapshot().status, RunStatus::Paused);

        let waiter = state.clone();
        let handle = tokio::spawn(async move { waiter.wait_while_paused().await });
        tokio::task::yield_now().await;
        state.resume();
        assert!(handle.await.unwrap());
        assert_eq!(state.snapshot().status, RunStatus::Running);
    }

    #[tokio::test]
    async fn stopping_a_paused_run_releases_it() {
        let state = RunState::new(&[]);
        state.pause();
        let waiter = state.clone();
        let handle = tokio::spawn(async move { waiter.wait_while_paused().await });
        tokio::task::yield_now().await;
        state.stop();
        // False means "do not carry on", which is what a stopped run needs.
        assert!(!handle.await.unwrap());
    }
}
