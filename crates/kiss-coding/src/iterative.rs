//! Bounded loop and autoresearch jobs in persistent child sessions.

use crate::child_turn;
use crate::session_runner::AgentSession;
use crate::subagents::{AgentStatus, ForkTurns};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore, watch};
use tokio_util::sync::CancellationToken;

pub type JobId = u64;

const MAX_ACTIVE_JOBS: usize = 4;
const MAX_RETAINED_JOBS: usize = 20;
pub const DEFAULT_LOOP_ITERATIONS: u32 = 10;
pub const DEFAULT_AUTORESEARCH_ITERATIONS: u32 = 25;
pub const MAX_ITERATIONS: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Loop,
    Autoresearch,
}

impl JobKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::Autoresearch => "autoresearch",
        }
    }

    pub fn default_limit(self) -> u32 {
        match self {
            Self::Loop => DEFAULT_LOOP_ITERATIONS,
            Self::Autoresearch => DEFAULT_AUTORESEARCH_ITERATIONS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Stopped,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }
}

#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: JobId,
    pub kind: JobKind,
    pub goal: String,
    pub status: JobStatus,
    pub iteration: u32,
    pub limit: u32,
    pub tokens: u64,
    pub elapsed: Duration,
    pub session_id: String,
    pub latest_result: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JobSummary {
    pub id: JobId,
    pub kind: JobKind,
    pub goal: String,
    pub status: JobStatus,
    pub iteration: u32,
    pub limit: u32,
    pub tokens: u64,
    pub elapsed: Duration,
}

struct JobState {
    status: JobStatus,
    iteration: u32,
    tokens: u64,
    latest_result: Option<String>,
    error: Option<String>,
}

pub struct JobRecord {
    id: JobId,
    kind: JobKind,
    goal: String,
    limit: u32,
    session_id: String,
    child: Arc<AgentSession>,
    started: Instant,
    state: Mutex<JobState>,
    cancel: CancellationToken,
    wake: Notify,
    version: watch::Sender<u64>,
    parent: Weak<AgentSession>,
}

impl JobRecord {
    pub fn snapshot(&self) -> JobSnapshot {
        let state = self.state.lock().unwrap();
        JobSnapshot {
            id: self.id,
            kind: self.kind,
            goal: self.goal.clone(),
            status: state.status,
            iteration: state.iteration,
            limit: self.limit,
            tokens: state.tokens,
            elapsed: self.started.elapsed(),
            session_id: self.session_id.clone(),
            latest_result: state.latest_result.clone(),
            error: state.error.clone(),
        }
    }

