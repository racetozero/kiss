//! AgentSession: the high-level facade every mode drives. Owns the session
//! manager, tool set, queues, retry, auto-compaction, and cost accounting.

use crate::compaction::{
    self, estimate_context_tokens, extract_file_ops, file_ops_details, plan_compaction,
    should_compact,
};
use crate::session::manager::SessionManager;
use crate::settings::{QueueMode, Settings};
use crate::subagents::{ForkTurns, SUBAGENT_SYSTEM_PROMPT, SubagentRuntime, fork_messages};
use crate::workflows::{WorkflowApprover, WorkflowRuntime};
use anyhow::Context as _;
use kiss_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, DynTool, EventSink, TurnUpdate,
};
use kiss_ai::{Model, Registry, StopReason, ThinkingLevel, Usage};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use tokio_util::sync::CancellationToken;

/// Harness-level events layered over the loop's AgentEvents.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Agent(Box<AgentEvent>),
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart {
        auto: bool,
    },
    CompactionEnd {
        summary: String,
        tokens_before: u64,
        error: Option<String>,
    },
    Retry {
        attempt: u32,
        max: u32,
        delay_ms: u64,
        error: String,
    },
    ModelChanged {
        provider: String,
        model_id: String,
    },
    /// A dynamic workflow changed. The terminal redraws from the shared
    /// snapshot; the event carries only the cheap version marker.
    Workflow {
        run: crate::workflows::RunId,
        version: u64,
    },
}

pub type SessionEventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

