//! Opt-in, in-process subagent sessions and Codex-style control tools.

use crate::session_runner::AgentSession;
use anyhow::{Context as _, Result};
use kiss_agent::{AgentMessage, AgentTool, DynTool, ToolResult, ToolUpdateSink};
use kiss_ai::{StopReason, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

const MAX_ACTIVE_TURNS: usize = 4;
const MAX_AGENT_RECORDS: usize = 16;
const DEFAULT_WAIT_MS: u64 = 30_000;
const MIN_WAIT_MS: u64 = 250;
const MAX_WAIT_MS: u64 = 600_000;

pub const SUBAGENT_SYSTEM_PROMPT: &str = "Subagent coordination:\n- Subagents share this working directory. Give each child one bounded task.\n- Fresh child context is the default. Copy parent turns only when the task needs them.\n- Use wait_agent when a child result is required. Do not use repeated list calls as polling.\n- Check child findings and edits before you give the final answer.\n- Start subagents only when the user or the current task justifies delegation.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl AgentStatus {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: String,
    pub task_name: String,
    pub canonical_path: String,
    pub status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkTurns {
    None,
    All,
    Recent(usize),
}

impl ForkTurns {
    fn parse(value: Option<&Value>) -> Result<Self> {
        match value {
            None => Ok(Self::None),
            Some(Value::String(value)) if value == "none" => Ok(Self::None),
            Some(Value::String(value)) if value == "all" => Ok(Self::All),
            Some(Value::Number(value)) => {
                let count = value
                    .as_u64()
                    .filter(|count| *count > 0)
                    .and_then(|count| usize::try_from(count).ok())
                    .context("fork_turns must be 'none', 'all', or a positive integer")?;
                Ok(Self::Recent(count))
            }
            _ => anyhow::bail!("fork_turns must be 'none', 'all', or a positive integer"),
        }
    }
}

struct AgentState {
    status: AgentStatus,
    result: Option<String>,
    error: Option<String>,
}

struct AgentRecord {
    id: String,
    task_name: String,
    canonical_path: String,
    session: Arc<AgentSession>,
    state: Mutex<AgentState>,
}

impl AgentRecord {
    fn snapshot(&self) -> AgentInfo {
        let state = self.state.lock().unwrap();
        AgentInfo {
            id: self.id.clone(),
            task_name: self.task_name.clone(),
            canonical_path: self.canonical_path.clone(),
            status: state.status,
            result: state.result.clone(),
            error: state.error.clone(),
        }
    }
}

pub struct SubagentRuntime {
    parent: Weak<AgentSession>,
    records: Mutex<Vec<Arc<AgentRecord>>>,
    targets: Mutex<HashMap<String, Arc<AgentRecord>>>,
    permits: Arc<Semaphore>,
    activity: watch::Sender<u64>,
    spawn_lock: Mutex<()>,
}

impl SubagentRuntime {
    pub(crate) fn new(parent: Weak<AgentSession>) -> Arc<Self> {
        Self::with_permits(parent, MAX_ACTIVE_TURNS)
    }

    fn with_permits(parent: Weak<AgentSession>, permits: usize) -> Arc<Self> {
        let (activity, _) = watch::channel(0);
        Arc::new(Self {
            parent,
            records: Mutex::new(Vec::new()),
            targets: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(permits)),
            activity,
            spawn_lock: Mutex::new(()),
        })
    }

    pub(crate) fn control_tools(self: &Arc<Self>) -> Vec<DynTool> {
        vec![
            Arc::new(SpawnAgentTool(self.clone())),
            Arc::new(SendMessageTool(self.clone())),
            Arc::new(FollowupTaskTool(self.clone())),
            Arc::new(WaitAgentTool(self.clone())),
            Arc::new(ListAgentsTool(self.clone())),
            Arc::new(InterruptAgentTool(self.clone())),
        ]
    }

    fn signal_activity(&self) {
        self.activity
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    fn resolve(&self, target: &str) -> Result<Arc<AgentRecord>> {
        self.targets
            .lock()
            .unwrap()
            .get(target)
            .cloned()
            .with_context(|| format!("unknown subagent target '{target}'"))
    }

    fn list(&self) -> Vec<AgentInfo> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.snapshot())
            .collect()
    }

    fn spawn(
        self: &Arc<Self>,
        task_name: String,
        prompt: String,
        fork_turns: ForkTurns,
        model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> Result<AgentInfo> {
        let _spawn_guard = self.spawn_lock.lock().unwrap();
        validate_task_name(&task_name)?;
        if prompt.trim().is_empty() {
            anyhow::bail!("prompt must not be empty");
        }
        {
            let records = self.records.lock().unwrap();
            if records.len() >= MAX_AGENT_RECORDS {
                anyhow::bail!("the subagent limit of {MAX_AGENT_RECORDS} has been reached");
            }
            if records.iter().any(|record| record.task_name == task_name) {
                anyhow::bail!(
                    "task name '{task_name}' already exists; use followup_task for that child"
                );
            }
        }

        let canonical_path = format!("/root/{task_name}");
        let parent = self
            .parent
            .upgrade()
            .context("the parent session is no longer available")?;
        let child = parent.create_subagent_session(
            &task_name,
            &canonical_path,
            fork_turns,
            model.as_deref(),
            reasoning_effort.as_deref(),
        )?;
        let id = child.manager.lock().unwrap().session_id().to_string();
        let record = Arc::new(AgentRecord {
            id: id.clone(),
            task_name: task_name.clone(),
            canonical_path: canonical_path.clone(),
            session: child,
            state: Mutex::new(AgentState {
                status: AgentStatus::Queued,
                result: None,
                error: None,
            }),
        });
        self.records.lock().unwrap().push(record.clone());
        let mut targets = self.targets.lock().unwrap();
        for target in [&id, &task_name, &canonical_path] {
            targets.insert(target.clone(), record.clone());
        }
        drop(targets);

        let info = record.snapshot();
        self.signal_activity();
        self.start_turn(record, prompt);
        Ok(info)
    }

    fn start_turn(self: &Arc<Self>, record: Arc<AgentRecord>, prompt: String) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let permit = match runtime.permits.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            {
                let mut state = record.state.lock().unwrap();
                if state.status == AgentStatus::Interrupted {
                    return;
                }
                state.status = AgentStatus::Running;
            }
            runtime.signal_activity();

            let usage_before = record.session.totals();
            record
                .session
                .prompt(vec![AgentMessage::user(prompt)])
                .await;
            let usage_after = record.session.totals();
            if let Some(parent) = runtime.parent.upgrade() {
                parent.record_subagent_usage(usage_delta(usage_after, usage_before));
            }
            drop(permit);

            let interrupted = record.state.lock().unwrap().status == AgentStatus::Interrupted;
            if !interrupted {
                let (status, result, error) = turn_outcome(&record.session);
                let mut state = record.state.lock().unwrap();
                state.status = status;
                state.result = result;
                state.error = error;
            }
            runtime.signal_activity();
        });
    }

    fn send_message(&self, target: &str, message: String) -> Result<AgentInfo> {
        if message.trim().is_empty() {
            anyhow::bail!("message must not be empty");
        }
        let record = self.resolve(target)?;
        let status = record.state.lock().unwrap().status;
        if !matches!(status, AgentStatus::Queued | AgentStatus::Running) {
            anyhow::bail!(
                "subagent '{}' is {}; use followup_task to start another turn",
                record.task_name,
                status_name(status)
            );
        }
        record.session.queue_steering(AgentMessage::user(message));
        self.signal_activity();
        Ok(record.snapshot())
    }

    fn followup(self: &Arc<Self>, target: &str, prompt: String) -> Result<AgentInfo> {
        if prompt.trim().is_empty() {
            anyhow::bail!("prompt must not be empty");
        }
        let record = self.resolve(target)?;
        {
            let mut state = record.state.lock().unwrap();
            if !state.status.is_terminal() {
                anyhow::bail!("subagent '{}' is still busy", record.task_name);
            }
            state.status = AgentStatus::Queued;
            state.result = None;
            state.error = None;
        }
        let _ = record.session.reclaim_queued();
        let info = record.snapshot();
        self.signal_activity();
        self.start_turn(record, prompt);
        Ok(info)
    }

    async fn wait(
        &self,
        targets: &[String],
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<WaitOutcome> {
        if targets.is_empty() {
            anyhow::bail!("targets must contain at least one subagent");
        }
        let records = targets
            .iter()
            .map(|target| self.resolve(target))
            .collect::<Result<Vec<_>>>()?;
        let timeout_ms = timeout_ms.clamp(MIN_WAIT_MS, MAX_WAIT_MS);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut activity = self.activity.subscribe();

        loop {
            let agents = records
                .iter()
                .map(|record| record.snapshot())
                .collect::<Vec<_>>();
            if agents.iter().any(|agent| agent.status.is_terminal()) {
                return Ok(WaitOutcome {
                    timed_out: false,
                    agents,
                });
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(WaitOutcome {
                    timed_out: true,
                    agents,
                });
            }
            tokio::select! {
                _ = cancel.cancelled() => anyhow::bail!("wait cancelled"),
                result = tokio::time::timeout(remaining, activity.changed()) => {
                    if result.is_err() {
                        return Ok(WaitOutcome { timed_out: true, agents });
                    }
                }
            }
        }
    }

    fn interrupt(&self, target: &str) -> Result<AgentInfo> {
        let record = self.resolve(target)?;
        interrupt_record(&record);
        self.signal_activity();
        Ok(record.snapshot())
    }

    pub(crate) fn interrupt_all(&self) {
        let records = self.records.lock().unwrap().clone();
        for record in records {
            interrupt_record(&record);
        }
        self.signal_activity();
    }

    pub(crate) fn reset(&self) {
        self.interrupt_all();
        self.records.lock().unwrap().clear();
        self.targets.lock().unwrap().clear();
        self.signal_activity();
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

fn interrupt_record(record: &AgentRecord) {
    let mut state = record.state.lock().unwrap();
    if matches!(state.status, AgentStatus::Queued | AgentStatus::Running) {
        state.status = AgentStatus::Interrupted;
        state.result = None;
        state.error = Some("interrupted".into());
        record.session.abort();
    }
}

pub(crate) fn turn_outcome(
    session: &AgentSession,
) -> (AgentStatus, Option<String>, Option<String>) {
    let manager = session.manager.lock().unwrap();
    let assistant = manager
        .build_session_context()
        .messages
        .into_iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Assistant(assistant) => Some(assistant),
            _ => None,
        });
    let Some(assistant) = assistant else {
        return (
            AgentStatus::Failed,
            None,
            Some("the child returned no assistant message".into()),
        );
    };
    match assistant.stop_reason {
        StopReason::Error => (
            AgentStatus::Failed,
            None,
            Some(
                assistant
                    .error_message
                    .unwrap_or_else(|| "the child request failed".into()),
            ),
        ),
        StopReason::Aborted => (AgentStatus::Interrupted, None, Some("interrupted".into())),
        _ => {
            let text = assistant.text();
            if text.trim().is_empty() {
                (
                    AgentStatus::Failed,
                    None,
                    Some("the child returned an empty result".into()),
                )
            } else {
                (AgentStatus::Completed, Some(text), None)
            }
        }
    }
}