    fn summary(&self) -> JobSummary {
        let state = self.state.lock().unwrap();
        JobSummary {
            id: self.id,
            kind: self.kind,
            goal: self.goal.clone(),
            status: state.status,
            iteration: state.iteration,
            limit: self.limit,
            tokens: state.tokens,
            elapsed: self.started.elapsed(),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    pub fn pause(&self) {
        let mut state = self.state.lock().unwrap();
        if matches!(state.status, JobStatus::Running | JobStatus::Queued) {
            state.status = JobStatus::Paused;
            drop(state);
            self.signal();
        }
    }

    pub fn resume(&self) {
        let mut state = self.state.lock().unwrap();
        if state.status == JobStatus::Paused {
            state.status = JobStatus::Running;
            drop(state);
            self.wake.notify_waiters();
            self.signal();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.state.lock().unwrap().status == JobStatus::Paused
    }

    pub fn stop(&self) {
        if self.state.lock().unwrap().status.is_finished() {
            return;
        }
        self.cancel.cancel();
        self.child.abort();
        self.wake.notify_waiters();
    }

    fn signal(&self) {
        self.version
            .send_modify(|version| *version = version.wrapping_add(1));
        if let Some(parent) = self.parent.upgrade() {
            parent.emit_iterative(self.id, *self.version.borrow());
        }
    }

    fn set_status(&self, status: JobStatus) {
        self.state.lock().unwrap().status = status;
        self.signal();
    }

    async fn wait_until_ready(&self) -> bool {
        loop {
            if self.cancel.is_cancelled() {
                return false;
            }
            if !self.is_paused() {
                return true;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return false,
                _ = self.wake.notified() => {}
            }
        }
    }
}

pub struct IterativeRuntime {
    parent: Weak<AgentSession>,
    jobs: Mutex<Vec<Arc<JobRecord>>>,
    next_id: AtomicU64,
    permits: Arc<Semaphore>,
}

impl IterativeRuntime {
    pub(crate) fn new(parent: Weak<AgentSession>) -> Arc<Self> {
        Arc::new(Self {
            parent,
            jobs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            permits: Arc::new(Semaphore::new(MAX_ACTIVE_JOBS)),
        })
    }

    pub fn start(
        self: &Arc<Self>,
        kind: JobKind,
        goal: impl Into<String>,
        limit: Option<u32>,
    ) -> anyhow::Result<Arc<JobRecord>> {
        let goal = goal.into();
        if goal.trim().is_empty() {
            anyhow::bail!("the job goal cannot be empty");
        }
        let parent = self
            .parent
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the parent session has closed"))?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let limit = limit
            .unwrap_or_else(|| kind.default_limit())
            .clamp(1, MAX_ITERATIONS);
        let task_name = format!("{}_{}", kind.label(), id);
        let child = parent.create_subagent_session(
            &task_name,
            &format!("/root/{task_name}"),
            ForkTurns::All,
            None,
            None,
        )?;
        let session_id = child.manager.lock().unwrap().session_id().to_string();
        let (version, _) = watch::channel(0);
        let record = Arc::new(JobRecord {
            id,
            kind,
            goal,
            limit,
            session_id,
            child,
            started: Instant::now(),
            state: Mutex::new(JobState {
                status: JobStatus::Queued,
                iteration: 0,
                tokens: 0,
                latest_result: None,
                error: None,
            }),
            cancel: CancellationToken::new(),
            wake: Notify::new(),
            version,
            parent: self.parent.clone(),
        });

        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| anyhow::anyhow!("the job list is unavailable"))?;
        jobs.push(record.clone());
        while jobs.len() > MAX_RETAINED_JOBS {
            let Some(position) = jobs
                .iter()
                .position(|job| job.state.lock().unwrap().status.is_finished())
            else {
                break;
            };
            jobs.remove(position);
        }
        drop(jobs);

        let runtime = self.clone();
        let running = record.clone();
        tokio::spawn(async move { runtime.run(running).await });
        record.signal();
        Ok(record)
    }

    async fn run(self: Arc<Self>, job: Arc<JobRecord>) {
        let permit = tokio::select! {
            _ = job.cancel.cancelled() => None,
            permit = self.permits.clone().acquire_owned() => permit.ok(),
        };
        let Some(_permit) = permit else {
            job.set_status(JobStatus::Stopped);
            return;
        };
        if !job.wait_until_ready().await {
            job.set_status(JobStatus::Stopped);
            return;
        }
        job.set_status(JobStatus::Running);

        for iteration in 1..=job.limit {
            if !job.wait_until_ready().await {
                job.set_status(JobStatus::Stopped);
                return;
            }
            job.state.lock().unwrap().iteration = iteration;
            job.signal();

            let outcome = child_turn::run_child_turn(
                &job.parent,
                &job.child,
                iteration_prompt(job.kind, &job.goal, iteration, job.limit),
                Some(job.cancel.clone()),
                None,
            )
            .await;
            {
                let mut state = job.state.lock().unwrap();
                state.tokens = state.tokens.saturating_add(outcome.usage.total_tokens);
                state.latest_result = outcome.result.clone();
                state.error = outcome.error.clone();
            }
            job.signal();

            if job.cancel.is_cancelled() || outcome.status == AgentStatus::Interrupted {
                job.set_status(JobStatus::Stopped);
                return;
            }
            if outcome.status == AgentStatus::Failed {
                job.set_status(JobStatus::Failed);
                return;
            }
            if outcome
                .result
                .as_deref()
                .is_some_and(contains_completion_marker)
            {
                job.set_status(JobStatus::Completed);
                return;
            }
        }
        job.set_status(JobStatus::Completed);
    }

    pub fn get(&self, id: JobId) -> Option<Arc<JobRecord>> {
        self.jobs
            .lock()
            .ok()?
            .iter()
            .find(|job| job.id == id)
            .cloned()
    }

    pub fn summaries(&self) -> Vec<JobSummary> {
        self.jobs
            .lock()
            .map(|jobs| jobs.iter().map(|job| job.summary()).collect())
            .unwrap_or_default()
    }

    pub fn active_count(&self) -> usize {
        self.jobs
            .lock()
            .map(|jobs| {
                jobs.iter()
                    .filter(|job| !job.state.lock().unwrap().status.is_finished())
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn latest(&self) -> Option<Arc<JobRecord>> {
        self.jobs.lock().ok()?.last().cloned()
    }

    pub(crate) fn stop_all(&self) {
        if let Ok(jobs) = self.jobs.lock() {
            for job in jobs.iter() {
                job.stop();
            }
        }
    }
}

fn contains_completion_marker(result: &str) -> bool {
    result.to_ascii_lowercase().contains("[goal-complete]")
}

fn iteration_prompt(kind: JobKind, goal: &str, iteration: u32, limit: u32) -> String {
    let common = format!(
        "You are in iteration {iteration} of {limit} for this goal:\n\n{goal}\n\nWork directly in the shared repository. Make one useful, verified unit of progress. Do not ask for permission. End the answer with exactly [goal-complete] if the goal is fully achieved and verified. Otherwise end with exactly [continue]."
    );
    match kind {
        JobKind::Loop => common,
        JobKind::Autoresearch => format!(
            "Autoresearch job. On the first iteration, establish a repeatable baseline and success metric. On each iteration, test one small idea, run the same verification, keep an improvement, and revert a regression. Protect existing behavior with focused tests. Record the measured result and the decision in the answer.\n\n{common}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::{AssistantEvent, AssistantMessage, ContentBlock, EventStream, StopReason};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn job_defaults_are_bounded() {
        assert_eq!(JobKind::Loop.default_limit(), 10);
        assert_eq!(JobKind::Autoresearch.default_limit(), 25);
        assert!(JobKind::Autoresearch.default_limit() <= MAX_ITERATIONS);
    }

    #[test]
    fn prompts_define_progress_and_completion() {
        let loop_prompt = iteration_prompt(JobKind::Loop, "fix tests", 2, 10);
        assert!(loop_prompt.contains("iteration 2 of 10"));
        assert!(loop_prompt.contains("[goal-complete]"));
        assert!(loop_prompt.contains("[continue]"));

        let research = iteration_prompt(JobKind::Autoresearch, "make it faster", 1, 25);
        assert!(research.contains("baseline"));
        assert!(research.contains("success metric"));
        assert!(research.contains("revert a regression"));
    }

    #[test]
    fn completion_marker_is_case_insensitive() {
        assert!(contains_completion_marker("done\n[GOAL-COMPLETE]"));
        assert!(!contains_completion_marker("keep going\n[continue]"));
    }

    #[tokio::test]
    async fn one_job_reuses_its_forked_session_and_stops_early() {
        let registry = kiss_ai::Registry::from_builtin();
        let model = registry.all().first().unwrap().clone();
        let parent = AgentSession::new(
            crate::SessionManager::in_memory(std::path::Path::new("/test")),
            Vec::new(),
            registry,
            crate::Settings::default(),
            "test".into(),
            model,
            kiss_ai::ThinkingLevel::Off,
            None,
            Arc::new(|_| {}),
        );
        parent
            .manager
            .lock()
            .unwrap()
            .append_message(kiss_agent::AgentMessage::user("parent context"))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        parent.set_stream_fn(Some(Arc::new({
            let calls = calls.clone();
            move |_, _, _| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let text = if call == 0 {
                    "first change\n[continue]"
                } else {
                    "verified\n[goal-complete]"
                };
                let mut message = AssistantMessage::empty("fake", "fake", "fake");
                message.content.push(ContentBlock::text(text));
                message.stop_reason = StopReason::Stop;
                let (sink, stream) = EventStream::channel();
                sink.send(AssistantEvent::Start {
                    partial: message.clone(),
                });
                sink.done(message);
                stream
            }
        })));

        let runtime = parent.iterative_jobs().unwrap();
        let job = runtime.start(JobKind::Loop, "finish it", Some(8)).unwrap();
        let mut updates = job.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !job.snapshot().status.is_finished() {
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let snapshot = job.snapshot();
        assert_eq!(snapshot.status, JobStatus::Completed);
        assert_eq!(snapshot.iteration, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let messages = job
            .child
            .manager
            .lock()
            .unwrap()
            .build_session_context()
            .messages;
        assert!(matches!(
            messages.first(),
            Some(kiss_agent::AgentMessage::User(user)) if user.content.as_text() == "parent context"
        ));
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, kiss_agent::AgentMessage::Assistant(_)))
                .count(),
            2
        );
        assert!(!snapshot.session_id.is_empty());
    }
}