pub struct TreeNavigationOutcome {
    pub editor_text: Option<String>,
    pub summarized: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
pub struct EphemeralResponse {
    pub text: String,
    pub usage: Usage,
}

pub struct AgentSession {
    pub manager: Mutex<SessionManager>,
    pub registry: Registry,
    base_tools: Mutex<Vec<DynTool>>,
    tools: Mutex<Vec<DynTool>>,
    settings: Mutex<Settings>,
    system_prompt: Mutex<String>,
    model: Mutex<Model>,
    thinking: Mutex<ThinkingLevel>,
    steering: Arc<Mutex<VecDeque<AgentMessage>>>,
    follow_up: Arc<Mutex<VecDeque<AgentMessage>>>,
    cancel: Mutex<CancellationToken>,
    running: Mutex<bool>,
    totals: Mutex<Usage>,
    context_usage_cache: Mutex<Option<(u64, u64)>>,
    api_key_override: Option<(String, String)>,
    sink: SessionEventSink,
    subagents_allowed: bool,
    subagents: OnceLock<Arc<SubagentRuntime>>,
    workflows: OnceLock<Arc<WorkflowRuntime>>,
    /// Workflow mode is armed for one turn at a time, by the `/workflow`
    /// command, by a keyword the user typed, or by running a saved workflow.
    workflow_armed: Mutex<bool>,
    workflow_approver: Mutex<Option<WorkflowApprover>>,
}

impl AgentSession {
    pub(crate) fn emit_workflow(&self, run: crate::workflows::RunId, version: u64) {
        (self.sink)(SessionEvent::Workflow { run, version });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        manager: SessionManager,
        tools: Vec<DynTool>,
        registry: Registry,
        settings: Settings,
        system_prompt: String,
        model: Model,
        thinking: ThinkingLevel,
        api_key_override: Option<(String, String)>,
        sink: SessionEventSink,
    ) -> Arc<Self> {
        Self::new_with_subagents_allowed(
            manager,
            tools,
            registry,
            settings,
            system_prompt,
            model,
            thinking,
            api_key_override,
            sink,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_subagents_allowed(
        manager: SessionManager,
        tools: Vec<DynTool>,
        registry: Registry,
        settings: Settings,
        system_prompt: String,
        model: Model,
        thinking: ThinkingLevel,
        api_key_override: Option<(String, String)>,
        sink: SessionEventSink,
        subagents_allowed: bool,
    ) -> Arc<Self> {
        let session = Arc::new(AgentSession {
            manager: Mutex::new(manager),
            registry,
            base_tools: Mutex::new(tools.clone()),
            tools: Mutex::new(tools),
            settings: Mutex::new(settings),
            system_prompt: Mutex::new(system_prompt),
            model: Mutex::new(model),
            thinking: Mutex::new(thinking),
            steering: Default::default(),
            follow_up: Default::default(),
            cancel: Mutex::new(CancellationToken::new()),
            running: Mutex::new(false),
            totals: Default::default(),
            context_usage_cache: Default::default(),
            api_key_override,
            sink,
            subagents_allowed,
            subagents: OnceLock::new(),
            workflows: OnceLock::new(),
            workflow_armed: Mutex::new(false),
            workflow_approver: Mutex::new(None),
        });
        if subagents_allowed {
            let runtime = SubagentRuntime::new(Arc::downgrade(&session));
            assert!(session.subagents.set(runtime).is_ok());
            let workflows = WorkflowRuntime::new(Arc::downgrade(&session));
            assert!(session.workflows.set(workflows).is_ok());
        }
        session.rebuild_tools();
        session
    }

    pub fn model(&self) -> Model {
        self.model.lock().unwrap().clone()
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        *self.thinking.lock().unwrap()
    }

    pub fn totals(&self) -> Usage {
        *self.totals.lock().unwrap()
    }

    pub(crate) fn record_subagent_usage(&self, usage: Usage) {
        self.totals.lock().unwrap().add(&usage);
    }

    pub fn is_running(&self) -> bool {
        *self.running.lock().unwrap()
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn update_settings(&self, settings: Settings) {
        let was_enabled = self.subagents_enabled();
        *self.settings.lock().unwrap() = settings;
        let is_enabled = self.subagents_enabled();
        if was_enabled && !is_enabled {
            self.stop_child_work();
        }
        self.rebuild_tools();
    }

    /// Replace resources used by the next model request.
    pub fn reload_runtime(&self, settings: Settings, system_prompt: String, tools: Vec<DynTool>) {
        let was_enabled = self.subagents_enabled();
        *self.settings.lock().unwrap() = settings;
        *self.system_prompt.lock().unwrap() = system_prompt;
        *self.base_tools.lock().unwrap() = tools;
        let is_enabled = self.subagents_enabled();
        if was_enabled && !is_enabled {
            self.stop_child_work();
        }
        self.rebuild_tools();
    }

    /// Stop every child agent and workflow run this session started.
    fn stop_child_work(&self) {
        if let Some(runtime) = self.subagents.get() {
            runtime.interrupt_all();
        }
        if let Some(runtime) = self.workflows.get() {
            runtime.stop_all();
        }
    }

    fn subagents_enabled(&self) -> bool {
        self.subagents_allowed && self.settings.lock().unwrap().subagents.enabled
    }

    /// Dynamic workflows are built on child agents, so they need subagents on
    /// as well as their own setting.
    pub fn workflows_enabled(&self) -> bool {
        self.subagents_enabled() && self.settings.lock().unwrap().workflows.enabled
    }

    pub fn workflows(&self) -> Option<Arc<WorkflowRuntime>> {
        self.workflows.get().cloned()
    }

    /// Offer the workflow tool and its instructions on the next request.
    ///
    /// Arming is per turn because the instructions are long: they define a
    /// whole scripting language, and an ordinary coding turn should not pay for
    /// a feature it does not use.
    pub fn arm_workflow(&self) {
        *self.workflow_armed.lock().unwrap() = true;
        self.rebuild_tools();
    }

    pub fn disarm_workflow(&self) {
        let was_armed = std::mem::replace(&mut *self.workflow_armed.lock().unwrap(), false);
        if was_armed {
            self.rebuild_tools();
        }
    }

    pub fn workflow_armed(&self) -> bool {
        *self.workflow_armed.lock().unwrap()
    }

    /// Install the callback that asks the user to approve a run.
    pub fn set_workflow_approver(&self, approver: WorkflowApprover) {
        *self.workflow_approver.lock().unwrap() = Some(approver);
    }

    pub(crate) fn workflow_approver(&self) -> Option<WorkflowApprover> {
        self.workflow_approver.lock().unwrap().clone()
    }

    fn workflow_tool_active(&self) -> bool {
        self.workflows_enabled() && self.workflow_armed()
    }

    fn rebuild_tools(&self) {
        let mut tools = self.base_tools.lock().unwrap().clone();
        if self.subagents_enabled()
            && let Some(runtime) = self.subagents.get()
        {
            tools.extend(runtime.control_tools());
        }
        if self.workflow_tool_active()
            && let Some(runtime) = self.workflows.get()
        {
            tools.push(runtime.tool());
        }
        *self.tools.lock().unwrap() = tools;
    }

    pub fn available_tool_names(&self) -> Vec<String> {
        self.tools
            .lock()
            .unwrap()
            .iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }

    /// Switch the active session without appending synthetic history.
    pub fn replace_manager(&self, manager: SessionManager) {
        if let Some(runtime) = self.subagents.get() {
            runtime.reset();
        }
        if let Some(runtime) = self.workflows.get() {
            runtime.stop_all();
        }
        let context = manager.build_session_context();
        if let Some((provider, model_id)) = context.model
            && let Some((model, _)) = self.registry.resolve(&model_id, Some(&provider))
        {
            *self.model.lock().unwrap() = model;
        }
        if let Some(thinking) = context.thinking_level {
            *self.thinking.lock().unwrap() = thinking;
        }
        *self.manager.lock().unwrap() = manager;
        *self.context_usage_cache.lock().unwrap() = None;
        *self.totals.lock().unwrap() = Usage::default();
    }

    pub fn set_model(&self, model: Model) {
        {
            let mut m = self.manager.lock().unwrap();
            let _ = m.append_model_change(&model.provider, &model.id);
        }
        (self.sink)(SessionEvent::ModelChanged {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
        });
        *self.model.lock().unwrap() = model;
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        let _ = self
            .manager
            .lock()
            .unwrap()
            .append_thinking_level_change(level);
        *self.thinking.lock().unwrap() = level;
    }

    /// Move the active leaf in the current session tree, with an optional
    /// summary of the branch that is no longer active.
    pub async fn navigate_tree(
        self: &Arc<Self>,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<String>,
        cancel: CancellationToken,
    ) -> anyhow::Result<TreeNavigationOutcome> {
        let (old_leaf, new_leaf, editor_text, abandoned_messages) = {
            let manager = self.manager.lock().unwrap();
            let target = manager
                .get_entry(target_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown tree entry {target_id}"))?;
            let old_leaf = manager.leaf_id().map(str::to_string);
            if old_leaf.as_deref() == Some(target_id) {
                return Ok(TreeNavigationOutcome {
                    editor_text: None,
                    summarized: false,
                    cancelled: false,
                });
            }

            let (new_leaf, editor_text) = match &target {
                crate::session::entry::SessionEntry::Message {
                    message: AgentMessage::User(user),
                    ..
                } => (
                    target.parent_id().map(str::to_string),
                    Some(user.content.as_text()),
                ),
                _ => (Some(target_id.to_string()), None),
            };

            let target_ancestors: std::collections::HashSet<String> = new_leaf
                .as_deref()
                .map(|leaf| {
                    manager
                        .branch_entries(Some(leaf))
                        .into_iter()
                        .map(|entry| entry.id().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let common_ancestor = old_leaf.as_deref().and_then(|leaf| {
                manager
                    .branch_entries(Some(leaf))
                    .into_iter()
                    .rev()
                    .find(|entry| target_ancestors.contains(entry.id()))
                    .map(|entry| entry.id().to_string())
            });
            let abandoned_messages = old_leaf
                .as_deref()
                .map(|leaf| manager.branch_messages_after(leaf, common_ancestor.as_deref()))
                .unwrap_or_default();
            (old_leaf, new_leaf, editor_text, abandoned_messages)
        };

        let summary = if summarize && !abandoned_messages.is_empty() {
            let model = self.model();
            let api_key = self.resolve_api_key(&model.provider).await;
            let serialized = compaction::serialize_agent_messages(&abandoned_messages);
            let result = compaction::generate_summary(
                &model,
                api_key,
                &serialized,
                None,
                custom_instructions.as_deref(),
                cancel.clone(),
            )
            .await?;
            if cancel.is_cancelled() {
                return Ok(TreeNavigationOutcome {
                    editor_text: None,
                    summarized: false,
                    cancelled: true,
                });
            }
            Some(result)
        } else {
            None
        };

        let mut manager = self.manager.lock().unwrap();
        if manager.leaf_id() != old_leaf.as_deref() {
            anyhow::bail!("the session tree changed during navigation");
        }
        let summarized = if let Some(summary) = summary {
            let (read, modified) = extract_file_ops(&abandoned_messages);
            if let Some(usage) = &summary.usage {
                self.totals.lock().unwrap().add(usage);
            }
            manager.branch_with_summary(
                new_leaf.as_deref(),
                old_leaf.as_deref().unwrap_or(target_id),
                summary.summary,
                summary.usage,
                Some(file_ops_details(&read, &modified)),
            )?;
            true
        } else {
            if let Some(new_leaf) = new_leaf.as_deref() {
                manager.branch(new_leaf)?;
            } else {
                manager.reset_leaf();
            }
            false
        };

        Ok(TreeNavigationOutcome {
            editor_text,
            summarized,
            cancelled: false,
        })
    }

    pub fn queue_steering(&self, message: AgentMessage) {
        self.steering.lock().unwrap().push_back(message);
        self.emit_queues();
    }

    pub fn queue_follow_up(&self, message: AgentMessage) {
        self.follow_up.lock().unwrap().push_back(message);
        self.emit_queues();
    }

    /// Drain both queues back to the caller (Escape restores to editor).
    pub fn reclaim_queued(&self) -> Vec<AgentMessage> {
        let mut out: Vec<AgentMessage> = self.steering.lock().unwrap().drain(..).collect();
        out.extend(self.follow_up.lock().unwrap().drain(..));
        self.emit_queues();
        out
    }

    pub fn abort(&self) {
        self.cancel.lock().unwrap().cancel();
    }

    fn emit_queues(&self) {
        let preview = |q: &VecDeque<AgentMessage>| {
            q.iter()
                .map(|m| match m {
                    AgentMessage::User(u) => u.content.as_text().chars().take(80).collect(),
                    other => other.role().to_string(),
                })
                .collect::<Vec<String>>()
        };
        (self.sink)(SessionEvent::QueueUpdate {
            steering: preview(&self.steering.lock().unwrap()),
            follow_up: preview(&self.follow_up.lock().unwrap()),
        });
    }

    async fn resolve_api_key(&self, provider: &str) -> Option<String> {
        if let Some((override_provider, key)) = &self.api_key_override
            && override_provider == provider
        {
            return Some(key.clone());
        }
        kiss_ai::auth::resolve_api_key_async(provider, &self.registry.declared_keys)
            .await
            .ok()
            .flatten()
    }

    fn loop_config(&self, session_arc: &Arc<Self>) -> AgentLoopConfig {
        let mut config = AgentLoopConfig::new(self.model());
        config.thinking_level = self.thinking_level();
        config.session_id = Some(self.manager.lock().unwrap().session_id().to_string());
        let settings = self.settings();
        config.transport = settings.transport;
        let declared = self.registry.declared_keys.clone();
        let api_key_override = self.api_key_override.clone();
        config.get_api_key = Some(Arc::new(move |provider| {
            let declared = declared.clone();
            let api_key_override = api_key_override.clone();
            Box::pin(async move {
                if let Some((override_provider, key)) = api_key_override
                    && override_provider == provider
                {
                    return Some(key);
                }
                kiss_ai::auth::resolve_api_key_async(&provider, &declared)
                    .await
                    .ok()
                    .flatten()
            })
        }));

        let steering = self.steering.clone();
        let steering_mode = settings.steering_mode;
        let session_for_queues = session_arc.clone();
        config.get_steering_messages = Some(Arc::new(move || {
            let drained = drain_queue(&steering, steering_mode);
            session_for_queues.emit_queues();
            Box::pin(async move { drained })
        }));
        let follow_up = self.follow_up.clone();
        let follow_up_mode = settings.follow_up_mode;
        let session_for_queues = session_arc.clone();
        config.get_follow_up_messages = Some(Arc::new(move || {
            let drained = drain_queue(&follow_up, follow_up_mode);
            session_for_queues.emit_queues();
            Box::pin(async move { drained })
        }));
        let session_for_compaction = session_arc.clone();
        config.prepare_next_turn = Some(Arc::new(move |turn| {
            let has_tool_results = !turn.tool_results.is_empty();
            let session = session_for_compaction.clone();
            Box::pin(async move {
                if !has_tool_results {
                    return None;
                }
                let settings = session.settings();
                let cancel = session.cancel.lock().unwrap().clone();
                let context_window = session.model().context_window;
                let revision_before = {
                    let manager = session.manager.lock().unwrap();
                    let context = manager.build_session_context();
                    if !auto_compaction_needed(
                        &settings,
                        &context.messages,
                        context_window,
                        cancel.is_cancelled(),
                    ) {
                        return None;
                    }
                    manager.context_revision()
                };

                session.compact(None, true).await;
                let revision_after = session.manager.lock().unwrap().context_revision();
                (revision_after != revision_before).then(|| TurnUpdate {
                    context: Some(session.build_context()),
                    ..Default::default()
                })
            })
        }));
        config
    }

    fn build_context(&self) -> AgentContext {
        let model = self.model();
        let manager = self.manager.lock().unwrap();
        let (openai_responses_input, messages) =
            if kiss_ai::api::openai_compaction::supports_remote_compaction(&model)
                && let Some(remote) = manager.build_openai_compaction_context(&model)
            {
                (Some(remote.replacement_history), remote.messages)
            } else {
                (None, manager.build_session_context().messages)
            };
        drop(manager);
        let mut system_prompt = self.system_prompt.lock().unwrap().clone();
        if self.subagents_enabled() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(SUBAGENT_SYSTEM_PROMPT);
        }
        if self.workflow_tool_active()
            && let Some(runtime) = self.workflows.get()
        {
            let limits = runtime.limits();
            let size = self.settings.lock().unwrap().workflows.size;
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&crate::workflows::authoring_prompt(
                size,
                limits.max_agents,
                limits.max_fanout,
            ));
        }
        AgentContext {
            system_prompt,
            openai_responses_input,
            messages,
            tools: self.tools.lock().unwrap().clone(),
        }
    }

    pub(crate) fn create_subagent_session(
        self: &Arc<Self>,
        task_name: &str,
        canonical_path: &str,
        fork_turns: ForkTurns,
        model_pattern: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let (model, suggested_thinking) = match model_pattern {
            Some(pattern) => self
                .registry
                .resolve(pattern, None)
                .with_context(|| format!("no model matches subagent model '{pattern}'"))?,
            None => (self.model(), None),
        };
        let thinking = match reasoning_effort {
            Some(level) => ThinkingLevel::parse(level)
                .with_context(|| format!("unknown subagent reasoning_effort '{level}'"))?,
            None => suggested_thinking.unwrap_or_else(|| self.thinking_level()),
        };
        let (mut manager, parent_messages, parent_id) = {
            let parent = self.manager.lock().unwrap();
            (
                parent.create_child()?,
                parent.build_session_context().messages,
                parent.session_id().to_string(),
            )
        };
        for message in fork_messages(&parent_messages, fork_turns) {
            manager.append_message(message)?;
        }
        manager.append_custom(
            "subagent",
            Some(serde_json::json!({
                "taskName": task_name,
                "canonicalPath": canonical_path,
                "parentSessionId": parent_id,
            })),
        )?;

        let mut settings = self.settings();
        settings.subagents.enabled = false;
        let mut system_prompt = self.system_prompt.lock().unwrap().clone();
        system_prompt.push_str(&format!(
            "\n\nYou are child agent {canonical_path}. Complete only the assigned task. Return a concise result to the parent agent."
        ));

        Ok(Self::new_with_subagents_allowed(
            manager,
            self.base_tools.lock().unwrap().clone(),
            self.registry.clone(),
            settings,
            system_prompt,
            model,
            thinking,
            self.api_key_override.clone(),
            Arc::new(|_| {}),
            false,
        ))
    }

    async fn run_ephemeral(
        self: &Arc<Self>,
        system_prompt: String,
        prompt: String,
        tools: Vec<DynTool>,
        max_tokens: u64,
        cancel: CancellationToken,
    ) -> anyhow::Result<EphemeralResponse> {
        let mut config = self.loop_config(self);
        config.thinking_level = ThinkingLevel::Off;
        config.max_tokens = Some(max_tokens);
        config.session_id = Some(format!("ephemeral-{}", uuid::Uuid::new_v4()));
        config.get_steering_messages = None;
        config.get_follow_up_messages = None;
        config.prepare_next_turn = None;

        let context = AgentContext {
            system_prompt,
            openai_responses_input: None,
            messages: Vec::new(),
            tools,
        };
        let sink: EventSink = Arc::new(|_| {});
        let messages = kiss_agent::run_agent_loop(
            vec![AgentMessage::user(prompt)],
            context,
            config,
            cancel.clone(),
            sink,
        )
        .await;
        if cancel.is_cancelled() {
            anyhow::bail!("request cancelled");
        }

        let mut usage = Usage::default();
        for message in &messages {
            if let AgentMessage::Assistant(assistant) = message {
                usage.add(&assistant.usage);
            }
        }
        let assistant = messages.iter().rev().find_map(|message| match message {
            AgentMessage::Assistant(assistant) => Some(assistant),
            _ => None,
        });
        let Some(assistant) = assistant else {
            anyhow::bail!("the provider returned no answer");
        };
        if assistant.stop_reason == StopReason::Error {
            anyhow::bail!(
                "{}",
                assistant
                    .error_message
                    .as_deref()
                    .unwrap_or("the provider request failed")
            );
        }
        let text = assistant.text();
        if text.trim().is_empty() {
            anyhow::bail!("the provider returned an empty answer");
        }
        self.totals.lock().unwrap().add(&usage);
        Ok(EphemeralResponse { text, usage })
    }

    /// Answer a short side question without changing the active session.
    pub async fn answer_btw(
        self: &Arc<Self>,
        question: &str,
        cancel: CancellationToken,
    ) -> anyhow::Result<EphemeralResponse> {
        let messages = self
            .manager
            .lock()
            .unwrap()
            .build_session_context()
            .messages;
        let transcript = transcript_excerpt(&messages, 4, 4_000);
        let prompt = if transcript.is_empty() {
            format!("Side question:\n{question}")
        } else {
            format!("Recent session context:\n{transcript}\n\nSide question:\n{question}")
        };
        let read_tools = self
            .tools
            .lock()
            .unwrap()
            .iter()
            .filter(|tool| tool.name() == "read")
            .cloned()
            .collect();
        self.run_ephemeral(
            "Answer the side question from the supplied session context. This is a read-only request. Use the read tool only when a file is needed. Do not propose or perform edits. Give no more than 150 words or 600 characters. Use no more than five bullets. Return only the answer.".into(),
            prompt,
            read_tools,
            500,
            cancel,
        )
        .await
    }

    /// Create a one-line recap without changing the active session.
    pub async fn generate_recap(
        self: &Arc<Self>,
        previous_recap: Option<&str>,
        cancel: CancellationToken,
    ) -> anyhow::Result<EphemeralResponse> {
        let messages = self
            .manager
            .lock()
            .unwrap()
            .build_session_context()
            .messages;
        let transcript = transcript_excerpt(&messages, 12, 12_000);
        if transcript.is_empty() {
            anyhow::bail!("the session has no conversation to recap");
        }
        let previous = previous_recap
            .filter(|recap| !recap.trim().is_empty())
            .map(|recap| format!("\n\nPrevious recap:\n{recap}"))
            .unwrap_or_default();
        self.run_ephemeral(
            "Summarize the supplied coding session in one plain-text line of at most 120 characters. State what was done and the next action when one is clear. Do not use a prefix, Markdown, or a newline. Return only the recap.".into(),
            format!("Session transcript:\n{transcript}{previous}"),
            Vec::new(),
            160,
            cancel,
        )
        .await
    }

    /// Run one prompt to completion, including retry and auto-compaction.
    pub async fn prompt(self: &Arc<Self>, prompts: Vec<AgentMessage>) {
        {
            let mut running = self.running.lock().unwrap();
            if *running {
                // Already running: enqueue as steering instead.
                drop(running);
                for p in prompts {
                    self.queue_steering(p);
                }
                return;
            }
            *running = true;
        }
        let cancel = {
            let mut guard = self.cancel.lock().unwrap();
            *guard = CancellationToken::new();
            guard.clone()
        };

        // Persist prompts and run.
        {
            let mut manager = self.manager.lock().unwrap();
            for p in &prompts {
                let _ = manager.append_message(p.clone());
            }
        }

        let session = self.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            session.on_agent_event(&event);
            (session.sink)(SessionEvent::Agent(Box::new(event)));
        });

        let mut config = self.loop_config(self);
        let mut context = self.build_context();
        // The prompts were already persisted. Context includes them, so run
        // as a continuation without a second prompt list.

        let mut attempt: u32 = 0;
        loop {
            let messages = kiss_agent::run_agent_loop_continue(
                context.clone(),
                config.clone(),
                cancel.clone(),
                sink.clone(),
            )
            .await;

            // Retry on transient error stops.
            let last_error = messages.iter().rev().find_map(|m| match m {
                AgentMessage::Assistant(a) if a.stop_reason == StopReason::Error => {
                    Some(a.error_message.clone().unwrap_or_default())
                }
                _ => None,
            });
            let settings = self.settings();
            let retry = &settings.retry;
            if let Some(error) = last_error
                && retry.enabled
                && attempt < retry.max_retries
                && is_transient(&error)
                && !cancel.is_cancelled()
            {
                attempt += 1;
                let delay = retry.base_delay_ms.saturating_mul(1u64 << (attempt - 1));
                (self.sink)(SessionEvent::Retry {
                    attempt,
                    max: retry.max_retries,
                    delay_ms: delay,
                    error,
                });
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                context = self.build_context();
                // Drop the trailing error assistant message from context.
                while matches!(
                    context.messages.last(),
                    Some(AgentMessage::Assistant(a)) if a.stop_reason == StopReason::Error
                ) {
                    context.messages.pop();
                }
                config.model = self.model();
                config.thinking_level = self.thinking_level();
                continue;
            }

            // Auto-compaction check after a completed run.
            let ctx = self.manager.lock().unwrap().build_session_context();
            if auto_compaction_needed(
                &settings,
                &ctx.messages,
                self.model().context_window,
                cancel.is_cancelled(),
            ) {
                self.compact(None, true).await;
            }
            break;
        }

        // Workflow mode covers the turn the user armed it for, and no more, so
        // the next ordinary turn carries neither the tool nor its instructions.
        self.disarm_workflow();
        *self.running.lock().unwrap() = false;
    }

    fn on_agent_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageEnd { message } => {
                // Persist assistant + tool results (prompts persisted earlier;
                // steering/follow-up user messages arrive here too).
                let persist = match message {
                    AgentMessage::Assistant(a) => {
                        let mut totals = self.totals.lock().unwrap();
                        totals.add(&a.usage);
                        true
                    }
                    AgentMessage::ToolResult(_)
                    | AgentMessage::User(_)
                    | AgentMessage::Custom(_) => true,
                    _ => false,
                };
                if persist {
                    // User prompts were persisted in prompt(); avoid double
                    // writes by checking the current leaf message identity.
                    let mut manager = self.manager.lock().unwrap();
                    let duplicate = matches!(
                        (manager.entries().last(), message),
                        (Some(crate::session::entry::SessionEntry::Message { message: last, .. }), m) if last == m
                    );
                    if !duplicate {
                        let _ = manager.append_message(message.clone());
                    }
                }
            }
            AgentEvent::AgentEnd { .. } => {}
            _ => {}
        }
    }

    /// Manual or automatic compaction.
    pub async fn compact(self: &Arc<Self>, custom_instructions: Option<String>, auto: bool) {
        (self.sink)(SessionEvent::CompactionStart { auto });
        let ctx = self.manager.lock().unwrap().build_session_context();
        let previous_summary = ctx.messages.iter().rev().find_map(|m| match m {
            AgentMessage::CompactionSummary(c) => Some(c.summary.clone()),
            _ => None,
        });
        let keep_recent_tokens = self.settings().compaction.keep_recent_tokens;
        let plan = plan_compaction(&ctx.messages, keep_recent_tokens);
        if plan.to_summarize.is_empty() && plan.turn_prefix.is_empty() {
            (self.sink)(SessionEvent::CompactionEnd {
                summary: String::new(),
                tokens_before: plan.tokens_before,
                error: Some("Nothing to compact".into()),
            });
            return;
        }

        let model = self.model();
        let api_key = self.resolve_api_key(&model.provider).await;

        let mut serialized = compaction::serialize_agent_messages(&plan.to_summarize);
        if plan.is_split_turn {
            serialized.push_str(
                "\n\n[The following is the earlier part of the still-active task turn:]\n\n",
            );
            serialized.push_str(&compaction::serialize_agent_messages(&plan.turn_prefix));
        }

        let summary_cancel = self.cancel.lock().unwrap().clone();
        let remote_request = if kiss_ai::api::openai_compaction::supports_remote_compaction(&model)
        {
            let context = self.build_context();
            Some((
                kiss_ai::Context {
                    system_prompt: Some(context.system_prompt),
                    openai_responses_input: context.openai_responses_input,
                    messages: kiss_agent::convert_to_llm(&context.messages),
                    tools: context.tools.iter().map(|tool| tool.to_def()).collect(),
                },
                kiss_ai::StreamOptions {
                    api_key: api_key.clone(),
                    reasoning: self.thinking_level(),
                    session_id: Some(self.manager.lock().unwrap().session_id().to_string()),
                    cancel: summary_cancel.clone(),
                    ..Default::default()
                },
            ))
        } else {
            None
        };
        let local_future = compaction::generate_summary(
            &model,
            api_key.clone(),
            &serialized,
            previous_summary.as_deref(),
            custom_instructions.as_deref(),
            summary_cancel.clone(),
        );
        let remote_model = model.clone();
        let remote_future = async move {
            match remote_request {
                Some((context, options)) => Some(
                    kiss_ai::api::openai_compaction::compact(&remote_model, &context, &options)
                        .await,
                ),
                None => None,
            }
        };
        let (local_outcome, remote_outcome) = tokio::join!(local_future, remote_future);

        match select_compaction_outcome(&model, local_outcome, remote_outcome) {
            Ok(result) => {
                let mut summarized_all = plan.to_summarize.clone();
                summarized_all.extend(plan.turn_prefix.clone());
                let (read, modified) = extract_file_ops(&summarized_all);
                {
                    let mut totals = self.totals.lock().unwrap();
                    if let Some(u) = &result.local_usage {
                        totals.add(u);
                    }
                    if let Some(u) = &result.remote_usage {
                        totals.add(u);
                    }
                }
                let details = merge_compaction_details(
                    file_ops_details(&read, &modified),
                    result.remote_details,
                );
                let mut manager = self.manager.lock().unwrap();
                let _ = manager.append_compaction(
                    result.summary.clone(),
                    plan.tokens_before,
                    plan.kept.clone(),
                    result.local_usage,
                    Some(details),
                );
                (self.sink)(SessionEvent::CompactionEnd {
                    summary: result.summary,
                    tokens_before: plan.tokens_before,
                    error: None,
                });
            }
            Err(error) => {
                (self.sink)(SessionEvent::CompactionEnd {
                    summary: String::new(),
                    tokens_before: plan.tokens_before,
                    error: Some(format!("{error:#}")),
                });
            }
        }
    }

    /// Current estimated context tokens and window fraction.
    pub fn context_usage(&self) -> (u64, u64) {
        let manager = self.manager.lock().unwrap();
        let revision = manager.context_revision();
        let used = if let Some((cached_revision, tokens)) =
            *self.context_usage_cache.lock().unwrap()
            && cached_revision == revision
        {
            tokens
        } else {
            let tokens = estimate_context_tokens(&manager.build_session_context().messages);
            *self.context_usage_cache.lock().unwrap() = Some((revision, tokens));
            tokens
        };
        drop(manager);
        (used, self.model().context_window)
    }
}

struct SelectedCompaction {
    summary: String,
    local_usage: Option<Usage>,
    remote_usage: Option<Usage>,
    remote_details: Option<serde_json::Value>,
}

fn select_compaction_outcome(
    model: &Model,
    local: anyhow::Result<compaction::SummaryOutcome>,
    remote: Option<anyhow::Result<kiss_ai::api::openai_compaction::RemoteCompactionResult>>,
) -> anyhow::Result<SelectedCompaction> {
    match (local, remote) {
        (Ok(local), Some(Ok(remote))) => Ok(SelectedCompaction {
            summary: local.summary,
            local_usage: local.usage,
            remote_usage: remote.usage,
            remote_details: Some(
                kiss_ai::api::openai_compaction::build_remote_compaction_details(model, &remote),
            ),
        }),
        (Ok(local), Some(Err(_)) | None) => Ok(SelectedCompaction {
            summary: local.summary,
            local_usage: local.usage,
            remote_usage: None,
            remote_details: None,
        }),
        (Err(_), Some(Ok(remote))) => Ok(SelectedCompaction {
            summary: format!(
                "OpenAI server-side compaction was applied for {}/{}. The provider-native context is stored in this session, and this notice keeps the compaction boundary readable for other providers.",
                model.provider, model.id
            ),
            local_usage: None,
            remote_usage: remote.usage,
            remote_details: Some(
                kiss_ai::api::openai_compaction::build_remote_compaction_details(model, &remote),
            ),
        }),
        (Err(local), Some(Err(remote))) => anyhow::bail!(
            "local compaction failed: {local:#}; OpenAI remote compaction failed: {remote:#}"
        ),
        (Err(error), None) => Err(error),
    }
}

fn merge_compaction_details(
    mut local: serde_json::Value,
    remote: Option<serde_json::Value>,
) -> serde_json::Value {
    let Some(remote) = remote else {
        return local;
    };
    let Some(local_object) = local.as_object_mut() else {
        return remote;
    };
    if let Some(remote_object) = remote.as_object() {
        for (key, value) in remote_object {
            local_object.insert(key.clone(), value.clone());
        }
    }
    local
}

fn transcript_excerpt(messages: &[AgentMessage], max_messages: usize, max_chars: usize) -> String {
    let mut entries = messages
        .iter()
        .rev()
        .filter_map(|message| match message {
            AgentMessage::User(user) => Some(("User", user.content.as_text())),
            AgentMessage::Assistant(assistant) => Some(("Assistant", assistant.text())),
            _ => None,
        })
        .filter(|(_, text)| !text.trim().is_empty())
        .take(max_messages)
        .collect::<Vec<_>>();
    entries.reverse();
    let transcript = entries
        .into_iter()
        .map(|(role, text)| format!("{role}: {}", text.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    let count = transcript.chars().count();
    if count <= max_chars {
        return transcript;
    }
    let omitted = count - max_chars;
    let tail = transcript.chars().skip(omitted).collect::<String>();
    format!("[earlier text omitted]\n{tail}")
}

fn drain_queue(queue: &Arc<Mutex<VecDeque<AgentMessage>>>, mode: QueueMode) -> Vec<AgentMessage> {
    let mut q = queue.lock().unwrap();
    match mode {
        QueueMode::All => q.drain(..).collect(),
        QueueMode::OneAtATime => q.pop_front().into_iter().collect(),
    }
}

fn auto_compaction_needed(
    settings: &Settings,
    messages: &[AgentMessage],
    context_window: u64,
    cancelled: bool,
) -> bool {
    settings.compaction.enabled
        && !cancelled
        && context_window > 0
        && should_compact(
            estimate_context_tokens(messages),
            context_window,
            settings.compaction.reserve_tokens,
        )
}

fn is_transient(error: &str) -> bool {
    let e = error.to_lowercase();
    [
        "429",
        "500",
        "502",
        "503",
        "504",
        "overloaded",
        "rate limit",
        "timeout",
        "timed out",
        "connection",
        "stream error",
        "request failed",
        "unexpectedly",
    ]
    .iter()
    .any(|needle| e.contains(needle))
}

#[cfg(test)]
mod ephemeral_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn openai_model() -> Model {
        Model {
            id: "gpt-test".into(),
            name: "GPT test".into(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: Default::default(),
            context_window: 100_000,
            max_tokens: 1_000,
            compat: None,
            thinking_level_map: BTreeMap::new(),
            headers: BTreeMap::new(),
        }
    }

    fn remote_result() -> kiss_ai::api::openai_compaction::RemoteCompactionResult {
        kiss_ai::api::openai_compaction::RemoteCompactionResult {
            replacement_history: vec![serde_json::json!({
                "type": "compaction",
                "encrypted_content": "opaque"
            })],
            usage: Some(Usage {
                input: 10,
                output: 2,
                total_tokens: 12,
                ..Default::default()
            }),
        }
    }

    fn settings_test_session(settings: Settings, subagents_allowed: bool) -> Arc<AgentSession> {
        let registry = Registry::from_builtin();
        let model = registry.all().first().expect("built-in model").clone();
        AgentSession::new_with_subagents_allowed(
            SessionManager::in_memory(std::path::Path::new("/test")),
            Vec::new(),
            registry,
            settings,
            "root prompt".into(),
            model,
            ThinkingLevel::Off,
            None,
            Arc::new(|_| {}),
            subagents_allowed,
        )
    }

    fn benchmark_tools() -> Vec<DynTool> {
        let cwd = std::path::PathBuf::from("/synthetic");
        vec![
            Arc::new(kiss_agent::tools::read::ReadTool { cwd: cwd.clone() }),
            Arc::new(kiss_agent::tools::write::WriteTool { cwd: cwd.clone() }),
            Arc::new(kiss_agent::tools::edit::EditTool { cwd: cwd.clone() }),
            Arc::new(kiss_agent::tools::bash::BashTool::new(cwd)),
        ]
    }

    #[test]
    fn subagent_tools_follow_settings_and_command_line_authority() {
        let session = settings_test_session(Settings::default(), true);
        assert!(session.available_tool_names().is_empty());
        assert!(
            !session
                .build_context()
                .system_prompt
                .contains("Subagent coordination")
        );

        let mut enabled = session.settings();
        enabled.subagents.enabled = true;
        session.update_settings(enabled.clone());
        assert_eq!(
            session.available_tool_names(),
            [
                "spawn_agent",
                "send_message",
                "followup_task",
                "wait_agent",
                "list_agents",
                "interrupt_agent"
            ]
        );
        assert!(
            session
                .build_context()
                .system_prompt
                .contains("Subagent coordination")
        );

        enabled.subagents.enabled = false;
        session.update_settings(enabled);
        assert!(session.available_tool_names().is_empty());

        let mut blocked_settings = Settings::default();
        blocked_settings.subagents.enabled = true;
        let blocked = settings_test_session(blocked_settings, false);
        assert!(blocked.available_tool_names().is_empty());
        assert!(
            !blocked
                .build_context()
                .system_prompt
                .contains("Subagent coordination")
        );
    }

    #[test]
    fn the_workflow_tool_appears_only_while_a_turn_is_armed() {
        let mut settings = Settings::default();
        settings.subagents.enabled = true;
        let session = settings_test_session(settings, true);

        // Subagents on, workflow not armed: an ordinary coding turn pays
        // nothing for the feature.
        assert!(
            !session
                .available_tool_names()
                .contains(&"run_workflow".into())
        );
        assert!(
            !session
                .build_context()
                .system_prompt
                .contains("Writing a dynamic workflow")
        );

        session.arm_workflow();
        assert!(
            session
                .available_tool_names()
                .contains(&"run_workflow".into())
        );
        assert!(
            session
                .build_context()
                .system_prompt
                .contains("Writing a dynamic workflow")
        );

        session.disarm_workflow();
        assert!(
            !session
                .available_tool_names()
                .contains(&"run_workflow".into())
        );
    }

    #[test]
    fn arming_a_workflow_does_nothing_while_subagents_are_off() {
        // Workflows are built on child agents, so the subagent setting is the
        // authority for both.
        let session = settings_test_session(Settings::default(), true);
        assert!(!session.workflows_enabled());
        session.arm_workflow();
        assert!(
            !session
                .available_tool_names()
                .contains(&"run_workflow".into())
        );

        let mut settings = session.settings();
        settings.subagents.enabled = true;
        settings.workflows.enabled = false;
        session.update_settings(settings.clone());
        assert!(!session.workflows_enabled());
        assert!(
            !session
                .available_tool_names()
                .contains(&"run_workflow".into())
        );

        settings.workflows.enabled = true;
        session.update_settings(settings);
        assert!(session.workflows_enabled());
        assert!(
            session
                .available_tool_names()
                .contains(&"run_workflow".into())
        );
    }

    #[test]
    fn a_session_without_subagent_authority_has_no_workflow_runtime() {
        let mut settings = Settings::default();
        settings.subagents.enabled = true;
        let child = settings_test_session(settings, false);
        assert!(child.workflows().is_none());
        assert!(!child.workflows_enabled());
    }

    #[test]
    fn child_session_has_safe_forked_context_without_control_tools() {
        let mut settings = Settings::default();
        settings.subagents.enabled = true;
        let parent = settings_test_session(settings, true);
        parent
            .manager
            .lock()
            .unwrap()
            .append_message(AgentMessage::user("parent context"))
            .unwrap();

        let child = parent
            .create_subagent_session("inspect", "/root/inspect", ForkTurns::All, None, None)
            .unwrap();
        assert!(child.available_tool_names().is_empty());
        let context = child.manager.lock().unwrap().build_session_context();
        assert!(matches!(
            context.messages.as_slice(),
            [AgentMessage::User(user)] if user.content.as_text() == "parent context"
        ));
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_subagent_overhead() {
        let registry = Registry::from_builtin();
        let model = registry.all().first().expect("built-in model").clone();
        let tools = benchmark_tools();
        let make_session = |enabled: bool| {
            let mut settings = Settings::default();
            settings.subagents.enabled = enabled;
            AgentSession::new_with_subagents_allowed(
                SessionManager::in_memory(std::path::Path::new("/synthetic")),
                tools.clone(),
                registry.clone(),
                settings,
                "benchmark root prompt".into(),
                model.clone(),
                ThinkingLevel::Off,
                None,
                Arc::new(|_| {}),
                true,
            )
        };

        kiss_bench::measure_pair(
            (
                "agent_session_create_subagents_off",
                "agent_session_create_subagents_on",
            ),
            21,
            500,
            (
                "new_root_session_4_base_tools_0_control_tools",
                "new_root_session_4_base_tools_6_control_tools",
            ),
            || make_session(false),
            || make_session(true),
        );

        let off = make_session(false);
        let on = make_session(true);
        kiss_bench::measure_pair(
            (
                "agent_context_build_subagents_off",
                "agent_context_build_subagents_on",
            ),
            21,
            10_000,
            (
                "empty_session_4_base_tools_0_control_tools",
                "empty_session_4_base_tools_6_control_tools",
            ),
            || off.build_context(),
            || on.build_context(),
        );
    }

    #[test]
    fn transcript_excerpt_keeps_only_recent_user_and_assistant_text() {
        let messages = vec![
            AgentMessage::user("old"),
            AgentMessage::BashExecution(kiss_agent::BashExecutionMessage {
                command: "pwd".into(),
                output: "ignored".into(),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
                full_output_path: None,
                exclude_from_context: false,
                timestamp: 1,
            }),
            AgentMessage::user("new"),
        ];
        let excerpt = transcript_excerpt(&messages, 1, 100);
        assert_eq!(excerpt, "User: new");
    }

    #[test]
    fn transcript_excerpt_enforces_character_budget_from_the_tail() {
        let excerpt = transcript_excerpt(&[AgentMessage::user("abcdefghij")], 4, 5);
        assert!(excerpt.ends_with("fghij"));
        assert!(excerpt.starts_with("[earlier text omitted]"));
    }

    #[test]
    fn hybrid_compaction_keeps_local_summary_and_remote_details() {
        let selected = select_compaction_outcome(
            &openai_model(),
            Ok(compaction::SummaryOutcome {
                summary: "portable".into(),
                usage: None,
            }),
            Some(Ok(remote_result())),
        )
        .unwrap();
        assert_eq!(selected.summary, "portable");
        assert_eq!(selected.remote_usage.unwrap().input, 10);
        assert_eq!(
            selected.remote_details.unwrap()["remoteCompaction"]["version"],
            2
        );
    }

    #[test]
    fn remote_failure_falls_back_to_local_compaction() {
        let selected = select_compaction_outcome(
            &openai_model(),
            Ok(compaction::SummaryOutcome {
                summary: "portable".into(),
                usage: None,
            }),
            Some(Err(anyhow::anyhow!("remote unavailable"))),
        )
        .unwrap();
        assert_eq!(selected.summary, "portable");
        assert!(selected.remote_details.is_none());
    }

    #[test]
    fn remote_success_survives_local_summary_failure() {
        let selected = select_compaction_outcome(
            &openai_model(),
            Err(anyhow::anyhow!("summary unavailable")),
            Some(Ok(remote_result())),
        )
        .unwrap();
        assert!(
            selected
                .summary
                .contains("server-side compaction was applied")
        );
        assert!(selected.remote_details.is_some());
    }

    #[test]
    fn details_merge_keeps_file_operations_and_remote_artifact() {
        let merged = merge_compaction_details(
            serde_json::json!({"readFiles": ["a.rs"], "modifiedFiles": []}),
            Some(serde_json::json!({"remoteCompaction": {"version": 2}})),
        );
        assert_eq!(merged["readFiles"][0], "a.rs");
        assert_eq!(merged["remoteCompaction"]["version"], 2);
    }

    #[test]
    fn auto_compaction_guard_checks_settings_threshold_and_cancel() {
        let mut settings = Settings::default();
        settings.compaction.reserve_tokens = 20;
        let messages = vec![AgentMessage::user("x".repeat(360))];
        assert!(auto_compaction_needed(&settings, &messages, 100, false));
        assert!(!auto_compaction_needed(&settings, &messages, 100, true));
        settings.compaction.enabled = false;
        assert!(!auto_compaction_needed(&settings, &messages, 100, false));
    }
}