fn validate_task_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        anyhow::bail!(
            "task_name must use 1 to 64 lower-case ASCII letters, digits, or underscores"
        );
    }
    Ok(())
}

fn status_name(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "queued",
        AgentStatus::Running => "running",
        AgentStatus::Completed => "completed",
        AgentStatus::Failed => "failed",
        AgentStatus::Interrupted => "interrupted",
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WaitOutcome {
    timed_out: bool,
    agents: Vec<AgentInfo>,
}

fn json_result(value: &impl Serialize) -> Result<ToolResult> {
    Ok(ToolResult::text(serde_json::to_string_pretty(value)?))
}

#[derive(Deserialize)]
struct SpawnArgs {
    task_name: String,
    prompt: String,
    #[serde(default)]
    fork_turns: Option<Value>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

struct SpawnAgentTool(Arc<SubagentRuntime>);

#[async_trait::async_trait]
impl AgentTool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> String {
        "Start a named child coding agent in the background. Fresh context is the default.".into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_name": {"type": "string", "pattern": "^[a-z0-9_]{1,64}$"},
                "prompt": {"type": "string", "minLength": 1},
                "fork_turns": {"oneOf": [
                    {"type": "string", "enum": ["none", "all"]},
                    {"type": "integer", "minimum": 1}
                ]},
                "model": {"type": "string"},
                "reasoning_effort": {"type": "string"}
            },
            "required": ["task_name", "prompt"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> Result<ToolResult> {
        let args: SpawnArgs = serde_json::from_value(args)?;
        let fork_turns = ForkTurns::parse(args.fork_turns.as_ref())?;
        json_result(&self.0.spawn(
            args.task_name,
            args.prompt,
            fork_turns,
            args.model,
            args.reasoning_effort,
        )?)
    }
}

#[derive(Deserialize)]
struct MessageArgs {
    target: String,
    message: String,
}

struct SendMessageTool(Arc<SubagentRuntime>);

#[async_trait::async_trait]
impl AgentTool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> String {
        "Send steering guidance to a queued or running child agent.".into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {"type": "string"},
                "message": {"type": "string", "minLength": 1}
            },
            "required": ["target", "message"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> Result<ToolResult> {
        let args: MessageArgs = serde_json::from_value(args)?;
        json_result(&self.0.send_message(&args.target, args.message)?)
    }
}

#[derive(Deserialize)]
struct FollowupArgs {
    target: String,
    prompt: String,
}

struct FollowupTaskTool(Arc<SubagentRuntime>);

#[async_trait::async_trait]
impl AgentTool for FollowupTaskTool {
    fn name(&self) -> &str {
        "followup_task"
    }

    fn description(&self) -> String {
        "Start a new turn in an idle child agent and keep its existing context.".into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {"type": "string"},
                "prompt": {"type": "string", "minLength": 1}
            },
            "required": ["target", "prompt"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> Result<ToolResult> {
        let args: FollowupArgs = serde_json::from_value(args)?;
        json_result(&self.0.followup(&args.target, args.prompt)?)
    }
}

#[derive(Deserialize)]
struct WaitArgs {
    targets: Vec<String>,
    #[serde(default = "default_wait_ms")]
    timeout_ms: u64,
}

fn default_wait_ms() -> u64 {
    DEFAULT_WAIT_MS
}

struct WaitAgentTool(Arc<SubagentRuntime>);

#[async_trait::async_trait]
impl AgentTool for WaitAgentTool {
    fn name(&self) -> &str {
        "wait_agent"
    }

    fn description(&self) -> String {
        "Wait for one requested child agent to finish or for a bounded timeout.".into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "targets": {"type": "array", "items": {"type": "string"}, "minItems": 1, "uniqueItems": true},
                "timeout_ms": {"type": "integer", "minimum": MIN_WAIT_MS, "maximum": MAX_WAIT_MS}
            },
            "required": ["targets"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> Result<ToolResult> {
        let args: WaitArgs = serde_json::from_value(args)?;
        json_result(&self.0.wait(&args.targets, args.timeout_ms, cancel).await?)
    }
}

struct ListAgentsTool(Arc<SubagentRuntime>);

#[async_trait::async_trait]
impl AgentTool for ListAgentsTool {
    fn name(&self) -> &str {
        "list_agents"
    }

    fn description(&self) -> String {
        "List child agents in creation order with their current status.".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> Result<ToolResult> {
        json_result(&self.0.list())
    }
}

#[derive(Deserialize)]
struct TargetArgs {
    target: String,
}

struct InterruptAgentTool(Arc<SubagentRuntime>);

#[async_trait::async_trait]
impl AgentTool for InterruptAgentTool {
    fn name(&self) -> &str {
        "interrupt_agent"
    }

    fn description(&self) -> String {
        "Interrupt a queued or running child agent.".into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"target": {"type": "string"}},
            "required": ["target"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> Result<ToolResult> {
        let args: TargetArgs = serde_json::from_value(args)?;
        json_result(&self.0.interrupt(&args.target)?)
    }
}

pub(crate) fn fork_messages(messages: &[AgentMessage], fork: ForkTurns) -> Vec<AgentMessage> {
    let safe = messages
        .iter()
        .filter(|message| match message {
            AgentMessage::User(_)
            | AgentMessage::BranchSummary(_)
            | AgentMessage::CompactionSummary(_) => true,
            AgentMessage::Assistant(assistant) => assistant.tool_calls().next().is_none(),
            AgentMessage::ToolResult(_)
            | AgentMessage::BashExecution(_)
            | AgentMessage::Custom(_) => false,
        })
        .cloned()
        .collect::<Vec<_>>();

    match fork {
        ForkTurns::None => Vec::new(),
        ForkTurns::All => safe,
        ForkTurns::Recent(count) => {
            let start = safe
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, message)| matches!(message, AgentMessage::User(_)))
                .nth(count.saturating_sub(1))
                .map(|(index, _)| index)
                .unwrap_or(0);
            safe[start..].to_vec()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::{AssistantMessage, ContentBlock, ToolCall};

    fn assistant(text: &str) -> AgentMessage {
        let mut message = AssistantMessage::empty("fake", "fake", "fake");
        message.content.push(ContentBlock::text(text));
        AgentMessage::Assistant(message)
    }

    fn parent_session() -> Arc<AgentSession> {
        let registry = kiss_ai::Registry::from_builtin();
        let model = registry.all().first().expect("built-in model").clone();
        AgentSession::new_with_subagents_allowed(
            crate::SessionManager::in_memory(std::path::Path::new("/test")),
            Vec::new(),
            registry,
            crate::Settings::default(),
            "test".into(),
            model,
            kiss_ai::ThinkingLevel::Off,
            None,
            Arc::new(|_| {}),
            false,
        )
    }

    #[test]
    fn task_names_are_bounded_and_portable() {
        assert!(validate_task_name("review_2").is_ok());
        assert!(validate_task_name("").is_err());
        assert!(validate_task_name("Review").is_err());
        assert!(validate_task_name("has-dash").is_err());
        assert!(validate_task_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn fork_turns_accepts_only_documented_values() {
        assert_eq!(ForkTurns::parse(None).unwrap(), ForkTurns::None);
        assert_eq!(
            ForkTurns::parse(Some(&json!("all"))).unwrap(),
            ForkTurns::All
        );
        assert_eq!(
            ForkTurns::parse(Some(&json!(2))).unwrap(),
            ForkTurns::Recent(2)
        );
        assert!(ForkTurns::parse(Some(&json!(0))).is_err());
        assert!(ForkTurns::parse(Some(&json!("recent"))).is_err());
    }

    #[test]
    fn context_fork_removes_tool_pairs_and_keeps_recent_turns() {
        let mut tool_message = AssistantMessage::empty("fake", "fake", "fake");
        tool_message.content.push(ContentBlock::ToolCall(ToolCall {
            id: "call".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
        }));
        let history = vec![
            AgentMessage::user("first"),
            assistant("one"),
            AgentMessage::Assistant(tool_message),
            AgentMessage::user("second"),
            assistant("two"),
        ];

        assert!(fork_messages(&history, ForkTurns::None).is_empty());
        let all = fork_messages(&history, ForkTurns::All);
        assert_eq!(all.len(), 4);
        assert!(
            all.iter()
                .all(|message| !matches!(message, AgentMessage::ToolResult(_)))
        );
        let recent = fork_messages(&history, ForkTurns::Recent(1));
        assert_eq!(recent, history[3..]);
    }

    #[test]
    fn control_tool_catalog_uses_codex_names() {
        let parent = Weak::new();
        let runtime = SubagentRuntime::new(parent);
        let names = runtime
            .control_tools()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "spawn_agent",
                "send_message",
                "followup_task",
                "wait_agent",
                "list_agents",
                "interrupt_agent"
            ]
        );
        for tool in runtime.control_tools() {
            let schema = tool.parameters();
            assert_eq!(schema["type"], "object", "{} schema", tool.name());
            assert_eq!(
                schema["additionalProperties"],
                false,
                "{} schema",
                tool.name()
            );
        }
        let spawn = runtime.control_tools().remove(0).parameters();
        assert_eq!(spawn["required"], json!(["task_name", "prompt"]));
        assert_eq!(
            spawn["properties"]["fork_turns"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn queued_agent_can_be_listed_interrupted_and_waited_for() {
        let parent = parent_session();
        let runtime = SubagentRuntime::with_permits(Arc::downgrade(&parent), 0);
        let spawned = runtime
            .spawn(
                "review_tests".into(),
                "Review the tests".into(),
                ForkTurns::None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(spawned.status, AgentStatus::Queued);
        assert_eq!(runtime.list(), vec![spawned.clone()]);
        assert!(
            runtime
                .spawn(
                    "review_tests".into(),
                    "Duplicate".into(),
                    ForkTurns::None,
                    None,
                    None,
                )
                .is_err()
        );

        let waiting_runtime = runtime.clone();
        let target = spawned.id.clone();
        let waiting = tokio::spawn(async move {
            waiting_runtime
                .wait(&[target], 5_000, CancellationToken::new())
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        let interrupted = runtime.interrupt("review_tests").unwrap();
        assert_eq!(interrupted.status, AgentStatus::Interrupted);
        runtime.permits.add_permits(1);

        let outcome = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("mailbox wait")
            .unwrap();
        assert!(!outcome.timed_out);
        assert_eq!(outcome.agents[0].status, AgentStatus::Interrupted);
    }
}
