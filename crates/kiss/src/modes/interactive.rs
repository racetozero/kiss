//! Interactive mode: transcript + editor + footer over the differential
//! renderer, with slash commands, queueing, and pickers.

use crate::args::Args;
use crate::file_search::{FileSearchMatch, FileSearchQuery, FileSearchResult, FileSearchService};
use crate::job_ui::{self, JobView, JobViewAction};
use crate::session_picker::{SessionPicker, SessionPickerAction};
use crate::session_sources::{self, SessionRecord, SessionSource};
use crate::setup::{Startup, build_startup, reload_runtime};
use crate::slash_commands;
use crate::workflow_ui::{self, ViewAction, WorkflowView};
use anyhow::{Context as _, Result};
use kiss_agent::{AgentEvent, AgentMessage};
use kiss_ai::{AssistantEvent, ContentBlock, StopReason, ThinkingLevel, Transport};
use kiss_coding::decision::DecisionModel;
use kiss_coding::session_runner::{PromptMode, QueueKind, RunPhase, SessionEvent};
use kiss_coding::settings::{DecisionMode as _, MermaidRendering, QueueMode};
use kiss_coding::workflows::workflow_trigger;
use kiss_tui::{
    Action, DiffRenderer, Editor, EditorSubmission, InputDecoder, InputEvent, Key, KeyEvent,
    Keybindings, MarkdownRenderer, MermaidMode, ProgramStatus, SelectItem, SelectList,
    StreamingMarkdownCache, Terminal, Theme,
};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One rendered transcript cell.
enum Cell {
    User(String),
    AssistantStreaming(String),
    AssistantFinal(String),
    Thinking(String),
    ToolCall {
        /// The tool call this cell shows. Parallel calls finish in any order.
        id: String,
        title: String,
        output: String,
        is_error: bool,
        done: bool,
    },
    Notice(String),
    ReasoningEffort {
        decision_model: String,
        level: ThinkingLevel,
        generations: u8,
    },
    Error(String),
    BashExecution {
        command: String,
        output: String,
        exclude_from_context: bool,
    },
}

struct CachedCellLines {
    width: usize,
    key: u64,
    lines: Vec<Arc<str>>,
    streaming: Option<StreamingMarkdownCache>,
}

struct App {
    theme: Theme,
    md: MarkdownRenderer,
    cells: Vec<Cell>,
    cell_render_cache: Vec<Option<CachedCellLines>>,
    editor: Editor,
    voice_mode: VoiceMode,
    voice: VoiceState,
    /// Identifies the current capture, so late events of a cancelled one drop.
    voice_generation: u64,
    keybindings: Keybindings,
    startup_lines: Vec<String>,
    queue_note: Option<String>,
    spinner_frame: usize,
    session_title: Option<String>,
    terminal_title_dirty: bool,
    title_generation_pending: bool,
    idle_status: ProgramStatus,
    notifications: crate::notifications::Notifications,
    picker: Option<Picker>,
    model_catalog: Option<Arc<kiss_ai::Registry>>,
    command_menu: Option<CommandCompletion>,
    file_menu: Option<FileCompletion>,
    file_search_request: Option<u64>,
    /// The `/resume` browser. It owns the keyboard while it is open.
    session_picker: Option<SessionPicker>,
    secret_prompt: Option<SecretPrompt>,
    /// The one background command that may run. See `begin_command`.
    command: Option<BackgroundCommand>,
    next_command_id: u64,
    ctrl_c_armed: bool,
    escape_armed: bool,
    hide_thinking: bool,
    expand_tools: bool,
    mermaid_mode: MermaidMode,
    git_branch: Option<String>,
    btw_panel: Option<BtwPanel>,
    btw_request_id: u64,
    recap: Option<String>,
    recap_job: Option<RecapJob>,
    /// The id the next recap request gets.
    recap_request_id: u64,
    last_user_activity: Instant,
    idle_recap_armed: bool,
    mcp_manager: Option<kiss_mcp::McpManager>,
    mcp_servers: Vec<McpPanelServer>,
    mcp_config_paths: Option<kiss_mcp::config::ConfigPaths>,
    webmcp_manager: kiss_webmcp::WebMcpManager,
    /// The open `/workflows` progress view, if any.
    workflow_view: Option<WorkflowView>,
    /// The open `/jobs` loop and autoresearch view, if any.
    job_view: Option<JobView>,
    /// Runtime-verified workflow results waiting for the end of this turn.
    workflow_outcomes: Vec<String>,
    /// `/accounts add` in progress: provider and optional label. The next
    /// successful login for that provider becomes a new pooled account.
    pending_account_add: Option<(String, Option<String>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoiceMode {
    Off,
    Hold,
    Tap,
}

/// One voice capture. Both active states keep the `Recording`, because
/// dropping it cancels the capture and its transcription.
enum VoiceState {
    Idle,
    Recording {
        recording: crate::voice::Recording,
        preview: String,
    },
    Transcribing {
        _recording: crate::voice::Recording,
        preview: String,
    },
}

impl VoiceMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "" | "hold" => Some(Self::Hold),
            "tap" => Some(Self::Tap),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// A background task that the footer reports and that blocks other commands.
///
/// Results carry this task's ID. A second cancel can release the slot while
/// the task stops, without letting its late results change a newer command.
struct BackgroundCommand {
    id: u64,
    status: String,
    control: CommandControl,
}

/// The sending side of the command that holds the command slot. When the
/// last copy is dropped, because the task ended or panicked, the slot is
/// released; a handler that already released it is not affected, because
/// the release names this command's id.
#[derive(Clone)]
struct CommandTask(Arc<CommandSender>);

struct CommandSender {
    id: u64,
    sender: mpsc::UnboundedSender<CommandEvent>,
}

impl Drop for CommandSender {
    fn drop(&mut self) {
        let _ = self.sender.send(CommandEvent::ForCommand {
            id: self.id,
            event: Box::new(CommandEvent::CommandFinished),
        });
    }
}

impl CommandTask {
    fn new(app: &App, sender: &mpsc::UnboundedSender<CommandEvent>) -> Self {
        let id = app
            .command
            .as_ref()
            .expect("command admission precedes task creation")
            .id;
        Self::with_id(id, sender)
    }

    fn with_id(id: u64, sender: &mpsc::UnboundedSender<CommandEvent>) -> Self {
        Self(Arc::new(CommandSender {
            id,
            sender: sender.clone(),
        }))
    }

    fn send(&self, event: CommandEvent) -> std::result::Result<(), ()> {
        self.0
            .sender
            .send(CommandEvent::ForCommand {
                id: self.0.id,
                event: Box::new(event),
            })
            .map_err(|_| ())
    }
}

enum CommandControl {
    Cancellable(CancellationToken),
    LoggingIn(CancellationToken),
    Uncancellable,
    /// The token fired and the task has not reported back. Another cancel
    /// request releases the slot, in case the task never ends.
    Stopping,
    StoppingLogin,
}

/// A recap request in flight.
struct RecapJob {
    /// Matches the job's result; a result whose job was replaced or
    /// cancelled finds no job and is dropped.
    request_id: u64,
    automatic: bool,
    cancel: CancellationToken,
}

struct BtwPanel {
    request_id: u64,
    question: String,
    state: BtwState,
}

enum BtwState {
    Answering(CancellationToken),
    Answered(String),
    Failed(String),
}

enum PickerKind {
    Model(Vec<kiss_ai::Model>),
    Thinking,
    ScopedModels,
    Tree,
    TreeSummary(String),
    Fork,
    LoginProviders(Vec<String>),
    LoginMethods(String, Vec<LoginChoice>),
    Logout(Vec<String>),
    /// Providers for `/accounts`; the action decides what selecting one does.
    AccountProviders(Vec<String>, AccountAction),
    /// One provider's accounts. `ADD_ACCOUNT` selects the add row.
    Accounts(String, AccountAction),
    /// Use or remove one account (zero-based index).
    AccountActions(String, usize),
    Settings,
    Trust,
    ImportConfirm(PathBuf),
    Llama(Vec<LlamaModel>),
    McpServers,
    McpActions(String, Vec<McpPanelAction>),
    McpTools(String),
    /// Approve a workflow before it starts. The sender carries the answer back
    /// to the tool call that is waiting on it.
    WorkflowApproval(
        Box<kiss_coding::workflows::WorkflowPlan>,
        tokio::sync::oneshot::Sender<kiss_coding::workflows::ApprovalDecision>,
    ),
    /// Pick which run to open in the progress view.
    WorkflowRuns(Vec<kiss_coding::workflows::RunId>),
    /// Pick where to save a run's script.
    WorkflowSaveLocation(kiss_coding::workflows::RunId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountAction {
    Manage,
    Add,
    Use,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum McpPanelState {
    Checking,
    Connected,
    NeedsAuthentication(String),
    Failed(String),
    Disabled,
    /// Enabled in the config, but this session's manager loaded it
    /// disabled, so it connects after /reload.
    EnabledAfterReload,
}

#[derive(Debug, Clone)]
struct McpPanelServer {
    name: String,
    entry: kiss_mcp::ServerEntry,
    state: McpPanelState,
    tools: Vec<kiss_mcp::CachedTool>,
    authenticated: bool,
    source: String,
    /// The scope kiss edits for this server, or none when its definition
    /// lives in a file that kiss does not edit.
    scope: Option<kiss_mcp::config::ConfigScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpPanelAction {
    ViewTools,
    Authenticate,
    ClearAuthentication,
    Reconnect,
    ToggleDisabled,
}

struct Picker {
    kind: PickerKind,
    list: SelectList,
}

struct PickerSelection {
    kind: PickerKind,
    value: usize,
    filter: String,
}

struct CommandCompletion {
    list: SelectList,
    replacements: Vec<CompletionReplacement>,
}

#[derive(Clone)]
enum CompletionReplacement {
    CommandName(String),
    CommandArgument {
        command: String,
        value: String,
    },
    Skill {
        prefix: String,
        sigil: char,
        name: String,
    },
}

struct FileCompletion {
    prefix: String,
    list: SelectList,
    values: Vec<FileSearchMatch>,
    index_limited: bool,
}

struct SecretPrompt {
    kind: SecretPromptKind,
    value: String,
}

enum SecretPromptKind {
    ApiKey(String),
    ProviderConfig(String),
    Llama,
    AnthropicManual(kiss_ai::auth::anthropic::PendingAuthorization),
    OpenRouterManual(kiss_ai::auth::openrouter::PendingAuthorization),
    GitHubEnterpriseDomain,
    GoogleApplicationDefault,
    AwsProfile,
    TreeLabel(String),
    BranchSummary(String),
    /// Name to save a workflow run's script under.
    WorkflowName(
        kiss_coding::workflows::RunId,
        kiss_coding::workflows::SaveLocation,
    ),
}

impl SecretPromptKind {
    /// Whether the prompt asks for login input. A login prompt cancels a
    /// pending account add when dismissed, and the terminal host shows it
    /// as waiting for authentication.
    fn is_login(&self) -> bool {
        match self {
            SecretPromptKind::ApiKey(_)
            | SecretPromptKind::ProviderConfig(_)
            | SecretPromptKind::Llama
            | SecretPromptKind::AnthropicManual(_)
            | SecretPromptKind::OpenRouterManual(_)
            | SecretPromptKind::GitHubEnterpriseDomain
            | SecretPromptKind::GoogleApplicationDefault
            | SecretPromptKind::AwsProfile => true,
            SecretPromptKind::TreeLabel(_)
            | SecretPromptKind::BranchSummary(_)
            | SecretPromptKind::WorkflowName(..) => false,
        }
    }
}

enum LoginChoice {
    Method(kiss_ai::auth::LoginMethod),
    External(kiss_ai::auth::external::ExternalCredentialSource),
}

struct InteractiveResources {
    settings: kiss_coding::Settings,
    skills: Vec<kiss_coding::skills::Skill>,
    prompt_templates: Vec<kiss_coding::prompts::PromptTemplate>,
    context_file_paths: Vec<PathBuf>,
    enabled_models: Vec<kiss_ai::Model>,
    /// Workflows saved on disk, each of which is a slash command.
    saved_workflows: Vec<kiss_coding::workflows::SavedWorkflow>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LlamaModel {
    id: String,
    status: LlamaStatus,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LlamaStatus {
    value: String,
}

enum CommandEvent {
    ForCommand {
        id: u64,
        event: Box<CommandEvent>,
    },
    /// A task with no result event of its own to end the command.
    CommandFinished,
    DesktopNotificationFinished(std::result::Result<(), String>),
    BrowserLoginUrl {
        url: String,
        opened: bool,
    },
    BrowserLoginFinished {
        provider: String,
        result: std::result::Result<(), String>,
    },
    DeviceLoginNotice {
        provider: String,
        url: String,
        code: String,
        opened: bool,
    },
    /// The model list with every account list that loaded. `warnings` name
    /// the lists that did not; a list with warnings is not kept, so the
    /// next /model tries again.
    ModelCatalog {
        registry: Arc<kiss_ai::Registry>,
        warnings: Vec<String>,
        pattern: Option<String>,
    },
    LlamaModels(std::result::Result<Vec<LlamaModel>, String>),
    LlamaActionFinished(std::result::Result<String, String>),
    ShellOutput(String),
    ShellFinished(std::result::Result<ShellRunResult, String>),
    ShareFinished(std::result::Result<String, String>),
    UpdateFinished(std::result::Result<String, String>),
    TreeNavigationFinished(std::result::Result<kiss_coding::TreeNavigationOutcome, String>),
    BtwFinished {
        request_id: u64,
        result: std::result::Result<kiss_coding::EphemeralResponse, String>,
    },
    RecapFinished {
        request_id: u64,
        automatic: bool,
        result: std::result::Result<kiss_coding::EphemeralResponse, String>,
    },
    SessionTitleFinished {
        session_id: String,
        result: std::result::Result<String, String>,
    },
    McpServerChecked(Box<McpPanelServer>),
    McpLoginUrl {
        name: String,
        url: String,
        opened: bool,
    },
    McpActionFinished {
        name: String,
        action: String,
        result: std::result::Result<String, String>,
    },
    WebMcpFinished(std::result::Result<WebMcpCommandResult, String>),
    /// A workflow is waiting for the user to approve it before it starts.
    WorkflowApproval {
        plan: Box<kiss_coding::workflows::WorkflowPlan>,
        reply: tokio::sync::oneshot::Sender<kiss_coding::workflows::ApprovalDecision>,
    },
    /// A saved workflow, started from its slash command, has finished.
    WorkflowFinished {
        name: String,
        result: std::result::Result<String, String>,
    },
    /// A saved workflow passed its cost prompt and is ready to show.
    WorkflowStarted(Arc<kiss_coding::workflows::RunRecord>),
    VoiceFinished {
        generation: u64,
        result: std::result::Result<String, String>,
    },
    VoicePreview {
        generation: u64,
        text: String,
    },
}

enum WebMcpCommandResult {
    /// The manager that connected, which may no longer be the app's
    /// manager after a /reload.
    Connected(kiss_webmcp::WebMcpManager),
    Listed(Vec<kiss_webmcp::WebMcpToolInfo>),
    Disconnected,
}

#[derive(Debug)]
struct ShellRunResult {
    command: String,
    output: String,
    exit_code: Option<i32>,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<String>,
    exclude_from_context: bool,
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const AUTO_RECAP_IDLE: Duration = Duration::from_secs(5 * 60);
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_millis(33);
#[cfg(not(unix))]
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RESIZE_SETTLE_DELAY: Duration = Duration::from_millis(75);
const MAX_READY_EVENTS_PER_TICK: usize = 256;

#[derive(Debug)]
struct ResizeState {
    accepted: (usize, usize),
    observed: (usize, usize),
    settle_at: Option<Instant>,
    recheck_at: Option<Instant>,
}

impl ResizeState {
    fn new(size: (usize, usize)) -> Self {
        Self {
            accepted: size,
            observed: size,
            settle_at: None,
            recheck_at: None,
        }
    }

    fn observe(&mut self, size: (usize, usize), now: Instant) {
        if size != self.observed {
            self.observed = size;
            self.settle_at = Some(now + RESIZE_SETTLE_DELAY);
            self.recheck_at = None;
        }
    }

    fn settle(&mut self, now: Instant) -> bool {
        if !self.settle_at.is_some_and(|deadline| now >= deadline) {
            return false;
        }
        self.settle_at = None;
        if self.accepted == self.observed {
            return false;
        }
        self.accepted = self.observed;
        self.recheck_at = Some(now + RESIZE_SETTLE_DELAY);
        true
    }

    fn recheck(&mut self, size: (usize, usize), now: Instant) {
        if self.recheck_at.is_some_and(|deadline| now >= deadline) {
            self.recheck_at = None;
            self.observe(size, now);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.settle_at.or(self.recheck_at)
    }

    fn pending(&self) -> bool {
        self.settle_at.is_some()
    }
}

fn resize_notifications(initial_size: (usize, usize)) -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    #[cfg(unix)]
    tokio::spawn(async move {
        let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        else {
            return;
        };
        while signal.recv().await.is_some() {
            if tx.send(()).is_err() {
                break;
            }
        }
    });
    #[cfg(not(unix))]
    tokio::spawn(async move {
        let mut observed = initial_size;
        let mut ticker = tokio::time::interval(RESIZE_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let size = Terminal::size();
            if size != observed {
                observed = size;
                if tx.send(()).is_err() {
                    break;
                }
            }
        }
    });
    let _ = initial_size;
    rx
}

#[derive(Debug, PartialEq, Eq)]
enum CommandMenuAction {
    Handled,
    Submit(EditorSubmission),
}

/// The part of the interface that owns the keyboard and the input area. This
/// is the only place that orders the layers. A lower layer keeps its contents
/// and resumes when the layers above it close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputLayer {
    Btw,
    SecretPrompt,
    WorkflowView,
    JobView,
    SessionPicker,
    Picker,
    Editor,
}

impl App {
    /// Whether this screen is waiting on work: a run, a command, a side
    /// answer, a recap or a title. The spinner runs and no recap starts.
    fn busy(&self, session: &kiss_coding::AgentSession) -> bool {
        session.is_running()
            || self.command.is_some()
            || self
                .btw_panel
                .as_ref()
                .is_some_and(|panel| matches!(panel.state, BtwState::Answering(_)))
            || self.recap_job.is_some()
            || self.title_generation_pending
    }

    /// `busy`, or background work the terminal host should show as working:
    /// a workflow run or a loop job, which may sleep for hours between steps.
    fn working(&self, session: &kiss_coding::AgentSession) -> bool {
        self.busy(session)
            || session
                .workflows()
                .is_some_and(|runtime| runtime.active().is_some())
            || session
                .iterative_jobs()
                .is_some_and(|runtime| runtime.has_queued_or_running())
    }

    fn input_layer(&self) -> InputLayer {
        let blocking_picker = matches!(
            self.picker.as_ref().map(|picker| &picker.kind),
            Some(PickerKind::WorkflowApproval(..) | PickerKind::Trust)
        );
        if self.btw_panel.is_some() {
            InputLayer::Btw
        } else if self.secret_prompt.is_some() {
            InputLayer::SecretPrompt
        } else if blocking_picker {
            InputLayer::Picker
        } else if self.workflow_view.is_some() {
            InputLayer::WorkflowView
        } else if self.job_view.is_some() {
            InputLayer::JobView
        } else if self.session_picker.is_some() {
            InputLayer::SessionPicker
        } else if self.picker.is_some() {
            InputLayer::Picker
        } else {
            InputLayer::Editor
        }
    }

    /// Open a picker unless a workflow approval is waiting. Replacing that
    /// picker would drop its reply channel and silently cancel the workflow.
    fn show_picker(&mut self, picker: Picker) {
        if matches!(
            self.picker.as_ref().map(|picker| &picker.kind),
            Some(PickerKind::WorkflowApproval(..))
        ) {
            self.cells.push(Cell::Notice(
                "answer the workflow approval first, then try again".into(),
            ));
            return;
        }
        self.picker = Some(picker);
    }

    fn render(&mut self, width: usize, session: &Arc<kiss_coding::AgentSession>) -> Vec<Arc<str>> {
        let mut lines: Vec<String> = Vec::new();
        lines.extend(self.startup_lines.iter().cloned());

        // The progress view takes the place of the transcript while it is open.
        // It needs the height, and `esc` brings the transcript straight back.
        let layer = self.input_layer();
        if layer == InputLayer::WorkflowView
            && let Some(view) = &mut self.workflow_view
        {
            let theme = self.theme.clone();
            lines.push(String::new());
            lines.extend(view.render(width, &theme));
            lines.push(String::new());
            lines.extend(self.editor.render(width));
            lines.extend(self.footer(width, session));
            return lines.into_iter().map(Arc::from).collect();
        }

        if layer == InputLayer::JobView
            && let Some(view) = &mut self.job_view
        {
            lines.push(String::new());
            lines.extend(view.render(width, &self.theme));
            lines.push(String::new());
            lines.extend(self.footer(width, session));
            return lines.into_iter().map(Arc::from).collect();
        }

        let mut frame: Vec<Arc<str>> = lines.into_iter().map(Arc::from).collect();

        if self.cell_render_cache.len() < self.cells.len() {
            self.cell_render_cache
                .resize_with(self.cells.len(), || None);
        } else {
            self.cell_render_cache.truncate(self.cells.len());
        }

        for (cell_index, cell) in self.cells.iter().enumerate() {
            if !frame.is_empty() {
                frame.push(Arc::from(""));
            }
            let key = cell_render_key(
                cell,
                self.hide_thinking,
                self.expand_tools,
                self.mermaid_mode,
            );
            if let Some(cached) = self.cell_render_cache[cell_index].as_ref()
                && cached.width == width
                && cached.key == key
            {
                frame.extend(cached.lines.iter().cloned());
                continue;
            }
            if let Cell::AssistantStreaming(text) = cell {
                let mut streaming = self.cell_render_cache[cell_index]
                    .take()
                    .and_then(|cached| cached.streaming)
                    .unwrap_or_default();
                let rendered: Vec<Arc<str>> = streaming
                    .render(&self.md, text, width, self.mermaid_mode)
                    .into_iter()
                    .map(Arc::from)
                    .collect();
                frame.extend(rendered.iter().cloned());
                self.cell_render_cache[cell_index] = Some(CachedCellLines {
                    width,
                    key,
                    lines: rendered,
                    streaming: Some(streaming),
                });
                continue;
            }
            let mut lines = Vec::new();
            match cell {
                Cell::User(text) => {
                    for l in kiss_tui::text::wrap_text(text, width.saturating_sub(2)) {
                        lines.push(format!(
                            "{}{}\x1b[49m",
                            self.theme.color("userMessageBg").bg_code(),
                            kiss_tui::text::fit_to_width(&format!(" {l}"), width)
                        ));
                    }
                }
                Cell::AssistantStreaming(text) => {
                    unreachable!("streaming assistant cells use their incremental cache: {text}")
                }
                Cell::AssistantFinal(text) => {
                    lines.extend(
                        self.md
                            .render_with_state(text, width, false, self.mermaid_mode),
                    )
                }
                Cell::Thinking(text) => {
                    if !self.hide_thinking {
                        for l in kiss_tui::text::wrap_text(text, width.saturating_sub(2)) {
                            lines.push(
                                self.theme
                                    .fg("thinkingText", &self.theme.italic(&format!("  {l}"))),
                            );
                        }
                    }
                }
                Cell::ToolCall {
                    id: _,
                    title,
                    output,
                    is_error,
                    done,
                } => {
                    let marker = if !*done {
                        self.theme.fg("warning", "●")
                    } else if *is_error {
                        self.theme.fg("error", "●")
                    } else {
                        self.theme.fg("success", "●")
                    };
                    lines.push(format!(
                        "{marker} {}",
                        self.theme.fg("toolTitle", &self.theme.bold(title))
                    ));
                    let max_lines = if self.expand_tools { usize::MAX } else { 6 };
                    let all: Vec<&str> = output.lines().collect();
                    let shown = all.len().min(max_lines);
                    for l in &all[..shown] {
                        lines.push(self.theme.fg(
                            "toolOutput",
                            &format!(
                                "  {}",
                                kiss_tui::text::truncate_to_width(l, width.saturating_sub(2))
                            ),
                        ));
                    }
                    if all.len() > shown {
                        lines.push(self.theme.fg(
                            "dim",
                            &format!("  … {} more lines (ctrl+r expands)", all.len() - shown),
                        ));
                    }
                }
                Cell::Notice(text) => {
                    for source_line in text.lines() {
                        for line in kiss_tui::text::wrap_text(source_line, width) {
                            lines.push(self.theme.fg("muted", &line));
                        }
                    }
                }
                Cell::ReasoningEffort {
                    decision_model,
                    level,
                    generations,
                } => {
                    let message = format!(
                        "◆ {decision_model} changed reasoning to {} for {generations} generation{}",
                        level.as_str(),
                        if *generations == 1 { "" } else { "s" }
                    );
                    for line in kiss_tui::text::wrap_text(&message, width) {
                        lines.push(self.theme.fg(thinking_color_token(*level), &line));
                    }
                }
                Cell::Error(text) => {
                    for l in kiss_tui::text::wrap_text(text, width.saturating_sub(2)) {
                        lines.push(self.theme.fg("error", &format!("✗ {l}")));
                    }
                }
                Cell::BashExecution {
                    command,
                    output,
                    exclude_from_context,
                } => {
                    let marker = if *exclude_from_context { "!!" } else { "!" };
                    lines.push(self.theme.fg("success", &format!("{marker} {command}")));
                    for l in output.lines().take(20) {
                        lines.push(self.theme.fg(
                            "toolOutput",
                            &format!(
                                "  {}",
                                kiss_tui::text::truncate_to_width(l, width.saturating_sub(2))
                            ),
                        ));
                    }
                }
            }
            let rendered: Vec<Arc<str>> = lines.into_iter().map(Arc::from).collect();
            frame.extend(rendered.iter().cloned());
            self.cell_render_cache[cell_index] = Some(CachedCellLines {
                width,
                key,
                lines: rendered,
                streaming: None,
            });
        }

        let mut lines = vec![String::new()];
        let activity = match session.run_phase() {
            RunPhase::Idle => None,
            RunPhase::Streaming => Some("working"),
            RunPhase::RetryWait => Some("waiting to retry"),
            RunPhase::Compacting => Some("compacting"),
        };
        if let Some(activity) = activity {
            let spin = SPINNER[self.spinner_frame % SPINNER.len()];
            lines.push(self.theme.fg(
                "accent",
                &format!("{spin} {activity}… (esc or ctrl+c to cancel)"),
            ));
        }
        if let Some(command) = &self.command {
            let spin = SPINNER[self.spinner_frame % SPINNER.len()];
            let hint = match command.control {
                CommandControl::Cancellable(_) | CommandControl::LoggingIn(_) => {
                    " (esc or ctrl+c to cancel)"
                }
                CommandControl::Uncancellable | CommandControl::StoppingLogin => "",
                CommandControl::Stopping => " (esc or ctrl+c to stop waiting)",
            };
            lines.push(
                self.theme
                    .fg("accent", &format!("{spin} {}{hint}", command.status)),
            );
        }
        if let Some(note) = &self.queue_note {
            lines.push(self.theme.fg("muted", note));
        }
        if let Some(run) = session.workflows().and_then(|runtime| runtime.active()) {
            let spin = SPINNER[self.spinner_frame % SPINNER.len()];
            let warn_over = session.settings().workflows.size.target_agents();
            lines.push(workflow_ui::progress_line(
                &run.snapshot(),
                &run.name,
                spin,
                warn_over,
                &self.theme,
            ));
        }
        if let Some(runtime) = session.iterative_jobs()
            && runtime.active_count() > 0
        {
            lines.push(job_ui::progress_line(&runtime, &self.theme, width));
        }
        if let Some(recap) = &self.recap {
            lines.push(self.theme.fg("muted", &format!("※ recap: {recap}")));
        } else if self.recap_job.is_some() {
            let spin = SPINNER[self.spinner_frame % SPINNER.len()];
            lines.push(self.theme.fg("muted", &format!("{spin} creating recap...")));
        }

        if let Some(panel) = &self.btw_panel {
            lines.push(self.theme.fg("accent", &self.theme.bold("BTW")));
            for line in kiss_tui::text::wrap_text(&panel.question, width.saturating_sub(2)) {
                lines.push(self.theme.fg("muted", &format!("  {line}")));
            }
            match &panel.state {
                BtwState::Answered(answer) => {
                    lines.push(String::new());
                    lines.extend(self.md.render(answer, width));
                    lines.push(self.theme.fg("dim", "esc or enter closes"));
                }
                BtwState::Failed(error) => {
                    lines.push(self.theme.fg("error", &format!("✗ {error}")));
                    lines.push(self.theme.fg("dim", "esc or enter closes"));
                }
                BtwState::Answering(_) => {
                    let spin = SPINNER[self.spinner_frame % SPINNER.len()];
                    lines.push(self.theme.fg("accent", &format!("{spin} answering...")));
                    lines.push(self.theme.fg("dim", "esc cancels"));
                }
            }
        } else if let Some(prompt) = &self.secret_prompt {
            let title = match &prompt.kind {
                SecretPromptKind::ApiKey(provider) => format!("API key for {provider}"),
                SecretPromptKind::ProviderConfig(provider) => match provider.as_str() {
                    "google-vertex" => "Google API_KEY|PROJECT|LOCATION".into(),
                    "cloudflare-workers-ai" => "Cloudflare API_TOKEN|ACCOUNT_ID".into(),
                    "cloudflare-ai-gateway" => "Cloudflare API_KEY|ACCOUNT_ID|GATEWAY_ID".into(),
                    "databricks-unity-gateway" => "Databricks TOKEN|WORKSPACE_URL".into(),
                    "snowflake-cortex" => "Snowflake PAT|ACCOUNT_OR_GATEWAY_URL".into(),
                    _ => format!("Authentication values for {provider}"),
                },
                SecretPromptKind::Llama => "llama.cpp router URL, or URL|API_KEY".into(),
                SecretPromptKind::AnthropicManual(_) => {
                    "Paste the Anthropic callback URL or authorization code".into()
                }
                SecretPromptKind::OpenRouterManual(_) => {
                    "Paste the OpenRouter callback URL or authorization code".into()
                }
                SecretPromptKind::GitHubEnterpriseDomain => {
                    "GitHub Enterprise domain (blank for github.com)".into()
                }
                SecretPromptKind::GoogleApplicationDefault => {
                    "Google PROJECT|LOCATION for Application Default Credentials".into()
                }
                SecretPromptKind::AwsProfile => "AWS PROFILE|REGION".into(),
                SecretPromptKind::TreeLabel(_) => "Label for the selected tree entry".into(),
                SecretPromptKind::BranchSummary(_) => "Custom branch summary instructions".into(),
                SecretPromptKind::WorkflowName(_, location) => format!(
                    "Save the workflow as (lower-case, dashes): {}",
                    location.display_dir()
                ),
            };
            lines.push(self.theme.fg("accent", &self.theme.bold(&title)));
            if matches!(
                &prompt.kind,
                SecretPromptKind::ApiKey(_)
                    | SecretPromptKind::ProviderConfig(_)
                    | SecretPromptKind::Llama
                    | SecretPromptKind::AnthropicManual(_)
                    | SecretPromptKind::OpenRouterManual(_)
            ) {
                lines.push("•".repeat(prompt.value.chars().count()));
            } else {
                lines.push(prompt.value.clone());
            }
            lines.push(self.theme.fg("dim", "enter save · esc cancel"));
        } else if layer == InputLayer::SessionPicker
            && let Some(picker) = &mut self.session_picker
        {
            lines.extend(picker.render(width, &self.theme));
        } else if let Some(picker) = &mut self.picker {
            if let PickerKind::McpActions(name, _) = &picker.kind
                && let Some(server) = self.mcp_servers.iter().find(|server| &server.name == name)
            {
                lines.extend(mcp_server_intro(&self.theme, server, width));
            }
            lines.extend(picker.list.render(width));
            if matches!(picker.kind, PickerKind::McpServers) {
                lines.push(
                    self.theme
                        .fg("dim", "↑/↓ navigate · enter inspect · esc close"),
                );
            } else if matches!(
                picker.kind,
                PickerKind::McpActions(_, _) | PickerKind::McpTools(_)
            ) {
                lines.push(
                    self.theme
                        .fg("dim", "↑/↓ navigate · enter select · esc back"),
                );
            }
        } else {
            lines.extend(self.editor.render(width));
            if let VoiceState::Recording { preview, .. }
            | VoiceState::Transcribing { preview, .. } = &self.voice
            {
                lines.push(self.theme.fg(
                    "dim",
                    &format!(
                        "  🎤 {}",
                        kiss_tui::text::truncate_to_width(preview, width.saturating_sub(5))
                    ),
                ));
            }
            if let Some(menu) = &mut self.command_menu {
                lines.extend(menu.list.render_compact(width, ""));
                if let Some(position) = menu.list.position_label() {
                    lines.push(self.theme.fg("dim", &format!("  {position}")));
                }
            } else if self.file_menu.is_some() || self.file_search_request.is_some() {
                let mut rows = self
                    .file_menu
                    .as_mut()
                    .filter(|menu| !menu.values.is_empty())
                    .map(|menu| menu.list.render_compact(width, ""))
                    .unwrap_or_default();
                rows.resize(FILE_MENU_ROWS, String::new());
                lines.extend(rows);
                let status = match &mut self.file_menu {
                    Some(menu) if !self.file_search_request.is_some() && menu.values.is_empty() => {
                        "No matching files".to_string()
                    }
                    Some(menu) if !self.file_search_request.is_some() => format!(
                        "{}↑↓ select · tab insert · esc close{}",
                        menu.list
                            .position_label()
                            .map(|position| format!("{position} · "))
                            .unwrap_or_default(),
                        if menu.index_limited {
                            " · 500000-entry index limit"
                        } else {
                            ""
                        }
                    ),
                    _ => "Searching files…".to_string(),
                };
                lines.push(self.theme.fg(
                    "dim",
                    &kiss_tui::text::truncate_to_width(&format!("  {status}"), width),
                ));
            }
            lines.extend(self.footer(width, session));
        }
        frame.extend(lines.into_iter().map(Arc::from));
        frame
    }

    fn footer(&self, width: usize, session: &Arc<kiss_coding::AgentSession>) -> Vec<String> {
        let model = session.model();
        let totals = session.totals();
        let (used, window) = session.context_usage();
        let pct = if window > 0 {
            (used as f64 / window as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        let manager = session.manager.lock().unwrap();
        let name = manager
            .session_name()
            .map(|name| format!(" • {name}"))
            .unwrap_or_default();
        let thinking = if model.reasoning {
            format!(" · thinking {}", session.thinking_level().as_str())
        } else {
            String::new()
        };
        let branch = self
            .git_branch
            .as_deref()
            .map(|branch| format!(" ({branch})"))
            .unwrap_or_default();
        let path = format!(
            "{}{branch}{name}",
            kiss_ai::paths::abbreviate_home(&manager.cwd().display().to_string())
        );
        drop(manager);
        let left = format!(
            "↑{} ↓{} R{} W{} · {pct:.1}% ctx · {} · ${:.3}",
            format_tokens(totals.input),
            format_tokens(totals.output),
            format_tokens(totals.cache_read),
            format_tokens(totals.cache_write),
            crate::cache::usage_rate(&totals)
                .map(|rate| format!("{rate:.1}% cache"))
                .unwrap_or_else(|| "n/a cache".into()),
            totals.cost.total,
        );
        let voice = match self.voice_mode {
            VoiceMode::Off => "".to_owned(),
            VoiceMode::Hold => " · voice hold".to_owned(),
            VoiceMode::Tap => " · voice tap".to_owned(),
        };
        let accounts = account_indicator(session.registry.credential_provider(&model.provider));
        let right = format!(
            "({}) {}{thinking}{accounts}{voice}",
            model.provider, model.id
        );
        let left_width = kiss_tui::text::display_width(&left);
        let right_width = kiss_tui::text::display_width(&right);
        let stats = if left_width + right_width + 2 <= width {
            format!(
                "{left}{}{right}",
                " ".repeat(width - left_width - right_width)
            )
        } else {
            let available = width.saturating_sub(left_width + 2);
            if available > 0 {
                let right = kiss_tui::text::truncate_to_width(&right, available);
                let padding =
                    width.saturating_sub(left_width + kiss_tui::text::display_width(&right));
                format!("{left}{}{right}", " ".repeat(padding))
            } else {
                kiss_tui::text::truncate_to_width(&left, width)
            }
        };
        vec![
            self.theme
                .fg("dim", &kiss_tui::text::truncate_to_width(&path, width)),
            self.theme.fg("dim", &stats),
        ]
    }

    fn apply_theme(&mut self, theme: Theme) {
        self.theme = theme.clone();
        let code_indent = self.md.code_indent.clone();
        self.md = MarkdownRenderer::new(theme.clone());
        self.md.code_indent = code_indent;
        self.editor.set_theme(theme);
        self.cell_render_cache.clear();
    }
}

fn cell_render_key(
    cell: &Cell,
    hide_thinking: bool,
    expand_tools: bool,
    mermaid_mode: MermaidMode,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(cell).hash(&mut hasher);
    hide_thinking.hash(&mut hasher);
    expand_tools.hash(&mut hasher);
    (mermaid_mode as u8).hash(&mut hasher);
    match cell {
        Cell::User(text)
        | Cell::AssistantStreaming(text)
        | Cell::AssistantFinal(text)
        | Cell::Thinking(text)
        | Cell::Notice(text)
        | Cell::Error(text) => text.hash(&mut hasher),
        Cell::ReasoningEffort {
            decision_model,
            level,
            generations,
        } => {
            decision_model.hash(&mut hasher);
            (*level as u8).hash(&mut hasher);
            generations.hash(&mut hasher);
        }
        Cell::ToolCall {
            title,
            output,
            is_error,
            done,
            ..
        } => {
            title.hash(&mut hasher);
            output.hash(&mut hasher);
            is_error.hash(&mut hasher);
            done.hash(&mut hasher);
        }
        Cell::BashExecution {
            command,
            output,
            exclude_from_context,
        } => {
            command.hash(&mut hasher);
            output.hash(&mut hasher);
            exclude_from_context.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=9_999 => format!("{:.1}k", tokens as f64 / 1_000.0),
        10_000..=999_999 => format!("{}k", tokens / 1_000),
        1_000_000..=9_999_999 => format!("{:.1}M", tokens as f64 / 1_000_000.0),
        _ => format!("{}M", tokens / 1_000_000),
    }
}

fn read_git_branch(cwd: &Path) -> Option<String> {
    for directory in cwd.ancestors() {
        let dot_git = directory.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let pointer = std::fs::read_to_string(&dot_git).ok()?;
            let target = pointer.trim().strip_prefix("gitdir:")?.trim();
            let target = PathBuf::from(target);
            if target.is_absolute() {
                target
            } else {
                directory.join(target)
            }
        } else {
            continue;
        };
        let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
        return head
            .trim()
            .strip_prefix("ref: refs/heads/")
            .map(str::to_string);
    }
    None
}

fn refresh_git_branch(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    let cwd = session.manager.lock().unwrap().cwd().to_path_buf();
    app.git_branch = read_git_branch(&cwd);
}

fn mermaid_mode(setting: MermaidRendering) -> MermaidMode {
    match setting {
        MermaidRendering::Off => MermaidMode::Off,
        MermaidRendering::Final => MermaidMode::Final,
        MermaidRendering::Streaming => MermaidMode::Streaming,
    }
}

fn idle_recap_deadline(
    app: &App,
    session: &kiss_coding::AgentSession,
    settings: &kiss_coding::Settings,
) -> Option<Instant> {
    // A recap appears in the transcript, so it waits until the editor has
    // focus and nothing is working.
    (settings.auto_recap_enabled()
        && app.idle_recap_armed
        && !app.busy(session)
        && app.input_layer() == InputLayer::Editor)
        .then(|| app.last_user_activity + AUTO_RECAP_IDLE)
}

fn session_has_conversation(session: &Arc<kiss_coding::AgentSession>) -> bool {
    session
        .manager
        .lock()
        .unwrap()
        .build_session_context()
        .messages
        .iter()
        .any(|message| match message {
            AgentMessage::User(user) => !user.content.as_text().trim().is_empty(),
            AgentMessage::Assistant(assistant) => !assistant.text().trim().is_empty(),
            _ => false,
        })
}

fn note_user_activity(app: &mut App) {
    app.last_user_activity = Instant::now();
    app.idle_recap_armed = true;
    cancel_automatic_recap(app);
}

/// A recap that the user asked for survives input. An automatic one does not.
/// Turn automatic recaps on or off. Off also cancels one in flight; on
/// starts the idle timer from now.
fn set_auto_recap(app: &mut App, resources: &mut InteractiveResources, enabled: bool) {
    resources.settings.auto_recap = Some(enabled);
    app.idle_recap_armed = enabled;
    if enabled {
        app.last_user_activity = Instant::now();
    } else {
        cancel_automatic_recap(app);
    }
}

fn cancel_automatic_recap(app: &mut App) {
    if let Some(job) = app.recap_job.take_if(|job| job.automatic) {
        job.cancel.cancel();
    }
}

fn tool_title(name: &str, args: &serde_json::Value) -> String {
    if name == "mcp"
        && args["action"] == "call"
        && let (Some(server), Some(tool)) = (args["server"].as_str(), args["name"].as_str())
        && !server.is_empty()
        && !tool.is_empty()
    {
        return format!("mcp {server}.{tool}");
    }
    let detail = args["path"]
        .as_str()
        .or_else(|| args["pattern"].as_str())
        .or_else(|| args["command"].as_str())
        .unwrap_or("");
    let detail = detail.replace('\n', " ");
    let detail: String = detail.chars().take(80).collect();
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupInput {
    Continue,
    Submit,
    Quit,
}

fn handle_startup_input(editor: &mut Editor, event: &InputEvent) -> StartupInput {
    if let InputEvent::Key(key) = event {
        if key.ctrl && matches!(key.key, Key::Char('c') | Key::Char('d')) {
            return StartupInput::Quit;
        }
        if matches!(key.key, Key::Enter) && !key.ctrl && !key.alt && !key.shift {
            return StartupInput::Submit;
        }
    }
    let _ = editor.handle_event(event);
    StartupInput::Continue
}

fn provisional_lines(editor: &mut Editor, theme: &Theme, width: usize) -> Vec<String> {
    let mut lines = vec![
        theme.fg(
            "accent",
            &theme.bold(&format!("kiss v{}", env!("CARGO_PKG_VERSION"))),
        ),
        theme.fg("dim", "Loading the session. You can type now."),
        String::new(),
    ];
    lines.extend(editor.render(width));
    lines
}

// Empty bytes signal the decoder's standalone-Escape deadline, not EOF.
async fn read_terminal_input(
    receiver: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    escape_deadline: Option<Instant>,
) -> Option<Vec<u8>> {
    if let Some(deadline) = escape_deadline {
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), receiver.recv())
            .await
        {
            Ok(bytes) => bytes,
            Err(_) => Some(Vec::new()),
        }
    } else {
        receiver.recv().await
    }
}

pub async fn run(args: &Args) -> Result<i32> {
    let hosts = crate::terminal_hosts::TerminalHosts::from_env();
    let result = run_with_hosts(args, &hosts).await;
    hosts.shutdown().await;
    result
}

async fn run_with_hosts(args: &Args, hosts: &crate::terminal_hosts::TerminalHosts) -> Result<i32> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let sink = {
        let tx = event_tx.clone();
        Arc::new(move |event: SessionEvent| {
            let _ = tx.send(event);
        })
    };
    Terminal::install_panic_hook();
    let mut terminal = Terminal::new()?;
    terminal.set_activity(true, 0)?;
    let mut renderer = DiffRenderer::new();
    let mut decoder = InputDecoder::default();
    let mut notifications = crate::notifications::Notifications::from_env();
    let provisional_theme = Theme::dark();
    let mut provisional_editor = Editor::new(provisional_theme.clone());
    provisional_editor.placeholder =
        "Ask anything. / for commands, @ for files, ! for shell.".into();

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut resize_state = ResizeState::new(Terminal::size());
    let mut resize_rx = resize_notifications(resize_state.accepted);
    let (width, height) = resize_state.accepted;
    let lines = provisional_lines(&mut provisional_editor, &provisional_theme, width);
    {
        let mut out = std::io::stdout().lock();
        renderer.render_frame(&lines, width, height, &mut out)?;
        out.flush()?;
    }

    let startup_args = args.clone();
    let mut startup_task =
        tokio::spawn(async move { build_startup(&startup_args, true, sink).await });
    let mut submit_after_startup = false;
    let mut startup_dirty = false;
    let mut startup_render_at = Instant::now() + MIN_FRAME_INTERVAL;
    let startup = 'startup: loop {
        if startup_dirty
            && !resize_state.pending()
            && (stdin_rx.is_empty() || Instant::now() >= startup_render_at)
        {
            let (width, height) = resize_state.accepted;
            let lines = provisional_lines(&mut provisional_editor, &provisional_theme, width);
            let mut out = std::io::stdout().lock();
            renderer.render_frame(&lines, width, height, &mut out)?;
            out.flush()?;
            startup_dirty = false;
            startup_render_at = Instant::now() + MIN_FRAME_INTERVAL;
        }
        let resize_deadline = resize_state
            .next_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(startup_render_at)),
                if startup_dirty && !resize_state.pending() => {}
            result = &mut startup_task => {
                let result = result.context("startup task stopped")?;
                match result {
                    Ok(startup) => break 'startup startup,
                    Err(error) => {
                        terminal.restore();
                        return Err(error);
                    }
                }
            }
            Some(bytes) = read_terminal_input(&mut stdin_rx, decoder.escape_deadline()) => {
                let mut events = Vec::new();
                decoder.feed(&bytes, &mut events);
                if bytes.is_empty() { decoder.flush_escape(&mut events); }
                for event in events {
                    notifications.input(&event);
                    match handle_startup_input(&mut provisional_editor, &event) {
                        StartupInput::Continue => {}
                        StartupInput::Submit => submit_after_startup = true,
                        StartupInput::Quit => {
                            startup_task.abort();
                            terminal.restore();
                            return Ok(0);
                        }
                    }
                }
                startup_dirty = true;
            }
            Some(()) = resize_rx.recv() => {
                let now = Instant::now();
                resize_state.observe(Terminal::size(), now);
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(resize_deadline)),
                if resize_state.next_deadline().is_some() => {
                let now = Instant::now();
                let redraw = resize_state.settle(now);
                resize_state.recheck(Terminal::size(), now);
                if redraw {
                    startup_dirty = true;
                    startup_render_at = now;
                }
            }
        }
    };
    let Startup {
        session,
        settings,
        skills,
        prompt_templates,
        context_file_paths,
        enabled_models,
        mut initial_message,
        needs_login,
        warnings,
    } = startup;
    let mut resources = InteractiveResources {
        settings: settings.clone(),
        skills,
        prompt_templates,
        context_file_paths,
        enabled_models,
        saved_workflows: discover_saved_workflows(&session, args),
    };

    let theme = Theme::named(settings.theme.as_deref());
    provisional_editor.set_theme(theme.clone());
    if submit_after_startup {
        let draft = provisional_editor.take();
        if !draft.trim().is_empty() {
            initial_message = Some(match initial_message {
                Some(message) => format!("{message}\n\n{draft}"),
                None => draft,
            });
        }
    }
    let keybindings = load_keybindings();

    let latest_update = crate::update::check_on_launch();
    let mut startup_lines: Vec<String> = Vec::new();
    if !settings.quiet_startup {
        startup_lines.push(theme.fg(
            "accent",
            &theme.bold(&format!("kiss v{}", env!("CARGO_PKG_VERSION"))),
        ));
        // The keys come from the keybindings, as in /hotkeys, so the hint
        // stays right when the user rebinds them.
        let key = |action| key_label(&keybindings, action);
        startup_lines.push(theme.fg(
            "dim",
            &format!(
                "{} send · {} effort · {} cancel · {} exit · / commands",
                key(Action::Submit),
                key(Action::CycleThinking),
                key(Action::Abort),
                key(Action::Quit),
            ),
        ));
        if let Some(latest) = latest_update
            && let Ok(current) = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        {
            startup_lines.push(theme.fg("warning", &crate::update::notice(&current, &latest)));
        }
        if !resources.context_file_paths.is_empty() {
            let names: Vec<String> = resources
                .context_file_paths
                .iter()
                .map(|p| kiss_ai::paths::abbreviate_home(&p.display().to_string()))
                .collect();
            startup_lines.push(theme.fg("muted", &format!("context: {}", names.join(", "))));
        }
        if !resources.skills.is_empty() {
            startup_lines.push(theme.fg(
                "muted",
                &format!(
                        "skills: {}",
                        resources
                            .skills
                            .iter()
                            .map(|s| s.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
            ));
        }
        if !resources.prompt_templates.is_empty() {
            startup_lines.push(theme.fg(
                "muted",
                &format!(
                        "templates: {}",
                        resources
                            .prompt_templates
                            .iter()
                            .map(|t| format!("/{}", t.name))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
            ));
        }
    }

    let session_title = session.manager.lock().unwrap().session_name();
    let mut app = App {
        md: MarkdownRenderer::new(theme.clone()),
        editor: provisional_editor,
        voice_mode: VoiceMode::Off,
        voice: VoiceState::Idle,
        voice_generation: 0,
        theme,
        cells: Vec::new(),
        cell_render_cache: Vec::new(),
        keybindings,
        startup_lines,
        queue_note: None,
        spinner_frame: 0,
        session_title,
        terminal_title_dirty: true,
        title_generation_pending: false,
        idle_status: ProgramStatus::Idle,
        notifications,
        picker: None,
        model_catalog: None,
        command_menu: None,
        file_menu: None,
        file_search_request: None,
        session_picker: None,
        secret_prompt: None,
        command: None,
        next_command_id: 0,
        ctrl_c_armed: false,
        escape_armed: false,
        hide_thinking: settings.hide_thinking_block,
        expand_tools: false,
        mermaid_mode: mermaid_mode(settings.markdown.mermaid),
        git_branch: None,
        btw_panel: None,
        btw_request_id: 0,
        recap: None,
        recap_job: None,
        recap_request_id: 0,
        last_user_activity: Instant::now(),
        idle_recap_armed: true,
        mcp_manager: None,
        mcp_servers: Vec::new(),
        mcp_config_paths: None,
        webmcp_manager: webmcp_manager(&settings.webmcp),
        workflow_view: None,
        job_view: None,
        workflow_outcomes: Vec::new(),
        pending_account_add: None,
    };
    if let Some(indent) = &settings.markdown.code_block_indent {
        app.md.code_indent = indent.clone();
    }
    app.editor.placeholder = "Ask anything. / commands, $ skills, @ files, ! shell.".into();
    for warning in warnings {
        app.cells.push(Cell::Notice(format!("warning: {warning}")));
    }
    if needs_login {
        app.cells.push(Cell::Notice(
            "No credentials yet. Type /login to sign in.".into(),
        ));
    }
    update_thinking_border(&mut app, session.thinking_level());
    refresh_git_branch(&mut app, &session);

    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<CommandEvent>();

    // Install the cost approval gate before any prompt can call a workflow.
    session.set_workflow_approver(workflow_approver(&command_tx));

    // Kick off initial message if provided.
    if let Some(message) = initial_message {
        let prompt_mode = session.prompt_mode_for(&message);
        deliver_prompt(
            &mut app,
            &session,
            message.clone(),
            message,
            prompt_mode,
            QueueKind::Steering,
            &command_tx,
        );
    }

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut next_render_at = Instant::now();
    let (file_search_tx, mut file_search_rx) = mpsc::unbounded_channel::<FileSearchResult>();
    let mut file_search = FileSearchService::new(file_search_tx);
    file_search.warm(session.manager.lock().unwrap().cwd().to_path_buf());

    'main: loop {
        let status = terminal_host_state(&app, &session);
        if session.is_running() {
            app.notifications.started();
        }
        app.notifications.status(
            status,
            resources.settings.desktop_notifications,
            Instant::now(),
        );
        // Drain already buffered input before deciding the user is away.
        let input_is_idle = stdin_rx.is_empty() && decoder.escape_deadline().is_none();
        if input_is_idle {
            deliver_notifications(&mut app, &command_tx);
        }
        let notification_deadline = app.notifications.deadline().filter(|_| input_is_idle);
        let notification_at =
            notification_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        if hosts.enabled() {
            hosts.update(status, &session, args);
        }
        terminal.set_program_status(status)?;
        let resize_deadline = resize_state
            .next_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        let render_is_active = app.busy(&session);
        let recap_deadline = idle_recap_deadline(&app, &session, &resources.settings);
        let recap_at =
            recap_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        if app.terminal_title_dirty {
            terminal.set_session_title(app.session_title.as_deref());
            app.terminal_title_dirty = false;
        }
        terminal.set_activity(render_is_active, app.spinner_frame)?;
        if dirty
            && !resize_state.pending()
            && ((!render_is_active && stdin_rx.is_empty()) || Instant::now() >= next_render_at)
        {
            let (width, height) = resize_state.accepted;
            let lines = app.render(width, &session);
            let mut out = std::io::stdout().lock();
            renderer.render_shared_frame(&lines, width, height, &mut out)?;
            out.flush()?;
            dirty = false;
            next_render_at = Instant::now()
                + if render_is_active {
                    ACTIVE_FRAME_INTERVAL
                } else {
                    MIN_FRAME_INTERVAL
                };
        }

        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(notification_at)),
                if notification_deadline.is_some() => { dirty = true; }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_render_at)),
                if dirty && !resize_state.pending() && Instant::now() < next_render_at => {}
            Some(()) = resize_rx.recv() => {
                let now = Instant::now();
                resize_state.observe(Terminal::size(), now);
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(resize_deadline)),
                if resize_state.next_deadline().is_some() => {
                let now = Instant::now();
                let redraw = resize_state.settle(now);
                resize_state.recheck(Terminal::size(), now);
                if redraw {
                    dirty = true;
                    next_render_at = now;
                }
            }
            _ = ticker.tick(), if render_is_active => {
                app.spinner_frame += 1;
                dirty = true;
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(recap_at)),
                if recap_deadline.is_some() => {
                app.idle_recap_armed = false;
                if session_has_conversation(&session) {
                    start_recap(&mut app, &session, true, &command_tx);
                    dirty = true;
                }
            }
            Some(event) = event_rx.recv() => {
                handle_session_event(&mut app, event, &mut file_search, &session);
                for _ in 1..MAX_READY_EVENTS_PER_TICK {
                    let Ok(event) = event_rx.try_recv() else { break };
                    handle_session_event(&mut app, event, &mut file_search, &session);
                }
                dirty = true;
            }
            Some(event) = command_rx.recv() => {
                handle_command_event(&mut app, event, &mut file_search, &session);
                for _ in 1..MAX_READY_EVENTS_PER_TICK {
                    let Ok(event) = command_rx.try_recv() else { break };
                    handle_command_event(&mut app, event, &mut file_search, &session);
                }
                dirty = true;
            }
            Some(result) = file_search_rx.recv() => {
                apply_file_search_result(&mut app, result);
                for _ in 1..MAX_READY_EVENTS_PER_TICK {
                    let Ok(result) = file_search_rx.try_recv() else { break };
                    apply_file_search_result(&mut app, result);
                }
                dirty = true;
            }
            Some(bytes) = read_terminal_input(&mut stdin_rx, decoder.escape_deadline()) => {
                let mut events = Vec::new();
                decoder.feed(&bytes, &mut events);
                if bytes.is_empty() { decoder.flush_escape(&mut events); }
                for event in events {
                    let flow = handle_input(
                        &mut app,
                        &session,
                        &event,
                        args,
                        &mut resources,
                        &mut file_search,
                        &command_tx,
                    );
                    update_thinking_border(&mut app, session.thinking_level());
                    match flow {
                        Flow::Continue => {}
                        Flow::Quit => break 'main,
                    }
                }
                dirty = true;
                next_render_at = Instant::now();
            }
        }
    }

    terminal.restore();
    if let Some(file) = session.manager.lock().unwrap().session_file() {
        println!(
            "session saved: {}",
            kiss_ai::paths::abbreviate_home(&file.display().to_string())
        );
        println!("resume with: kiss -c");
    }
    Ok(0)
}

fn finish_notification(app: &mut App, result: std::result::Result<(), String>) {
    app.notifications.delivery_finished(&result);
    if let Err(error) = result {
        app.cells.push(Cell::Notice(format!(
            "desktop notification failed (disabled for this session): {error}"
        )));
    }
}

fn deliver_notifications(app: &mut App, command_tx: &mpsc::UnboundedSender<CommandEvent>) {
    let result = app
        .notifications
        .poll(Instant::now(), &mut std::io::stdout().lock());
    match result {
        Ok(Some(command)) => {
            let tx = command_tx.clone();
            tokio::spawn(async move {
                let result = crate::notifications::send(command)
                    .await
                    .map_err(|error| format!("{error:#}"));
                let _ = tx.send(CommandEvent::DesktopNotificationFinished(result));
            });
        }
        Ok(None) => {}
        Err(error) => finish_notification(app, Err(format!("{error:#}"))),
    }
}

enum Flow {
    Continue,
    Quit,
}

fn terminal_host_state(app: &App, session: &kiss_coding::AgentSession) -> ProgramStatus {
    use kiss_tui::BlockedKind;
    let blocked = match app.picker.as_ref().map(|picker| &picker.kind) {
        Some(PickerKind::WorkflowApproval(..)) => {
            Some((BlockedKind::Permission, "Workflow approval required"))
        }
        Some(PickerKind::Trust) => {
            Some((BlockedKind::Permission, "Project trust decision required"))
        }
        _ if app
            .secret_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.kind.is_login()) =>
        {
            Some((BlockedKind::Auth, "Login input required"))
        }
        _ => None,
    };
    if let Some((kind, message)) = blocked {
        return ProgramStatus::Blocked(kind, message);
    }
    if app.working(session) {
        ProgramStatus::Working
    } else {
        app.idle_status
    }
}

/// The assistant answer that is still streaming. Notices can follow it, so it
/// is not always the last cell.
fn streaming_cell(app: &App) -> Option<usize> {
    app.cells
        .iter()
        .rposition(|cell| matches!(cell, Cell::AssistantStreaming(_)))
}

/// The unfinished cell of one tool call.
fn active_tool_cell<'a>(app: &'a mut App, tool_call_id: &str) -> Option<&'a mut Cell> {
    app.cells
        .iter_mut()
        .rev()
        .find(|cell| matches!(cell, Cell::ToolCall { id, done: false, .. } if id == tool_call_id))
}

fn handle_session_event(
    app: &mut App,
    event: SessionEvent,
    file_search: &mut FileSearchService,
    session: &Arc<kiss_coding::AgentSession>,
) {
    match event {
        SessionEvent::PromptEnd { cancelled } => {
            // A message can arrive between Esc and the end of the run.
            if cancelled {
                restore_queued_to_editor(app, session);
            }
            app.notifications.prompt_finished(
                cancelled,
                session.settings().desktop_notifications,
                Instant::now(),
            );
        }
        SessionEvent::Agent(agent_event) => match *agent_event {
            AgentEvent::AgentStart => app.notifications.started(),
            AgentEvent::AgentEnd { .. } => {
                // Solidify any streaming cell.
                if let Some(cell) = streaming_cell(app)
                    && let Cell::AssistantStreaming(text) = &mut app.cells[cell]
                {
                    app.cells[cell] = Cell::AssistantFinal(std::mem::take(text));
                }
                for outcome in app.workflow_outcomes.drain(..) {
                    app.cells.push(Cell::Notice(outcome));
                }
            }
            AgentEvent::MessageStart { message } => match &message {
                AgentMessage::User(u) => {
                    // Steering/follow-up injections appear mid-run. The
                    // initial prompt cell is pushed by the submitter.
                    let text = u.content.as_text();
                    let display = visible_user_text(&text);
                    let already = matches!(app.cells.last(), Some(Cell::User(t)) if *t == display);
                    if !already {
                        app.cells.push(Cell::User(display));
                    }
                }
                AgentMessage::Assistant(_) => {
                    app.cells.push(Cell::AssistantStreaming(String::new()))
                }
                _ => {}
            },
            AgentEvent::MessageUpdate { assistant_event } => match *assistant_event {
                AssistantEvent::TextDelta { delta, .. } => {
                    if let Some(cell) = streaming_cell(app)
                        && let Cell::AssistantStreaming(text) = &mut app.cells[cell]
                    {
                        text.push_str(&delta);
                    }
                }
                AssistantEvent::TextEnd { content, .. } => {
                    if let Some(cell) = streaming_cell(app)
                        && let Cell::AssistantStreaming(text) = &mut app.cells[cell]
                        && text.is_empty()
                    {
                        *text = content;
                    }
                }
                AssistantEvent::ThinkingStart { .. } => {
                    if let Some(position) = streaming_cell(app)
                        && (position == 0 || !matches!(app.cells[position - 1], Cell::Thinking(_)))
                    {
                        app.cells.insert(position, Cell::Thinking(String::new()));
                        app.cell_render_cache.truncate(position);
                    }
                }
                AssistantEvent::ThinkingDelta { delta, .. } => {
                    if let Some(position) = streaming_cell(app) {
                        if position > 0
                            && let Cell::Thinking(thinking) = &mut app.cells[position - 1]
                        {
                            thinking.push_str(&delta);
                        } else {
                            app.cells.insert(position, Cell::Thinking(delta));
                            app.cell_render_cache.truncate(position);
                        }
                    }
                }
                AssistantEvent::ThinkingEnd { content, .. } => {
                    if let Some(position) = streaming_cell(app)
                        && position > 0
                        && let Cell::Thinking(thinking) = &mut app.cells[position - 1]
                    {
                        *thinking = content;
                    }
                }
                _ => {}
            },
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(a),
            } => {
                app.idle_status = match a.stop_reason {
                    StopReason::Error => ProgramStatus::Error,
                    StopReason::Aborted => ProgramStatus::Idle,
                    _ => ProgramStatus::Done,
                };
                if a.stop_reason == StopReason::Error {
                    app.cells.push(Cell::Error(
                        a.error_message
                            .clone()
                            .unwrap_or_else(|| "unknown error".into()),
                    ));
                }
                if a.stop_reason == StopReason::Aborted {
                    app.cells.push(Cell::Notice("aborted".into()));
                }
                if let Some(cell) = streaming_cell(app) {
                    app.cells[cell] = Cell::AssistantFinal(a.text());
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                app.cells.push(Cell::ToolCall {
                    id: tool_call_id,
                    title: tool_title(&tool_name, &args),
                    output: String::new(),
                    is_error: false,
                    done: false,
                });
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial,
                ..
            } => {
                if let Some(Cell::ToolCall { output, .. }) = active_tool_cell(app, &tool_call_id) {
                    *output = partial.output_text();
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                if let Some(Cell::ToolCall {
                    output,
                    is_error: err_slot,
                    done,
                    ..
                }) = active_tool_cell(app, &tool_call_id)
                {
                    *output = result.output_text();
                    *err_slot = is_error;
                    *done = true;
                }
                if matches!(tool_name.as_str(), "write" | "edit" | "bash") {
                    reset_file_search(app, file_search);
                    file_search.refresh_all();
                    refresh_git_branch(app, session);
                }
            }
            _ => {}
        },
        SessionEvent::QueueUpdate {
            steering,
            follow_up,
        } => {
            app.queue_note = if steering.is_empty() && follow_up.is_empty() {
                None
            } else {
                let mut parts = Vec::new();
                if !steering.is_empty() {
                    parts.push(format!("queued: {}", steering.join(" | ")));
                }
                if !follow_up.is_empty() {
                    parts.push(format!("follow-up: {}", follow_up.join(" | ")));
                }
                Some(parts.join("  ·  "))
            };
        }
        SessionEvent::CompactionStart { auto } => {
            app.cells.push(Cell::Notice(if auto {
                "auto-compacting context…".into()
            } else {
                "compacting context…".into()
            }));
        }
        SessionEvent::CompactionEnd {
            tokens_before,
            error,
            ..
        } => match error {
            Some(err) => app
                .cells
                .push(Cell::Error(format!("compaction failed: {err}"))),
            None => app.cells.push(Cell::Notice(format!(
                "compacted {tokens_before} tokens of history"
            ))),
        },
        SessionEvent::Retry {
            attempt,
            max,
            delay_ms,
            error,
        } => {
            app.cells.push(Cell::Notice(format!(
                "transient error, retry {attempt}/{max} in {}s: {error}",
                delay_ms / 1000
            )));
        }
        SessionEvent::ModelChanged { provider, model_id } => {
            app.cells
                .push(Cell::Notice(format!("model: {provider}/{model_id}")));
        }
        SessionEvent::ReasoningEffortChanged {
            decision_model,
            level,
            generations,
        } => {
            app.cells.push(Cell::ReasoningEffort {
                decision_model,
                level,
                generations,
            });
        }
        SessionEvent::ReasoningEffortFallback { level, reason } => {
            app.cells.push(Cell::Error(format!(
                "Decision model could not select reasoning: {reason}. Using saved effort {}",
                level.as_str()
            )));
        }
        // Every session event marks the frame dirty, which is all a
        // workflow progress update needs.
        SessionEvent::Workflow { .. } => {}
        SessionEvent::WorkflowOutcome { run, name, status } => {
            let run = run.map(|id| format!(" run {id}")).unwrap_or_default();
            let result = match status {
                kiss_coding::WorkflowTurnStatus::Cancelled => "was cancelled. No agents ran",
                kiss_coding::WorkflowTurnStatus::Completed => "completed",
                kiss_coding::WorkflowTurnStatus::Failed => "failed",
                kiss_coding::WorkflowTurnStatus::Stopped => "was stopped",
            };
            app.workflow_outcomes
                .push(format!("verified workflow{run} `{name}` {result}"));
        }
        SessionEvent::Iterative { .. } => {}
        SessionEvent::SaveFailed { error } => {
            app.cells.push(Cell::Error(format!(
                "could not save the session file: {error}. The conversation continues, and kiss saves it again with the next message"
            )));
        }
        SessionEvent::AccountSwitched(switch) => {
            app.cells.push(Cell::Notice(format!(
                "{}: {} hit its rate limit (ready again in {}); switched to account {}/{} ({})",
                switch.provider,
                switch.from,
                format_duration_secs(switch.retry_after_secs),
                switch.index + 1,
                switch.total,
                switch.to,
            )));
        }
    }
}

/// Footer suffix such as ` · ⇄ 2/4 personal` for a provider with a pool of
/// two or more accounts.
fn account_indicator(provider: &str) -> String {
    let Some(pool) = kiss_ai::auth::accounts::pool(provider) else {
        return String::new();
    };
    if pool.accounts.len() < 2 {
        return String::new();
    }
    let Some(active) = pool.active() else {
        return String::new();
    };
    format!(
        " · ⇄ {}/{} {}",
        active.index + 1,
        pool.accounts.len(),
        active.label
    )
}

fn account_provider(name: &str) -> Option<&'static str> {
    kiss_ai::registry::BUILTIN_PROVIDER_IDS
        .iter()
        .copied()
        .find(|provider| *provider == name)
}

fn account_status(account: &kiss_ai::auth::accounts::AccountInfo) -> String {
    let state = if account.active { "active" } else { "ready" };
    match account.limited_until.filter(|_| account.is_limited()) {
        Some(until) => {
            let seconds = (until - chrono::Utc::now().timestamp_millis()) as u64 / 1000;
            let limited = format!("rate limited · ready in {}", format_duration_secs(seconds));
            if account.active {
                format!("active · {limited}")
            } else {
                limited
            }
        }
        None => state.into(),
    }
}

/// Providers offered first when adding an account.
const FEATURED_ACCOUNT_PROVIDERS: &[&str] = &[
    "openai-codex",
    "anthropic",
    "cursor",
    "google",
    "meta",
    "xai",
];

const ADD_ACCOUNT: usize = usize::MAX;

fn run_accounts_command(app: &mut App, arguments: &str) {
    let parts = arguments.split_whitespace().collect::<Vec<_>>();
    let (action, rest) = match parts.first().copied() {
        None | Some("list") => (AccountAction::Manage, &parts[parts.len().min(1)..]),
        Some("add") => (AccountAction::Add, &parts[1..]),
        Some("use") => (AccountAction::Use, &parts[1..]),
        Some("remove") => (AccountAction::Remove, &parts[1..]),
        Some(_) => (AccountAction::Manage, &parts[..]),
    };
    let Some(provider) = rest.first().and_then(|name| account_provider(name)) else {
        open_account_providers(app, action, rest.first().copied());
        return;
    };
    let index = rest
        .get(1)
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|number| *number > 0)
        .map(|number| number - 1);
    match (action, index) {
        (AccountAction::Add, _) => {
            let label = (rest.len() > 1).then(|| rest[1..].join(" "));
            start_account_add(app, provider, label);
        }
        (AccountAction::Use | AccountAction::Remove, Some(index)) => {
            apply_account_action(app, provider, index, action == AccountAction::Use);
        }
        _ => open_accounts_picker(app, provider, action),
    }
}

fn open_account_providers(app: &mut App, action: AccountAction, filter: Option<&str>) {
    let pools = kiss_ai::auth::accounts::pools();
    let mut providers: Vec<String> = pools.iter().map(|pool| pool.provider.clone()).collect();
    if matches!(action, AccountAction::Manage | AccountAction::Add) {
        for provider in FEATURED_ACCOUNT_PROVIDERS
            .iter()
            .chain(kiss_ai::registry::BUILTIN_PROVIDER_IDS)
        {
            if !providers.iter().any(|known| known == provider) {
                providers.push(provider.to_string());
            }
        }
    }
    if providers.is_empty() {
        app.cells.push(Cell::Notice(
            "no accounts yet. Run /accounts add to log in the first one".into(),
        ));
        return;
    }
    let items = providers
        .iter()
        .enumerate()
        .map(|(value, provider)| SelectItem {
            label: provider.clone(),
            detail: Some(match pools.iter().find(|pool| &pool.provider == provider) {
                Some(pool) => format!(
                    "{} account{} · active: {}",
                    pool.accounts.len(),
                    if pool.accounts.len() == 1 { "" } else { "s" },
                    pool.active()
                        .map_or("none", |account| account.label.as_str())
                ),
                None => "no accounts".into(),
            }),
            value,
        })
        .collect();
    let title = match action {
        AccountAction::Manage => "Accounts · rate-limited accounts fail over to the next one",
        AccountAction::Add => "Add an account to which provider?",
        AccountAction::Use => "Switch the active account for which provider?",
        AccountAction::Remove => "Remove an account from which provider?",
    };
    let mut list = SelectList::new(title, items, app.theme.clone());
    list.max_visible = 14;
    if let Some(filter) = filter {
        list.set_filter(filter.to_string());
    }
    app.show_picker(Picker {
        kind: PickerKind::AccountProviders(providers, action),
        list,
    });
}

fn open_accounts_picker(app: &mut App, provider: &str, action: AccountAction) {
    let accounts = kiss_ai::auth::accounts::pool(provider)
        .map(|pool| pool.accounts)
        .unwrap_or_default();
    if accounts.is_empty() && action != AccountAction::Manage {
        app.cells.push(Cell::Notice(format!(
            "{provider} has no accounts. Run /accounts add {provider}"
        )));
        return;
    }
    let mut items: Vec<SelectItem> = accounts
        .iter()
        .map(|account| SelectItem {
            label: format!(
                "{} {}. {}",
                if account.active { "▸" } else { " " },
                account.index + 1,
                account.label
            ),
            detail: Some(account_status(account)),
            value: account.index,
        })
        .collect();
    if action == AccountAction::Manage {
        items.push(SelectItem {
            label: "+ Add account".into(),
            detail: Some("Log in another account; the active one stays in use".into()),
            value: ADD_ACCOUNT,
        });
    }
    let title = match action {
        AccountAction::Use => format!("Make which {provider} account active?"),
        AccountAction::Remove => format!("Remove which {provider} account?"),
        _ => format!("{provider} accounts"),
    };
    let mut list = SelectList::new(title, items, app.theme.clone());
    list.max_visible = 12;
    app.show_picker(Picker {
        kind: PickerKind::Accounts(provider.into(), action),
        list,
    });
}

fn open_account_actions(app: &mut App, provider: &str, index: usize) {
    let Some(account) = kiss_ai::auth::accounts::pool(provider)
        .and_then(|pool| pool.accounts.into_iter().nth(index))
    else {
        return open_accounts_picker(app, provider, AccountAction::Manage);
    };
    let items = [
        ("Use this account", "Make it the active account"),
        ("Remove this account", "Forget its saved credential"),
        ("Back", "Return to the account list"),
    ]
    .into_iter()
    .enumerate()
    .map(|(value, (label, detail))| SelectItem {
        label: label.into(),
        detail: Some(detail.into()),
        value,
    })
    .collect();
    app.show_picker(Picker {
        kind: PickerKind::AccountActions(provider.into(), index),
        list: SelectList::new(
            format!("{provider} · {}. {}", index + 1, account.label),
            items,
            app.theme.clone(),
        ),
    });
}

fn start_account_add(app: &mut App, provider: &str, label: Option<String>) {
    if command_blocks(app, "adding an account") {
        return;
    }
    if let Err(error) = kiss_ai::auth::accounts::begin_add(provider) {
        app.cells
            .push(Cell::Error(format!("could not add an account: {error:#}")));
        return;
    }
    app.pending_account_add = Some((provider.to_string(), label));
    app.cells.push(Cell::Notice(format!(
        "log in to the {provider} account to add. The current account stays active"
    )));
    open_login_methods_picker(app, provider);
}

/// Refresh what depends on a provider's credentials after a login, logout
/// or account switch: the model lists that come from the account. A Copilot
/// account's allowed models need nothing here, because `ModelAccess` reads
/// them for every model list.
fn credentials_changed(app: &mut App, provider: &str) {
    if kiss_ai::Registry::is_dynamic(provider) {
        app.model_catalog = None;
    }
}

/// Remove the stored credentials of `provider`, for /logout and its picker.
fn log_out(app: &mut App, provider: &str) {
    let removed = kiss_ai::auth::remove_api_key(provider);
    if matches!(removed, Ok(true)) {
        credentials_changed(app, provider);
    }
    app.cells.push(match removed {
        Ok(true) => Cell::Notice(format!("removed stored credentials for {provider}")),
        Ok(false) => Cell::Notice(format!("no stored credentials for {provider}")),
        Err(error) => Cell::Error(format!("logout failed: {error:#}")),
    });
}

/// Make account `index` active (`activate`) or remove it.
fn apply_account_action(app: &mut App, provider: &str, index: usize, activate: bool) {
    use kiss_ai::auth::accounts;
    let result = if activate {
        accounts::activate(provider, index).map(|account| {
            credentials_changed(app, provider);
            format!(
                "{provider}: account {} ({}) is active",
                index + 1,
                account.label
            )
        })
    } else {
        accounts::remove(provider, index).map(|account| {
            format!(
                "{provider}: removed account {} ({})",
                index + 1,
                account.label
            )
        })
    };
    app.cells.push(match result {
        Ok(message) => Cell::Notice(message),
        Err(error) => Cell::Error(format!("{error:#}")),
    });
}

/// Finish `/accounts add` after a login for `provider` completes.
fn complete_account_add(app: &mut App, provider: &str, succeeded: bool) {
    if app
        .pending_account_add
        .as_ref()
        .is_none_or(|(pending, _)| pending != provider)
    {
        return;
    }
    if !succeeded {
        cancel_account_add(app);
        return;
    }
    let (_, label) = app.pending_account_add.take().unwrap();
    match kiss_ai::auth::accounts::finish_add(provider, label.as_deref()) {
        Ok(Some(added)) => app
            .cells
            .push(Cell::Notice(kiss_ai::auth::accounts::added_message(
                provider, &added,
            ))),
        Ok(None) => app.cells.push(Cell::Notice(format!(
            "{provider}: that account is already in the pool"
        ))),
        Err(error) => app.cells.push(Cell::Error(format!(
            "could not add the {provider} account: {error:#}"
        ))),
    }
}

fn cancel_account_add(app: &mut App) {
    if let Some((provider, _)) = app.pending_account_add.take()
        && let Err(error) = kiss_ai::auth::accounts::cancel_owned_add(&provider)
    {
        app.cells.push(Cell::Error(format!(
            "could not cancel the {provider} account add: {error:#}"
        )));
    }
}

/// Short human duration such as `45s`, `12m`, or `2h13m`.
fn format_duration_secs(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds.div_ceil(60)),
        _ => match (seconds / 3600, (seconds % 3600) / 60) {
            (hours, 0) => format!("{hours}h"),
            (hours, minutes) => format!("{hours}h{minutes:02}m"),
        },
    }
}

fn handle_command_event(
    app: &mut App,
    event: CommandEvent,
    file_search: &mut FileSearchService,
    session: &Arc<kiss_coding::AgentSession>,
) {
    match event {
        CommandEvent::ForCommand { id, event } => {
            if app.command.as_ref().is_some_and(|command| command.id == id) {
                handle_command_event(app, *event, file_search, session);
            }
        }
        CommandEvent::CommandFinished => app.command = None,
        CommandEvent::DesktopNotificationFinished(result) => {
            finish_notification(app, result);
        }
        CommandEvent::VoicePreview { generation, text } => {
            if generation == app.voice_generation
                && let VoiceState::Recording { preview, .. }
                | VoiceState::Transcribing { preview, .. } = &mut app.voice
            {
                *preview = text;
            }
        }
        CommandEvent::VoiceFinished { generation, result } => {
            if generation == app.voice_generation {
                app.voice = VoiceState::Idle;
                match result {
                    Ok(text) if !text.is_empty() => {
                        app.editor.insert(&text);
                        app.cells
                            .push(Cell::Notice("voice transcript inserted into draft".into()));
                    }
                    Ok(_) => app.cells.push(Cell::Notice("no speech detected".into())),
                    Err(error) => app.cells.push(Cell::Error(format!("voice: {error}"))),
                }
            }
        }
        CommandEvent::BrowserLoginUrl { url, opened } => {
            let message = if opened {
                "finish authentication in the browser".to_string()
            } else {
                format!("the browser did not open. Open this URL: {url}")
            };
            app.cells.push(Cell::Notice(message));
        }
        CommandEvent::BrowserLoginFinished { provider, result } => {
            app.command = None;
            match result {
                Ok(()) => {
                    credentials_changed(app, &provider);
                    app.cells
                        .push(Cell::Notice(format!("logged in to {provider}")));
                    complete_account_add(app, &provider, true);
                }
                Err(error) => {
                    complete_account_add(app, &provider, false);
                    app.cells
                        .push(Cell::Error(format!("login failed for {provider}: {error}")))
                }
            }
        }
        CommandEvent::DeviceLoginNotice {
            provider,
            url,
            code,
            opened,
        } => {
            set_command_status(app, format!("waiting for {provider} device approval"));
            app.cells.push(Cell::Notice(format!(
                "{} {url} and enter code {code}",
                if opened { "Use" } else { "Open" }
            )));
        }
        CommandEvent::ModelCatalog {
            registry,
            warnings,
            pattern,
        } => {
            for warning in &warnings {
                app.cells.push(Cell::Notice(format!(
                    "{warning}. Check your connection or sign in again with /login, then run /model again"
                )));
            }
            if let Some((model, thinking)) = pattern
                .as_deref()
                .filter(|pattern| !kiss_ai::Registry::is_dynamic(pattern))
                .and_then(|pattern| registry.resolve(pattern, Some(&session.model().provider)))
            {
                session.set_model(model);
                if let Some(thinking) = thinking {
                    session.set_thinking_level(thinking);
                    update_thinking_border(app, session.thinking_level());
                }
            } else {
                open_model_picker_with_filter(app, &registry, pattern.as_deref());
            }
            if warnings.is_empty() {
                app.model_catalog = Some(registry);
            }
        }
        CommandEvent::LlamaModels(result) => {
            app.command = None;
            match result {
                Ok(models) if models.is_empty() => app
                    .cells
                    .push(Cell::Notice("the llama.cpp router has no models".into())),
                Ok(models) => open_llama_picker(app, models),
                Err(error) => app.cells.push(Cell::Error(error)),
            }
        }
        CommandEvent::LlamaActionFinished(result) => {
            app.command = None;
            match result {
                Ok(message) => app.cells.push(Cell::Notice(message)),
                Err(error) => app.cells.push(Cell::Error(error)),
            }
        }
        CommandEvent::ShellOutput(chunk) => {
            if let Some(Cell::BashExecution { output, .. }) = app
                .cells
                .iter_mut()
                .rev()
                .find(|cell| matches!(cell, Cell::BashExecution { .. }))
            {
                output.push_str(&chunk);
                // While it runs, show the tail, as the finished result keeps.
                if output.len() > kiss_sdk::shell::MAX_INLINE_BYTES {
                    let start =
                        output.ceil_char_boundary(output.len() - kiss_sdk::shell::MAX_INLINE_BYTES);
                    output.drain(..start);
                }
            }
        }
        CommandEvent::ShellFinished(result) => {
            reset_file_search(app, file_search);
            file_search.refresh_all();
            refresh_git_branch(app, session);
            app.command = None;
            // Show what the session recorded, including where the full
            // output was saved when it was long.
            if let Ok(result) = &result
                && let Some(Cell::BashExecution { output, .. }) = app
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| matches!(cell, Cell::BashExecution { .. }))
            {
                output.clone_from(&result.output);
            }
            match result {
                Ok(result) if result.cancelled => {}
                Ok(result) if result.exit_code != Some(0) => {
                    app.cells.push(Cell::Notice(format!(
                        "shell command exited with {}",
                        result
                            .exit_code
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "no status".into())
                    )));
                }
                Ok(_) => {}
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("shell failed: {error}"))),
            }
        }
        CommandEvent::ShareFinished(result) => {
            app.command = None;
            match result {
                Ok(url) => app
                    .cells
                    .push(Cell::Notice(format!("shared session: {url}"))),
                Err(error) => app.cells.push(Cell::Error(error)),
            }
        }
        CommandEvent::UpdateFinished(result) => {
            app.command = None;
            match result {
                Ok(message) => app.cells.push(Cell::Notice(message)),
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("update failed: {error}"))),
            }
        }
        CommandEvent::TreeNavigationFinished(result) => {
            app.command = None;
            match result {
                Ok(outcome) if outcome.cancelled => {}
                Ok(outcome) => {
                    if let Some(text) = outcome.editor_text
                        && app.editor.is_empty()
                    {
                        app.editor.set_text(&text);
                    }
                    app.cells.push(Cell::Notice(if outcome.summarized {
                        "summarized the old branch and moved to the selected entry".into()
                    } else {
                        "moved to the selected entry".into()
                    }));
                }
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("tree navigation failed: {error}"))),
            }
        }
        CommandEvent::BtwFinished { request_id, result } => {
            let Some(panel) = app.btw_panel.as_mut() else {
                return;
            };
            if panel.request_id != request_id {
                return;
            }
            panel.state = match result {
                Ok(response) => BtwState::Answered(response.text),
                Err(error) => BtwState::Failed(error),
            };
        }
        CommandEvent::RecapFinished {
            request_id,
            automatic,
            result,
        } => {
            if app
                .recap_job
                .take_if(|job| job.request_id == request_id)
                .is_none()
            {
                return;
            }
            match result {
                Ok(response) => app.recap = Some(normalize_recap(&response.text)),
                Err(error) if automatic => app
                    .cells
                    .push(Cell::Notice(format!("automatic recap failed: {error}"))),
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("recap failed: {error}"))),
            }
        }
        CommandEvent::SessionTitleFinished { session_id, result } => {
            let mut manager = session.manager.lock().unwrap();
            if manager.session_id() != session_id {
                return;
            }
            app.title_generation_pending = false;
            if let Ok(title) = result
                && manager.session_name().is_none()
                && manager.append_session_info(&title).is_ok()
            {
                drop(manager);
                set_session_title(app, Some(title));
            }
        }
        CommandEvent::McpServerChecked(server) => {
            let server = *server;
            // A check that was running when the user disabled the server
            // must not bring it back.
            if let Some(current) = app
                .mcp_servers
                .iter_mut()
                .find(|current| current.name == server.name)
                && current.state != McpPanelState::Disabled
            {
                *current = server;
            }
            if matches!(
                app.picker.as_ref().map(|picker| &picker.kind),
                Some(PickerKind::McpServers)
            ) {
                show_mcp_server_picker(app);
            }
        }
        CommandEvent::McpLoginUrl { name, url, opened } => {
            app.cells.push(Cell::Notice(if opened {
                format!("finish authentication for MCP server {name} in the browser")
            } else {
                format!("the browser did not open. Open this URL for MCP server {name}: {url}")
            }));
        }
        CommandEvent::McpActionFinished {
            name,
            action,
            result,
        } => {
            app.command = None;
            match result {
                Ok(message) => app.cells.push(Cell::Notice(message)),
                Err(error) => app.cells.push(Cell::Error(format!(
                    "MCP server {name} {action} failed: {error}"
                ))),
            }
        }
        CommandEvent::WebMcpFinished(result) => {
            app.command = None;
            match result {
                Ok(WebMcpCommandResult::Connected(manager))
                    if !manager.same_as(&app.webmcp_manager) =>
                {
                    tokio::spawn(async move { manager.disconnect().await });
                }
                Ok(WebMcpCommandResult::Connected(manager)) => {
                    session.install_session_tool(manager.agent_tool());
                    app.cells.push(Cell::Notice(
                        "WebMCP connected to Chrome. Tool discovery continues in the background. Run /webmcp list to see active tools. Page tool metadata and results are untrusted."
                            .into(),
                    ));
                }
                Ok(WebMcpCommandResult::Listed(tools)) if tools.is_empty() => {
                    app.cells.push(Cell::Notice(
                        "WebMCP is connected, but no allowed page tools are active".into(),
                    ));
                }
                Ok(WebMcpCommandResult::Listed(tools)) => {
                    let text = tools
                        .into_iter()
                        .map(|tool| format!("{}  {}  ({})", tool.origin, tool.name, tool.title))
                        .collect::<Vec<_>>()
                        .join("\n");
                    app.cells.push(Cell::Notice(format!(
                        "WebMCP tools (untrusted page metadata):\n{text}"
                    )));
                }
                Ok(WebMcpCommandResult::Disconnected) => {
                    session.remove_session_tool(kiss_webmcp::TOOL_NAME);
                    app.cells.push(Cell::Notice("WebMCP disconnected".into()));
                }
                Err(error) => {
                    session.remove_session_tool(kiss_webmcp::TOOL_NAME);
                    app.cells
                        .push(Cell::Error(format!("WebMCP failed: {error}")));
                }
            }
        }
        CommandEvent::WorkflowApproval { plan, reply } => {
            if reply.is_closed() {
                return;
            }
            // The progress view would hide the question, so close it first.
            app.workflow_view = None;
            open_workflow_approval(app, plan, reply);
        }
        CommandEvent::WorkflowFinished { name, result } => {
            app.command = None;
            app.workflow_view = None;
            match result {
                Ok(report) => {
                    app.cells.push(Cell::Notice(format!(
                        "workflow {name} finished\n\n{report}"
                    )));
                    // The next turn can act on the report without the user
                    // pasting it back.
                    session.add_for_next_turn(AgentMessage::user(format!(
                        "The /{name} workflow finished and reported:\n\n{report}"
                    )));
                }
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("workflow {name} failed: {error}"))),
            }
        }
        CommandEvent::WorkflowStarted(record) => {
            set_command_status(app, format!("running workflow {}", record.name));
            app.workflow_view = Some(WorkflowView::new(record));
        }
    }
}

fn command_items(
    prompt_templates: &[kiss_coding::prompts::PromptTemplate],
    skills: &[kiss_coding::skills::Skill],
    workflows: &[kiss_coding::workflows::SavedWorkflow],
) -> (Vec<SelectItem>, Vec<CompletionReplacement>) {
    let mut replacements = Vec::new();
    let mut items: Vec<SelectItem> = slash_commands::commands()
        .enumerate()
        .map(|(value, command)| {
            replacements.push(CompletionReplacement::CommandName(command.name.to_string()));
            SelectItem {
                label: command.name.to_string(),
                detail: Some(match command.argument_hint {
                    Some(hint) => format!("{hint}  {}", command.description),
                    None => command.description.to_string(),
                }),
                value,
            }
        })
        .collect();
    for template in prompt_templates {
        let value = items.len();
        let detail = if template.description.is_empty() {
            "Prompt template".to_string()
        } else {
            format!("Prompt: {}", template.description)
        };
        items.push(SelectItem {
            label: template.name.clone(),
            detail: Some(detail),
            value,
        });
        replacements.push(CompletionReplacement::CommandName(template.name.clone()));
    }
    let mut used_names = items
        .iter()
        .map(|item| item.label.clone())
        .collect::<HashSet<_>>();
    for workflow in workflows {
        if !used_names.insert(workflow.name.clone()) {
            continue;
        }
        let value = items.len();
        items.push(SelectItem {
            label: workflow.name.clone(),
            detail: Some(if workflow.description.is_empty() {
                "Saved workflow".into()
            } else {
                format!("Workflow: {}", workflow.description)
            }),
            value,
        });
        replacements.push(CompletionReplacement::CommandName(workflow.name.clone()));
    }
    for skill in skills {
        if !used_names.insert(skill.name.clone()) {
            continue;
        }
        let value = items.len();
        items.push(SelectItem {
            label: skill.name.clone(),
            detail: Some(format!("Skill: {}", skill.description)),
            value,
        });
        replacements.push(CompletionReplacement::Skill {
            prefix: format!("/{}", skill.name),
            sigil: '/',
            name: skill.name.clone(),
        });
    }
    (items, replacements)
}

fn command_query(text: &str) -> Option<&str> {
    // Submission trims the text, so leading whitespace still runs a command.
    let text = text.trim_start();
    let query = text.strip_prefix('/')?;
    if text.contains('\n') || query.chars().any(char::is_whitespace) {
        return None;
    }
    Some(query)
}

fn command_argument_items(
    session: &Arc<kiss_coding::AgentSession>,
    command: &str,
    query: &str,
) -> Option<(Vec<SelectItem>, Vec<CompletionReplacement>)> {
    let mut candidates: Vec<(String, Option<String>, String, String)> = match command {
        "model" => {
            let access = ModelAccess::current();
            session
                .registry
                .all()
                .iter()
                .filter(|model| access.allows(model))
                .map(|model| {
                    let replacement = format!("{}/{}", model.provider, model.id);
                    (
                        model.id.clone(),
                        Some(model.provider.clone()),
                        replacement.clone(),
                        format!("{replacement} {}", model.display_name()),
                    )
                })
                .collect()
        }
        "thinking" if session.model().reasoning => session
            .model()
            .supported_thinking_levels()
            .iter()
            .map(|level| {
                let value = level.as_str().to_string();
                (value.clone(), None, value.clone(), value)
            })
            .collect(),
        "thinking" => Vec::new(),
        "login" => login_providers()
            .map(|provider| {
                (
                    provider.to_string(),
                    Some("provider authentication".into()),
                    provider.to_string(),
                    provider.to_string(),
                )
            })
            .collect(),
        "webmcp" => [
            ("connect", "Connect to Chrome"),
            ("list", "Show active page tools"),
            ("disconnect", "Disconnect from Chrome"),
        ]
        .into_iter()
        .map(|(value, detail)| {
            (
                value.to_string(),
                Some(detail.to_string()),
                value.to_string(),
                value.to_string(),
            )
        })
        .collect(),
        _ => return None,
    };

    let prepared = kiss_tui::fuzzy::PreparedFuzzyQuery::new(query);
    let mut ranked = candidates
        .drain(..)
        .filter_map(|candidate| prepared.score(&candidate.3).map(|score| (candidate, score)))
        .collect::<Vec<_>>();
    ranked.sort_by(|(left, left_score), (right, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut replacements = Vec::with_capacity(ranked.len());
    let items = ranked
        .into_iter()
        .enumerate()
        .map(|(value, ((label, detail, replacement, _), _))| {
            replacements.push(CompletionReplacement::CommandArgument {
                command: command.to_string(),
                value: replacement,
            });
            SelectItem {
                label,
                detail,
                value,
            }
        })
        .collect();
    Some((items, replacements))
}

fn skill_token_query(
    editor: &Editor,
    skills: &[kiss_coding::skills::Skill],
) -> Option<(String, char, String)> {
    let before = editor.current_line_before_cursor();
    let token_start = before
        .char_indices()
        .rev()
        .find_map(|(index, character)| character.is_whitespace().then_some(index + 1))
        .unwrap_or(0);
    let token = &before[token_start..];
    let sigil = match token.chars().next()? {
        '$' => '$',
        '/' if token_start > 0 => '/',
        _ => return None,
    };
    let preceding = before[..token_start].trim();
    if sigil == '/'
        && !preceding.is_empty()
        && !kiss_coding::skills::parse_invocation(preceding, skills)
            .is_some_and(|invocation| invocation.request.is_empty())
    {
        return None;
    }
    Some((
        token.to_string(),
        sigil,
        token[sigil.len_utf8()..].to_string(),
    ))
}

fn skill_completion(
    prefix: String,
    sigil: char,
    skills: &[kiss_coding::skills::Skill],
) -> (Vec<SelectItem>, Vec<CompletionReplacement>) {
    let mut replacements = Vec::with_capacity(skills.len());
    let items = skills
        .iter()
        .enumerate()
        .map(|(value, skill)| {
            replacements.push(CompletionReplacement::Skill {
                prefix: prefix.clone(),
                sigil,
                name: skill.name.clone(),
            });
            SelectItem {
                label: skill.name.clone(),
                detail: Some(skill.description.clone()),
                value,
            }
        })
        .collect();
    (items, replacements)
}

fn sync_command_menu(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    prompt_templates: &[kiss_coding::prompts::PromptTemplate],
    skills: &[kiss_coding::skills::Skill],
    workflows: &[kiss_coding::workflows::SavedWorkflow],
) {
    let text = app.editor.text();
    if let Some(query) = command_query(&text) {
        let (items, mut replacements) = command_items(prompt_templates, skills, workflows);
        for replacement in &mut replacements {
            if let CompletionReplacement::Skill { prefix, .. } = replacement {
                *prefix = format!("/{query}");
            }
        }
        let mut list = SelectList::new("Commands", items, app.theme.clone());
        list.max_visible = 8;
        list.set_filter(query.to_string());
        app.command_menu = Some(CommandCompletion { list, replacements });
    } else if let Some((prefix, sigil, query)) = skill_token_query(&app.editor, skills) {
        let (items, replacements) = skill_completion(prefix, sigil, skills);
        let mut list = SelectList::new("Skills", items, app.theme.clone());
        list.max_visible = 8;
        list.set_filter(query);
        app.command_menu = Some(CommandCompletion { list, replacements });
    } else if let Some(text) = text.strip_prefix('/')
        && !text.contains('\n')
        && let Some((command, query)) = text.split_once(' ')
        && let Some((items, replacements)) = command_argument_items(session, command, query)
        && !items.is_empty()
    {
        let mut list = SelectList::new(format!("/{command}"), items, app.theme.clone());
        list.max_visible = 8;
        app.command_menu = Some(CommandCompletion { list, replacements });
    } else {
        app.command_menu = None;
    }
    // `sync_file_menu` runs next and owns the file menu.
}

fn path_token_boundary(character: char) -> bool {
    matches!(character, ' ' | '\t' | '"' | '\'' | '=')
}

fn file_completion_prefix(editor: &Editor) -> Option<String> {
    let text = editor.current_line_before_cursor();
    if let Some(start) = text.rfind("@\"")
        && (start == 0
            || text[..start].chars().next_back().is_some_and(|character| {
                path_token_boundary(character) || matches!(character, '(' | '[' | '{' | '<' | '`')
            }))
        && !text[start + 2..].contains('"')
    {
        return Some(text[start..].to_string());
    }
    let start = text
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            path_token_boundary(character).then_some(index + character.len_utf8())
        })
        .unwrap_or(0);
    let mut token = &text[start..];
    while let Some(first) = token.chars().next() {
        let closer = match first {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            '<' => '>',
            '`' => '`',
            _ => break,
        };
        if token[1..].contains(closer) {
            break;
        }
        token = &token[1..];
    }
    token.starts_with('@').then(|| token.to_string())
}

fn file_completion_label(path: &str) -> String {
    let path = path.trim_end_matches('/');
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// The file menu always draws this many result rows, so the rows below it
/// do not move while the user types and the results change.
const FILE_MENU_ROWS: usize = 8;

fn reset_file_search(app: &mut App, file_search: &mut FileSearchService) {
    file_search.cancel();
    app.file_menu = None;
    app.file_search_request = None;
}

fn sync_file_menu(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    file_search: &mut FileSearchService,
) {
    if app.command_menu.is_some() {
        reset_file_search(app, file_search);
        return;
    }
    let Some(prefix) = file_completion_prefix(&app.editor) else {
        reset_file_search(app, file_search);
        return;
    };
    let cwd = session.manager.lock().unwrap().cwd().to_path_buf();
    let Some(query) = FileSearchQuery::from_prefix(&cwd, dirs::home_dir().as_deref(), &prefix)
    else {
        reset_file_search(app, file_search);
        return;
    };
    let ticket = file_search.search(query);
    app.file_search_request = Some(ticket.request_id);
}

fn apply_file_search_result(app: &mut App, result: FileSearchResult) {
    if app.file_search_request != Some(result.request_id)
        || file_completion_prefix(&app.editor).as_deref() != Some(result.prefix.as_str())
    {
        return;
    }
    app.file_search_request = None;
    // An empty result keeps the menu open with a "No matching files" row, so
    // the menu does not close and open again while the user types a name.
    let items = result
        .values
        .iter()
        .enumerate()
        .map(|(value, candidate)| SelectItem {
            label: format!(
                "{}{}",
                file_completion_label(&candidate.path),
                if candidate.is_directory { "/" } else { "" }
            ),
            detail: Some(candidate.path.trim_end_matches('/').to_string()),
            value,
        })
        .collect();
    let mut list = SelectList::new("", items, app.theme.clone());
    list.max_visible = FILE_MENU_ROWS;
    app.file_menu = Some(FileCompletion {
        prefix: result.prefix,
        list,
        values: result.values,
        index_limited: result.index_limited,
    });
}

fn handle_file_menu_key(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    file_search: &mut FileSearchService,
    key: &KeyEvent,
) -> bool {
    let Some(menu) = app.file_menu.as_mut() else {
        return false;
    };
    // With no matches, only Escape belongs to the menu. Enter still submits
    // and the arrows still move through the prompt history.
    if menu.values.is_empty() && key.key != Key::Escape {
        return false;
    }
    match key.key {
        Key::Escape => {
            reset_file_search(app, file_search);
            true
        }
        Key::Up => {
            menu.list.move_selection(-1);
            true
        }
        Key::Down => {
            menu.list.move_selection(1);
            true
        }
        Key::Tab | Key::Enter if app.file_search_request.is_some() => true,
        Key::Tab | Key::Enter if !key.ctrl && !key.alt && !key.shift => {
            let selected = menu.list.current().map(|item| item.value);
            let prefix = menu.prefix.clone();
            let value = selected
                .and_then(|value| menu.values.get(value))
                .map(|value| {
                    let needs_quotes = value.quoted || value.path.contains(' ');
                    if needs_quotes {
                        if value.is_directory {
                            format!("@\"{}", value.path)
                        } else {
                            format!("@\"{}\" ", value.path)
                        }
                    } else if value.is_directory {
                        format!("@{}", value.path)
                    } else {
                        format!("@{} ", value.path)
                    }
                });
            app.file_menu = None;
            if let Some(value) = value {
                app.editor.replace_prefix_before_cursor(&prefix, &value);
                sync_file_menu(app, session, file_search);
            }
            true
        }
        _ => false,
    }
}

fn handle_command_menu_key(app: &mut App, key: &KeyEvent) -> Option<CommandMenuAction> {
    let menu = app.command_menu.as_mut()?;
    match key.key {
        Key::Escape => {
            app.command_menu = None;
            Some(CommandMenuAction::Handled)
        }
        Key::Up => {
            menu.list.move_selection(-1);
            Some(CommandMenuAction::Handled)
        }
        Key::Down => {
            menu.list.move_selection(1);
            Some(CommandMenuAction::Handled)
        }
        Key::Tab if !key.ctrl && !key.alt && !key.shift => {
            let selected = menu
                .list
                .current()
                .and_then(|item| menu.replacements.get(item.value))
                .cloned();
            if let Some(selected) = selected {
                apply_completion(&mut app.editor, &selected);
                app.command_menu = None;
            }
            Some(CommandMenuAction::Handled)
        }
        Key::Enter if !key.ctrl && !key.alt && !key.shift => {
            let selected = menu
                .list
                .current()
                .and_then(|item| menu.replacements.get(item.value))
                .cloned();
            let is_skill = selected
                .as_ref()
                .is_some_and(|selected| matches!(selected, CompletionReplacement::Skill { .. }));
            if let Some(selected) = selected {
                apply_completion(&mut app.editor, &selected);
            }
            app.command_menu = None;
            if is_skill {
                Some(CommandMenuAction::Handled)
            } else {
                Some(CommandMenuAction::Submit(app.editor.take_submission()))
            }
        }
        _ => None,
    }
}

fn apply_completion(editor: &mut Editor, replacement: &CompletionReplacement) {
    match replacement {
        CompletionReplacement::CommandName(name) => editor.set_text(&format!("/{name} ")),
        CompletionReplacement::CommandArgument { command, value } => {
            editor.set_text(&format!("/{command} {value}"));
        }
        CompletionReplacement::Skill {
            prefix,
            sigil,
            name,
        } => {
            editor.replace_prefix_before_cursor(prefix, &format!("{sigil}{name} "));
        }
    }
}

fn slash_invocable_skills(resources: &InteractiveResources) -> Vec<kiss_coding::skills::Skill> {
    let reserved = slash_commands::commands()
        .map(|command| command.name)
        .chain(
            resources
                .prompt_templates
                .iter()
                .map(|template| template.name.as_str()),
        )
        .collect::<HashSet<_>>();
    resources
        .skills
        .iter()
        .filter(|skill| !reserved.contains(skill.name.as_str()))
        .cloned()
        .collect()
}

fn prepare_skill_input(
    submission: &EditorSubmission,
    resources: &InteractiveResources,
) -> Result<Option<String>> {
    let display = submission.display_text.trim();
    let skills = if display.starts_with("/skill:")
        || display
            .split_whitespace()
            .any(|token| token.starts_with('$'))
    {
        resources.skills.clone()
    } else {
        slash_invocable_skills(resources)
    };
    let Some(display_invocation) = kiss_coding::skills::parse_invocation(display, &skills) else {
        return Ok(None);
    };
    let mut model_invocation =
        kiss_coding::skills::parse_invocation(submission.text.trim(), &skills)
            .context("could not match the visible skill invocation to the submitted input")?;
    model_invocation.skill_names = display_invocation.skill_names;
    kiss_coding::skills::expand_invocation(&model_invocation, &skills).map(Some)
}

const USER_DISPLAY_PREFIX: &str = "<!-- kiss-user-display:";
const USER_DISPLAY_SUFFIX: &str = " -->";

fn stored_user_text(display_text: &str, model_text: &str) -> String {
    if display_text == model_text {
        return model_text.to_string();
    }
    use base64::Engine as _;
    let display = base64::engine::general_purpose::STANDARD_NO_PAD.encode(display_text);
    format!("{USER_DISPLAY_PREFIX}{display}{USER_DISPLAY_SUFFIX}\n{model_text}")
}

fn visible_user_text(stored_text: &str) -> String {
    use base64::Engine as _;
    let Some((header, _)) = stored_text.split_once('\n') else {
        return stored_text.to_string();
    };
    let Some(encoded) = header
        .strip_prefix(USER_DISPLAY_PREFIX)
        .and_then(|value| value.strip_suffix(USER_DISPLAY_SUFFIX))
    else {
        return stored_text.to_string();
    };
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| stored_text.to_string())
}

fn voice_space(event: &InputEvent) -> Option<bool> {
    match event {
        InputEvent::Key(key) if *key == KeyEvent::char(' ') => Some(true),
        InputEvent::KeyRelease(key) if *key == KeyEvent::char(' ') => Some(false),
        _ => None,
    }
}

fn stop_voice(app: &mut App) {
    if let VoiceState::Recording { recording, preview } =
        std::mem::replace(&mut app.voice, VoiceState::Idle)
    {
        recording.stop();
        app.voice = VoiceState::Transcribing {
            _recording: recording,
            preview,
        };
        app.cells.push(Cell::Notice("transcribing voice…".into()));
    }
}

/// Drop the current capture and any late events from it.
fn reset_voice(app: &mut App) {
    app.voice = VoiceState::Idle;
    app.voice_generation = app.voice_generation.wrapping_add(1);
}

fn voice_input(
    app: &mut App,
    event: &InputEvent,
    backend: crate::voice::Backend,
    language: &str,
    tx: &mpsc::UnboundedSender<CommandEvent>,
) -> bool {
    if app.voice_mode == VoiceMode::Off {
        return false;
    }
    if matches!(event, InputEvent::KeyRepeat(key) if *key == KeyEvent::char(' ')) {
        // Holding Space for a capture repeats it; the repeats are part of
        // the hold. Without a capture they type spaces.
        return !matches!(app.voice, VoiceState::Idle);
    }
    let Some(pressed) = voice_space(event) else {
        return false;
    };
    match app.voice {
        VoiceState::Transcribing { .. } => return true,
        VoiceState::Recording { .. } => {
            if pressed == (app.voice_mode == VoiceMode::Tap) {
                stop_voice(app);
            }
            return true;
        }
        VoiceState::Idle => {}
    }
    if app.input_layer() != InputLayer::Editor {
        return false;
    }
    if !pressed {
        return true;
    }
    // Allow /voice off, /config, and other slash commands to be typed normally.
    if app.editor.text().starts_with('/') {
        return false;
    }
    match crate::voice::start(backend, language.to_owned()) {
        Ok((recording, mut events)) => {
            let generation = app.voice_generation;
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    let command = match event {
                        crate::voice::Event::Preview(text) => {
                            CommandEvent::VoicePreview { generation, text }
                        }
                        crate::voice::Event::Finished(result) => {
                            CommandEvent::VoiceFinished { generation, result }
                        }
                    };
                    if tx.send(command).is_err() {
                        break;
                    }
                }
            });
            app.voice = VoiceState::Recording {
                recording,
                preview: String::new(),
            };
            app.cells.push(Cell::Notice(format!(
                "recording voice ({})… (Esc cancels)",
                backend.name()
            )));
        }
        Err(error) => app
            .cells
            .push(Cell::Error(format!("could not record voice: {error:#}"))),
    }
    true
}

fn handle_input(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    event: &InputEvent,
    args: &Args,
    resources: &mut InteractiveResources,
    file_search: &mut FileSearchService,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) -> Flow {
    app.notifications.input(event);
    if matches!(event, InputEvent::Focus(_)) {
        return Flow::Continue;
    }
    note_user_activity(app);

    if !matches!(app.voice, VoiceState::Idle)
        && matches!(event, InputEvent::Key(key) if key.key == Key::Escape || *key == KeyEvent::ctrl('c'))
    {
        reset_voice(app);
        app.cells
            .push(Cell::Notice("voice recording cancelled".into()));
        return Flow::Continue;
    }
    if voice_input(
        app,
        event,
        crate::voice::Backend::from_settings(&resources.settings),
        crate::voice::language(&resources.settings),
        command_tx,
    ) {
        return Flow::Continue;
    }
    if matches!(event, InputEvent::KeyRelease(_)) {
        return Flow::Continue;
    }
    let repeated;
    let event = match event {
        InputEvent::KeyRepeat(key) => {
            repeated = InputEvent::Key(key.clone());
            &repeated
        }
        event => event,
    };

    match app.input_layer() {
        InputLayer::Btw => {
            if let InputEvent::Key(key) = event
                && matches!(key.key, Key::Escape | Key::Enter)
                && let Some(BtwPanel {
                    state: BtwState::Answering(cancel),
                    ..
                }) = app.btw_panel.take()
            {
                cancel.cancel();
            }
            return Flow::Continue;
        }
        InputLayer::SecretPrompt => {
            let flow = handle_secret_prompt(app, session, event, command_tx);
            // An account add waits only while its login runs: a prompt is
            // still open or a login command works. Any other way out of the
            // prompt, such as input it rejected, ends the add.
            if app.secret_prompt.is_none()
                && !app
                    .command
                    .as_ref()
                    .is_some_and(|command| matches!(command.control, CommandControl::LoggingIn(_)))
            {
                cancel_account_add(app);
            }
            return flow;
        }
        // The progress view owns the keyboard while it is open, so that `x`,
        // `p`, and the arrow keys steer the run rather than the editor.
        InputLayer::WorkflowView => {
            if let InputEvent::Key(key) = event
                && let Some(view) = &mut app.workflow_view
            {
                match view.handle_key(key) {
                    ViewAction::None => {}
                    ViewAction::Close => app.workflow_view = None,
                    ViewAction::Save => {
                        let run = view.run.id;
                        open_workflow_save_picker(app, run);
                    }
                }
            }
            return Flow::Continue;
        }
        InputLayer::JobView => {
            if let InputEvent::Key(key) = event
                && let Some(view) = &mut app.job_view
                && view.handle_key(key) == JobViewAction::Close
            {
                app.job_view = None;
            }
            return Flow::Continue;
        }
        InputLayer::SessionPicker => {
            if let InputEvent::Key(key) = event {
                handle_session_picker_key(app, session, key);
            }
            return Flow::Continue;
        }
        // Picker input intercepts everything.
        InputLayer::Picker => {
            if let InputEvent::Key(key) = event {
                return handle_picker_key(app, session, key, resources, command_tx);
            }
            return Flow::Continue;
        }
        InputLayer::Editor => {}
    }

    if let InputEvent::Key(key) = event
        && handle_file_menu_key(app, session, file_search, key)
    {
        return Flow::Continue;
    }

    if let InputEvent::Key(key) = event
        && let Some(action) = handle_command_menu_key(app, key)
    {
        return match action {
            CommandMenuAction::Handled => Flow::Continue,
            CommandMenuAction::Submit(submission) => {
                let command = submission.text.trim().strip_prefix('/').unwrap_or("");
                if command.is_empty() {
                    Flow::Continue
                } else {
                    run_slash_command(app, session, command, args, resources, command_tx)
                }
            }
        };
    }

    if let InputEvent::Key(key) = event
        && key.key == Key::Escape
        && app.file_search_request.is_some()
    {
        reset_file_search(app, file_search);
        return Flow::Continue;
    }

    if let InputEvent::Key(key) = event {
        // Ctrl+C interrupts active work. At idle it clears once and exits on
        // a second press, matching Pi's editor behavior.
        if *key == KeyEvent::ctrl('c') {
            if cancel_command(app) {
                return Flow::Continue;
            }
            if session.is_running() {
                abort_active(app, session);
                app.ctrl_c_armed = false;
                return Flow::Continue;
            }
            if app.ctrl_c_armed {
                return Flow::Quit;
            }
            app.ctrl_c_armed = true;
            app.editor.clear();
            app.command_menu = None;
            reset_file_search(app, file_search);
            return Flow::Continue;
        }
        app.ctrl_c_armed = false;

        if matches!(app.keybindings.action_for(key), Some(Action::Quit)) {
            if app.editor.is_empty() {
                return Flow::Quit;
            }
            app.editor.delete_forward();
            sync_command_menu(
                app,
                session,
                &resources.prompt_templates,
                &resources.skills,
                &resources.saved_workflows,
            );
            sync_file_menu(app, session, file_search);
            return Flow::Continue;
        }

        if key.key == Key::Escape {
            if cancel_command(app) {
                return Flow::Continue;
            }
            if session.is_running() {
                abort_active(app, session);
                return Flow::Continue;
            }
            if app.escape_armed {
                app.escape_armed = false;
                open_tree_picker(app, session);
                return Flow::Continue;
            }
            app.escape_armed = true;
            return Flow::Continue;
        }
        app.escape_armed = false;

        match app.keybindings.action_for(key) {
            Some(Action::Newline) => {
                app.editor.newline();
                app.command_menu = None;
                reset_file_search(app, file_search);
                return Flow::Continue;
            }
            Some(Action::CycleModel) => {
                cycle_model(session, &resources.enabled_models, 1);
                return Flow::Continue;
            }
            Some(Action::CycleModelBackward) => {
                cycle_model(session, &resources.enabled_models, -1);
                return Flow::Continue;
            }
            Some(Action::SelectModel) => {
                open_model_picker(app, session, None, command_tx);
                return Flow::Continue;
            }
            Some(Action::CycleThinking) => {
                if !session.model().reasoning {
                    app.cells
                        .push(Cell::Notice("this model has no thinking levels".into()));
                    return Flow::Continue;
                }
                session.set_thinking_level(
                    session
                        .model()
                        .next_thinking_level(session.thinking_level()),
                );
                update_thinking_border(app, session.thinking_level());
                return Flow::Continue;
            }
            Some(Action::ToggleThinking) => {
                app.hide_thinking.toggle();
                return Flow::Continue;
            }
            Some(Action::ExpandTools) => {
                app.expand_tools.toggle();
                return Flow::Continue;
            }
            Some(Action::QueueFollowUp) => {
                let submission = app.editor.take_submission();
                app.command_menu = None;
                reset_file_search(app, file_search);
                if !submission.text.trim().is_empty() {
                    let model_text = match prepare_skill_input(&submission, resources) {
                        Ok(Some(text)) => text,
                        Ok(None) => submission.text.trim().to_string(),
                        Err(error) => {
                            app.cells
                                .push(Cell::Error(format!("could not invoke skill: {error:#}")));
                            return Flow::Continue;
                        }
                    };
                    let display_text = submission.display_text.trim().to_string();
                    deliver_prompt(
                        app,
                        session,
                        display_text,
                        model_text,
                        PromptMode::Ordinary,
                        QueueKind::FollowUp,
                        command_tx,
                    );
                }
                return Flow::Continue;
            }
            Some(Action::Dequeue) => {
                restore_queued_to_editor(app, session);
                return Flow::Continue;
            }
            Some(Action::CopyLastResponse) => {
                copy_last_response(app);
                return Flow::Continue;
            }
            _ => {}
        }
    }

    // Enter -> submit or queue steering.
    if let Some(submission) = app.editor.handle_event(event) {
        app.command_menu = None;
        reset_file_search(app, file_search);
        let text = submission.text.trim().to_string();
        let display_text = submission.display_text.trim().to_string();
        if text.is_empty() {
            return Flow::Continue;
        }
        match prepare_skill_input(&submission, resources) {
            Ok(Some(model_text)) => {
                deliver_prompt(
                    app,
                    session,
                    display_text,
                    model_text,
                    PromptMode::Ordinary,
                    QueueKind::Steering,
                    command_tx,
                );
                return Flow::Continue;
            }
            Ok(None) => {}
            Err(error) => {
                app.cells
                    .push(Cell::Error(format!("could not invoke skill: {error:#}")));
                return Flow::Continue;
            }
        }
        if let Some(command) = text.strip_prefix('/') {
            return run_slash_command(app, session, command, args, resources, command_tx);
        }
        if let Some(shell) = text.strip_prefix('!') {
            if shell.trim_start_matches('!').trim().is_empty() {
                deliver_prompt(
                    app,
                    session,
                    display_text,
                    text,
                    PromptMode::Ordinary,
                    QueueKind::Steering,
                    command_tx,
                );
            } else {
                start_shell_passthrough(app, session, shell.to_string(), command_tx);
            }
            return Flow::Continue;
        }
        let prompt_mode = session.prompt_mode_for(&text);
        deliver_prompt(
            app,
            session,
            display_text,
            text,
            prompt_mode,
            QueueKind::Steering,
            command_tx,
        );
    } else {
        sync_command_menu(
            app,
            session,
            &resources.prompt_templates,
            &resources.skills,
            &resources.saved_workflows,
        );
        sync_file_menu(app, session, file_search);
    }
    Flow::Continue
}

fn handle_secret_prompt(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    event: &InputEvent,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) -> Flow {
    match event {
        InputEvent::KeyRelease(_) | InputEvent::Focus(_) => {}
        InputEvent::Paste(text) => {
            if let Some(prompt) = &mut app.secret_prompt {
                prompt.value.push_str(text.trim());
            }
        }
        InputEvent::Key(key) | InputEvent::KeyRepeat(key) => match key.key {
            Key::Escape => {
                let login = app
                    .secret_prompt
                    .as_ref()
                    .is_some_and(|prompt| prompt.kind.is_login());
                app.secret_prompt = None;
                app.cells.push(Cell::Notice(if login {
                    "login cancelled".into()
                } else {
                    "input cancelled".into()
                }));
            }
            Key::Enter => {
                let Some(prompt) = app.secret_prompt.take() else {
                    return Flow::Continue;
                };
                let key = prompt.value.trim();
                if key.is_empty()
                    && !matches!(&prompt.kind, SecretPromptKind::GitHubEnterpriseDomain)
                    && !matches!(&prompt.kind, SecretPromptKind::TreeLabel(_))
                {
                    let message = match &prompt.kind {
                        SecretPromptKind::BranchSummary(_) => {
                            "summary instructions cannot be empty"
                        }
                        SecretPromptKind::WorkflowName(..) => "workflow name cannot be empty",
                        _ => "authentication input cannot be empty",
                    };
                    app.cells.push(Cell::Error(message.into()));
                } else {
                    match prompt.kind {
                        SecretPromptKind::ApiKey(provider) => {
                            match kiss_ai::auth::store_api_key(&provider, key) {
                                Ok(()) => {
                                    credentials_changed(app, &provider);
                                    app.cells.push(Cell::Notice(format!(
                                        "saved API key for {provider}"
                                    )));
                                    complete_account_add(app, &provider, true);
                                }
                                Err(error) => {
                                    complete_account_add(app, &provider, false);
                                    app.cells.push(Cell::Error(format!(
                                        "could not save API key: {error:#}"
                                    )))
                                }
                            }
                        }
                        SecretPromptKind::ProviderConfig(provider) => {
                            let parts = key.split('|').map(str::trim).collect::<Vec<_>>();
                            let configured = match provider.as_str() {
                                "google-vertex" if parts.len() == 3 => {
                                    let mut env = std::collections::BTreeMap::new();
                                    env.insert("GOOGLE_CLOUD_PROJECT".into(), parts[1].into());
                                    env.insert("GOOGLE_CLOUD_LOCATION".into(), parts[2].into());
                                    kiss_ai::auth::store_api_key_with_env(&provider, parts[0], env)
                                }
                                "cloudflare-workers-ai" if parts.len() == 2 => {
                                    let mut env = std::collections::BTreeMap::new();
                                    env.insert("CLOUDFLARE_ACCOUNT_ID".into(), parts[1].into());
                                    kiss_ai::auth::store_api_key_with_env(&provider, parts[0], env)
                                }
                                "cloudflare-ai-gateway" if parts.len() == 3 => {
                                    let mut env = std::collections::BTreeMap::new();
                                    env.insert("CLOUDFLARE_ACCOUNT_ID".into(), parts[1].into());
                                    env.insert("CLOUDFLARE_GATEWAY_ID".into(), parts[2].into());
                                    kiss_ai::auth::store_api_key_with_env(&provider, parts[0], env)
                                }
                                "databricks-unity-gateway" | "snowflake-cortex"
                                    if parts.len() == 2 =>
                                {
                                    kiss_ai::auth::store_gateway_credential(
                                        &provider, parts[0], parts[1],
                                    )
                                }
                                "databricks-unity-gateway" => Err(anyhow::anyhow!(
                                    "Databricks login needs TOKEN|WORKSPACE_URL. Enter a non-empty bearer token and an HTTP or HTTPS workspace root"
                                )),
                                "snowflake-cortex" => Err(anyhow::anyhow!(
                                    "Snowflake login needs PAT|ACCOUNT_OR_GATEWAY_URL. Enter a non-empty PAT and an HTTP or HTTPS account, Cortex, or AI Gateway URL"
                                )),
                                _ => Err(anyhow::anyhow!("invalid value format for {provider}")),
                            };
                            match configured {
                                Ok(()) => {
                                    app.cells.push(Cell::Notice(format!(
                                        "saved authentication for {provider}. Run /reload to apply endpoint settings"
                                    )));
                                    complete_account_add(app, &provider, true);
                                }
                                Err(error) => {
                                    complete_account_add(app, &provider, false);
                                    app.cells.push(Cell::Error(format!(
                                        "could not save authentication: {error:#}"
                                    )))
                                }
                            }
                        }
                        SecretPromptKind::Llama => {
                            let (url, key) = key
                                .split_once('|')
                                .map(|(url, key)| (url.trim(), key.trim()))
                                .unwrap_or((key, "llama.cpp"));
                            let mut env = std::collections::BTreeMap::new();
                            env.insert("LLAMA_BASE_URL".into(), url.to_string());
                            match kiss_ai::auth::store_api_key_with_env("llama.cpp", key, env) {
                                Ok(()) => app
                                    .cells
                                    .push(Cell::Notice("saved llama.cpp settings".into())),
                                Err(error) => app.cells.push(Cell::Error(format!(
                                    "could not save llama.cpp settings: {error:#}"
                                ))),
                            }
                        }
                        SecretPromptKind::AnthropicManual(pending) => {
                            let input = key.to_string();
                            let Some(cancel) = begin_login(app, "finishing Anthropic login".into())
                            else {
                                return Flow::Continue;
                            };
                            let tx = CommandTask::new(app, command_tx);
                            tokio::spawn(async move {
                                let result = kiss_ai::auth::anthropic::finish_authorization(
                                    &Default::default(),
                                    &pending,
                                    &input,
                                    &cancel,
                                )
                                .await
                                .and_then(|credential| {
                                    kiss_ai::auth::store_oauth("anthropic", credential)
                                })
                                .map_err(|error| format!("{error:#}"));
                                let _ = tx.send(CommandEvent::BrowserLoginFinished {
                                    provider: "anthropic".into(),
                                    result,
                                });
                            });
                        }
                        SecretPromptKind::OpenRouterManual(pending) => {
                            let input = key.to_string();
                            let Some(cancel) =
                                begin_login(app, "finishing OpenRouter login".into())
                            else {
                                return Flow::Continue;
                            };
                            let tx = CommandTask::new(app, command_tx);
                            tokio::spawn(async move {
                                let result = kiss_ai::auth::openrouter::finish_authorization(
                                    &Default::default(),
                                    &pending,
                                    &input,
                                    &cancel,
                                )
                                .await
                                .and_then(|credential| {
                                    kiss_ai::auth::store_oauth("openrouter", credential)
                                })
                                .map_err(|error| format!("{error:#}"));
                                let _ = tx.send(CommandEvent::BrowserLoginFinished {
                                    provider: "openrouter".into(),
                                    result,
                                });
                            });
                        }
                        SecretPromptKind::GitHubEnterpriseDomain => {
                            match kiss_ai::auth::github_copilot::normalize_domain(key) {
                                Ok(domain) => start_provider_device_login(
                                    app,
                                    "github-copilot",
                                    command_tx,
                                    Some(domain),
                                ),
                                Err(error) => app.cells.push(Cell::Error(format!(
                                    "invalid GitHub Enterprise domain: {error:#}"
                                ))),
                            }
                        }
                        SecretPromptKind::GoogleApplicationDefault => {
                            let parts = key.split('|').map(str::trim).collect::<Vec<_>>();
                            if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
                                app.cells.push(Cell::Error(
                                    "use PROJECT|LOCATION for Google credentials".into(),
                                ));
                            } else {
                                let mut env = std::collections::BTreeMap::new();
                                env.insert("GOOGLE_CLOUD_PROJECT".into(), parts[0].into());
                                env.insert("GOOGLE_CLOUD_LOCATION".into(), parts[1].into());
                                match kiss_ai::auth::store_api_key_with_env(
                                    "google-vertex",
                                    "google-application-default-credentials",
                                    env,
                                ) {
                                    Ok(()) => app.cells.push(Cell::Notice(
                                        "saved Google Application Default Credentials settings"
                                            .into(),
                                    )),
                                    Err(error) => app.cells.push(Cell::Error(format!(
                                        "could not save Google settings: {error:#}"
                                    ))),
                                }
                            }
                        }
                        SecretPromptKind::AwsProfile => {
                            let parts = key.split('|').map(str::trim).collect::<Vec<_>>();
                            if parts.is_empty() || parts[0].is_empty() || parts.len() > 2 {
                                app.cells
                                    .push(Cell::Error("use PROFILE or PROFILE|REGION".into()));
                            } else {
                                let mut env = std::collections::BTreeMap::new();
                                env.insert("AWS_PROFILE".into(), parts[0].into());
                                if let Some(region) = parts.get(1).filter(|value| !value.is_empty())
                                {
                                    env.insert("AWS_REGION".into(), (*region).into());
                                }
                                match kiss_ai::auth::store_api_key_with_env(
                                    "amazon-bedrock",
                                    "",
                                    env,
                                ) {
                                    Ok(()) => app
                                        .cells
                                        .push(Cell::Notice("saved AWS profile settings".into())),
                                    Err(error) => app.cells.push(Cell::Error(format!(
                                        "could not save AWS profile: {error:#}"
                                    ))),
                                }
                            }
                        }
                        SecretPromptKind::TreeLabel(target_id) => {
                            let label = (!key.is_empty()).then(|| key.to_string());
                            match session
                                .manager
                                .lock()
                                .unwrap()
                                .append_label(&target_id, label)
                            {
                                Ok(_) => app.cells.push(Cell::Notice(if key.is_empty() {
                                    "cleared tree entry label".into()
                                } else {
                                    format!("labeled tree entry: {key}")
                                })),
                                Err(error) => app.cells.push(Cell::Error(format!(
                                    "could not label tree entry: {error:#}"
                                ))),
                            }
                        }
                        SecretPromptKind::BranchSummary(target_id) => {
                            start_tree_navigation(
                                app,
                                session,
                                target_id,
                                true,
                                Some(key.to_string()),
                                command_tx,
                            );
                        }
                        SecretPromptKind::WorkflowName(run, location) => {
                            save_workflow_run(app, session, run, location, key);
                        }
                    }
                }
            }
            _ => {
                if let Some(prompt) = &mut app.secret_prompt {
                    kiss_tui::edit_single_line(&mut prompt.value, key);
                }
            }
        },
    }
    Flow::Continue
}

fn abort_active(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    session.abort();
    restore_queued_to_editor(app, session);
}

fn restore_queued_to_editor(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    let text = session
        .reclaim_queued()
        .into_iter()
        .filter_map(|message| match message {
            AgentMessage::User(user) => Some(visible_user_text(&user.content.as_text())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if !text.is_empty() {
        let draft = app.editor.text();
        if draft.is_empty() {
            app.editor.set_text(&text);
        } else {
            app.editor.set_text(&format!("{draft}\n\n{text}"));
        }
    }
}

fn update_thinking_border(app: &mut App, level: ThinkingLevel) {
    app.editor.border_color_token = if app.editor.text().starts_with('!') {
        "bashMode"
    } else {
        thinking_color_token(level)
    }
    .into();
}

fn thinking_color_token(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "thinkingOff",
        ThinkingLevel::Minimal => "thinkingMinimal",
        ThinkingLevel::Low => "thinkingLow",
        ThinkingLevel::Medium => "thinkingMedium",
        ThinkingLevel::High => "thinkingHigh",
        ThinkingLevel::Xhigh => "thinkingXhigh",
        ThinkingLevel::Max => "thinkingMax",
    }
}

fn copy_last_response(app: &mut App) {
    let text = app.cells.iter().rev().find_map(|cell| match cell {
        Cell::AssistantFinal(text) | Cell::AssistantStreaming(text) => Some(text.clone()),
        _ => None,
    });
    match text {
        Some(text) => {
            copy_text_to_clipboard(&text);
            app.cells
                .push(Cell::Notice("copied last response to clipboard".into()));
        }
        None => app.cells.push(Cell::Notice("nothing to copy".into())),
    }
}

fn copy_text_to_clipboard(text: &str) {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut output = std::io::stdout();
    let _ = write!(output, "\x1b]52;c;{encoded}\x07");
    let _ = output.flush();
}

fn cycle_model(
    session: &Arc<kiss_coding::AgentSession>,
    enabled_models: &[kiss_ai::Model],
    direction: isize,
) {
    let available = session.registry.available_models();
    let source: Vec<&kiss_ai::Model> = if enabled_models.is_empty() {
        available.iter().map(|(_, model)| *model).collect()
    } else {
        enabled_models
            .iter()
            .filter(|model| {
                available.iter().any(|(_, candidate)| {
                    candidate.provider == model.provider && candidate.id == model.id
                })
            })
            .collect()
    };
    let access = ModelAccess::current();
    let models = source
        .into_iter()
        .filter(|model| access.allows(model))
        .collect::<Vec<_>>();
    if models.is_empty() {
        return;
    }
    let current = session.model();
    let position = models
        .iter()
        .position(|model| model.id == current.id && model.provider == current.provider)
        .unwrap_or(0);
    let next = (position as isize + direction).rem_euclid(models.len() as isize) as usize;
    session.set_model(models[next].clone());
}

fn set_session_title(app: &mut App, title: Option<String>) {
    if app.session_title != title {
        app.session_title = title;
        app.terminal_title_dirty = true;
    }
}

/// Reset what belongs to the previous session after the active session changes.
/// What the transcript shows after the session changes.
#[derive(Clone, Copy, PartialEq)]
enum Transcript {
    /// Rebuild it from the new session's history.
    Rebuild,
    /// Start it empty, as after /new or a fork from a message.
    Empty,
}

/// Reset everything that belonged to the previous session after
/// `replace_manager`, `new_session` or a fork: the title, the transcript,
/// the git branch, the thinking border, and any recap or side answer.
fn adopt_session(app: &mut App, session: &Arc<kiss_coding::AgentSession>, transcript: Transcript) {
    let title = session.manager.lock().unwrap().session_name();
    set_session_title(app, title);
    app.title_generation_pending = false;
    app.idle_status = ProgramStatus::Idle;
    app.cells = match transcript {
        Transcript::Rebuild => session_cells(session),
        Transcript::Empty => Vec::new(),
    };
    app.cell_render_cache.clear();
    if let Some(job) = app.recap_job.take() {
        job.cancel.cancel();
    }
    app.recap = None;
    if let Some(BtwPanel {
        state: BtwState::Answering(cancel),
        ..
    }) = app.btw_panel.take()
    {
        cancel.cancel();
    }
    update_thinking_border(app, session.thinking_level());
    refresh_git_branch(app, session);
}

fn maybe_generate_session_title(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    prompt: &str,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    if app.session_title.is_some() || app.title_generation_pending || prompt.trim().is_empty() {
        return;
    }
    app.title_generation_pending = true;
    let session_id = session.manager.lock().unwrap().session_id().to_string();
    let prompt = prompt.to_string();
    let session = session.clone();
    let tx = command_tx.clone();
    tokio::spawn(async move {
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            session.generate_session_title(&prompt, cancel.clone()),
        )
        .await
        .map_err(|_| "session title request timed out".to_string())
        .and_then(|result| result.map_err(|error| error.to_string()));
        cancel.cancel();
        let _ = tx.send(CommandEvent::SessionTitleFinished { session_id, result });
    });
}

/// Report the running command that blocks `action`, if there is one.
fn command_blocks(app: &mut App, action: &str) -> bool {
    let Some(command) = &app.command else {
        return false;
    };
    let notice = format!(
        "{action} did not start because \"{}\" is still running. Wait for it or press Esc to cancel it, then try again",
        command.status
    );
    app.cells.push(Cell::Notice(notice));
    true
}

/// Claim the command slot for a task that accepts cancellation.
fn begin_command(app: &mut App, status: String) -> Option<CancellationToken> {
    if command_blocks(app, &status) {
        return None;
    }
    let cancel = CancellationToken::new();
    app.next_command_id = app.next_command_id.wrapping_add(1);
    app.command = Some(BackgroundCommand {
        id: app.next_command_id,
        status,
        control: CommandControl::Cancellable(cancel.clone()),
    });
    Some(cancel)
}

/// A login keeps its slot until credential storage has finished, even after
/// cancellation. A second login must not race the first credential write.
fn begin_login(app: &mut App, status: String) -> Option<CancellationToken> {
    let cancel = begin_command(app, status)?;
    app.command.as_mut()?.control = CommandControl::LoggingIn(cancel.clone());
    Some(cancel)
}

/// Claim the command slot for a task that cannot stop early.
fn begin_uncancellable_command(app: &mut App, status: String) -> bool {
    if command_blocks(app, &status) {
        return false;
    }
    app.next_command_id = app.next_command_id.wrapping_add(1);
    app.command = Some(BackgroundCommand {
        id: app.next_command_id,
        status,
        control: CommandControl::Uncancellable,
    });
    true
}

fn set_command_status(app: &mut App, status: String) {
    if let Some(command) = &mut app.command
        && !matches!(
            command.control,
            CommandControl::Stopping | CommandControl::StoppingLogin
        )
    {
        command.status = status;
    }
}

/// Handle Esc or Ctrl+C for the running command. Returns false when the key
/// has no effect on it.
fn cancel_command(app: &mut App) -> bool {
    let Some(command) = &mut app.command else {
        return false;
    };
    match &command.control {
        CommandControl::Uncancellable => false,
        CommandControl::Cancellable(cancel) | CommandControl::LoggingIn(cancel) => {
            cancel.cancel();
            command.status = format!("stopping \"{}\"", command.status);
            command.control = if matches!(command.control, CommandControl::LoggingIn(_)) {
                CommandControl::StoppingLogin
            } else {
                CommandControl::Stopping
            };
            app.cells.push(Cell::Notice("command cancelled".into()));
            true
        }
        CommandControl::Stopping => {
            app.command = None;
            app.cells
                .push(Cell::Notice("stopped waiting for the command".into()));
            true
        }
        CommandControl::StoppingLogin => {
            app.cells.push(Cell::Notice(
                "login is stopping. Wait for it to finish before you start another command".into(),
            ));
            true
        }
    }
}

/// Start a prompt, or queue it when the session is busy. A queued prompt gets
/// its transcript cell when the run delivers it.
fn deliver_prompt(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    display_text: String,
    model_text: String,
    prompt_mode: PromptMode,
    kind: QueueKind,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    if prompt_mode == PromptMode::Workflow
        && let Some(trigger) = workflow_trigger(&model_text)
    {
        app.cells.push(Cell::Notice(format!(
            "running this as a dynamic workflow ({trigger}) · /workflows opens the progress view"
        )));
    }
    let stored_text = stored_user_text(&display_text, &model_text);
    if let Some(run) = session.submit(vec![AgentMessage::user(stored_text)], prompt_mode, kind) {
        maybe_generate_session_title(app, session, &display_text, command_tx);
        app.cells.push(Cell::User(display_text));
        tokio::spawn(run);
    }
}

fn start_shell_passthrough(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    command: String,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    // !! excludes output from the model context.
    let (command, exclude) = match command.strip_prefix('!') {
        Some(rest) => (rest.trim().to_string(), true),
        None => (command.trim().to_string(), false),
    };
    let (cwd, session_id) = {
        let manager = session.manager.lock().unwrap();
        (
            manager.cwd().to_path_buf(),
            manager.session_id().to_string(),
        )
    };
    // The same shell as the bash tool and SDK shell commands: shellPath,
    // else bash.
    let settings = session.settings();
    let shell = settings.shell_path;
    let prefix = settings.shell_command_prefix;
    let Some(cancel) = begin_command(app, format!("running !{command}")) else {
        // Keep what the user typed, so it can run after the other command.
        if app.editor.is_empty() {
            let prefix = if exclude { "!!" } else { "!" };
            app.editor.set_text(&format!("{prefix}{command}"));
        }
        return;
    };
    app.cells.push(Cell::BashExecution {
        command: command.clone(),
        output: String::new(),
        exclude_from_context: exclude,
    });
    let tx = CommandTask::new(app, command_tx);
    let session = session.clone();
    tokio::spawn(async move {
        let result = run_shell_command(
            shell.as_deref(),
            prefix.as_deref(),
            &command,
            &cwd,
            exclude,
            &cancel,
            &tx,
        )
        .await;
        if let Ok(result) = &result {
            let message = AgentMessage::BashExecution(kiss_agent::BashExecutionMessage {
                command: result.command.clone(),
                output: result.output.clone(),
                exit_code: result.exit_code,
                cancelled: result.cancelled,
                truncated: result.truncated,
                full_output_path: result.full_output_path.clone(),
                exclude_from_context: result.exclude_from_context,
                timestamp: kiss_ai::now_ms(),
            });
            session.record_user_shell(&session_id, message);
        }
        let _ = tx.send(CommandEvent::ShellFinished(result));
    });
}

/// Run a `!command` with the same runner as SDK shell commands: the
/// configured prefix, group kill on cancel, and the output tail with the
/// full output saved to a file when it is long.
async fn run_shell_command(
    shell: Option<&str>,
    prefix: Option<&str>,
    command: &str,
    cwd: &std::path::Path,
    exclude_from_context: bool,
    cancel: &CancellationToken,
    tx: &CommandTask,
) -> std::result::Result<ShellRunResult, String> {
    let updates = tx.clone();
    let result = kiss_sdk::shell::run(
        command,
        cwd,
        shell,
        prefix,
        cancel.clone(),
        Arc::new(move |chunk| {
            let _ = updates.send(CommandEvent::ShellOutput(chunk));
        }),
    )
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(ShellRunResult {
        command: command.to_string(),
        output: result.output,
        exit_code: result.exit_code,
        cancelled: result.cancelled,
        truncated: result.truncated,
        full_output_path: result.full_output_path,
        exclude_from_context,
    })
}

fn open_tree_picker(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    let manager = session.manager.lock().unwrap();
    let mut items = Vec::new();
    for (i, entry) in manager.entries().iter().enumerate() {
        use kiss_coding::SessionEntry;
        let label = match entry {
            SessionEntry::Message { message, .. } => match message {
                AgentMessage::User(u) => {
                    format!(
                        "user: {}",
                        preview(&visible_user_text(&u.content.as_text()))
                    )
                }
                AgentMessage::Assistant(a) => format!("assistant: {}", preview(&a.text())),
                AgentMessage::ToolResult(t) => format!("tool: {}", t.tool_name),
                other => other.role().to_string(),
            },
            SessionEntry::Compaction { .. } => "· compaction".into(),
            SessionEntry::BranchSummary { .. } => "· branch summary".into(),
            _ => continue,
        };
        let marker = if Some(entry.id()) == manager.leaf_id() {
            " ← leaf"
        } else {
            ""
        };
        let detail = manager.label_of(entry.id());
        items.push(SelectItem {
            label: format!("{label}{marker}"),
            detail,
            value: i,
        });
    }
    drop(manager);
    if items.is_empty() {
        app.cells.push(Cell::Notice("session is empty".into()));
        return;
    }
    let mut list = SelectList::new(
        "Tree (enter select · ctrl+y copy · ctrl+l label · esc cancel)",
        items,
        app.theme.clone(),
    );
    list.max_visible = 15;
    list.selected = list.items.len().saturating_sub(1);
    app.show_picker(Picker {
        kind: PickerKind::Tree,
        list,
    });
}

fn open_tree_summary_picker(app: &mut App, target_id: String) {
    let choices = [
        ("No summary", "Move without a summary"),
        ("Summarize", "Keep a summary of the branch that you leave"),
        (
            "Summarize with custom prompt",
            "Add instructions for the branch summary",
        ),
    ];
    let items = choices
        .iter()
        .enumerate()
        .map(|(value, (label, detail))| SelectItem {
            label: (*label).into(),
            detail: Some((*detail).into()),
            value,
        })
        .collect();
    app.show_picker(Picker {
        kind: PickerKind::TreeSummary(target_id),
        list: SelectList::new("Summarize branch?", items, app.theme.clone()),
    });
}

fn start_tree_navigation(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    target_id: String,
    summarize: bool,
    custom_instructions: Option<String>,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let Some(cancel) = begin_command(
        app,
        if summarize {
            "summarizing branch".into()
        } else {
            "moving in session tree".into()
        },
    ) else {
        return;
    };
    let session = session.clone();
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let result = session
            .navigate_tree(&target_id, summarize, custom_instructions, cancel)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = tx.send(CommandEvent::TreeNavigationFinished(result));
    });
}

fn open_model_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    pattern: Option<&str>,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let pattern = pattern.map(str::to_owned);
    if let Some(registry) = &app.model_catalog {
        let _ = command_tx.send(CommandEvent::ModelCatalog {
            registry: Arc::clone(registry),
            warnings: Vec::new(),
            pattern,
        });
        return;
    }
    let mut registry = (*session.registry).clone();
    let signed_in: Vec<&str> = kiss_ai::Registry::DYNAMIC_PROVIDERS
        .into_iter()
        .filter(|provider| registry.signed_in(provider))
        .collect();
    let Some(cancel) = begin_command(app, "reading account models".into()) else {
        return;
    };
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        // A list that fails to load leaves the others usable.
        let warnings = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(30), registry.refresh_dynamic(|provider| signed_in.contains(&provider))) => {
                result.unwrap_or_else(|_| vec!["account model discovery timed out".into()])
            }
            _ = cancel.cancelled() => {
                let _ = tx.send(CommandEvent::CommandFinished);
                return;
            }
        };
        let _ = tx.send(CommandEvent::ModelCatalog {
            registry: Arc::new(registry),
            warnings,
            pattern,
        });
        let _ = tx.send(CommandEvent::CommandFinished);
    });
}

/// The key bound to `action`, written as help text writes keys
/// ("Ctrl+Shift+P"), or "unbound".
fn key_label(keybindings: &Keybindings, action: Action) -> String {
    let Some(key) = keybindings.key_for(action) else {
        return "unbound".into();
    };
    key.to_string()
        .split('+')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn save_default_hint(keybindings: &Keybindings) -> String {
    match keybindings.key_for(Action::SaveDefault) {
        Some(key) => format!("{key} or shift+enter"),
        None => "shift+enter".into(),
    }
}

fn open_model_picker_with_filter(
    app: &mut App,
    registry: &kiss_ai::Registry,
    filter: Option<&str>,
) {
    let access = ModelAccess::current();
    let models: Vec<kiss_ai::Model> = registry
        .available_models()
        .into_iter()
        .filter(|(_, model)| access.allows(model))
        .map(|(_, model)| model.clone())
        .collect();
    let items: Vec<SelectItem> = models
        .iter()
        .enumerate()
        .map(|(value, model)| SelectItem {
            label: format!("{}/{}", model.provider, model.id),
            detail: Some(model.display_name().to_string()),
            value,
        })
        .collect();
    let mut list = SelectList::new(
        format!(
            "Select model from connected providers · /login connects more (enter uses, {} saves default)",
            save_default_hint(&app.keybindings)
        ),
        items,
        app.theme.clone(),
    );
    list.max_visible = 12;
    if let Some(filter) = filter {
        list.set_filter(filter.to_string());
    }
    app.show_picker(Picker {
        kind: PickerKind::Model(models),
        list,
    });
}

fn open_thinking_picker(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    if !session.model().reasoning {
        app.cells
            .push(Cell::Notice("this model has no thinking levels".into()));
        return;
    }
    let current = session.thinking_level();
    let supported = session.model().supported_thinking_levels();
    let items = ThinkingLevel::ALL
        .iter()
        .enumerate()
        .filter(|(_, level)| supported.contains(level))
        .map(|(value, level)| SelectItem {
            label: level.as_str().to_string(),
            detail: (*level == current).then(|| "current".into()),
            value,
        })
        .collect();
    let mut list = SelectList::new(
        format!(
            "Thinking level (enter uses, {} saves default)",
            save_default_hint(&app.keybindings)
        ),
        items,
        app.theme.clone(),
    );
    list.selected = supported
        .iter()
        .position(|level| *level == current)
        .unwrap_or(0);
    app.show_picker(Picker {
        kind: PickerKind::Thinking,
        list,
    });
}

fn open_scoped_models_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
) {
    app.show_picker(scoped_models_picker(app, session, resources));
}

fn scoped_models_picker(
    app: &App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
) -> Picker {
    let access = ModelAccess::current();
    let items = session
        .registry
        .available_models()
        .into_iter()
        .filter(|(_, model)| access.allows(model))
        .map(|(value, model)| {
            let enabled = resources
                .enabled_models
                .iter()
                .any(|candidate| candidate.provider == model.provider && candidate.id == model.id);
            SelectItem {
                label: format!(
                    "{} {}/{}",
                    if enabled { "[x]" } else { "[ ]" },
                    model.provider,
                    model.id
                ),
                detail: Some(model.display_name().to_string()),
                value,
            }
        })
        .collect();
    let mut list = SelectList::new(
        "Models for Ctrl+P (enter toggles)",
        items,
        app.theme.clone(),
    );
    list.max_visible = 14;
    Picker {
        kind: PickerKind::ScopedModels,
        list,
    }
}

fn reopen_scoped_models_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
    filter: String,
    selected_value: usize,
) {
    let mut picker = scoped_models_picker(app, session, resources);
    picker.list.set_filter(filter);
    picker.list.select_value(selected_value);
    app.show_picker(picker);
}

/// Which catalog models the signed-in accounts may use. Only a GitHub
/// Copilot account restricts them, to the models it listed at login.
struct ModelAccess(Option<Vec<String>>);

impl ModelAccess {
    fn current() -> Self {
        Self(kiss_ai::auth::stored_oauth_model_ids("github-copilot"))
    }

    fn allows(&self, model: &kiss_ai::Model) -> bool {
        model.provider != "github-copilot"
            || self
                .0
                .as_ref()
                .is_none_or(|ids| ids.iter().any(|id| id == &model.id))
    }
}

fn open_session_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    global: bool,
    query: String,
) {
    let manager = session.manager.lock().unwrap();
    let listings = session_sources::discover(manager.cwd(), manager.session_dir(), global);
    let current = manager.session_file().map(PathBuf::from);
    drop(manager);
    match listings {
        // An empty project list still opens, so tab can show all projects.
        Ok(listings) => {
            app.session_picker = Some(SessionPicker::new(listings, global, current, query));
        }
        Err(error) => app.cells.push(Cell::Error(format!(
            "could not list sessions: {error:#}. Check that the session folders are readable, then run /resume again"
        ))),
    }
}

fn handle_session_picker_key(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    key: &KeyEvent,
) {
    let Some(picker) = app.session_picker.as_mut() else {
        return;
    };
    match picker.handle_key(key) {
        SessionPickerAction::None => {}
        SessionPickerAction::Close => app.session_picker = None,
        SessionPickerAction::ToggleScope => {
            let (global, query) = (!picker.global, std::mem::take(&mut picker.query));
            open_session_picker(app, session, global, query);
        }
        SessionPickerAction::Resume(record) => {
            app.session_picker = None;
            switch_session(app, session, &record);
        }
        SessionPickerAction::Rename { path, name } => {
            let (is_current, renamed) = {
                let mut manager = session.manager.lock().unwrap();
                if manager
                    .session_file()
                    .is_some_and(|current| current == path)
                {
                    (true, manager.append_session_info(&name).map(|_| ()))
                } else {
                    drop(manager);
                    let renamed = kiss_coding::SessionManager::open(&path)
                        .and_then(|mut manager| manager.append_session_info(&name).map(|_| ()));
                    (false, renamed)
                }
            };
            if is_current && renamed.is_ok() {
                set_session_title(app, Some(name.clone()));
            }
            if let Some(picker) = app.session_picker.as_mut() {
                match renamed {
                    Ok(()) => picker.renamed(&path, &name),
                    Err(error) => {
                        picker.notice = Some(format!("Could not rename the session: {error:#}"))
                    }
                }
            }
        }
    }
}

fn open_fork_picker(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    let manager = session.manager.lock().unwrap();
    let items: Vec<SelectItem> = manager
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(value, entry)| match entry {
            kiss_coding::SessionEntry::Message {
                message: AgentMessage::User(user),
                ..
            } => Some(SelectItem {
                label: preview(&visible_user_text(&user.content.as_text())),
                detail: None,
                value,
            }),
            _ => None,
        })
        .collect();
    drop(manager);
    if items.is_empty() {
        app.cells
            .push(Cell::Notice("no user messages to fork from".into()));
        return;
    }
    let mut list = SelectList::new("Fork from user message", items, app.theme.clone());
    list.max_visible = 14;
    list.selected = list.items.len().saturating_sub(1);
    app.show_picker(Picker {
        kind: PickerKind::Fork,
        list,
    });
}

fn open_login_picker(app: &mut App) {
    open_login_picker_with_filter(app, None);
}

/// Every provider `/login` offers: the built-in catalog, the decision-model
/// services, and a local llama.cpp server.
fn login_providers() -> impl Iterator<Item = &'static str> {
    kiss_ai::registry::BUILTIN_PROVIDER_IDS
        .iter()
        .copied()
        .chain(["typesafe", "liquid", "llama.cpp"])
}

fn open_login_picker_with_filter(app: &mut App, filter: Option<&str>) {
    let providers: Vec<String> = login_providers().map(str::to_string).collect();
    let items = providers
        .iter()
        .enumerate()
        .map(|(value, provider)| SelectItem {
            label: provider.clone(),
            detail: Some(if provider == "llama.cpp" {
                "Router URL and optional API key".into()
            } else {
                let methods = kiss_ai::auth::login_methods(provider);
                let status = match kiss_ai::auth::stored_auth_kind(provider) {
                    Some(kiss_ai::auth::StoredAuthKind::OAuth) => "saved OAuth",
                    Some(kiss_ai::auth::StoredAuthKind::AzureEntraId) => "saved Microsoft Entra ID",
                    Some(kiss_ai::auth::StoredAuthKind::ApiKey) => "saved API key",
                    None => "not configured",
                };
                format!(
                    "{} · {status}",
                    methods
                        .iter()
                        .map(|method| method.label())
                        .collect::<Vec<_>>()
                        .join(" / ")
                )
            }),
            value,
        })
        .collect();
    let mut list = SelectList::new(
        "Configure provider authentication",
        items,
        app.theme.clone(),
    );
    list.max_visible = 14;
    if let Some(filter) = filter {
        list.set_filter(filter.to_string());
    }
    app.show_picker(Picker {
        kind: PickerKind::LoginProviders(providers),
        list,
    });
}

fn open_login_methods_picker(app: &mut App, provider: &str) {
    if provider == "llama.cpp" {
        app.secret_prompt = Some(SecretPrompt {
            kind: SecretPromptKind::Llama,
            value: "http://127.0.0.1:8080".into(),
        });
        return;
    }
    if app
        .pending_account_add
        .as_ref()
        .is_none_or(|(pending, _)| pending != provider)
        && let Err(error) = kiss_ai::auth::accounts::cancel_add(provider)
    {
        app.cells.push(Cell::Error(format!(
            "could not start login for {provider}: {error:#}"
        )));
        return;
    }
    // Ambient identities (Entra ID, gcloud ADC, AWS profiles) are not
    // separate accounts, so `/accounts add` offers only real credentials.
    let adding_account = app.pending_account_add.is_some();
    let mut choices = kiss_ai::auth::login_methods(provider)
        .into_iter()
        .filter(|method| {
            !adding_account
                || matches!(
                    method,
                    kiss_ai::auth::LoginMethod::BrowserOAuth
                        | kiss_ai::auth::LoginMethod::DeviceOAuth
                        | kiss_ai::auth::LoginMethod::ManualOAuth
                        | kiss_ai::auth::LoginMethod::ApiKey
                )
        })
        .map(LoginChoice::Method)
        .collect::<Vec<_>>();
    choices.extend(
        kiss_ai::auth::external::discover()
            .into_iter()
            .filter(|source| source.provider == provider)
            .map(LoginChoice::External),
    );
    let items = choices
        .iter()
        .enumerate()
        .map(|(value, choice)| match choice {
            LoginChoice::Method(method) => SelectItem {
                label: method.label().into(),
                detail: Some(match method {
                    kiss_ai::auth::LoginMethod::BrowserOAuth => {
                        "Uses a local browser callback".into()
                    }
                    kiss_ai::auth::LoginMethod::DeviceOAuth => {
                        "Works on a remote or headless host".into()
                    }
                    kiss_ai::auth::LoginMethod::ManualOAuth => {
                        "Paste the final callback into Kiss".into()
                    }
                    kiss_ai::auth::LoginMethod::ApiKey => "Stores a provider API key".into(),
                    kiss_ai::auth::LoginMethod::AzureEntraId => {
                        "Uses the Microsoft Entra default credential chain".into()
                    }
                    kiss_ai::auth::LoginMethod::GoogleApplicationDefault => {
                        "Uses gcloud application-default credentials".into()
                    }
                    kiss_ai::auth::LoginMethod::AwsProfile => "Uses a named AWS SDK profile".into(),
                    kiss_ai::auth::LoginMethod::AwsAmbient => {
                        "Uses the AWS SDK default chain".into()
                    }
                }),
                value,
            },
            LoginChoice::External(source) => SelectItem {
                label: format!("Import from {}", source.application),
                detail: Some(source.location.clone()),
                value,
            },
        })
        .collect();
    let mut list = SelectList::new(
        format!("Authentication for {provider}"),
        items,
        app.theme.clone(),
    );
    list.max_visible = 12;
    app.show_picker(Picker {
        kind: PickerKind::LoginMethods(provider.into(), choices),
        list,
    });
}

fn open_logout_picker(app: &mut App) {
    let providers = kiss_ai::auth::stored_provider_ids();
    if providers.is_empty() {
        app.cells.push(Cell::Notice(
            "no stored credentials. Environment variables are unchanged".into(),
        ));
        return;
    }
    let items = providers
        .iter()
        .enumerate()
        .map(|(value, provider)| SelectItem {
            label: provider.clone(),
            detail: Some("Stored credential".into()),
            value,
        })
        .collect();
    app.show_picker(Picker {
        kind: PickerKind::Logout(providers),
        list: SelectList::new("Remove provider authentication", items, app.theme.clone()),
    });
}

fn open_settings_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
) {
    app.show_picker(settings_picker(app, session, resources));
}

fn decision_model_detail(
    model: Option<DecisionModel>,
    fallback: &str,
    session: &kiss_coding::AgentSession,
) -> String {
    match model {
        None => fallback.into(),
        Some(model) if model.available(&session.registry) => model.label().into(),
        Some(model) => format!(
            "{} unavailable · /login {}",
            model.label(),
            model.provider()
        ),
    }
}

/// The rows of `/settings`, in display order. A row's picker value is its
/// position here.
#[derive(Clone, Copy, PartialEq, Debug)]
enum SettingRow {
    ThinkingLevel,
    Theme,
    ThinkingBlocks,
    Transport,
    SteeringDelivery,
    FollowUpDelivery,
    AutoCompaction,
    AutomaticRetry,
    StartupDetails,
    DefaultProjectTrust,
    SkillCommands,
    MermaidRendering,
    AutomaticRecap,
    Subagents,
    DynamicWorkflows,
    CompactionMethod,
    DynamicReasoning,
    DesktopNotifications,
}

impl SettingRow {
    const ALL: [SettingRow; 18] = [
        SettingRow::ThinkingLevel,
        SettingRow::Theme,
        SettingRow::ThinkingBlocks,
        SettingRow::Transport,
        SettingRow::SteeringDelivery,
        SettingRow::FollowUpDelivery,
        SettingRow::AutoCompaction,
        SettingRow::AutomaticRetry,
        SettingRow::StartupDetails,
        SettingRow::DefaultProjectTrust,
        SettingRow::SkillCommands,
        SettingRow::MermaidRendering,
        SettingRow::AutomaticRecap,
        SettingRow::Subagents,
        SettingRow::DynamicWorkflows,
        SettingRow::CompactionMethod,
        SettingRow::DynamicReasoning,
        SettingRow::DesktopNotifications,
    ];
}

fn settings_picker(
    app: &App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
) -> Picker {
    let settings = &resources.settings;
    let theme = settings.theme.as_deref().unwrap_or("dark");
    let items = vec![
        SelectItem {
            label: "Thinking level".into(),
            detail: Some(session.thinking_level().as_str().into()),
            value: SettingRow::ThinkingLevel as usize,
        },
        SelectItem {
            label: "Theme".into(),
            detail: Some(theme.into()),
            value: SettingRow::Theme as usize,
        },
        SelectItem {
            label: "Thinking blocks".into(),
            detail: Some(if settings.hide_thinking_block {
                "hidden".into()
            } else {
                "visible".into()
            }),
            value: SettingRow::ThinkingBlocks as usize,
        },
        SelectItem {
            label: "Transport".into(),
            detail: Some(format_transport(settings.transport).into()),
            value: SettingRow::Transport as usize,
        },
        SelectItem {
            label: "Steering delivery".into(),
            detail: Some(format_queue_mode(settings.steering_mode).into()),
            value: SettingRow::SteeringDelivery as usize,
        },
        SelectItem {
            label: "Follow-up delivery".into(),
            detail: Some(format_queue_mode(settings.follow_up_mode).into()),
            value: SettingRow::FollowUpDelivery as usize,
        },
        SelectItem {
            label: "Auto compaction".into(),
            detail: Some(
                if settings.compaction.enabled {
                    "on"
                } else {
                    "off"
                }
                .into(),
            ),
            value: SettingRow::AutoCompaction as usize,
        },
        SelectItem {
            label: "Automatic retry".into(),
            detail: Some(if settings.retry.enabled { "on" } else { "off" }.into()),
            value: SettingRow::AutomaticRetry as usize,
        },
        SelectItem {
            label: "Startup details".into(),
            detail: Some(
                if settings.quiet_startup {
                    "quiet"
                } else {
                    "shown"
                }
                .into(),
            ),
            value: SettingRow::StartupDetails as usize,
        },
        SelectItem {
            label: "Default project trust".into(),
            detail: Some(
                match settings.default_project_trust {
                    kiss_coding::settings::ProjectTrustDefault::Ask => "ask",
                    kiss_coding::settings::ProjectTrustDefault::Always => "always",
                    kiss_coding::settings::ProjectTrustDefault::Never => "never",
                }
                .into(),
            ),
            value: SettingRow::DefaultProjectTrust as usize,
        },
        SelectItem {
            label: "Skill slash commands".into(),
            detail: Some(
                if settings.enable_skill_commands.unwrap_or(true) {
                    "on"
                } else {
                    "off"
                }
                .into(),
            ),
            value: SettingRow::SkillCommands as usize,
        },
        SelectItem {
            label: "Mermaid rendering".into(),
            detail: Some(settings.markdown.mermaid.as_str().into()),
            value: SettingRow::MermaidRendering as usize,
        },
        SelectItem {
            label: "Automatic recap".into(),
            detail: Some(
                if settings.auto_recap_enabled() {
                    "on"
                } else {
                    "off"
                }
                .into(),
            ),
            value: SettingRow::AutomaticRecap as usize,
        },
        SelectItem {
            label: "Subagents".into(),
            detail: Some(
                if settings.subagents.enabled {
                    "on"
                } else {
                    "off"
                }
                .into(),
            ),
            value: SettingRow::Subagents as usize,
        },
        SelectItem {
            label: "Dynamic workflows".into(),
            detail: Some(if !settings.subagents.enabled {
                "off (needs subagents)".into()
            } else if settings.workflows.enabled {
                format!("on · {} size", settings.workflows.size.as_str())
            } else {
                "off".into()
            }),
            value: SettingRow::DynamicWorkflows as usize,
        },
        SelectItem {
            label: "Compaction method".into(),
            detail: Some(decision_model_detail(
                settings.compaction.mode.decision_model(),
                "summary",
                session,
            )),
            value: SettingRow::CompactionMethod as usize,
        },
        SelectItem {
            label: "Dynamic reasoning".into(),
            detail: Some(decision_model_detail(
                settings.reasoning_effort.mode.decision_model(),
                "fixed",
                session,
            )),
            value: SettingRow::DynamicReasoning as usize,
        },
        SelectItem {
            label: "Desktop notifications".into(),
            detail: Some(if settings.desktop_notifications {
                format!("on · {}", app.notifications.description())
            } else {
                "off".into()
            }),
            value: SettingRow::DesktopNotifications as usize,
        },
    ];
    Picker {
        kind: PickerKind::Settings,
        list: SelectList::new(
            "Settings (enter/space changes a value)",
            items,
            app.theme.clone(),
        ),
    }
}

fn reopen_settings_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
    filter: String,
    selected_value: usize,
) {
    let mut picker = settings_picker(app, session, resources);
    picker.list.set_filter(filter);
    picker.list.select_value(selected_value);
    app.show_picker(picker);
}

/// Run a workflow that is already written and saved on disk.
///
/// The script says what to do, so this needs no model turn: it parses the file,
/// asks for approval, and runs it, opening the progress view so the work is
/// visible from the first agent.
fn start_saved_workflow(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    path: PathBuf,
    arguments: &str,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let Some(runtime) = session.workflows().filter(|_| session.workflows_enabled()) else {
        app.cells
            .push(Cell::Notice(WORKFLOWS_NEED_SUBAGENTS.into()));
        return;
    };
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            app.cells.push(Cell::Error(format!(
                "could not read {}: {error}",
                path.display()
            )));
            return;
        }
    };
    let script = match kiss_coding::workflows::Script::parse(&source) {
        Ok(script) => script,
        Err(diagnostic) => {
            app.cells.push(Cell::Error(diagnostic.render(&source)));
            return;
        }
    };

    // Structured input when the argument text is JSON, plain text otherwise, so
    // a script can use `args.length` or `args` directly without parsing.
    let args = if arguments.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(arguments.trim())
            .unwrap_or_else(|_| serde_json::Value::String(arguments.trim().to_string()))
    };

    let plan = kiss_coding::workflows::WorkflowPlan::from_script(&script);
    let name = plan.name.clone();
    let Some(cancel) = begin_command(app, format!("waiting to run workflow {name}")) else {
        return;
    };
    let command_tx = CommandTask::new(app, command_tx);
    let runtime = runtime.clone();
    tokio::spawn(async move {
        let decision = tokio::select! {
            biased;
            _ = cancel.cancelled() => kiss_coding::workflows::ApprovalDecision::Cancel,
            decision = runtime.approve(plan.clone()) => decision,
        };
        if decision == kiss_coding::workflows::ApprovalDecision::Cancel {
            let _ = command_tx.send(CommandEvent::CommandFinished);
            return;
        }
        let record = match runtime.prepare(&plan, script, args) {
            Ok(record) => record,
            Err(error) => {
                let _ = command_tx.send(CommandEvent::WorkflowFinished {
                    name,
                    result: Err(format!("could not start: {error:#}")),
                });
                return;
            }
        };
        let _ = command_tx.send(CommandEvent::WorkflowStarted(record.clone()));
        let result = runtime
            .run(&record, cancel)
            .await
            .map(kiss_coding::workflows::report_text);
        let _ = command_tx.send(CommandEvent::WorkflowFinished { name, result });
    });
}

/// Write a run's script to disk so it becomes a slash command later.
fn save_workflow_run(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    run: kiss_coding::workflows::RunId,
    location: kiss_coding::workflows::SaveLocation,
    name: &str,
) {
    let Some(record) = session.workflows().and_then(|runtime| runtime.get(run)) else {
        app.cells.push(Cell::Error(
            "that workflow run is no longer available".into(),
        ));
        return;
    };
    let cwd = session.manager.lock().unwrap().cwd().to_path_buf();
    match kiss_coding::workflows::save(location, &cwd, name, &record.source, false) {
        Ok(path) => app.cells.push(Cell::Notice(format!(
            "saved {}\nrun it with /{name} after /reload",
            kiss_ai::paths::abbreviate_home(&path.display().to_string())
        ))),
        Err(error) => app
            .cells
            .push(Cell::Error(format!("could not save workflow: {error:#}"))),
    }
}

/// Open the progress view on one run, or on the newest one.
fn open_workflow_view(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    run: Option<kiss_coding::workflows::RunId>,
) {
    let Some(runtime) = session.workflows() else {
        app.cells
            .push(Cell::Notice(WORKFLOWS_NEED_SUBAGENTS.into()));
        return;
    };
    let record = match run {
        Some(id) => runtime.get(id),
        // Prefer a run still working. Otherwise show the most recent one.
        None => runtime.active().or_else(|| runtime.latest()),
    };
    match record {
        Some(record) => app.workflow_view = Some(WorkflowView::new(record)),
        None => app.cells.push(Cell::Notice(
            "no workflow has run in this session yet".into(),
        )),
    }
}

fn start_or_open_job(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    kind: kiss_coding::iterative::JobKind,
    arguments: &str,
) {
    if arguments.trim().is_empty() {
        open_job_view(app, session);
        return;
    }
    let request = match kiss_coding::iterative::JobRequest::parse(kind, arguments) {
        Ok(request) => request,
        Err(error) => {
            app.cells.push(Cell::Error(error));
            return;
        }
    };
    let Some(runtime) = session.iterative_jobs() else {
        app.cells.push(Cell::Error(
            "iterative jobs are not available in this session".into(),
        ));
        return;
    };
    match runtime.start(kind, request) {
        Ok(job) => {
            let snapshot = job.snapshot();
            let schedule = match (snapshot.limit, snapshot.interval) {
                (Some(limit), None) => format!("up to {limit} iterations"),
                (None, Some(interval)) => format!(
                    "every {} until stopped",
                    kiss_coding::iterative::format_interval(interval)
                ),
                (None, None) => "no iteration limit".into(),
                (Some(_), Some(_)) => unreachable!("the runtime rejects two schedules"),
            };
            app.cells.push(Cell::Notice(format!(
                "started {} job #{} in session {} · {} · /jobs opens the job view",
                snapshot.kind.label(),
                snapshot.id,
                snapshot.session_id,
                schedule,
            )));
        }
        Err(error) => app.cells.push(Cell::Error(format!(
            "could not start {}: {error:#}",
            kind.label()
        ))),
    }
}

fn open_job_view(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    let Some(runtime) = session.iterative_jobs() else {
        app.cells.push(Cell::Notice(
            "iterative jobs are not available in this session".into(),
        ));
        return;
    };
    if runtime.summaries().is_empty() {
        app.cells.push(Cell::Notice(
            "no loop or autoresearch jobs have run in this session".into(),
        ));
        return;
    }
    app.job_view = Some(JobView::new(runtime));
}

/// Choose which run to open when there is more than one.
fn open_workflow_runs_picker(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    let Some(runtime) = session.workflows() else {
        app.cells
            .push(Cell::Notice(WORKFLOWS_NEED_SUBAGENTS.into()));
        return;
    };
    let summaries = runtime.summaries();
    if summaries.is_empty() {
        app.cells.push(Cell::Notice(
            "no workflow has run in this session yet".into(),
        ));
        return;
    }
    if summaries.len() == 1 {
        open_workflow_view(app, session, Some(summaries[0].id));
        return;
    }
    // Newest first: that is the one a user coming back to `/workflows` means.
    let items = summaries
        .iter()
        .rev()
        .enumerate()
        .map(|(value, summary)| SelectItem {
            label: summary.name.clone(),
            detail: Some(workflow_ui::summary_detail(summary)),
            value,
        })
        .collect();
    let ids = summaries.iter().rev().map(|summary| summary.id).collect();
    app.show_picker(Picker {
        kind: PickerKind::WorkflowRuns(ids),
        list: SelectList::new("Workflow runs", items, app.theme.clone()),
    });
}

fn open_workflow_save_picker(app: &mut App, run: kiss_coding::workflows::RunId) {
    app.workflow_view = None;
    let items = kiss_coding::workflows::SaveLocation::ALL
        .iter()
        .enumerate()
        .map(|(value, location)| {
            let (label, reach) = match location {
                kiss_coding::workflows::SaveLocation::Project => (
                    "Save in this project",
                    "is shared with everyone who clones it",
                ),
                kiss_coding::workflows::SaveLocation::Personal => {
                    ("Save for me", "is available in every project")
                }
            };
            SelectItem {
                label: label.into(),
                detail: Some(format!("{} {reach}", location.display_dir())),
                value,
            }
        })
        .collect();
    app.show_picker(Picker {
        kind: PickerKind::WorkflowSaveLocation(run),
        list: SelectList::new("Save this workflow", items, app.theme.clone()),
    });
}

/// Show the plan and ask before any agent starts.
fn open_workflow_approval(
    app: &mut App,
    plan: Box<kiss_coding::workflows::WorkflowPlan>,
    reply: tokio::sync::oneshot::Sender<kiss_coding::workflows::ApprovalDecision>,
) {
    let items = vec![
        SelectItem {
            label: "Run it".into(),
            detail: Some(plan.agent_estimate()),
            value: 0,
        },
        SelectItem {
            label: "View the script".into(),
            detail: Some("read it before deciding".into()),
            value: 1,
        },
        SelectItem {
            label: "Cancel".into(),
            detail: Some("nothing runs".into()),
            value: 2,
        },
    ];
    let title = workflow_ui::plan_summary(&plan);
    app.show_picker(Picker {
        kind: PickerKind::WorkflowApproval(plan, reply),
        list: SelectList::new(title, items, app.theme.clone()),
    });
}

/// Build the callback the workflow runtime uses to ask for approval.
///
/// The tool call runs on another task, so it hands the plan to the terminal
/// through the command channel and waits for the answer on a one-shot channel.
fn workflow_approver(
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) -> kiss_coding::workflows::WorkflowApprover {
    let command_tx = command_tx.clone();
    Arc::new(move |plan| {
        let command_tx = command_tx.clone();
        Box::pin(async move {
            let (reply, answer) = tokio::sync::oneshot::channel();
            if command_tx
                .send(CommandEvent::WorkflowApproval {
                    plan: Box::new(plan),
                    reply,
                })
                .is_err()
            {
                return kiss_coding::workflows::ApprovalDecision::Cancel;
            }
            // A closed channel means the terminal went away, which is not
            // consent.
            answer
                .await
                .unwrap_or(kiss_coding::workflows::ApprovalDecision::Cancel)
        })
    })
}

fn open_trust_picker(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    let cwd = session.manager.lock().unwrap().cwd().display().to_string();
    let items = vec![
        SelectItem {
            label: "Trust project".into(),
            detail: Some("Load project settings, prompts, and skills".into()),
            value: 0,
        },
        SelectItem {
            label: "Do not trust project".into(),
            detail: Some("Ignore project-local resources".into()),
            value: 1,
        },
    ];
    app.show_picker(Picker {
        kind: PickerKind::Trust,
        list: SelectList::new(format!("Trust for {cwd}"), items, app.theme.clone()),
    });
}

fn open_llama_picker(app: &mut App, models: Vec<LlamaModel>) {
    let items = models
        .iter()
        .enumerate()
        .map(|(value, model)| SelectItem {
            label: model.id.clone(),
            detail: Some(model.status.value.clone()),
            value,
        })
        .collect();
    let mut list = SelectList::new(
        "llama.cpp models (enter loads or unloads)",
        items,
        app.theme.clone(),
    );
    list.max_visible = 14;
    app.show_picker(Picker {
        kind: PickerKind::Llama(models),
        list,
    });
}

fn mcp_state_label(state: &McpPanelState) -> &'static str {
    match state {
        McpPanelState::Checking => "◌ checking",
        McpPanelState::Connected => "✔ connected",
        McpPanelState::NeedsAuthentication(_) => "△ needs authentication",
        McpPanelState::Failed(_) => "✗ failed",
        McpPanelState::Disabled => "◯ disabled",
        McpPanelState::EnabledAfterReload => "◌ enabled after /reload",
    }
}

fn mcp_can_authenticate(server: &McpPanelServer) -> bool {
    server.entry.auth_mode() == Some(kiss_mcp::AuthMode::OAuth)
        || matches!(server.state, McpPanelState::NeedsAuthentication(_))
}

fn mcp_server_intro(theme: &Theme, server: &McpPanelServer, width: usize) -> Vec<String> {
    let mut lines = vec![theme.fg(
        "accent",
        &theme.bold(&kiss_tui::text::truncate_to_width(
            &format!("{} MCP Server", server.name),
            width,
        )),
    )];
    lines.push(format!("Status:  {}", mcp_state_label(&server.state)));
    if mcp_can_authenticate(server) || server.authenticated {
        lines.push(format!(
            "Auth:    {}",
            if server.authenticated {
                "✔ authenticated"
            } else {
                "not authenticated"
            }
        ));
    }
    if let Some(url) = &server.entry.url {
        lines.push(kiss_tui::text::truncate_to_width(
            &format!("URL:     {}", kiss_mcp::config::redact_url(url)),
            width,
        ));
    } else if let Some(command) = &server.entry.command {
        let command = std::iter::once(command.as_str())
            .chain(server.entry.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(kiss_tui::text::truncate_to_width(
            &format!("Command: {command}"),
            width,
        ));
    }
    lines.push(kiss_tui::text::truncate_to_width(
        &format!("Config:  {}", server.source),
        width,
    ));
    if !server.tools.is_empty() {
        lines.push("Capabilities: tools".to_string());
        lines.push(format!("Tools:   {} tools", server.tools.len()));
    }
    if let McpPanelState::NeedsAuthentication(error) | McpPanelState::Failed(error) = &server.state
    {
        lines.push(theme.fg(
            "error",
            &kiss_tui::text::truncate_to_width(&format!("Error:   {error}"), width),
        ));
    }
    lines
}

fn mcp_server_items(servers: &[McpPanelServer]) -> Vec<SelectItem> {
    servers
        .iter()
        .enumerate()
        .map(|(value, server)| SelectItem {
            label: server.name.clone(),
            detail: Some(if server.tools.is_empty() {
                mcp_state_label(&server.state).to_string()
            } else {
                format!(
                    "{} · {} tools",
                    mcp_state_label(&server.state),
                    server.tools.len()
                )
            }),
            value,
        })
        .collect()
}

fn show_mcp_server_picker(app: &mut App) {
    let count = app.mcp_servers.len();
    let mut list = SelectList::new(
        format!(
            "Manage MCP servers · {count} {}",
            if count == 1 { "server" } else { "servers" }
        ),
        mcp_server_items(&app.mcp_servers),
        app.theme.clone(),
    );
    list.max_visible = 12;
    app.show_picker(Picker {
        kind: PickerKind::McpServers,
        list,
    });
}

fn mcp_error_needs_authentication(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "401",
        "unauthorized",
        "oauth",
        "authentication",
        "access token",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn concise_mcp_error(error: &str, server: &kiss_mcp::ServerEntry) -> String {
    let mut message = error.lines().next().unwrap_or(error).trim().to_string();
    if let Some(url) = &server.url {
        message = message.replace(url, &kiss_mcp::config::redact_url(url));
    }
    message.chars().take(180).collect()
}

async fn check_mcp_server(
    manager: &kiss_mcp::McpManager,
    mut server: McpPanelServer,
) -> McpPanelServer {
    if let Some(url) = server.entry.url.as_deref() {
        server.authenticated = kiss_mcp::has_credentials(&server.name, url)
            .await
            .unwrap_or(false);
    }
    let cancel = CancellationToken::new();
    match tokio::time::timeout(
        Duration::from_secs(10),
        manager.list_tools(Some(&server.name), &cancel),
    )
    .await
    {
        Ok(Ok(listing)) => {
            server.state = McpPanelState::Connected;
            server.tools = listing.tools;
        }
        Ok(Err(error)) => {
            let message = format!("{error:#}");
            let concise = concise_mcp_error(&message, &server.entry);
            server.state = if mcp_error_needs_authentication(&message) {
                McpPanelState::NeedsAuthentication(concise)
            } else {
                McpPanelState::Failed(concise)
            };
        }
        Err(_) => {
            server.state = McpPanelState::Failed("connection timed out".into());
        }
    }
    server
}

fn start_mcp_check(
    manager: kiss_mcp::McpManager,
    server: McpPanelServer,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let tx = command_tx.clone();
    tokio::spawn(async move {
        let server = check_mcp_server(&manager, server).await;
        let _ = tx.send(CommandEvent::McpServerChecked(Box::new(server)));
    });
}

fn open_mcp_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    args: &Args,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let cwd = session.manager.lock().unwrap().cwd().to_path_buf();
    let loaded = match kiss_mcp::config::load(&cwd, args.project_trusted(&cwd)) {
        Ok(loaded) => loaded,
        Err(error) => {
            app.cells.push(Cell::Error(format!(
                "could not load MCP configuration: {error:#}"
            )));
            return;
        }
    };
    if loaded.config.mcp_servers.is_empty() {
        app.cells.push(Cell::Notice(
            "no MCP servers are configured. Use `kiss mcp add --help` to add one".into(),
        ));
        return;
    }
    let manager = match kiss_mcp::McpManager::new(loaded.clone()) {
        Ok(manager) => manager,
        Err(error) => {
            app.cells.push(Cell::Error(format!(
                "could not open MCP manager: {error:#}"
            )));
            return;
        }
    };
    app.mcp_config_paths = Some(loaded.paths.clone());
    app.mcp_manager = Some(manager.clone());
    app.mcp_servers = loaded
        .config
        .mcp_servers
        .iter()
        .map(|(name, entry)| McpPanelServer {
            name: name.clone(),
            entry: entry.clone(),
            state: if entry.disabled {
                McpPanelState::Disabled
            } else {
                McpPanelState::Checking
            },
            tools: Vec::new(),
            authenticated: false,
            source: loaded.source_labels(name).join(", "),
            scope: loaded.writable_scope(name),
        })
        .collect();
    show_mcp_server_picker(app);
    for server in app
        .mcp_servers
        .iter()
        .filter(|server| server.state != McpPanelState::Disabled)
        .cloned()
    {
        start_mcp_check(manager.clone(), server, command_tx);
    }
}

fn open_mcp_actions(app: &mut App, name: &str) {
    let Some(server) = app.mcp_servers.iter().find(|server| server.name == name) else {
        return;
    };
    let mut actions = Vec::new();
    if !server.tools.is_empty() {
        actions.push(McpPanelAction::ViewTools);
    }
    if mcp_can_authenticate(server) {
        actions.push(McpPanelAction::Authenticate);
        if server.authenticated {
            actions.push(McpPanelAction::ClearAuthentication);
        }
    }
    if server.state != McpPanelState::Disabled {
        actions.push(McpPanelAction::Reconnect);
    }
    actions.push(McpPanelAction::ToggleDisabled);
    let items = actions
        .iter()
        .enumerate()
        .map(|(value, action)| SelectItem {
            label: match action {
                McpPanelAction::ViewTools => "View tools",
                McpPanelAction::Authenticate if server.authenticated => "Re-authenticate",
                McpPanelAction::Authenticate => "Authenticate",
                McpPanelAction::ClearAuthentication => "Clear authentication",
                McpPanelAction::Reconnect => "Reconnect",
                McpPanelAction::ToggleDisabled if server.state == McpPanelState::Disabled => {
                    "Enable"
                }
                McpPanelAction::ToggleDisabled => "Disable",
            }
            .into(),
            detail: None,
            value,
        })
        .collect();
    app.show_picker(Picker {
        kind: PickerKind::McpActions(name.to_string(), actions),
        list: SelectList::new("Actions", items, app.theme.clone()),
    });
}

fn open_mcp_tools(app: &mut App, name: &str) {
    let Some(server) = app.mcp_servers.iter().find(|server| server.name == name) else {
        return;
    };
    let items = server
        .tools
        .iter()
        .enumerate()
        .map(|(value, tool)| SelectItem {
            label: tool.name.clone(),
            detail: tool.description.clone(),
            value,
        })
        .collect();
    app.show_picker(Picker {
        kind: PickerKind::McpTools(name.to_string()),
        list: SelectList::new(format!("{name} tools"), items, app.theme.clone()),
    });
}

fn start_mcp_reconnect(
    app: &mut App,
    name: &str,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let Some(manager) = app.mcp_manager.clone() else {
        app.cells
            .push(Cell::Error("MCP manager is not open".into()));
        return;
    };
    if !begin_uncancellable_command(app, format!("reconnecting MCP server {name}")) {
        return;
    }
    let Some(server) = app
        .mcp_servers
        .iter_mut()
        .find(|server| server.name == name)
    else {
        app.command = None;
        return;
    };
    server.state = McpPanelState::Checking;
    let server = server.clone();
    show_mcp_server_picker(app);
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let _ = manager.disconnect(&server.name).await;
        let server = check_mcp_server(&manager, server).await;
        let _ = tx.send(CommandEvent::McpServerChecked(Box::new(server)));
        let _ = tx.send(CommandEvent::CommandFinished);
    });
}

fn start_mcp_authentication(
    app: &mut App,
    server: McpPanelServer,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let name = server.name.clone();
    let Some(cancel) = begin_command(app, format!("authenticating MCP server {name}")) else {
        return;
    };
    app.picker = None;
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let result: Result<String> = async {
            let Some(pending) =
                crate::auth_flow::begin_mcp_login(&name, &server.entry, "/login", &cancel).await?
            else {
                return Ok(format!("authenticated MCP server {name}"));
            };
            let listener = crate::auth_flow::callback_listener(&pending.redirect_uri)
                .await?
                .context("MCP OAuth redirect is not local. Use `kiss mcp login --no-browser`")?;
            let opened = crate::auth_flow::open_browser(&pending.authorization_url);
            let _ = tx.send(CommandEvent::McpLoginUrl {
                name: name.clone(),
                url: pending.authorization_url.clone(),
                opened,
            });
            let callback = tokio::select! {
                result = crate::auth_flow::receive_callback(listener, &pending.redirect_uri) => result?,
                _ = cancel.cancelled() => anyhow::bail!("cancelled"),
            };
            kiss_mcp::finish_login(pending, &callback).await?;
            Ok(format!("authenticated MCP server {name}"))
        }
        .await;
        let _ = tx.send(CommandEvent::McpActionFinished {
            name,
            action: "authentication".into(),
            result: result.map_err(|error| format!("{error:#}")),
        });
    });
}

fn start_mcp_logout(
    app: &mut App,
    server: McpPanelServer,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let Some(url) = server.entry.url.clone() else {
        return;
    };
    let name = server.name;
    if !begin_uncancellable_command(
        app,
        format!("clearing authentication for MCP server {name}"),
    ) {
        return;
    }
    app.picker = None;
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let result = kiss_mcp::logout(&name, &url)
            .await
            .map(|removed| {
                if removed {
                    format!("cleared authentication for MCP server {name}")
                } else {
                    format!("MCP server {name} had no saved authentication")
                }
            })
            .map_err(|error| format!("{error:#}"));
        let _ = tx.send(CommandEvent::McpActionFinished {
            name,
            action: "logout".into(),
            result,
        });
    });
}

fn toggle_mcp_server(app: &mut App, name: &str) {
    let Some(paths) = app.mcp_config_paths.as_ref() else {
        app.cells.push(Cell::Error(
            "MCP configuration paths are not available".into(),
        ));
        return;
    };
    let Some(server) = app
        .mcp_servers
        .iter_mut()
        .find(|server| server.name == name)
    else {
        return;
    };
    let disabled = server.state != McpPanelState::Disabled;
    let Some(scope) = server.scope else {
        app.cells.push(Cell::Error(format!(
            "MCP server {} is defined in {}, which kiss does not edit. Set \"disabled\" there, or add the server to ~/.kiss/agent/mcp.json with `kiss mcp add`",
            server.name, server.source
        )));
        return;
    };
    match kiss_mcp::config::edit_server(paths, scope, &server.name, |entry| {
        entry.disabled = disabled;
        Ok(())
    }) {
        Ok(()) => {
            server.entry.disabled = disabled;
            server.state = if disabled {
                McpPanelState::Disabled
            } else {
                McpPanelState::EnabledAfterReload
            };
            app.cells.push(Cell::Notice(format!(
                "{} MCP server {}. Run /reload to apply the tool change",
                if disabled { "disabled" } else { "enabled" },
                server.name
            )));
            show_mcp_server_picker(app);
        }
        Err(error) => app.cells.push(Cell::Error(format!(
            "could not {} MCP server {}: {error:#}",
            if disabled { "disable" } else { "enable" },
            server.name
        ))),
    }
}

fn apply_mcp_action(
    app: &mut App,
    name: String,
    action: McpPanelAction,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let server = app
        .mcp_servers
        .iter()
        .find(|server| server.name == name)
        .cloned();
    match action {
        McpPanelAction::ViewTools => open_mcp_tools(app, &name),
        McpPanelAction::Authenticate => {
            if let Some(server) = server {
                start_mcp_authentication(app, server, command_tx);
            }
        }
        McpPanelAction::ClearAuthentication => {
            if let Some(server) = server {
                start_mcp_logout(app, server, command_tx);
            }
        }
        McpPanelAction::Reconnect => start_mcp_reconnect(app, &name, command_tx),
        McpPanelAction::ToggleDisabled => toggle_mcp_server(app, &name),
    }
}

fn preview(text: &str) -> String {
    let flat = text.replace('\n', " ");
    flat.chars().take(60).collect()
}

fn handle_picker_key(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    key: &KeyEvent,
    resources: &mut InteractiveResources,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) -> Flow {
    if key.key == Key::Escape
        && matches!(
            app.picker.as_ref().map(|picker| &picker.kind),
            Some(PickerKind::McpActions(_, _) | PickerKind::McpTools(_))
        )
    {
        show_mcp_server_picker(app);
        return Flow::Continue;
    }
    if (key.key == Key::Escape || *key == KeyEvent::ctrl('c'))
        && matches!(
            app.picker.as_ref().map(|picker| &picker.kind),
            Some(PickerKind::LoginProviders(_) | PickerKind::LoginMethods(..))
        )
    {
        cancel_account_add(app);
    }
    let Some(picker) = app.picker.as_mut() else {
        return Flow::Continue;
    };
    if *key == KeyEvent::ctrl('c') {
        app.picker = None;
        return Flow::Continue;
    }
    if matches!(&picker.kind, PickerKind::Tree)
        && (*key == KeyEvent::ctrl('y') || *key == KeyEvent::ctrl('l'))
    {
        let value = picker.list.current().map(|item| item.value);
        if let Some(entry) = value.and_then(|value| {
            session
                .manager
                .lock()
                .unwrap()
                .entries()
                .get(value)
                .cloned()
        }) {
            if *key == KeyEvent::ctrl('y') {
                let text = match &entry {
                    kiss_coding::SessionEntry::Message { message, .. } => match message {
                        AgentMessage::User(user) => visible_user_text(&user.content.as_text()),
                        AgentMessage::Assistant(assistant) => assistant.text(),
                        AgentMessage::ToolResult(tool) => tool
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        AgentMessage::BashExecution(shell) => shell.output.clone(),
                        _ => String::new(),
                    },
                    kiss_coding::SessionEntry::Compaction { summary, .. }
                    | kiss_coding::SessionEntry::BranchSummary { summary, .. } => summary.clone(),
                    _ => String::new(),
                };
                if text.is_empty() {
                    app.cells
                        .push(Cell::Notice("selected entry has no text to copy".into()));
                } else {
                    copy_text_to_clipboard(&text);
                    app.cells
                        .push(Cell::Notice("copied selected tree entry".into()));
                }
            } else {
                let current = session.manager.lock().unwrap().label_of(entry.id());
                app.picker = None;
                app.secret_prompt = Some(SecretPrompt {
                    kind: SecretPromptKind::TreeLabel(entry.id().into()),
                    value: current.unwrap_or_default(),
                });
            }
        }
        return Flow::Continue;
    }
    let activates_settings = matches!(&picker.kind, PickerKind::Settings)
        && picker.list.filter().is_empty()
        && key.key == Key::Char(' ')
        && !key.ctrl
        && !key.alt;
    let saves_default = matches!(&picker.kind, PickerKind::Model(_) | PickerKind::Thinking)
        && matches!(app.keybindings.action_for(key), Some(Action::SaveDefault));
    if key.key == Key::Enter || activates_settings || saves_default {
        let mut picker = app.picker.take().expect("picker is present");
        let value = picker.list.current().map(|item| item.value);
        let filter = picker.list.filter().to_string();
        if let Some(value) = value {
            apply_picker_selection(
                app,
                session,
                PickerSelection {
                    kind: picker.kind,
                    value,
                    filter,
                },
                key.shift || saves_default,
                resources,
                command_tx,
            );
        }
        return Flow::Continue;
    }
    match key.key {
        Key::Escape => {
            app.picker = None;
        }
        Key::Up => picker.list.move_selection(-1),
        Key::Down => picker.list.move_selection(1),
        _ => {
            picker.list.edit_filter(key);
        }
    }
    Flow::Continue
}

fn apply_picker_selection(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    selection: PickerSelection,
    save_default: bool,
    resources: &mut InteractiveResources,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let PickerSelection {
        kind,
        value,
        filter,
    } = selection;
    match kind {
        PickerKind::Model(models) => {
            if let Some(model) = models.get(value) {
                session.set_model(model.clone());
                if save_default {
                    resources.settings.default_provider = Some(model.provider.clone());
                    resources.settings.default_model = Some(model.id.clone());
                    keep_default_model_in_scope(resources, model);
                    save_interactive_settings(app, session, resources);
                    app.cells.push(Cell::Notice(format!(
                        "saved default model {}/{}",
                        model.provider, model.id
                    )));
                }
            }
        }
        PickerKind::Thinking => {
            if let Some(level) = ThinkingLevel::ALL.get(value).copied() {
                session.set_thinking_level(level);
                update_thinking_border(app, session.thinking_level());
                if save_default {
                    resources.settings.default_thinking_level = Some(level.as_str().into());
                    save_interactive_settings(app, session, resources);
                    app.cells.push(Cell::Notice(format!(
                        "saved default thinking level {}",
                        level.as_str()
                    )));
                }
            }
        }
        PickerKind::ScopedModels => {
            if let Some(model) = session.registry.all().get(value) {
                let enabled = resources.enabled_models.iter().position(|candidate| {
                    candidate.provider == model.provider && candidate.id == model.id
                });
                if let Some(position) = enabled {
                    resources.enabled_models.remove(position);
                } else {
                    resources.enabled_models.push(model.clone());
                }
                resources.settings.enabled_models = Some(
                    resources
                        .enabled_models
                        .iter()
                        .map(|model| format!("{}/{}", model.provider, model.id))
                        .collect(),
                );
                save_interactive_settings(app, session, resources);
                reopen_scoped_models_picker(app, session, resources, filter, value);
            }
        }
        PickerKind::Tree => {
            let target_id = session
                .manager
                .lock()
                .unwrap()
                .entries()
                .get(value)
                .map(|entry| entry.id().to_string());
            if let Some(target_id) = target_id {
                if session.manager.lock().unwrap().leaf_id() == Some(target_id.as_str()) {
                    app.cells
                        .push(Cell::Notice("already at the selected entry".into()));
                } else {
                    open_tree_summary_picker(app, target_id);
                }
            }
        }
        PickerKind::TreeSummary(target_id) => match value {
            0 => start_tree_navigation(app, session, target_id, false, None, command_tx),
            1 => start_tree_navigation(app, session, target_id, true, None, command_tx),
            2 => {
                app.secret_prompt = Some(SecretPrompt {
                    kind: SecretPromptKind::BranchSummary(target_id),
                    value: String::new(),
                });
            }
            _ => {}
        },
        PickerKind::Fork => {
            let selected = session
                .manager
                .lock()
                .unwrap()
                .entries()
                .get(value)
                .cloned();
            let selected_id = selected.as_ref().map(|entry| entry.id().to_string());
            if let Some(kiss_coding::SessionEntry::Message {
                message: AgentMessage::User(user),
                ..
            }) = selected
            {
                let text = visible_user_text(&user.content.as_text());
                match session.replace_manager(|current| {
                    current.fork_active_branch(selected_id.as_deref(), false)
                }) {
                    Ok(()) => {
                        adopt_session(app, session, Transcript::Empty);
                        app.editor.set_text(&text);
                        app.cells
                            .push(Cell::Notice("forked to a new session".into()));
                    }
                    Err(error) => app
                        .cells
                        .push(Cell::Error(format!("fork failed: {error:#}"))),
                }
            }
        }
        PickerKind::LoginProviders(providers) => {
            if let Some(provider) = providers.get(value) {
                open_login_methods_picker(app, provider);
            }
        }
        PickerKind::AccountProviders(providers, action) => {
            if let Some(provider) = providers.get(value) {
                if action == AccountAction::Add {
                    start_account_add(app, provider, None);
                } else {
                    open_accounts_picker(app, provider, action);
                }
            }
        }
        PickerKind::Accounts(provider, action) => match (value, action) {
            (ADD_ACCOUNT, _) => start_account_add(app, &provider, None),
            (index, AccountAction::Manage) => open_account_actions(app, &provider, index),
            (index, action) => {
                apply_account_action(app, &provider, index, action == AccountAction::Use)
            }
        },
        PickerKind::AccountActions(provider, index) => {
            if value < 2 {
                apply_account_action(app, &provider, index, value == 0);
            }
            open_accounts_picker(app, &provider, AccountAction::Manage);
        }
        PickerKind::LoginMethods(provider, choices) => {
            if let Some(choice) = choices.get(value) {
                match choice {
                    LoginChoice::Method(method) => {
                        start_login_method(app, &provider, *method, command_tx)
                    }
                    LoginChoice::External(source) => {
                        match kiss_ai::auth::external::import(source) {
                            Ok(()) => {
                                credentials_changed(app, &provider);
                                app.cells.push(Cell::Notice(format!(
                                    "imported {provider} credentials from {}",
                                    source.application
                                )));
                                complete_account_add(app, &provider, true);
                            }
                            Err(error) => {
                                complete_account_add(app, &provider, false);
                                app.cells.push(Cell::Error(format!(
                                    "credential import failed: {error:#}"
                                )))
                            }
                        }
                    }
                }
            }
        }
        PickerKind::Logout(providers) => {
            if let Some(provider) = providers.get(value) {
                log_out(app, provider);
            }
        }
        PickerKind::Settings => {
            apply_settings_selection(app, session, resources, value);
            reopen_settings_picker(app, session, resources, filter, value);
        }
        PickerKind::Trust => {
            let cwd = session.manager.lock().unwrap().cwd().to_path_buf();
            let trusted = value == 0;
            match kiss_coding::trust::save_decision(&cwd, trusted) {
                Ok(()) => app.cells.push(Cell::Notice(format!(
                    "saved project trust as {}. Run /reload to apply it",
                    if trusted { "trusted" } else { "untrusted" }
                ))),
                Err(error) => app.cells.push(Cell::Error(format!(
                    "could not save project trust: {error:#}"
                ))),
            }
        }
        PickerKind::ImportConfirm(path) => {
            if value == 0 {
                import_session(app, session, &path);
            } else {
                app.cells
                    .push(Cell::Notice("session import cancelled".into()));
            }
        }
        PickerKind::Llama(models) => {
            if let Some(model) = models.get(value).cloned() {
                start_llama_action(app, model, command_tx);
            }
        }
        PickerKind::McpServers => {
            if let Some(name) = app.mcp_servers.get(value).map(|server| server.name.clone()) {
                open_mcp_actions(app, &name);
            }
        }
        PickerKind::McpActions(name, actions) => {
            if let Some(action) = actions.get(value).copied() {
                apply_mcp_action(app, name, action, command_tx);
            }
        }
        PickerKind::McpTools(name) => open_mcp_tools(app, &name),
        PickerKind::WorkflowApproval(plan, reply) => match value {
            0 => {
                let _ = reply.send(kiss_coding::workflows::ApprovalDecision::Approve);
                app.cells.push(Cell::Notice(format!(
                    "running workflow {} · /workflows opens the progress view",
                    plan.name
                )));
            }
            1 => {
                // Show the script, then ask again rather than deciding for the
                // user.
                app.cells
                    .push(Cell::Notice(format!("workflow script\n\n{}", plan.source)));
                open_workflow_approval(app, plan, reply);
            }
            _ => {
                let _ = reply.send(kiss_coding::workflows::ApprovalDecision::Cancel);
                app.cells.push(Cell::Notice("workflow cancelled".into()));
            }
        },
        PickerKind::WorkflowRuns(ids) => {
            if let Some(id) = ids.get(value).copied() {
                open_workflow_view(app, session, Some(id));
            }
        }
        PickerKind::WorkflowSaveLocation(run) => {
            let Some(location) = kiss_coding::workflows::SaveLocation::ALL
                .get(value)
                .copied()
            else {
                return;
            };
            let suggested = session
                .workflows()
                .and_then(|runtime| runtime.get(run))
                .map(|record| record.name.clone())
                .unwrap_or_default();
            app.secret_prompt = Some(SecretPrompt {
                kind: SecretPromptKind::WorkflowName(run, location),
                value: suggested,
            });
        }
    }
}

fn keep_default_model_in_scope(
    resources: &mut InteractiveResources,
    model: &kiss_ai::Model,
) -> bool {
    if resources.enabled_models.is_empty()
        || resources
            .enabled_models
            .iter()
            .any(|candidate| candidate.provider == model.provider && candidate.id == model.id)
    {
        return false;
    }
    resources.enabled_models.push(model.clone());
    resources.settings.enabled_models = Some(
        resources
            .enabled_models
            .iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect(),
    );
    true
}

fn format_transport(transport: Transport) -> &'static str {
    match transport {
        Transport::Auto => "auto",
        Transport::Sse => "sse",
        Transport::WebSocket => "websocket",
        Transport::WebSocketCached => "websocket-cached",
    }
}

fn format_queue_mode(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::All => "all",
        QueueMode::OneAtATime => "one-at-a-time",
    }
}

/// Shown wherever a workflow is asked for while subagents are off; the
/// workflow runtime exists only when they are on.
const WORKFLOWS_NEED_SUBAGENTS: &str =
    "workflows run on subagents. Open /settings and turn Subagents on first";

fn save_interactive_settings(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &mut InteractiveResources,
) {
    match resources.settings.save_global() {
        Ok(()) => session.update_settings(resources.settings.clone()),
        Err(error) => app
            .cells
            .push(Cell::Error(format!("could not save settings: {error:#}"))),
    }
}

fn apply_settings_selection(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &mut InteractiveResources,
    value: usize,
) {
    let Some(row) = SettingRow::ALL.get(value).copied() else {
        return;
    };
    match row {
        SettingRow::ThinkingLevel => {
            let next = session
                .model()
                .next_thinking_level(session.thinking_level());
            session.set_thinking_level(next);
            resources.settings.default_thinking_level = Some(next.as_str().into());
            update_thinking_border(app, session.thinking_level());
        }
        SettingRow::Theme => {
            let next = if app.theme.name == "light" {
                "dark"
            } else {
                "light"
            };
            resources.settings.theme = Some(next.into());
            app.apply_theme(Theme::named(Some(next)));
            update_thinking_border(app, session.thinking_level());
        }
        SettingRow::ThinkingBlocks => {
            resources.settings.hide_thinking_block.toggle();
            app.hide_thinking = resources.settings.hide_thinking_block;
        }
        SettingRow::Transport => {
            resources.settings.transport = match resources.settings.transport {
                Transport::Auto => Transport::Sse,
                Transport::Sse => Transport::WebSocket,
                Transport::WebSocket => Transport::WebSocketCached,
                Transport::WebSocketCached => Transport::Auto,
            };
        }
        SettingRow::SteeringDelivery => {
            resources.settings.steering_mode = match resources.settings.steering_mode {
                QueueMode::All => QueueMode::OneAtATime,
                QueueMode::OneAtATime => QueueMode::All,
            };
        }
        SettingRow::FollowUpDelivery => {
            resources.settings.follow_up_mode = match resources.settings.follow_up_mode {
                QueueMode::All => QueueMode::OneAtATime,
                QueueMode::OneAtATime => QueueMode::All,
            };
        }
        SettingRow::AutoCompaction => resources.settings.compaction.enabled.toggle(),
        SettingRow::AutomaticRetry => resources.settings.retry.enabled.toggle(),
        SettingRow::StartupDetails => resources.settings.quiet_startup.toggle(),
        SettingRow::DefaultProjectTrust => {
            resources.settings.default_project_trust =
                match resources.settings.default_project_trust {
                    kiss_coding::settings::ProjectTrustDefault::Ask => {
                        kiss_coding::settings::ProjectTrustDefault::Always
                    }
                    kiss_coding::settings::ProjectTrustDefault::Always => {
                        kiss_coding::settings::ProjectTrustDefault::Never
                    }
                    kiss_coding::settings::ProjectTrustDefault::Never => {
                        kiss_coding::settings::ProjectTrustDefault::Ask
                    }
                };
        }
        SettingRow::SkillCommands => {
            resources.settings.enable_skill_commands =
                Some(!resources.settings.enable_skill_commands.unwrap_or(true));
        }
        SettingRow::MermaidRendering => {
            resources.settings.markdown.mermaid = match resources.settings.markdown.mermaid {
                MermaidRendering::Streaming => MermaidRendering::Final,
                MermaidRendering::Final => MermaidRendering::Off,
                MermaidRendering::Off => MermaidRendering::Streaming,
            };
            app.mermaid_mode = mermaid_mode(resources.settings.markdown.mermaid);
        }
        SettingRow::AutomaticRecap => {
            set_auto_recap(app, resources, !resources.settings.auto_recap_enabled())
        }
        SettingRow::Subagents => {
            resources.settings.subagents.enabled.toggle();
        }
        SettingRow::DynamicWorkflows => {
            if !resources.settings.subagents.enabled {
                // Workflows are built on child agents, so this row cannot be
                // turned on by itself.
                app.cells
                    .push(Cell::Notice(WORKFLOWS_NEED_SUBAGENTS.into()));
                return;
            }
            resources.settings.workflows.enabled.toggle();
        }
        SettingRow::CompactionMethod => {
            let Some(mode) = resources
                .settings
                .compaction
                .mode
                .next_available(|model| model.available(&session.registry))
            else {
                app.cells.push(Cell::Notice(
                    "Decision compaction needs credentials. Run /login typesafe, /login liquid, or /login cloudflare-workers-ai"
                        .into(),
                ));
                return;
            };
            resources.settings.compaction.mode = mode;
        }
        SettingRow::DynamicReasoning => {
            let Some(mode) = resources
                .settings
                .reasoning_effort
                .mode
                .next_available(|model| model.available(&session.registry))
            else {
                app.cells.push(Cell::Notice(
                    "Dynamic reasoning needs credentials. Run /login typesafe, /login liquid, or /login cloudflare-workers-ai"
                        .into(),
                ));
                return;
            };
            resources.settings.reasoning_effort.mode = mode;
        }
        SettingRow::DesktopNotifications => resources.settings.desktop_notifications.toggle(),
    }
    save_interactive_settings(app, session, resources);
}

fn switch_session(app: &mut App, session: &Arc<kiss_coding::AgentSession>, record: &SessionRecord) {
    let result = session.replace_manager(|current| match &record.source {
        SessionSource::Kiss(path) => kiss_coding::SessionManager::open(path),
        source => {
            let messages = session_sources::import(source)?;
            if messages.is_empty() {
                anyhow::bail!("session has no user or assistant text");
            }
            let mut manager = current.create_sibling_in(&record.cwd)?;
            manager.append_messages_with_session_info(messages, &record.title)?;
            Ok(manager)
        }
    });
    match result {
        Ok(()) => {
            adopt_session(app, session, Transcript::Rebuild);
            app.cells.push(Cell::Notice(format!(
                "resumed {} session {}",
                record.source.name(),
                record.id
            )));
        }
        Err(error) => app
            .cells
            .push(Cell::Error(format!("could not resume session: {error:#}"))),
    }
}

fn session_cells(session: &Arc<kiss_coding::AgentSession>) -> Vec<Cell> {
    let manager = session.manager.lock().unwrap();
    let mut cells = Vec::new();
    for entry in manager.branch_entries(None) {
        if let kiss_coding::SessionEntry::Message { message, .. } = entry {
            match message {
                AgentMessage::User(user) => {
                    cells.push(Cell::User(visible_user_text(&user.content.as_text())))
                }
                AgentMessage::Assistant(assistant) => {
                    let thinking: String = assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                            _ => None,
                        })
                        .collect();
                    if !thinking.is_empty() {
                        cells.push(Cell::Thinking(thinking));
                    }
                    cells.push(Cell::AssistantFinal(assistant.text()));
                }
                AgentMessage::BashExecution(bash) => cells.push(Cell::BashExecution {
                    command: bash.command.clone(),
                    output: bash.output.clone(),
                    exclude_from_context: bash.exclude_from_context,
                }),
                _ => {}
            }
        }
    }
    cells
}

fn start_login_method(
    app: &mut App,
    provider: &str,
    method: kiss_ai::auth::LoginMethod,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    use kiss_ai::auth::LoginMethod;
    match method {
        LoginMethod::ApiKey => {
            app.secret_prompt = Some(SecretPrompt {
                kind: if matches!(
                    provider,
                    "google-vertex"
                        | "cloudflare-workers-ai"
                        | "cloudflare-ai-gateway"
                        | "databricks-unity-gateway"
                        | "snowflake-cortex"
                ) {
                    SecretPromptKind::ProviderConfig(provider.into())
                } else {
                    SecretPromptKind::ApiKey(provider.into())
                },
                value: String::new(),
            });
        }
        LoginMethod::AzureEntraId => match kiss_ai::auth::store_azure_entra_id() {
            Ok(()) => app.cells.push(Cell::Notice(
                "enabled Microsoft Entra ID default credentials for Azure OpenAI".into(),
            )),
            Err(error) => app.cells.push(Cell::Error(format!(
                "could not save Azure authentication: {error:#}"
            ))),
        },
        LoginMethod::BrowserOAuth => start_provider_browser_login(app, provider, command_tx),
        LoginMethod::DeviceOAuth if provider == "github-copilot" => {
            app.secret_prompt = Some(SecretPrompt {
                kind: SecretPromptKind::GitHubEnterpriseDomain,
                value: String::new(),
            });
        }
        LoginMethod::DeviceOAuth => start_provider_device_login(app, provider, command_tx, None),
        LoginMethod::ManualOAuth if provider == "anthropic" => {
            match kiss_ai::auth::anthropic::start_authorization(&Default::default()) {
                Ok(pending) => {
                    let opened = crate::auth_flow::open_browser(&pending.authorization_url);
                    app.cells.push(Cell::Notice(if opened {
                        "finish Anthropic authentication, then paste the callback here".into()
                    } else {
                        format!(
                            "open this URL, then paste the callback here: {}",
                            pending.authorization_url
                        )
                    }));
                    app.secret_prompt = Some(SecretPrompt {
                        kind: SecretPromptKind::AnthropicManual(pending),
                        value: String::new(),
                    });
                }
                Err(error) => {
                    cancel_account_add(app);
                    app.cells.push(Cell::Error(format!(
                        "could not start Anthropic login: {error:#}"
                    )));
                }
            }
        }
        LoginMethod::ManualOAuth if provider == "openrouter" => {
            let callback = "http://127.0.0.1/oauth/callback";
            match kiss_ai::auth::openrouter::start_authorization(&Default::default(), callback) {
                Ok(pending) => {
                    let opened = crate::auth_flow::open_browser(&pending.authorization_url);
                    app.cells.push(Cell::Notice(if opened {
                        "finish OpenRouter authentication, then paste the final redirect URL".into()
                    } else {
                        format!(
                            "open this URL, then paste the final redirect URL: {}",
                            pending.authorization_url
                        )
                    }));
                    app.secret_prompt = Some(SecretPrompt {
                        kind: SecretPromptKind::OpenRouterManual(pending),
                        value: String::new(),
                    });
                }
                Err(error) => {
                    cancel_account_add(app);
                    app.cells.push(Cell::Error(format!(
                        "could not start OpenRouter login: {error:#}"
                    )));
                }
            }
        }
        LoginMethod::GoogleApplicationDefault => {
            app.secret_prompt = Some(SecretPrompt {
                kind: SecretPromptKind::GoogleApplicationDefault,
                value: String::new(),
            });
        }
        LoginMethod::AwsProfile => {
            app.secret_prompt = Some(SecretPrompt {
                kind: SecretPromptKind::AwsProfile,
                value: String::new(),
            });
        }
        LoginMethod::AwsAmbient => {
            let mut env = std::collections::BTreeMap::new();
            env.insert("AWS_AMBIENT".into(), "true".into());
            match kiss_ai::auth::store_api_key_with_env("amazon-bedrock", "", env) {
                Ok(()) => app.cells.push(Cell::Notice(
                    "enabled the AWS SDK default credential chain".into(),
                )),
                Err(error) => app.cells.push(Cell::Error(format!(
                    "could not save AWS authentication: {error:#}"
                ))),
            }
        }
        _ => {
            cancel_account_add(app);
            app.cells.push(Cell::Error(format!(
                "{} is not available for {provider}",
                method.label()
            )));
        }
    }
}

fn start_provider_browser_login(
    app: &mut App,
    provider: &str,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let provider = provider.to_string();
    let Some(cancel) = begin_login(app, format!("logging in to {provider}")) else {
        cancel_account_add(app);
        return;
    };
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let url_tx = tx.clone();
        let result = crate::auth_flow::login_browser(&provider, &cancel, move |url| {
            let opened = crate::auth_flow::open_browser(url);
            let _ = url_tx.send(CommandEvent::BrowserLoginUrl {
                url: url.to_string(),
                opened,
            });
        })
        .await
        .map_err(|error| format!("{error:#}"));
        let _ = tx.send(CommandEvent::BrowserLoginFinished { provider, result });
    });
}

fn start_provider_device_login(
    app: &mut App,
    provider: &str,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
    github_domain: Option<String>,
) {
    let provider = provider.to_string();
    let Some(cancel) = begin_login(app, format!("starting device login for {provider}")) else {
        cancel_account_add(app);
        return;
    };
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let result: Result<()> = async {
            match provider.as_str() {
                "openai-codex" => {
                    let config = kiss_ai::auth::openai_codex::OAuthConfig::default();
                    let device =
                        kiss_ai::auth::openai_codex::start_device_authorization(&config, &cancel)
                            .await?;
                    let opened = crate::auth_flow::open_browser(&device.verification_uri);
                    let _ = tx.send(CommandEvent::DeviceLoginNotice {
                        provider: provider.clone(),
                        url: device.verification_uri.clone(),
                        code: device.user_code.clone(),
                        opened,
                    });
                    let credential = kiss_ai::auth::openai_codex::finish_device_authorization(
                        &config, &device, &cancel,
                    )
                    .await?;
                    kiss_ai::auth::store_oauth(&provider, credential)
                }
                "github-copilot" => {
                    let config = kiss_ai::auth::github_copilot::OAuthConfig {
                        domain: github_domain.unwrap_or_else(|| "github.com".into()),
                    };
                    let device = kiss_ai::auth::github_copilot::start(&config, &cancel).await?;
                    let opened = crate::auth_flow::open_browser(&device.verification_uri);
                    let _ = tx.send(CommandEvent::DeviceLoginNotice {
                        provider: provider.clone(),
                        url: device.verification_uri.clone(),
                        code: device.user_code.clone(),
                        opened,
                    });
                    let credential =
                        kiss_ai::auth::github_copilot::finish(&config, &device, &cancel).await?;
                    kiss_ai::auth::store_oauth(&provider, credential)
                }
                "kimi-coding" => {
                    let config = kiss_ai::auth::kimi_coding::OAuthConfig::default();
                    let device = kiss_ai::auth::kimi_coding::start(&config, &cancel).await?;
                    let opened = crate::auth_flow::open_browser(&device.verification_uri);
                    let _ = tx.send(CommandEvent::DeviceLoginNotice {
                        provider: provider.clone(),
                        url: device.verification_uri.clone(),
                        code: device.user_code.clone(),
                        opened,
                    });
                    let credential =
                        kiss_ai::auth::kimi_coding::finish(&config, &device, &cancel).await?;
                    kiss_ai::auth::store_oauth(&provider, credential)
                }
                "meta" => {
                    let config = kiss_ai::auth::meta::OAuthConfig::default();
                    let device = kiss_ai::auth::meta::start(&config, &cancel).await?;
                    let opened = crate::auth_flow::open_browser(&device.verification_uri);
                    let _ = tx.send(CommandEvent::DeviceLoginNotice {
                        provider: provider.clone(),
                        url: device.verification_uri.clone(),
                        code: device.user_code.clone(),
                        opened,
                    });
                    let credential = kiss_ai::auth::meta::finish(&config, &device, &cancel).await?;
                    kiss_ai::auth::store_oauth(&provider, credential)
                }
                "xai" => {
                    let config = kiss_ai::auth::xai::OAuthConfig::default();
                    let device = kiss_ai::auth::xai::start(&config, &cancel).await?;
                    let opened = crate::auth_flow::open_browser(&device.verification_uri);
                    let _ = tx.send(CommandEvent::DeviceLoginNotice {
                        provider: provider.clone(),
                        url: device.verification_uri.clone(),
                        code: device.user_code.clone(),
                        opened,
                    });
                    let credential = kiss_ai::auth::xai::finish(&config, &device, &cancel).await?;
                    kiss_ai::auth::store_oauth(&provider, credential)
                }
                "radius" => {
                    let config = kiss_ai::auth::radius::OAuthConfig::default();
                    let device = kiss_ai::auth::radius::start_device(&config, &cancel).await?;
                    let opened = crate::auth_flow::open_browser(&device.verification_uri);
                    let _ = tx.send(CommandEvent::DeviceLoginNotice {
                        provider: provider.clone(),
                        url: device.verification_uri.clone(),
                        code: device.user_code.clone(),
                        opened,
                    });
                    let credential =
                        kiss_ai::auth::radius::finish_device(&config, &device, &cancel).await?;
                    kiss_ai::auth::store_oauth(&provider, credential)
                }
                _ => Err(anyhow::anyhow!(
                    "device login is not available for {provider}"
                )),
            }
        }
        .await;
        let result = result.map_err(|error| format!("{error:#}"));
        let _ = tx.send(CommandEvent::BrowserLoginFinished { provider, result });
    });
}

fn llama_configuration() -> Result<(String, Option<String>)> {
    let env = kiss_ai::auth::stored_credential_env("llama.cpp");
    let url = std::env::var("LLAMA_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| env.get("LLAMA_BASE_URL").cloned())
        .ok_or_else(|| anyhow::anyhow!("configure llama.cpp with /login llama.cpp first"))?;
    let key = kiss_ai::auth::resolve_api_key("llama.cpp", &Default::default());
    Ok((
        url.trim_end_matches('/').trim_end_matches("/v1").into(),
        key,
    ))
}

async fn llama_request(
    url: &str,
    key: Option<&str>,
    path: &str,
    method: reqwest::Method,
    body: Option<serde_json::Value>,
    cancel: &CancellationToken,
) -> Result<serde_json::Value> {
    let mut request = kiss_ai::stream::http_client().request(method, format!("{url}{path}"));
    if let Some(key) = key.filter(|key| *key != "llama.cpp") {
        request = request.bearer_auth(key);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = tokio::select! {
        response = request.send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("llama.cpp command cancelled"),
    };
    let status = response.status();
    let payload: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        let message = payload
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("llama.cpp request failed");
        anyhow::bail!("{message} (HTTP {status})");
    }
    Ok(payload)
}

fn start_llama_list(app: &mut App, command_tx: &mpsc::UnboundedSender<CommandEvent>) {
    let (url, key) = match llama_configuration() {
        Ok(configuration) => configuration,
        Err(error) => {
            app.cells.push(Cell::Error(format!("{error:#}")));
            return;
        }
    };
    let Some(cancel) = begin_command(app, "reading llama.cpp models".into()) else {
        return;
    };
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let result = llama_request(
            &url,
            key.as_deref(),
            "/models",
            reqwest::Method::GET,
            None,
            &cancel,
        )
        .await
        .and_then(|payload| {
            serde_json::from_value::<Vec<LlamaModel>>(payload["data"].clone())
                .map_err(anyhow::Error::from)
        })
        .map_err(|error| format!("could not read llama.cpp models: {error:#}"));
        let _ = tx.send(CommandEvent::LlamaModels(result));
    });
}

fn start_llama_action(
    app: &mut App,
    model: LlamaModel,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let (url, key) = match llama_configuration() {
        Ok(configuration) => configuration,
        Err(error) => {
            app.cells.push(Cell::Error(format!("{error:#}")));
            return;
        }
    };
    let unload = matches!(model.status.value.as_str(), "loaded" | "sleeping");
    let action = if unload { "unload" } else { "load" };
    let Some(cancel) = begin_command(app, format!("{action}ing {}", model.id)) else {
        return;
    };
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let result = llama_request(
            &url,
            key.as_deref(),
            &format!("/models/{action}"),
            reqwest::Method::POST,
            Some(serde_json::json!({ "model": model.id })),
            &cancel,
        )
        .await
        .map(|_| format!("{} request sent for {}", action, model.id))
        .map_err(|error| format!("llama.cpp {action} failed: {error:#}"));
        let _ = tx.send(CommandEvent::LlamaActionFinished(result));
    });
}

fn run_provider_command(app: &mut App, arguments: &str) {
    use kiss_ai::provider_config::{self, AddProvider, ProviderApi};

    let parts = arguments.split_whitespace().collect::<Vec<_>>();
    match parts.first().copied().unwrap_or("list") {
        "list" if parts.len() == 1 => match provider_config::list() {
            Ok(providers) if providers.is_empty() => app.cells.push(Cell::Notice(format!(
                "No custom providers configured.\n\n{}",
                provider_tui_usage()
            ))),
            Ok(providers) => {
                let rows = providers
                    .iter()
                    .map(|provider| {
                        format!(
                            "• {} · {} · {}\n  models: {}",
                            provider.id,
                            provider.api,
                            provider.base_url,
                            provider.models.join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                app.cells.push(Cell::Notice(format!(
                    "Custom providers\n\n{rows}\n\n{}",
                    provider_tui_usage()
                )));
            }
            Err(error) => app.cells.push(Cell::Error(format!(
                "could not list custom providers: {error:#}"
            ))),
        },
        "add" if (5..=6).contains(&parts.len()) => {
            let api = match parts[2].parse::<ProviderApi>() {
                Ok(api) => api,
                Err(error) => {
                    app.cells.push(Cell::Error(format!("{error:#}")));
                    return;
                }
            };
            let credential = parts.get(5).copied();
            let auth_provider = credential
                .and_then(|value| value.strip_prefix("auth:"))
                .map(str::to_owned);
            let api_key_env = credential
                .filter(|value| !value.starts_with("auth:") && *value != "--no-auth")
                .map(|value| value.trim_start_matches('$').to_owned());
            let id = parts[1].to_string();
            let result = provider_config::add(&AddProvider {
                id: id.clone(),
                name: None,
                base_url: parts[3].to_string(),
                api,
                model_id: parts[4].to_string(),
                model_name: None,
                api_key_env,
                auth_provider,
                no_auth: credential == Some("--no-auth"),
                headers: std::collections::BTreeMap::new(),
                reasoning: api != ProviderApi::ChatCompletions,
                context_window: 128_000,
                max_tokens: 16_384,
            });
            match result {
                Ok(_) => app.cells.push(Cell::Notice(format!(
                    "Saved provider {id}. Restart KISS to load its models."
                ))),
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("could not save provider: {error:#}"))),
            }
        }
        "remove" if parts.len() == 2 => match provider_config::remove(parts[1]) {
            Ok(true) => app.cells.push(Cell::Notice(format!(
                "Removed provider {}. Restart KISS to update the model list.",
                parts[1]
            ))),
            Ok(false) => app.cells.push(Cell::Notice(format!(
                "No custom provider named {}.",
                parts[1]
            ))),
            Err(error) => app
                .cells
                .push(Cell::Error(format!("could not remove provider: {error:#}"))),
        },
        _ => app.cells.push(Cell::Notice(provider_tui_usage().into())),
    }
}

fn provider_tui_usage() -> &'static str {
    "Usage:\n/provider add <id> <chat-completions|responses|codex> <base-url> <model> [KEY_ENV|auth:<provider>|--no-auth]\n/provider list\n/provider remove <id>"
}

fn webmcp_manager(settings: &kiss_coding::settings::WebMcpSettings) -> kiss_webmcp::WebMcpManager {
    kiss_webmcp::WebMcpManager::new(kiss_webmcp::WebMcpConfig {
        allowed_origins: settings.allowed_origins.clone(),
        disallowed_origins: settings.disallowed_origins.clone(),
        cdp: settings.cdp.clone(),
    })
}

fn start_webmcp(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    command: &str,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let manager = app.webmcp_manager.clone();
    match command {
        "" | "connect" => {
            if !begin_uncancellable_command(app, "connecting to Chrome WebMCP".into()) {
                return;
            }
            let tx = CommandTask::new(app, command_tx);
            session.remove_session_tool(kiss_webmcp::TOOL_NAME);
            tokio::spawn(async move {
                let result = manager
                    .connect()
                    .await
                    .map(|_| WebMcpCommandResult::Connected(manager.clone()))
                    .map_err(|error| format!("{error:#}"));
                let _ = tx.send(CommandEvent::WebMcpFinished(result));
            });
        }
        "list" => {
            if !begin_uncancellable_command(app, "reading Chrome WebMCP tools".into()) {
                return;
            }
            let tx = CommandTask::new(app, command_tx);
            tokio::spawn(async move {
                let result = if manager.is_connected().await {
                    Ok(WebMcpCommandResult::Listed(manager.tools().await))
                } else {
                    Err("not connected. Run /webmcp first".into())
                };
                let _ = tx.send(CommandEvent::WebMcpFinished(result));
            });
        }
        "disconnect" => {
            if !begin_uncancellable_command(app, "disconnecting Chrome WebMCP".into()) {
                return;
            }
            let tx = CommandTask::new(app, command_tx);
            session.remove_session_tool(kiss_webmcp::TOOL_NAME);
            tokio::spawn(async move {
                manager.disconnect().await;
                let _ = tx.send(CommandEvent::WebMcpFinished(Ok(
                    WebMcpCommandResult::Disconnected,
                )));
            });
        }
        _ => app.cells.push(Cell::Notice(
            "usage: /webmcp [connect|list|disconnect]".into(),
        )),
    }
}

fn run_slash_command(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    command: &str,
    args: &Args,
    resources: &mut InteractiveResources,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) -> Flow {
    let mut parts = command.splitn(2, ' ');
    let name = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim().to_string();

    match name {
        "quit" | "exit" => return Flow::Quit,
        "help" | "hotkeys" => {
            // Rebindable actions show the key keybindings.json gives them.
            let key = |action| key_label(&app.keybindings, action);
            app.cells.push(Cell::Notice(format!(
                "Input\n  Enter send · {} or Alt+Enter newline · Ctrl+W delete previous word · / commands and skills · $ skills · @ files · ! shell · !! shell outside context\nModels and effort\n  {} effort · {} select model · {} next model · {} previous model\nSession\n  {} exit · Esc cancel · Ctrl+C interrupt or clear · double Esc tree\nDisplay and queues\n  {} tools · {} thinking · {} copy · {} follow-up · {} dequeue\nWorkflows\n  /workflow <prompt> runs one task as a workflow · /workflows browses runs\n  arrows select · Enter open · Esc back · j/k scroll · f filter · p pause · x stop · r restart · s save",
                key(Action::Newline),
                key(Action::CycleThinking),
                key(Action::SelectModel),
                key(Action::CycleModel),
                key(Action::CycleModelBackward),
                key(Action::Quit),
                key(Action::ExpandTools),
                key(Action::ToggleThinking),
                key(Action::CopyLastResponse),
                key(Action::QueueFollowUp),
                key(Action::Dequeue),
            )));
        }
        "model" => {
            let dynamic = kiss_ai::Registry::is_dynamic;
            if rest.is_empty()
                || rest.split('/').next().is_some_and(dynamic)
                || (dynamic(&session.model().provider) && !rest.contains('/'))
            {
                open_model_picker(
                    app,
                    session,
                    (!rest.is_empty()).then_some(rest.as_str()),
                    command_tx,
                );
            } else if let Some((model, thinking)) = session.registry.resolve(&rest, None) {
                let access = ModelAccess::current();
                if access.allows(&model) {
                    session.set_model(model);
                    if let Some(thinking) = thinking {
                        session.set_thinking_level(thinking);
                        update_thinking_border(app, session.thinking_level());
                    }
                } else {
                    app.cells.push(Cell::Error(format!(
                        "model {rest} is not available for this GitHub Copilot account"
                    )));
                }
            } else {
                open_model_picker_with_filter(app, &session.registry, Some(&rest));
            }
        }
        "thinking" => {
            if rest.is_empty() {
                open_thinking_picker(app, session);
            } else if !session.model().reasoning {
                app.cells
                    .push(Cell::Notice("this model has no thinking levels".into()));
            } else if let Some(level) = ThinkingLevel::parse(&rest) {
                session.set_thinking_level(level);
                let applied = session.thinking_level();
                update_thinking_border(app, applied);
                app.cells.push(Cell::Notice(if applied == level {
                    format!("thinking level set to {}", level.as_str())
                } else {
                    format!(
                        "this model does not support thinking level {}, so it is set to {}",
                        level.as_str(),
                        applied.as_str()
                    )
                }));
            } else {
                app.cells.push(Cell::Error(format!(
                    "invalid thinking level: {rest}. Use off, minimal, low, medium, high, xhigh, or max"
                )));
            }
        }
        "fast"
            if rest.is_empty() && !session.fast_mode() && !session.model().supports_fast_mode() =>
        {
            app.cells.push(Cell::Error(format!(
                "fast mode is not available for {}/{}",
                session.model().provider,
                session.model().id
            )));
        }
        "fast" if rest.is_empty() => {
            let enabled = !session.fast_mode();
            session.set_fast_mode(enabled);
            let message = if enabled {
                "fast mode is on for this session. Provider costs can increase"
            } else {
                "fast mode is off for this session"
            };
            app.cells.push(Cell::Notice(message.into()));
        }
        "fast" => app.cells.push(Cell::Notice("usage: /fast".into())),
        "cache-usage" => {
            let values = rest.split_whitespace().collect::<Vec<_>>();
            let report = match values.as_slice() {
                [] => Ok(crate::cache::render_manager(
                    &session.manager.lock().unwrap(),
                    None,
                )),
                ["all"] => {
                    let session_dir = session.manager.lock().unwrap().session_dir().to_path_buf();
                    crate::cache::render_saved(&session_dir, None, None)
                }
                [provider] => Ok(crate::cache::render_manager(
                    &session.manager.lock().unwrap(),
                    Some(provider),
                )),
                ["all", provider] => {
                    let session_dir = session.manager.lock().unwrap().session_dir().to_path_buf();
                    crate::cache::render_saved(&session_dir, None, Some(provider))
                }
                _ => {
                    app.cells
                        .push(Cell::Notice("usage: /cache-usage [all] [provider]".into()));
                    return Flow::Continue;
                }
            };
            match report {
                Ok(report) => app.cells.push(Cell::Notice(report)),
                Err(error) => app.cells.push(Cell::Error(format!(
                    "could not read cache history: {error:#}"
                ))),
            }
        }
        "bug" => {
            const URL: &str = "https://github.com/racetozero/kiss/issues/new";
            if crate::auth_flow::open_browser(URL) {
                app.cells
                    .push(Cell::Notice("opened the KISS bug report page".into()));
            } else {
                app.cells.push(Cell::Notice(format!(
                    "a browser is not available. Open a bug report here: {URL}"
                )));
            }
        }
        "scoped-models" => open_scoped_models_picker(app, session, resources),
        "settings" => open_settings_picker(app, session, resources),
        "voice" => {
            if let Some(backend) = crate::voice::Backend::parse(&rest) {
                if let Err(error) = crate::voice::check_setup(backend) {
                    app.cells
                        .push(Cell::Error(format!("voice unavailable: {error:#}")));
                    return Flow::Continue;
                }
                reset_voice(app);
                resources.settings.voice_backend = Some(backend.name().into());
                save_interactive_settings(app, session, resources);
                if app.voice_mode == VoiceMode::Off {
                    app.voice_mode = VoiceMode::Hold;
                }
                app.cells.push(Cell::Notice(format!(
                    "voice backend: {}. {}",
                    backend.name(),
                    if backend == crate::voice::Backend::Local {
                        "Audio stays local."
                    } else {
                        "Microphone audio is sent to this provider."
                    }
                )));
                return Flow::Continue;
            }
            match VoiceMode::parse(&rest) {
                Some(mode) => {
                    if mode != VoiceMode::Off
                        && let Err(error) = crate::voice::check_setup(
                            crate::voice::Backend::from_settings(&resources.settings),
                        )
                    {
                        app.cells
                            .push(Cell::Error(format!("voice unavailable: {error:#}")));
                        return Flow::Continue;
                    }
                    if mode == VoiceMode::Off {
                        reset_voice(app);
                    }
                    app.voice_mode = mode;
                    let notice = match mode {
                        VoiceMode::Hold => {
                            "Voice mode enabled (hold). Hold space to record; release to transcribe. Requires a terminal with Kitty key-release support; use /voice tap otherwise."
                        }
                        VoiceMode::Tap => {
                            "Voice mode enabled (tap). Press space to start, then space to stop recording."
                        }
                        VoiceMode::Off => "Voice mode disabled.",
                    };
                    let backend = crate::voice::Backend::from_settings(&resources.settings);
                    app.cells.push(Cell::Notice(format!("{notice} Backend: {}{} Dictation language: {} (/config voice-language <code> to change).", backend.name(), if mode != VoiceMode::Off && backend != crate::voice::Backend::Local { " (microphone audio sent to provider)." } else { "." }, crate::voice::language(&resources.settings))));
                }
                None => app.cells.push(Cell::Error(
                    "usage: /voice [hold|tap|off|local|deepgram|elevenlabs]".into(),
                )),
            }
        }
        "config" => {
            if let Some(lang) = rest.strip_prefix("voice-language ") {
                if lang == "auto"
                    || (2..=3).contains(&lang.len()) && lang.bytes().all(|b| b.is_ascii_lowercase())
                {
                    resources.settings.voice_language = Some(lang.into());
                    save_interactive_settings(app, session, resources);
                    app.cells
                        .push(Cell::Notice(format!("dictation language: {lang}")));
                } else {
                    app.cells.push(Cell::Error(
                        "use a lowercase two- or three-letter language code (or auto)".into(),
                    ));
                }
            } else {
                app.cells.push(Cell::Notice(
                    "usage: /config voice-language <code> (for example en, es, or auto)".into(),
                ));
            }
        }
        "mcp" => open_mcp_picker(app, session, args, command_tx),
        "webmcp" => start_webmcp(app, session, &rest, command_tx),
        "provider" => run_provider_command(app, &rest),
        "providers" => run_provider_command(app, "list"),
        "btw" => {
            if rest.is_empty() {
                app.cells
                    .push(Cell::Notice("usage: /btw <question>".into()));
            } else {
                start_btw(app, session, rest, command_tx);
            }
        }
        "recap" => match rest.as_str() {
            "" | "now" => start_recap(app, session, false, command_tx),
            "on" => {
                set_auto_recap(app, resources, true);
                save_interactive_settings(app, session, resources);
                app.cells.push(Cell::Notice("automatic recap is on".into()));
            }
            "off" => {
                set_auto_recap(app, resources, false);
                save_interactive_settings(app, session, resources);
                app.cells
                    .push(Cell::Notice("automatic recap is off".into()));
            }
            _ => app
                .cells
                .push(Cell::Notice("usage: /recap [now|on|off]".into())),
        },
        "workflow" => {
            if !session.workflows_enabled() {
                app.cells
                    .push(Cell::Notice(WORKFLOWS_NEED_SUBAGENTS.into()));
            } else if rest.is_empty() {
                app.cells
                    .push(Cell::Notice("usage: /workflow <prompt>".into()));
            } else {
                app.cells.push(Cell::Notice(
                    "dynamic workflow is active for this turn".into(),
                ));
                deliver_prompt(
                    app,
                    session,
                    rest.clone(),
                    rest,
                    PromptMode::Workflow,
                    QueueKind::Steering,
                    command_tx,
                );
            }
        }
        "workflows" => open_workflow_runs_picker(app, session),
        "loop" => start_or_open_job(app, session, kiss_coding::iterative::JobKind::Loop, &rest),
        "autoresearch" => start_or_open_job(
            app,
            session,
            kiss_coding::iterative::JobKind::Autoresearch,
            &rest,
        ),
        "jobs" => open_job_view(app, session),
        "tree" => open_tree_picker(app, session),
        "fork" => open_fork_picker(app, session),
        "clone" => {
            if session.manager.lock().unwrap().leaf_id().is_none() {
                app.cells.push(Cell::Notice("nothing to clone yet".into()));
                return Flow::Continue;
            }
            match session
                .replace_manager(|current| current.fork_active_branch(current.leaf_id(), true))
            {
                Ok(()) => {
                    adopt_session(app, session, Transcript::Rebuild);
                    app.cells
                        .push(Cell::Notice("cloned to a new session".into()));
                }
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("clone failed: {error:#}"))),
            }
        }
        "trust" => open_trust_picker(app, session),
        "accounts" => run_accounts_command(app, &rest),
        "login" => {
            // A login in flight may belong to an account add. It must finish
            // first, or its credential would replace the active account.
            if command_blocks(app, "login") {
                return Flow::Continue;
            }
            cancel_account_add(app);
            let rest = kiss_ai::registry::canonical_provider_id(&rest);
            if rest.is_empty() {
                open_login_picker(app);
            } else {
                let provider =
                    login_providers().find(|provider| provider.eq_ignore_ascii_case(rest));
                if let Some(provider) = provider {
                    open_login_methods_picker(app, provider);
                } else {
                    open_login_picker_with_filter(app, Some(rest));
                }
            }
        }
        "logout" => {
            let rest = kiss_ai::registry::canonical_provider_id(&rest);
            if rest.is_empty() {
                open_logout_picker(app);
            } else {
                log_out(app, rest);
            }
        }
        "name" => {
            if rest.is_empty() {
                let current = session.manager.lock().unwrap().session_name();
                app.cells.push(Cell::Notice(current.map_or_else(
                    || "usage: /name <session name>".into(),
                    |name| format!("session name: {name}"),
                )));
            } else {
                match session.manager.lock().unwrap().append_session_info(&rest) {
                    Ok(_) => {
                        set_session_title(app, Some(rest.clone()));
                        app.cells
                            .push(Cell::Notice(format!("session named: {rest}")));
                    }
                    Err(error) => app
                        .cells
                        .push(Cell::Error(format!("could not name session: {error:#}"))),
                }
            }
        }
        "session" => {
            let manager = session.manager.lock().unwrap();
            let totals = session.totals();
            let file = manager
                .session_file()
                .map(|f| f.display().to_string())
                .unwrap_or_else(|| "(in-memory)".into());
            let mut user = 0;
            let mut assistant = 0;
            let mut tools = 0;
            let mut shell = 0;
            let mut compactions = 0;
            for entry in manager.entries() {
                match entry {
                    kiss_coding::SessionEntry::Message { message, .. } => match message {
                        AgentMessage::User(_) => user += 1,
                        AgentMessage::Assistant(_) => assistant += 1,
                        AgentMessage::ToolResult(_) => tools += 1,
                        AgentMessage::BashExecution(_) => shell += 1,
                        _ => {}
                    },
                    kiss_coding::SessionEntry::Compaction { .. } => compactions += 1,
                    _ => {}
                }
            }
            let model = session.model();
            let info = format!(
                "Session {}\n  name: {}\n  cwd: {}\n  file: {}\n  entries: {} (user {user}, assistant {assistant}, tools {tools}, shell {shell}, compactions {compactions})\n  model: {}/{} · thinking {}\n  tokens: input {} · output {} · cache read {} · cache write {} · total {}\n  cost: ${:.4}",
                manager.session_id(),
                manager.session_name().unwrap_or_else(|| "(unnamed)".into()),
                manager.cwd().display(),
                file,
                manager.entries().len(),
                model.provider,
                model.id,
                session.thinking_level().as_str(),
                totals.input,
                totals.output,
                totals.cache_read,
                totals.cache_write,
                totals.total_tokens,
                totals.cost.total,
            );
            drop(manager);
            app.cells.push(Cell::Notice(info));
        }
        "compact" => {
            let instructions = if rest.is_empty() { None } else { Some(rest) };
            // A refusal reaches the transcript as a compaction error event.
            tokio::spawn(session.compact(instructions));
        }
        "copy" => {
            copy_last_response(app);
        }
        "changelog" => app.cells.push(Cell::Notice(
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../CHANGELOG.md")).into(),
        )),
        "new" => match session.new_session() {
            Ok(()) => {
                adopt_session(app, session, Transcript::Empty);
                app.cells.push(Cell::Notice("started a new session".into()));
            }
            Err(error) => app
                .cells
                .push(Cell::Error(format!("could not create session: {error:#}"))),
        },
        "resume" => open_session_picker(app, session, false, String::new()),
        "import" => {
            if let Some(path) = parse_path_argument(&rest) {
                let items = vec![
                    SelectItem {
                        label: "Import into a new Kiss session".into(),
                        detail: Some(path.display().to_string()),
                        value: 0,
                    },
                    SelectItem {
                        label: "Cancel".into(),
                        detail: None,
                        value: 1,
                    },
                ];
                app.show_picker(Picker {
                    kind: PickerKind::ImportConfirm(path),
                    list: SelectList::new("Confirm session import", items, app.theme.clone()),
                });
            } else {
                app.cells
                    .push(Cell::Notice("usage: /import <file.jsonl>".into()));
            }
        }
        "export" => {
            let system = session.system_context();
            let manager = session.manager.lock().unwrap();
            let target = parse_path_argument(&rest)
                .unwrap_or_else(|| format!("session-{}.html", manager.session_id()).into());
            match kiss_coding::export::export_session(&manager, &target, Some(&system)) {
                Ok(()) => app
                    .cells
                    .push(Cell::Notice(format!("exported to {}", target.display()))),
                Err(e) => app.cells.push(Cell::Error(format!("export failed: {e:#}"))),
            }
        }
        "share" => start_share_session(app, session, command_tx),
        "update" if rest.is_empty() => start_update(app, command_tx),
        "update" => app.cells.push(Cell::Notice("usage: /update".into())),
        "reload" => reload_interactive(app, session, args, resources),
        "llama" => start_llama_list(app, command_tx),
        other => {
            // Saved workflows as commands. These run their stored script
            // directly: the orchestration is already written, so there is
            // nothing for the model to decide.
            if let Some(workflow) = resources
                .saved_workflows
                .iter()
                .find(|workflow| workflow.name == other)
            {
                start_saved_workflow(app, session, workflow.path.clone(), &rest, command_tx);
                return Flow::Continue;
            }
            // Prompt templates and skills as commands.
            if let Some(template) = resources
                .prompt_templates
                .iter()
                .find(|template| template.name == other)
            {
                let args = kiss_coding::prompts::parse_args(&rest);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let expanded = kiss_coding::prompts::expand(&template.body, &args);
                deliver_prompt(
                    app,
                    session,
                    expanded.clone(),
                    expanded,
                    PromptMode::Ordinary,
                    QueueKind::Steering,
                    command_tx,
                );
                return Flow::Continue;
            }
            if let Some(skill_name) = other.strip_prefix("skill:")
                && resources
                    .skills
                    .iter()
                    .any(|skill| skill.name == skill_name)
            {
                let display_text = format!(
                    "/skill:{skill_name}{}",
                    if rest.is_empty() {
                        String::new()
                    } else {
                        format!(" {rest}")
                    }
                );
                let submission = EditorSubmission {
                    display_text: display_text.clone(),
                    text: display_text.clone(),
                };
                match prepare_skill_input(&submission, resources) {
                    Ok(Some(model_text)) => deliver_prompt(
                        app,
                        session,
                        display_text,
                        model_text,
                        PromptMode::Ordinary,
                        QueueKind::Steering,
                        command_tx,
                    ),
                    Ok(None) => app.cells.push(Cell::Error(format!(
                        "could not invoke skill `{skill_name}`"
                    ))),
                    Err(error) => app
                        .cells
                        .push(Cell::Error(format!("could not invoke skill: {error:#}"))),
                }
                return Flow::Continue;
            }
            app.cells.push(Cell::Notice(format!(
                "unknown command: /{other} (type / to list commands)"
            )));
        }
    }
    Flow::Continue
}

fn start_btw(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    question: String,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    if let Some(BtwPanel {
        state: BtwState::Answering(cancel),
        ..
    }) = app.btw_panel.take()
    {
        cancel.cancel();
    }
    app.btw_request_id = app.btw_request_id.wrapping_add(1);
    let request_id = app.btw_request_id;
    let cancel = CancellationToken::new();
    app.btw_panel = Some(BtwPanel {
        request_id,
        question: question.clone(),
        state: BtwState::Answering(cancel.clone()),
    });
    let session = session.clone();
    let tx = command_tx.clone();
    tokio::spawn(async move {
        let result = session
            .answer_btw(&question, cancel)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = tx.send(CommandEvent::BtwFinished { request_id, result });
    });
}

fn start_recap(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    automatic: bool,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    if let Some(job) = app.recap_job.take() {
        job.cancel.cancel();
    }
    app.recap_request_id = app.recap_request_id.wrapping_add(1);
    let request_id = app.recap_request_id;
    let cancel = CancellationToken::new();
    let previous = app.recap.clone();
    app.recap_job = Some(RecapJob {
        request_id,
        automatic,
        cancel: cancel.clone(),
    });
    if automatic {
        app.idle_recap_armed = false;
    }
    let session = session.clone();
    let tx = command_tx.clone();
    tokio::spawn(async move {
        let result = session
            .generate_recap(previous.as_deref(), cancel)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = tx.send(CommandEvent::RecapFinished {
            request_id,
            automatic,
            result,
        });
    });
}

fn normalize_recap(text: &str) -> String {
    let mut recap = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lowercase = recap.to_ascii_lowercase();
    for prefix in ["※ recap:", "recap:"] {
        if lowercase.starts_with(prefix) {
            recap = recap[prefix.len()..].trim_start().to_string();
            break;
        }
    }
    if recap.chars().count() > 120 {
        recap = recap.chars().take(119).collect::<String>();
        recap.push('…');
    }
    recap
}

/// Default keybindings with the user's `keybindings.json` overrides.
fn load_keybindings() -> Keybindings {
    let mut keybindings = Keybindings::default();
    if let Some(dir) = kiss_ai::paths::agent_dir() {
        keybindings.load_overrides(&dir.join("keybindings.json"));
    }
    keybindings
}

/// The first path in a command argument, quoted or not, with `~` expanded.
fn parse_path_argument(value: &str) -> Option<std::path::PathBuf> {
    let value = value.trim_start();
    let first = value.chars().next()?;
    let path = if matches!(first, '\'' | '"') {
        let rest = &value[first.len_utf8()..];
        &rest[..rest.find(first)?]
    } else {
        value.split_whitespace().next()?
    };
    Some(kiss_ai::paths::expand_home(path))
}

fn import_session(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    source: &std::path::Path,
) {
    let fallback_cwd = session.manager.lock().unwrap().cwd().to_path_buf();
    let imported_cwd = kiss_coding::SessionManager::open(source)
        .ok()
        .map(|manager| PathBuf::from(&manager.header().cwd))
        .filter(|path| path.is_dir());
    let cwd = imported_cwd.unwrap_or_else(|| {
        app.cells.push(Cell::Notice(
            "the imported working directory is unavailable. Using the current project".into(),
        ));
        fallback_cwd
    });
    match session.replace_manager(|current| current.fork_like(source, &cwd)) {
        Ok(()) => {
            adopt_session(app, session, Transcript::Rebuild);
            app.cells.push(Cell::Notice(format!(
                "imported {} into a new Kiss session",
                kiss_ai::paths::abbreviate_home(&source.display().to_string())
            )));
        }
        Err(error) => app
            .cells
            .push(Cell::Error(format!("could not import session: {error:#}"))),
    }
}

fn start_share_session(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    // Claim the command slot before writing the transcript to a temporary
    // file, so a busy slot leaves no file behind.
    let Some(cancel) = begin_command(app, "sharing session".into()) else {
        return;
    };
    let system = session.system_context();
    let (id, jsonl, target) = {
        let manager = session.manager.lock().unwrap();
        let id = manager.session_id().to_string();
        let jsonl = match manager.to_jsonl() {
            Ok(jsonl) => jsonl,
            Err(error) => {
                app.command = None;
                app.cells.push(Cell::Error(format!(
                    "could not prepare session share: {error:#}"
                )));
                return;
            }
        };
        let target = std::env::temp_dir().join(format!(
            "kiss-session-{}-{}.html",
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        ));
        if let Err(error) = kiss_coding::export::export_html(&manager, &target, Some(&system)) {
            app.command = None;
            app.cells.push(Cell::Error(format!(
                "could not prepare session share: {error:#}"
            )));
            return;
        }
        (id, jsonl, target)
    };
    let tx = CommandTask::new(app, command_tx);
    let destination = session.settings().session_share_destination;
    tokio::spawn(async move {
        let result = share_session_async(&id, &jsonl, &target, destination, &cancel)
            .await
            .map_err(|error| format!("session share failed: {error:#}"));
        let _ = std::fs::remove_file(&target);
        let _ = tx.send(CommandEvent::ShareFinished(result));
    });
}

fn start_update(app: &mut App, command_tx: &mpsc::UnboundedSender<CommandEvent>) {
    if !begin_uncancellable_command(app, "updating KISS".into()) {
        return;
    }
    let tx = CommandTask::new(app, command_tx);
    tokio::spawn(async move {
        let result = crate::update::run()
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = tx.send(CommandEvent::UpdateFinished(result));
    });
}

async fn share_session_async(
    id: &str,
    jsonl: &str,
    html_path: &std::path::Path,
    destination: kiss_coding::settings::SessionShareDestination,
    cancel: &CancellationToken,
) -> Result<String> {
    use kiss_coding::settings::SessionShareDestination;
    let radius_token = match destination {
        SessionShareDestination::Gist => None,
        SessionShareDestination::Auto => {
            kiss_ai::auth::resolve_api_key_async("radius", &Default::default()).await?
        }
        SessionShareDestination::Radius => Some(
            kiss_ai::auth::resolve_api_key_async("radius", &Default::default())
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "sessionShareDestination is \"radius\", but you are not signed in to Radius. Run /login radius, or set sessionShareDestination to \"auto\" or \"gist\""
                    )
                })?,
        ),
    };
    if let Some(token) = radius_token {
        let gateway =
            std::env::var("RADIUS_GATEWAY").unwrap_or_else(|_| "https://radius.pi.dev".into());
        let mut url =
            reqwest::Url::parse(&format!("{}/v1/artifacts", gateway.trim_end_matches('/')))?;
        url.query_pairs_mut()
            .append_pair("visibility", "organization")
            .append_pair("title", "Kiss session");
        let response = tokio::select! {
            response = kiss_ai::stream::http_client()
                .post(url)
                .bearer_auth(token)
                .header("content-type", "application/x-ndjson")
                .body(jsonl.to_string())
                .send() => response?,
            _ = cancel.cancelled() => anyhow::bail!("share cancelled"),
        };
        let status = response.status();
        let body = response
            .json::<serde_json::Value>()
            .await
            .unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "Radius artifact upload returned HTTP {status}: {}",
                body["error"].as_str().unwrap_or("unknown error")
            );
        }
        return body
            .pointer("/artifact/canonical_url")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Radius artifact response has no canonical URL"));
    }

    let output = tokio::select! {
        output = tokio::process::Command::new("gh")
            .args([
                "gist",
                "create",
                "--filename",
                "kiss-session.html",
                "--desc",
                &format!("Kiss session {id}"),
            ])
            .arg(html_path)
            .output() => output.context("run GitHub CLI")?,
        _ = cancel.cancelled() => anyhow::bail!("share cancelled"),
    };
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(if error.is_empty() {
            "GitHub gist sharing failed".into()
        } else {
            format!("GitHub gist sharing failed: {error}")
        });
    }
    let url = String::from_utf8(output.stdout)?.trim().to_string();
    if url.is_empty() {
        anyhow::bail!("GitHub CLI returned no gist URL");
    }
    Ok(url)
}

/// Saved workflows for this project and this user.
fn discover_saved_workflows(
    session: &Arc<kiss_coding::AgentSession>,
    args: &Args,
) -> Vec<kiss_coding::workflows::SavedWorkflow> {
    let cwd = session.manager.lock().unwrap().cwd().to_path_buf();
    kiss_coding::workflows::discover(&cwd, args.project_trusted(&cwd))
}

fn reload_interactive(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    args: &Args,
    resources: &mut InteractiveResources,
) {
    let cwd = session.manager.lock().unwrap().cwd().to_path_buf();
    let reloaded = match reload_runtime(args, &cwd) {
        Ok(reloaded) => reloaded,
        Err(error) => {
            app.cells
                .push(Cell::Error(format!("reload failed: {error:#}")));
            return;
        }
    };

    for problem in reloaded.problems.iter().chain(&reloaded.settings.problems) {
        app.cells.push(Cell::Notice(problem.clone()));
    }
    resources.enabled_models =
        crate::setup::enabled_models(args, &reloaded.settings, &session.registry);
    resources.skills = reloaded.skills;
    resources.prompt_templates = reloaded.prompt_templates;
    resources.context_file_paths = reloaded.context_file_paths;
    session.remove_session_tool(kiss_webmcp::TOOL_NAME);
    let previous = std::mem::replace(
        &mut app.webmcp_manager,
        webmcp_manager(&reloaded.settings.webmcp),
    );
    tokio::spawn(async move { previous.disconnect().await });
    resources.settings = reloaded.settings.clone();
    session.reload_runtime(reloaded.settings, reloaded.system_prompt, reloaded.tools);
    resources.saved_workflows = discover_saved_workflows(session, args);

    app.apply_theme(Theme::named(resources.settings.theme.as_deref()));
    app.hide_thinking = resources.settings.hide_thinking_block;
    app.mermaid_mode = mermaid_mode(resources.settings.markdown.mermaid);
    app.md.code_indent = resources
        .settings
        .markdown
        .code_block_indent
        .clone()
        .unwrap_or_else(|| "  ".into());
    app.idle_recap_armed = resources.settings.auto_recap_enabled();
    app.last_user_activity = Instant::now();
    app.keybindings = load_keybindings();
    update_thinking_border(app, session.thinking_level());
    refresh_git_branch(app, session);
    app.cells.push(Cell::Notice(format!(
        "reloaded {} skills, {} prompts, {} workflows, and {} context files",
        resources.skills.len(),
        resources.prompt_templates.len(),
        resources.saved_workflows.len(),
        resources.context_file_paths.len()
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use kiss_coding::settings::{CompactionMode, ReasoningEffortMode};

    #[test]
    fn help_names_the_bound_keys() {
        let keybindings = Keybindings::default();
        assert_eq!(key_label(&keybindings, Action::ExpandTools), "Ctrl+R");
        assert_eq!(
            key_label(&keybindings, Action::CycleModelBackward),
            "Ctrl+Shift+P"
        );
        assert_eq!(key_label(&keybindings, Action::OpenTree), "unbound");
    }

    #[test]
    fn a_setting_row_value_is_its_position() {
        for (position, row) in SettingRow::ALL.iter().enumerate() {
            assert_eq!(*row as usize, position, "{row:?}");
        }
    }
    use std::path::PathBuf;

    fn test_app() -> App {
        let theme = Theme::dark();
        App {
            md: MarkdownRenderer::new(theme.clone()),
            editor: Editor::new(theme.clone()),
            voice_mode: VoiceMode::Off,
            voice: VoiceState::Idle,
            voice_generation: 0,
            theme,
            cells: Vec::new(),
            cell_render_cache: Vec::new(),
            keybindings: Keybindings::default(),
            startup_lines: Vec::new(),
            queue_note: None,
            spinner_frame: 0,
            session_title: None,
            terminal_title_dirty: false,
            title_generation_pending: false,
            idle_status: ProgramStatus::Idle,
            notifications: Default::default(),
            picker: None,
            model_catalog: None,
            command_menu: None,
            file_menu: None,
            file_search_request: None,
            session_picker: None,
            secret_prompt: None,
            command: None,
            next_command_id: 0,
            ctrl_c_armed: false,
            escape_armed: false,
            hide_thinking: false,
            expand_tools: false,
            mermaid_mode: MermaidMode::Streaming,
            git_branch: None,
            btw_panel: None,
            btw_request_id: 0,
            recap: None,
            recap_job: None,
            recap_request_id: 0,
            last_user_activity: Instant::now(),
            idle_recap_armed: true,
            mcp_manager: None,
            mcp_servers: Vec::new(),
            mcp_config_paths: None,
            webmcp_manager: webmcp_manager(&Default::default()),
            workflow_view: None,
            job_view: None,
            workflow_outcomes: Vec::new(),
            pending_account_add: None,
        }
    }

    fn key(key: Key) -> KeyEvent {
        KeyEvent {
            key,
            ..KeyEvent::default()
        }
    }

    fn open_menu(app: &mut App, text: &str) {
        app.editor.set_text(text);
        let session = synthetic_session();
        sync_command_menu(app, &session, &[], &[], &[]);
    }

    fn test_workflow_plan() -> Box<kiss_coding::workflows::WorkflowPlan> {
        Box::new(kiss_coding::workflows::WorkflowPlan {
            name: "test".into(),
            description: "test".into(),
            phases: Vec::new(),
            estimated_agents: Some(1),
            source: "".into(),
        })
    }

    fn synthetic_session() -> Arc<kiss_coding::AgentSession> {
        test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )))
    }

    fn test_session(manager: kiss_coding::SessionManager) -> Arc<kiss_coding::AgentSession> {
        let registry = kiss_ai::Registry::from_builtin();
        test_session_with_registry(manager, registry)
    }

    fn test_session_with_registry(
        manager: kiss_coding::SessionManager,
        registry: kiss_ai::Registry,
    ) -> Arc<kiss_coding::AgentSession> {
        let model = registry.all().first().expect("built-in model").clone();
        kiss_coding::AgentSession::new(
            manager,
            Vec::new(),
            registry,
            kiss_coding::Settings::default(),
            "test prompt".into(),
            model,
            ThinkingLevel::Off,
            None,
            Arc::new(|_| {}),
        )
    }

    fn test_resources() -> InteractiveResources {
        InteractiveResources {
            settings: kiss_coding::Settings::default(),
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            context_file_paths: Vec::new(),
            enabled_models: Vec::new(),
            saved_workflows: Vec::new(),
        }
    }

    async fn populate_file_menu(
        app: &mut App,
        session: &Arc<kiss_coding::AgentSession>,
    ) -> FileSearchService {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(tx);
        sync_file_menu(app, session, &mut file_search);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        apply_file_search_result(app, result);
        file_search
    }

    fn run_command_for_test(
        app: &mut App,
        session: &Arc<kiss_coding::AgentSession>,
        resources: &mut InteractiveResources,
        command: &str,
    ) -> Flow {
        let args = Args::parse_from(["kiss"]);
        let (tx, _rx) = mpsc::unbounded_channel();
        run_slash_command(app, session, command, &args, resources, &tx)
    }

    fn session_with_cache_usage() -> Arc<kiss_coding::AgentSession> {
        let mut manager = kiss_coding::SessionManager::in_memory(Path::new("/synthetic"));
        let mut answer = kiss_ai::AssistantMessage::empty("openai-responses", "openai", "gpt-5");
        answer.usage = kiss_ai::Usage {
            input: 25,
            cache_read: 75,
            cache_read_available: true,
            ..Default::default()
        };
        manager
            .append_message(AgentMessage::Assistant(answer))
            .unwrap();
        test_session(manager)
    }

    #[test]
    fn terminal_hosts_follow_compaction_queue_and_required_decisions() {
        use crate::terminal_hosts::State;
        let mut app = test_app();
        let session = synthetic_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut search = FileSearchService::new(tx);
        assert_eq!(terminal_host_state(&app, &session), State::Idle);
        let compaction = session.compact(None);
        assert_eq!(terminal_host_state(&app, &session), State::Working);
        drop(compaction);
        assert_eq!(terminal_host_state(&app, &session), State::Idle);
        for (stop_reason, status) in [
            (StopReason::Error, State::Error),
            (StopReason::Stop, State::Done),
        ] {
            let mut message = kiss_ai::AssistantMessage::empty("test", "test", "test");
            message.stop_reason = stop_reason;
            handle_session_event(
                &mut app,
                SessionEvent::Agent(Box::new(AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(message),
                })),
                &mut search,
                &session,
            );
            assert_eq!(terminal_host_state(&app, &session), status);
        }
        // A message that waits for the next run is not active work.
        handle_session_event(
            &mut app,
            SessionEvent::QueueUpdate {
                steering: Vec::new(),
                follow_up: vec!["next task".into()],
            },
            &mut search,
            &session,
        );
        assert_eq!(terminal_host_state(&app, &session), State::Done);
        let (reply, _answer) = tokio::sync::oneshot::channel();
        open_workflow_approval(&mut app, test_workflow_plan(), reply);
        assert_eq!(
            terminal_host_state(&app, &session),
            State::Blocked(
                kiss_tui::BlockedKind::Permission,
                "Workflow approval required"
            )
        );
    }

    #[test]
    fn footer_shows_cache_rate_before_cost() {
        let app = test_app();
        let footer =
            kiss_tui::text::strip_ansi(&app.footer(160, &session_with_cache_usage()).join("\n"));
        let cache = footer.find("75.0% cache").unwrap();
        let cost = footer.find('$').unwrap();
        assert!(cache < cost);
    }

    #[test]
    fn cache_usage_slash_command_shows_current_session_history() {
        let mut app = test_app();
        let session = session_with_cache_usage();
        let mut resources = test_resources();
        run_command_for_test(&mut app, &session, &mut resources, "cache-usage");

        assert!(matches!(
            app.cells.last(),
            Some(Cell::Notice(report))
                if report.contains("Cache rate: session")
                    && report.contains("75.0%")
                    && report.contains("Provider history")
        ));
    }

    #[test]
    fn git_branch_is_read_from_a_nested_repository() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("src/deep");
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            temp.path().join(".git/HEAD"),
            "ref: refs/heads/feature/footer\n",
        )
        .unwrap();

        assert_eq!(read_git_branch(&nested).as_deref(), Some("feature/footer"));
    }

    #[test]
    fn git_branch_supports_worktree_pointer_files_and_detached_head() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("worktree");
        let git_dir = temp.path().join("main/.git/worktrees/topic");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        assert_eq!(read_git_branch(&worktree).as_deref(), Some("topic"));

        std::fs::write(git_dir.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(read_git_branch(&worktree), None);
    }

    #[test]
    fn thinking_border_tokens_and_shell_override_match_pi() {
        let mut app = test_app();
        let cases = [
            (ThinkingLevel::Off, "thinkingOff"),
            (ThinkingLevel::Minimal, "thinkingMinimal"),
            (ThinkingLevel::Low, "thinkingLow"),
            (ThinkingLevel::Medium, "thinkingMedium"),
            (ThinkingLevel::High, "thinkingHigh"),
            (ThinkingLevel::Xhigh, "thinkingXhigh"),
            (ThinkingLevel::Max, "thinkingMax"),
        ];
        for (level, token) in cases {
            update_thinking_border(&mut app, level);
            assert_eq!(app.editor.border_color_token, token);
        }
        app.editor.set_text("!git status");
        update_thinking_border(&mut app, ThinkingLevel::Max);
        assert_eq!(app.editor.border_color_token, "bashMode");
    }

    #[test]
    fn resize_waits_for_a_quiet_period_before_accepting_the_size() {
        let started = Instant::now();
        let mut resize = ResizeState::new((100, 30));

        resize.observe((120, 40), started);
        assert!(resize.pending());
        assert!(!resize.settle(started + RESIZE_SETTLE_DELAY - Duration::from_millis(1)));
        assert_eq!(resize.accepted, (100, 30));

        assert!(resize.settle(started + RESIZE_SETTLE_DELAY));
        assert_eq!(resize.accepted, (120, 40));
        assert!(!resize.pending());
    }

    #[test]
    fn resize_burst_moves_the_deadline_and_accepts_only_the_final_size() {
        let started = Instant::now();
        let mut resize = ResizeState::new((100, 30));

        resize.observe((110, 32), started);
        resize.observe((120, 34), started + Duration::from_millis(50));
        assert!(!resize.settle(started + RESIZE_SETTLE_DELAY));
        assert_eq!(resize.accepted, (100, 30));

        assert!(resize.settle(started + Duration::from_millis(50) + RESIZE_SETTLE_DELAY));
        assert_eq!(resize.accepted, (120, 34));
        assert!(!resize.settle(started + Duration::from_secs(1)));
    }

    #[test]
    fn resize_recheck_catches_a_late_terminal_size() {
        let started = Instant::now();
        let mut resize = ResizeState::new((100, 30));
        resize.observe((120, 40), started);
        let settled = started + RESIZE_SETTLE_DELAY;
        assert!(resize.settle(settled));

        resize.recheck((121, 40), settled + RESIZE_SETTLE_DELAY);

        assert!(resize.pending());
        assert_eq!(resize.accepted, (120, 40));
        assert!(resize.settle(settled + RESIZE_SETTLE_DELAY * 2));
        assert_eq!(resize.accepted, (121, 40));
    }

    #[test]
    fn ctrl_w_reaches_the_editor_and_deletes_the_previous_word() {
        let mut app = test_app();
        app.editor.set_text("one two");
        let session = synthetic_session();
        let mut resources = test_resources();

        let flow = press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(KeyEvent::ctrl('w')),
        );

        assert!(matches!(flow, Flow::Continue));
        assert_eq!(app.editor.text(), "one ");
        assert!(app.workflow_view.is_none());
    }

    #[test]
    fn mcp_tool_title_names_server_and_tool() {
        assert_eq!(
            tool_title(
                "mcp",
                &serde_json::json!({"action": "call", "server": "linkup", "name": "linkup-search"})
            ),
            "mcp linkup.linkup-search"
        );
        assert_eq!(
            tool_title("mcp", &serde_json::json!({"action": "list"})),
            "mcp"
        );
        assert_eq!(
            tool_title("mcp", &serde_json::json!({"action": "call"})),
            "mcp"
        );
        assert_eq!(
            tool_title("read", &serde_json::json!({"path": "file.rs"})),
            "read file.rs"
        );
    }

    #[test]
    fn tool_output_hides_prefixed_control_sequences() {
        let mut app = test_app();
        app.cells.push(Cell::ToolCall {
            id: "call".into(),
            title: "bash".into(),
            output: "a\x1b[<35;59;1Mb\x1b[>0;276;0cc\x1b[=5ud\x1b=keypad\x1b>numeric".into(),
            is_error: false,
            done: true,
        });
        let session = synthetic_session();
        let rendered = kiss_tui::text::strip_ansi(&app.render(80, &session).join("\n"));
        assert!(rendered.contains("abcd"), "{rendered:?}");
        assert!(rendered.contains("keypad"), "{rendered:?}");
        assert!(rendered.contains("numeric"), "{rendered:?}");
        for parameters in ["35;59;1M", "0;276;0c", "5u"] {
            assert!(!rendered.contains(parameters), "{rendered:?}");
        }
    }

    #[test]
    fn ctrl_r_expands_collapsed_tool_output() {
        let mut app = test_app();
        app.cells.push(Cell::ToolCall {
            id: "call".into(),
            title: "mcp".into(),
            output: (1..=9)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
            done: true,
        });
        let session = synthetic_session();
        assert!(app.render(80, &session).join("\n").contains("3 more lines"));

        let mut resources = test_resources();
        press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(KeyEvent::ctrl('r')),
        );

        let expanded = kiss_tui::text::strip_ansi(&app.render(80, &session).join("\n"));
        assert!(expanded.contains("line 9"));
        assert!(!expanded.contains("more lines"));
    }

    #[test]
    fn recap_is_one_line_without_a_duplicate_prefix_and_is_bounded() {
        assert_eq!(
            normalize_recap("Recap: merged the change\nthen ran tests"),
            "merged the change then ran tests"
        );
        let recap = normalize_recap(&"x".repeat(140));
        assert_eq!(recap.chars().count(), 120);
        assert!(recap.ends_with('…'));
    }

    #[test]
    fn automatic_recap_requires_five_idle_minutes_and_an_idle_ui() {
        let mut app = test_app();
        let session = synthetic_session();
        let settings = kiss_coding::Settings::default();
        let now = Instant::now();
        app.last_user_activity = now - AUTO_RECAP_IDLE;
        assert!(
            idle_recap_deadline(&app, &session, &settings).is_some_and(|deadline| deadline <= now)
        );

        let run = session.prompt(vec![AgentMessage::user("busy")]);
        assert!(idle_recap_deadline(&app, &session, &settings).is_none());
        drop(run);
        app.idle_recap_armed = false;
        assert!(idle_recap_deadline(&app, &session, &settings).is_none());

        app.idle_recap_armed = true;
        let mut disabled = settings;
        disabled.auto_recap = Some(false);
        assert!(idle_recap_deadline(&app, &session, &disabled).is_none());
    }

    #[test]
    fn voice_command_and_keyboard_transitions() {
        assert_eq!(VoiceMode::parse(""), Some(VoiceMode::Hold));
        assert_eq!(VoiceMode::parse("tap"), Some(VoiceMode::Tap));
        assert_eq!(VoiceMode::parse("off"), Some(VoiceMode::Off));
        assert_eq!(VoiceMode::parse("oops"), None);
        let mut app = test_app();
        let session = synthetic_session();
        let mut resources = test_resources();
        run_command_for_test(&mut app, &session, &mut resources, "voice nope");
        assert!(matches!(app.cells.last(), Some(Cell::Error(_))));
        run_command_for_test(&mut app, &session, &mut resources, "voice off");
        assert_eq!(app.voice_mode, VoiceMode::Off);
        assert_eq!(app.voice_generation, 1);
        assert_eq!(
            voice_space(&InputEvent::KeyRelease(KeyEvent::char(' '))),
            Some(false)
        );
        assert_eq!(
            voice_space(&InputEvent::Key(KeyEvent::char(' '))),
            Some(true)
        );
    }

    #[test]
    fn voice_result_edits_draft_without_sending() {
        let mut app = test_app();
        let session = synthetic_session();
        let (tx, _) = mpsc::unbounded_channel();
        let mut search = FileSearchService::new(tx);
        app.editor.set_text("before ");
        app.voice = VoiceState::Transcribing {
            _recording: crate::voice::Recording::without_worker(),
            preview: String::new(),
        };
        handle_command_event(
            &mut app,
            CommandEvent::VoiceFinished {
                generation: 0,
                result: Ok("hello".into()),
            },
            &mut search,
            &session,
        );
        assert_eq!(app.editor.text(), "before hello");
        assert!(matches!(app.voice, VoiceState::Idle));
        app.voice_generation = 1;
        handle_command_event(
            &mut app,
            CommandEvent::VoiceFinished {
                generation: 0,
                result: Ok("old".into()),
            },
            &mut search,
            &session,
        );
        assert_eq!(app.editor.text(), "before hello");
    }

    #[test]
    fn slash_opens_command_menu() {
        let mut app = test_app();
        open_menu(&mut app, "/");

        let menu = app.command_menu.as_mut().expect("command menu");
        assert_eq!(
            menu.list.current().map(|item| item.label.as_str()),
            Some("settings")
        );
    }

    #[test]
    fn command_menu_filters_from_editor_text() {
        let mut app = test_app();
        open_menu(&mut app, "/mod");

        let menu = app.command_menu.as_mut().expect("command menu");
        assert_eq!(
            menu.list.current().map(|item| item.label.as_str()),
            Some("model")
        );
    }

    #[test]
    fn command_menu_filters_login_and_logout() {
        let mut app = test_app();
        open_menu(&mut app, "/log");

        let menu = app.command_menu.as_mut().expect("command menu");
        let labels: Vec<&str> = menu
            .list
            .filtered_indices()
            .into_iter()
            .map(|index| menu.list.items[index].label.as_str())
            .collect();
        assert_eq!(&labels[..2], ["login", "logout"]);
    }

    #[test]
    fn command_menu_includes_mcp_management() {
        let mut app = test_app();
        open_menu(&mut app, "/mcp");

        let menu = app.command_menu.as_mut().expect("command menu");
        assert_eq!(
            menu.list.current().map(|item| item.label.as_str()),
            Some("mcp")
        );
    }

    #[test]
    fn mcp_panel_redacts_url_credentials_and_offers_claude_style_actions() {
        let redacted =
            kiss_mcp::config::redact_url("https://example.com/mcp?apiKey=secret&region=ca");
        assert!(!redacted.contains("secret"));
        assert!(redacted.contains("region=ca"));

        let mut app = test_app();
        app.mcp_servers.push(McpPanelServer {
            name: "example".into(),
            entry: kiss_mcp::ServerEntry {
                url: Some("https://example.com/mcp?apiKey=secret".into()),
                auth: Some(kiss_mcp::AuthSetting::Mode(kiss_mcp::AuthMode::OAuth)),
                ..Default::default()
            },
            state: McpPanelState::Connected,
            tools: Vec::new(),
            authenticated: true,
            source: "KISS global".into(),
            scope: Some(kiss_mcp::config::ConfigScope::User),
        });
        open_mcp_actions(&mut app, "example");
        let picker = app.picker.as_ref().expect("MCP action picker");
        let labels = picker
            .list
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "Re-authenticate",
                "Clear authentication",
                "Reconnect",
                "Disable"
            ]
        );
    }

    #[test]
    fn command_menu_moves_selection() {
        let mut app = test_app();
        open_menu(&mut app, "/");

        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Down)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(
            app.command_menu
                .as_mut()
                .and_then(|menu| menu.list.current())
                .map(|item| item.label.as_str()),
            Some("model")
        );
        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Up)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(
            app.command_menu
                .as_mut()
                .and_then(|menu| menu.list.current())
                .map(|item| item.label.as_str()),
            Some("settings")
        );
    }

    #[test]
    fn tab_completes_without_submitting() {
        let mut app = test_app();
        open_menu(&mut app, "/mod");

        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Tab)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(app.editor.text(), "/model ");
        assert!(app.command_menu.is_none());
    }

    #[test]
    fn enter_submits_selected_command() {
        let mut app = test_app();
        open_menu(&mut app, "/mod");

        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Enter)),
            Some(CommandMenuAction::Submit(EditorSubmission {
                display_text: "/model ".to_string(),
                text: "/model ".to_string(),
            }))
        );
        assert!(app.editor.is_empty());
        assert!(app.command_menu.is_none());

        app.editor.history_prev();
        assert_eq!(app.editor.text(), "/model ");
    }

    #[test]
    fn escape_closes_menu_and_keeps_text() {
        let mut app = test_app();
        open_menu(&mut app, "/mod");

        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Escape)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(app.editor.text(), "/mod");
        assert!(app.command_menu.is_none());
    }

    #[test]
    fn command_arguments_complete_for_pi_core_commands() {
        let mut app = test_app();
        open_menu(&mut app, "/thinking hi");

        let menu = app.command_menu.as_mut().expect("thinking arguments");
        assert_eq!(
            menu.list.current().map(|item| item.label.as_str()),
            Some("high")
        );
        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Tab)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(app.editor.text(), "/thinking high");

        open_menu(&mut app, "/login anth");
        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Enter)),
            Some(CommandMenuAction::Submit(EditorSubmission {
                display_text: "/login anthropic".into(),
                text: "/login anthropic".into(),
            }))
        );
    }

    #[test]
    fn commands_without_argument_completion_close_the_menu() {
        let mut app = test_app();
        open_menu(&mut app, "/compact preserve details");

        assert!(app.command_menu.is_none());
    }

    #[test]
    fn unmatched_model_and_login_arguments_open_filtered_selectors() {
        let session = synthetic_session();
        let mut app = test_app();
        let mut resources = test_resources();

        run_command_for_test(&mut app, &session, &mut resources, "model missing-model");
        let picker = app.picker.as_ref().expect("filtered model picker");
        assert!(matches!(picker.kind, PickerKind::Model(_)));
        assert_eq!(picker.list.filter(), "missing-model");

        run_command_for_test(&mut app, &session, &mut resources, "login missing-provider");
        let picker = app.picker.as_ref().expect("filtered login picker");
        assert!(matches!(picker.kind, PickerKind::LoginProviders(_)));
        assert_eq!(picker.list.filter(), "missing-provider");
    }

    #[tokio::test]
    async fn refreshed_model_picker_selects_account_model_instead_of_startup_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("models.json");
        std::fs::write(
            &catalog,
            r#"{"providers":{"cursor":{"baseUrl":"http://localhost","api":"cursor-agent","models":[{"id":"gpt-5.5-high"},{"id":"cursor-grok-4.7-high"}]}}}"#,
        ).unwrap();
        let registry = kiss_ai::Registry::load(Some(&catalog));
        let session = synthetic_session();
        let (file_tx, _file_rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(file_tx);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let initial_model = session.model();
        for pattern in [None, Some("cursor/gpt-5.5-high")] {
            session.set_model(initial_model.clone());
            let mut app = test_app();
            let mut resources = test_resources();
            handle_command_event(
                &mut app,
                CommandEvent::ModelCatalog {
                    registry: Arc::new(registry.clone()),
                    warnings: Vec::new(),
                    pattern: pattern.map(str::to_owned),
                },
                &mut file_search,
                &session,
            );
            if pattern.is_none() {
                let picker = app.picker.take().unwrap();
                let PickerKind::Model(models) = &picker.kind else {
                    panic!("model picker")
                };
                assert!(
                    models
                        .iter()
                        .any(|model| model.id == "cursor-grok-4.7-high")
                );
                let value = models
                    .iter()
                    .position(|model| model.id == "gpt-5.5-high")
                    .unwrap();
                apply_picker_selection(
                    &mut app,
                    &session,
                    PickerSelection {
                        kind: picker.kind,
                        value,
                        filter: String::new(),
                    },
                    false,
                    &mut resources,
                    &tx,
                );
            }
            assert_eq!(session.model().provider, "cursor");
            assert_eq!(session.model().id, "gpt-5.5-high");
            for _ in 0..3 {
                open_model_picker(&mut app, &session, Some("cursor"), &tx);
                let event = rx
                    .try_recv()
                    .expect("cached model list must be ready without waiting for discovery");
                handle_command_event(&mut app, event, &mut file_search, &session);
                assert!(app.command.is_none());
                let picker = app.picker.as_ref().unwrap();
                let PickerKind::Model(models) = &picker.kind else {
                    panic!("model picker")
                };
                assert!(models.iter().any(|model| model.id == "gpt-5.5-high"));
                assert_eq!(picker.list.filter(), "cursor");
            }
            handle_command_event(
                &mut app,
                CommandEvent::BrowserLoginFinished {
                    provider: "cursor".into(),
                    result: Ok(()),
                },
                &mut file_search,
                &session,
            );
            assert!(
                app.model_catalog.is_none(),
                "a new login must clear the previous account's list"
            );
            handle_command_event(
                &mut app,
                CommandEvent::ModelCatalog {
                    registry: session.registry.clone(),
                    warnings: vec!["discovery failed".into()],
                    pattern: None,
                },
                &mut file_search,
                &session,
            );
            assert!(
                app.model_catalog.is_none(),
                "failed discovery must remain eligible for retry"
            );
        }
    }

    #[test]
    fn name_without_an_argument_shows_the_current_session_name() {
        let mut manager = kiss_coding::SessionManager::in_memory(Path::new("/synthetic"));
        manager.append_session_info("parity audit").unwrap();
        let session = test_session(manager);
        let mut app = test_app();
        let mut resources = test_resources();

        run_command_for_test(&mut app, &session, &mut resources, "name");

        assert!(matches!(
            app.cells.last(),
            Some(Cell::Notice(message)) if message == "session name: parity audit"
        ));
    }

    #[test]
    fn generated_session_title_persists_without_overwriting_manual_name() {
        let session = synthetic_session();
        let session_id = session.manager.lock().unwrap().session_id().to_string();
        let mut app = test_app();
        let mut resources = test_resources();
        let (file_tx, _file_rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(file_tx);

        app.title_generation_pending = true;
        handle_command_event(
            &mut app,
            CommandEvent::SessionTitleFinished {
                session_id: session_id.clone(),
                result: Ok("Fix login flow".into()),
            },
            &mut file_search,
            &session,
        );
        assert_eq!(app.session_title.as_deref(), Some("Fix login flow"));
        assert_eq!(
            session.manager.lock().unwrap().session_name().as_deref(),
            Some("Fix login flow")
        );

        run_command_for_test(&mut app, &session, &mut resources, "name Manual title");
        handle_command_event(
            &mut app,
            CommandEvent::SessionTitleFinished {
                session_id,
                result: Ok("Late automatic title".into()),
            },
            &mut file_search,
            &session,
        );
        assert_eq!(app.session_title.as_deref(), Some("Manual title"));
        assert_eq!(
            session.manager.lock().unwrap().session_name().as_deref(),
            Some("Manual title")
        );
    }

    #[test]
    fn fast_command_toggles_session_state_for_supported_models() {
        let session = synthetic_session();
        let model = session
            .registry
            .all()
            .iter()
            .find(|model| model.supports_fast_mode())
            .expect("fast-capable built-in model")
            .clone();
        session.set_model(model);
        let mut app = test_app();
        let mut resources = test_resources();

        run_command_for_test(&mut app, &session, &mut resources, "fast");
        assert!(session.fast_mode());
        run_command_for_test(&mut app, &session, &mut resources, "fast");
        assert!(!session.fast_mode());
        assert!(matches!(
            app.cells.last(),
            Some(Cell::Notice(message)) if message == "fast mode is off for this session"
        ));

        let mut unsupported = session.model();
        unsupported.provider = "unsupported".into();
        session.set_model(unsupported);
        run_command_for_test(&mut app, &session, &mut resources, "fast");
        assert!(!session.fast_mode());
        assert!(matches!(app.cells.last(), Some(Cell::Error(_))));
    }

    #[tokio::test]
    async fn a_command_task_that_ends_without_a_result_frees_the_slot() {
        let session = synthetic_session();
        let mut app = test_app();
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        assert!(begin_uncancellable_command(&mut app, "working".into()));
        let task = CommandTask::new(&app, &command_tx);
        // A panicking task drops its handle the same way.
        drop(task);
        let event = command_rx.recv().await.expect("release");
        let (file_tx, _file_rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(file_tx);
        handle_command_event(&mut app, event, &mut file_search, &session);
        assert!(app.command.is_none());
        assert!(begin_uncancellable_command(&mut app, "next".into()));
    }

    #[tokio::test]
    async fn update_command_uses_the_shared_updater() {
        let session = synthetic_session();
        let mut app = test_app();
        let mut resources = test_resources();
        let args = Args::parse_from(["kiss"]);
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        run_slash_command(
            &mut app,
            &session,
            "update",
            &args,
            &mut resources,
            &command_tx,
        );
        assert_eq!(
            app.command.as_ref().map(|command| command.status.as_str()),
            Some("updating KISS")
        );
        let event = command_rx.recv().await.expect("update result");

        let (file_tx, _file_rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(file_tx);
        handle_command_event(&mut app, event, &mut file_search, &session);
        assert!(app.command.is_none());
        assert!(
            matches!(app.cells.last(), Some(Cell::Error(error)) if error.starts_with("update failed:") && error.contains("not available in debug builds"))
        );
    }

    #[test]
    fn rebuilt_settings_picker_keeps_filter_selection_and_new_value() {
        let session = synthetic_session();
        let mut app = test_app();
        let mut resources = test_resources();
        open_settings_picker(&mut app, &session, &resources);

        let picker = app.picker.as_mut().expect("settings picker");
        picker.list.set_filter("automatic".into());
        assert!(
            picker
                .list
                .select_value(SettingRow::AutomaticRecap as usize)
        );

        resources.settings.auto_recap = Some(false);
        reopen_settings_picker(
            &mut app,
            &session,
            &resources,
            "automatic".into(),
            SettingRow::AutomaticRecap as usize,
        );

        let picker = app.picker.as_mut().expect("rebuilt settings picker");
        assert_eq!(picker.list.filter(), "automatic");
        assert_eq!(picker.list.current().map(|item| item.value), Some(12));
        assert_eq!(
            picker
                .list
                .current()
                .and_then(|item| item.detail.as_deref()),
            Some("off")
        );
    }

    #[test]
    fn decision_settings_cycle_available_models_and_can_always_disable() {
        for (compaction, reasoning, provider, next_compaction, next_reasoning) in [
            (
                CompactionMode::Summary,
                ReasoningEffortMode::Fixed,
                "none",
                None,
                None,
            ),
            (
                CompactionMode::Summary,
                ReasoningEffortMode::Fixed,
                "typesafe",
                Some(CompactionMode::Jev),
                Some(ReasoningEffortMode::Jev),
            ),
            (
                CompactionMode::Jev,
                ReasoningEffortMode::Jev,
                "none",
                Some(CompactionMode::Summary),
                Some(ReasoningEffortMode::Fixed),
            ),
            (
                CompactionMode::Summary,
                ReasoningEffortMode::Fixed,
                "liquid",
                Some(CompactionMode::D1Ultra),
                Some(ReasoningEffortMode::D1Ultra),
            ),
            (
                CompactionMode::D1Ultra,
                ReasoningEffortMode::D1Ultra,
                "liquid",
                Some(CompactionMode::D1),
                Some(ReasoningEffortMode::D1),
            ),
            (
                CompactionMode::D1,
                ReasoningEffortMode::D1,
                "liquid",
                Some(CompactionMode::Summary),
                Some(ReasoningEffortMode::Fixed),
            ),
            (
                CompactionMode::Summary,
                ReasoningEffortMode::Fixed,
                "cloudflare-workers-ai",
                Some(CompactionMode::ClefFlash),
                Some(ReasoningEffortMode::ClefFlash),
            ),
            (
                CompactionMode::ClefFlash,
                ReasoningEffortMode::ClefFlash,
                "cloudflare-workers-ai",
                Some(CompactionMode::Clef),
                Some(ReasoningEffortMode::Clef),
            ),
            (
                CompactionMode::Clef,
                ReasoningEffortMode::Clef,
                "none",
                Some(CompactionMode::Summary),
                Some(ReasoningEffortMode::Fixed),
            ),
        ] {
            assert_eq!(
                compaction.next_available(|model| model.provider() == provider),
                next_compaction
            );
            assert_eq!(
                reasoning.next_available(|model| model.provider() == provider),
                next_reasoning
            );
        }
    }

    #[test]
    fn settings_picker_shows_dynamic_reasoning_as_fixed_by_default() {
        let session = synthetic_session();
        let mut app = test_app();
        let resources = test_resources();

        open_settings_picker(&mut app, &session, &resources);
        let picker = app.picker.as_mut().expect("settings picker");
        assert!(
            picker
                .list
                .select_value(SettingRow::DynamicReasoning as usize)
        );
        assert_eq!(picker.list.current().unwrap().label, "Dynamic reasoning");
        assert_eq!(
            picker.list.current().unwrap().detail.as_deref(),
            Some("fixed")
        );
    }

    #[test]
    fn settings_picker_shows_desktop_notifications_opt_in() {
        let session = synthetic_session();
        let app = test_app();
        let mut resources = test_resources();
        for (enabled, expected) in [(false, "off"), (true, "on · unavailable in this session")] {
            resources.settings.desktop_notifications = enabled;
            let mut picker = settings_picker(&app, &session, &resources);
            assert!(
                picker
                    .list
                    .select_value(SettingRow::DesktopNotifications as usize)
            );
            let item = picker.list.current().unwrap();
            assert_eq!(item.label, "Desktop notifications");
            assert_eq!(item.detail.as_deref(), Some(expected));
        }
    }

    #[test]
    fn settings_picker_shows_the_opt_in_subagent_toggle() {
        let session = synthetic_session();
        let mut app = test_app();
        let mut resources = test_resources();

        open_settings_picker(&mut app, &session, &resources);
        let picker = app.picker.as_mut().expect("settings picker");
        assert!(picker.list.select_value(SettingRow::Subagents as usize));
        assert_eq!(picker.list.current().unwrap().label, "Subagents");
        assert_eq!(
            picker.list.current().unwrap().detail.as_deref(),
            Some("off")
        );

        resources.settings.subagents.enabled = true;
        reopen_settings_picker(
            &mut app,
            &session,
            &resources,
            String::new(),
            SettingRow::Subagents as usize,
        );
        let picker = app.picker.as_mut().unwrap();
        assert_eq!(picker.list.current().map(|item| item.value), Some(13));
        assert_eq!(picker.list.current().unwrap().detail.as_deref(), Some("on"));
    }

    #[test]
    fn rebuilt_scoped_models_picker_keeps_filter_and_selection() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("models.json");
        std::fs::write(
            &catalog,
            r#"{"providers":{"local":{"baseUrl":"http://localhost","api":"openai-completions","models":[{"id":"local-model"}]}}}"#,
        )
        .unwrap();
        let registry = kiss_ai::Registry::load(Some(&catalog));
        let selected_value = registry
            .all()
            .iter()
            .position(|model| model.provider == "local")
            .unwrap();
        let selected = registry.all()[selected_value].clone();
        let session = test_session_with_registry(
            kiss_coding::SessionManager::in_memory(Path::new("/synthetic")),
            registry,
        );
        let mut app = test_app();
        let mut resources = test_resources();

        resources.enabled_models.push(selected);
        reopen_scoped_models_picker(
            &mut app,
            &session,
            &resources,
            String::new(),
            selected_value,
        );

        let picker = app.picker.as_mut().expect("scoped model picker");
        assert_eq!(
            picker.list.current().map(|item| item.value),
            Some(selected_value)
        );
        assert!(
            picker
                .list
                .current()
                .is_some_and(|item| item.label.starts_with("[x] "))
        );
    }

    #[test]
    fn saved_default_model_stays_in_an_active_scope() {
        let session = synthetic_session();
        let mut resources = test_resources();
        let first = session.registry.all()[0].clone();
        let selected = session
            .registry
            .all()
            .iter()
            .find(|model| model.provider != first.provider || model.id != first.id)
            .unwrap()
            .clone();
        resources.enabled_models.push(first);

        assert!(keep_default_model_in_scope(&mut resources, &selected));
        assert!(!keep_default_model_in_scope(&mut resources, &selected));
        assert_eq!(
            resources
                .enabled_models
                .iter()
                .filter(|model| model.provider == selected.provider && model.id == selected.id)
                .count(),
            1
        );
        assert!(
            resources
                .settings
                .enabled_models
                .as_ref()
                .unwrap()
                .contains(&format!("{}/{}", selected.provider, selected.id))
        );
    }

    #[test]
    fn file_prefix_requires_a_token_boundary_and_supports_quotes() {
        let mut editor = Editor::new(Theme::dark());
        editor.set_text("review @src/ma");
        assert_eq!(file_completion_prefix(&editor).as_deref(), Some("@src/ma"));
        editor.set_text("review @\"folder with/sp");
        assert_eq!(
            file_completion_prefix(&editor).as_deref(),
            Some("@\"folder with/sp")
        );
        editor.set_text("mail@example.com");
        assert_eq!(file_completion_prefix(&editor), None);
        for wrapper in ["(", "[", "{", "<", "`"] {
            editor.set_text(&format!("review {wrapper}@src/ma"));
            assert_eq!(file_completion_prefix(&editor).as_deref(), Some("@src/ma"));
        }
        editor.set_text("review @app/[slug]/page");
        assert_eq!(
            file_completion_prefix(&editor).as_deref(),
            Some("@app/[slug]/page")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_menu_is_recursive_and_respects_gitignore() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(temp.path().join("ignored.rs"), "ignored").unwrap();
        std::fs::write(nested.join("main.rs"), "fn main() {}\n").unwrap();
        let session = test_session(kiss_coding::SessionManager::in_memory(temp.path()));
        let mut app = test_app();
        app.editor.set_text("read @main");

        populate_file_menu(&mut app, &session).await;

        let menu = app.file_menu.as_ref().expect("file menu");
        assert!(
            menu.values
                .iter()
                .any(|value| value.path == "src/nested/main.rs")
        );
        assert!(menu.values.iter().all(|value| value.path != "ignored.rs"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_menu_preserves_home_and_parent_scopes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("work/one/two");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("home-only.rs"), "home").unwrap();
        std::fs::write(temp.path().join("work/ancestor-only.rs"), "ancestor").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(tx);
        let mut app = test_app();
        app.editor.set_text("read @~/home-only");
        let home_query = FileSearchQuery::from_prefix(&cwd, Some(&home), "@~/home-only").unwrap();
        let ticket = file_search.search(home_query);
        app.file_search_request = Some(ticket.request_id);
        apply_file_search_result(
            &mut app,
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
        let home_values = &app.file_menu.as_ref().unwrap().values;
        assert!(
            home_values
                .iter()
                .any(|value| value.path == "~/home-only.rs")
        );

        app.editor.set_text("read @../../ancestor-only");
        let parent_query =
            FileSearchQuery::from_prefix(&cwd, Some(&home), "@../../ancestor-only").unwrap();
        let ticket = file_search.search(parent_query);
        app.file_search_request = Some(ticket.request_id);
        apply_file_search_result(
            &mut app,
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
        let parent_values = &app.file_menu.as_ref().unwrap().values;
        assert!(
            parent_values
                .iter()
                .any(|value| value.path == "../../ancestor-only.rs")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_menu_uses_base_name_as_the_primary_label() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("a/long/path");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("complete_file_name.rs"), "file").unwrap();
        let session = test_session(kiss_coding::SessionManager::in_memory(temp.path()));
        let mut app = test_app();
        app.editor.set_text("read @complete_file");

        populate_file_menu(&mut app, &session).await;

        let menu = app.file_menu.as_ref().expect("file menu");
        let item = menu
            .list
            .items
            .iter()
            .find(|item| item.label == "complete_file_name.rs")
            .expect("full file name label");
        assert_eq!(
            item.detail.as_deref(),
            Some("a/long/path/complete_file_name.rs")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_menu_enter_inserts_path_without_submitting() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
        let session = test_session(kiss_coding::SessionManager::in_memory(temp.path()));
        let mut app = test_app();
        app.editor.set_text("read @main");
        let mut file_search = populate_file_menu(&mut app, &session).await;

        assert!(handle_file_menu_key(
            &mut app,
            &session,
            &mut file_search,
            &key(Key::Enter)
        ));
        assert_eq!(app.editor.text(), "read @main.rs ");
        assert!(app.editor.history().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_menu_keeps_quotes_for_paths_with_spaces() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("folder with");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("file name.rs"), "file").unwrap();
        let session = test_session(kiss_coding::SessionManager::in_memory(temp.path()));
        let mut app = test_app();
        app.editor.set_text("read @\"folder with/file");
        let mut file_search = populate_file_menu(&mut app, &session).await;

        assert!(handle_file_menu_key(
            &mut app,
            &session,
            &mut file_search,
            &key(Key::Enter)
        ));
        assert_eq!(app.editor.text(), "read @\"folder with/file name.rs\" ");
        assert!(app.editor.history().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_menu_keeps_old_results_while_a_new_search_runs() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("alpha.rs"), "alpha").unwrap();
        std::fs::write(temp.path().join("beta.rs"), "beta").unwrap();
        let session = test_session(kiss_coding::SessionManager::in_memory(temp.path()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(tx);
        let mut app = test_app();

        app.editor.set_text("read @alpha");
        sync_file_menu(&mut app, &session, &mut file_search);
        apply_file_search_result(
            &mut app,
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
        assert!(app.file_menu.is_some());

        app.editor.set_text("read @beta");
        sync_file_menu(&mut app, &session, &mut file_search);

        let menu = app.file_menu.as_ref().expect("old file menu");
        assert_eq!(menu.prefix, "@alpha");
        assert!(menu.values.iter().any(|value| value.path == "alpha.rs"));
        assert!(app.file_search_request.is_some());
        assert!(handle_file_menu_key(
            &mut app,
            &session,
            &mut file_search,
            &key(Key::Enter)
        ));
        assert_eq!(app.editor.text(), "read @beta");

        apply_file_search_result(
            &mut app,
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
        assert!(
            app.file_menu
                .as_ref()
                .unwrap()
                .values
                .iter()
                .any(|value| value.path == "beta.rs")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_menu_height_is_stable_while_the_user_types() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["foo.py", "foobar.py", "food.rs", "fool.md"] {
            std::fs::write(temp.path().join(name), name).unwrap();
        }
        let session = test_session(kiss_coding::SessionManager::in_memory(temp.path()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(tx);
        let mut app = test_app();
        let mut heights = Vec::new();

        for text in [
            "read @foo",
            "read @foob",
            "read @foobar.py",
            "read @foobar.pyz",
        ] {
            app.editor.set_text(text);
            sync_command_menu(&mut app, &session, &[], &[], &[]);
            sync_file_menu(&mut app, &session, &mut file_search);
            heights.push(app.render(80, &session).len());
            apply_file_search_result(
                &mut app,
                tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
            heights.push(app.render(80, &session).len());
        }

        assert!(
            heights.windows(2).all(|pair| pair[0] == pair[1]),
            "{heights:?}"
        );
        let rendered = kiss_tui::text::strip_ansi(&app.render(80, &session).join("\n"));
        assert!(rendered.contains("No matching files"));
        assert!(
            !handle_file_menu_key(&mut app, &session, &mut file_search, &key(Key::Enter)),
            "enter must submit when no file matches"
        );
    }

    #[test]
    fn stale_file_search_result_does_not_replace_the_current_menu() {
        let mut app = test_app();
        app.editor.set_text("read @new");
        app.file_search_request = Some(2);

        apply_file_search_result(
            &mut app,
            FileSearchResult {
                request_id: 1,
                prefix: "@old".into(),
                values: vec![FileSearchMatch {
                    path: "old.rs".into(),
                    is_directory: false,
                    quoted: false,
                }],
                index_limited: false,
            },
        );

        assert!(app.file_menu.is_none());
        assert_eq!(app.file_search_request, Some(2));
        assert!(app.file_search_request.is_some());
    }

    #[test]
    fn context_usage_cache_is_invalidated_by_session_changes() {
        let mut manager = kiss_coding::SessionManager::in_memory(Path::new("/synthetic"));
        manager
            .append_message(AgentMessage::user("small message"))
            .unwrap();
        let session = test_session(manager);
        let initial = session.context_usage().0;
        assert_eq!(initial, 4);
        assert_eq!(session.context_usage().0, initial);

        session
            .manager
            .lock()
            .unwrap()
            .append_message(AgentMessage::user("larger message ".repeat(500)))
            .unwrap();

        assert_eq!(session.context_usage().0, 1879);
    }

    #[test]
    fn delta_only_updates_build_the_streaming_cells() {
        let session = synthetic_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(tx);
        let mut app = test_app();
        let mut partial = kiss_ai::AssistantMessage::empty("test", "test", "test");

        handle_session_event(
            &mut app,
            SessionEvent::Agent(Box::new(AgentEvent::MessageStart {
                message: AgentMessage::Assistant(partial.clone()),
            })),
            &mut file_search,
            &session,
        );
        for delta in ["hel", "lo"] {
            handle_session_event(
                &mut app,
                SessionEvent::Agent(Box::new(AgentEvent::MessageUpdate {
                    assistant_event: Box::new(AssistantEvent::TextDelta {
                        content_index: 0,
                        delta: delta.into(),
                    }),
                })),
                &mut file_search,
                &session,
            );
        }
        let _ = app.render(80, &session);
        assert_eq!(app.cell_render_cache.len(), 1);
        handle_session_event(
            &mut app,
            SessionEvent::Agent(Box::new(AgentEvent::MessageUpdate {
                assistant_event: Box::new(AssistantEvent::ThinkingDelta {
                    content_index: 1,
                    delta: "think".into(),
                }),
            })),
            &mut file_search,
            &session,
        );
        assert!(app.cell_render_cache.is_empty());
        partial.content = vec![ContentBlock::Text {
            text: "hello!".into(),
            text_signature: None,
        }];
        partial.stop_reason = StopReason::Stop;
        handle_session_event(
            &mut app,
            SessionEvent::Agent(Box::new(AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(partial),
            })),
            &mut file_search,
            &session,
        );

        assert!(matches!(app.cells.first(), Some(Cell::Thinking(text)) if text == "think"));
        assert!(matches!(app.cells.get(1), Some(Cell::AssistantFinal(text)) if text == "hello!"));
    }

    #[test]
    fn verified_workflow_cancellation_is_shown_after_assistant_text() {
        let session = synthetic_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(tx);
        let mut app = test_app();
        app.cells
            .push(Cell::AssistantFinal("the workflow completed".into()));

        handle_session_event(
            &mut app,
            SessionEvent::WorkflowOutcome {
                run: None,
                name: "audit".into(),
                status: kiss_coding::WorkflowTurnStatus::Cancelled,
            },
            &mut file_search,
            &session,
        );
        handle_session_event(
            &mut app,
            SessionEvent::Agent(Box::new(AgentEvent::AgentEnd {
                messages: Vec::new(),
            })),
            &mut file_search,
            &session,
        );

        assert!(matches!(
            app.cells.last(),
            Some(Cell::Notice(text))
                if text == "verified workflow `audit` was cancelled. No agents ran"
        ));
    }

    #[test]
    fn dynamic_reasoning_change_uses_the_matching_input_border_color() {
        let session = synthetic_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(tx);
        let mut app = test_app();

        handle_session_event(
            &mut app,
            SessionEvent::ReasoningEffortChanged {
                decision_model: "d1-ultra · Liquid".into(),
                level: ThinkingLevel::High,
                generations: 5,
            },
            &mut file_search,
            &session,
        );

        assert!(matches!(
            app.cells.last(),
            Some(Cell::ReasoningEffort {
                level: ThinkingLevel::High,
                generations: 5,
                decision_model,
            }) if decision_model == "d1-ultra · Liquid"
        ));
        let expected = app.theme.fg(
            thinking_color_token(ThinkingLevel::High),
            "◆ d1-ultra · Liquid changed reasoning to high for 5 generations",
        );
        assert!(
            app.render(80, &session)
                .iter()
                .any(|line| line.as_ref() == expected)
        );
    }

    #[test]
    fn dynamic_reasoning_fallback_is_visible_in_the_conversation() {
        let session = synthetic_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(tx);
        let mut app = test_app();

        handle_session_event(
            &mut app,
            SessionEvent::ReasoningEffortFallback {
                level: ThinkingLevel::Medium,
                reason: "request timed out".into(),
            },
            &mut file_search,
            &session,
        );

        assert!(matches!(
            app.cells.last(),
            Some(Cell::Error(text))
                if text == "Decision model could not select reasoning: request timed out. Using saved effort medium"
        ));
        assert!(
            app.render(120, &session)
                .iter()
                .any(|line| line.contains("Using saved effort medium"))
        );
    }

    #[test]
    fn command_menu_includes_templates_and_skills() {
        let templates = vec![kiss_coding::prompts::PromptTemplate {
            name: "review".to_string(),
            description: "Review this change".to_string(),
            argument_hint: None,
            body: "Review $@".to_string(),
            path: PathBuf::from("review.md"),
        }];
        let skills = vec![kiss_coding::skills::Skill {
            name: "release".to_string(),
            description: "Prepare a release".to_string(),
            file_path: PathBuf::from("release/SKILL.md"),
            disable_model_invocation: false,
        }];

        let labels: Vec<String> = command_items(&templates, &skills, &[])
            .0
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert!(labels.contains(&"review".to_string()));
        assert!(labels.contains(&"release".to_string()));
    }

    #[test]
    fn command_menu_includes_saved_workflows_without_shadowing_core_commands() {
        let workflows = vec![
            kiss_coding::workflows::SavedWorkflow {
                name: "audit-routes".into(),
                description: "Audit every route".into(),
                path: PathBuf::from("audit-routes.js"),
                from_project: true,
            },
            kiss_coding::workflows::SavedWorkflow {
                name: "workflow".into(),
                description: "Must not shadow /workflow".into(),
                path: PathBuf::from("workflow.js"),
                from_project: true,
            },
        ];
        let labels = command_items(&[], &[], &workflows)
            .0
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();

        assert!(labels.contains(&"audit-routes".to_string()));
        assert_eq!(
            labels
                .iter()
                .filter(|name| name.as_str() == "workflow")
                .count(),
            1
        );
    }

    #[test]
    fn workflow_keywords_match_only_complete_words_and_phrases() {
        for text in [
            "ultracode this audit",
            "Please USE A WORKFLOW for this",
            "run a workflow now",
            "do this as a workflow",
            "start a dynamic workflow",
        ] {
            assert!(workflow_trigger(text).is_some(), "{text}");
        }
        for text in [
            "myworkflow is a variable",
            "workflow_name is a setting",
            "src/workflows/run.rs",
            "mydynamic workflow label",
            "prefix_ultracode_suffix",
        ] {
            assert!(workflow_trigger(text).is_none(), "{text}");
        }
    }

    #[test]
    fn dollar_completion_opens_inline_and_preserves_prompt() {
        let mut app = test_app();
        let session = synthetic_session();
        let skills = vec![kiss_coding::skills::Skill {
            name: "release".into(),
            description: "Prepare a release".into(),
            file_path: "release/SKILL.md".into(),
            disable_model_invocation: false,
        }];
        app.editor.set_text("hello $");
        sync_command_menu(&mut app, &session, &[], &skills, &[]);

        assert_eq!(
            app.command_menu
                .as_mut()
                .and_then(|menu| menu.list.current())
                .map(|item| item.label.as_str()),
            Some("release")
        );
        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Enter)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(app.editor.text(), "hello $release ");
    }

    #[test]
    fn slash_skill_completion_uses_direct_name_and_allows_a_chain() {
        let mut app = test_app();
        let session = synthetic_session();
        let skills = vec![
            kiss_coding::skills::Skill {
                name: "review".into(),
                description: "Review code".into(),
                file_path: "review/SKILL.md".into(),
                disable_model_invocation: false,
            },
            kiss_coding::skills::Skill {
                name: "tests".into(),
                description: "Test code".into(),
                file_path: "tests/SKILL.md".into(),
                disable_model_invocation: false,
            },
        ];
        app.editor.set_text("/rev");
        sync_command_menu(&mut app, &session, &[], &skills, &[]);
        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Enter)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(app.editor.text(), "/review ");

        app.editor.insert("/te");
        sync_command_menu(&mut app, &session, &[], &skills, &[]);
        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Tab)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(app.editor.text(), "/review /tests ");
    }

    #[test]
    fn skill_preparation_hides_bodies_from_display_and_expands_pastes() {
        let dir = tempfile::tempdir().unwrap();
        let review_path = dir.path().join("review.md");
        let tests_path = dir.path().join("tests.md");
        std::fs::write(&review_path, "PRIVATE REVIEW BODY").unwrap();
        std::fs::write(&tests_path, "PRIVATE TEST BODY").unwrap();
        let mut resources = test_resources();
        resources.skills = vec![
            kiss_coding::skills::Skill {
                name: "review".into(),
                description: "Review code".into(),
                file_path: review_path,
                disable_model_invocation: false,
            },
            kiss_coding::skills::Skill {
                name: "tests".into(),
                description: "Test code".into(),
                file_path: tests_path,
                disable_model_invocation: false,
            },
        ];
        let mut editor = Editor::new(Theme::dark());
        editor.insert("$review /tests ");
        editor.paste("line one\nline two");
        let submission = editor.take_submission();

        let model_text = prepare_skill_input(&submission, &resources)
            .unwrap()
            .expect("skill invocation");

        assert_eq!(
            submission.display_text,
            "$review /tests [Pasted text #1 17 chars]"
        );
        assert!(!submission.display_text.contains("PRIVATE"));
        assert!(model_text.contains("PRIVATE REVIEW BODY"));
        assert!(model_text.contains("PRIVATE TEST BODY"));
        assert!(model_text.ends_with("<user_request>\nline one\nline two\n</user_request>"));

        let inline = EditorSubmission {
            display_text: "please inspect this $review".into(),
            text: "please inspect this $review".into(),
        };
        let inline_model_text = prepare_skill_input(&inline, &resources)
            .unwrap()
            .expect("inline skill invocation");
        assert!(inline_model_text.contains("PRIVATE REVIEW BODY"));
        assert!(
            inline_model_text
                .ends_with("<user_request>\nplease inspect this $review\n</user_request>")
        );

        let stored = stored_user_text(&submission.display_text, &model_text);
        assert_eq!(visible_user_text(&stored), submission.display_text);
        assert!(stored.contains("PRIVATE REVIEW BODY"));
    }

    #[test]
    fn slash_command_and_template_names_take_priority_over_skills() {
        let mut resources = test_resources();
        resources.skills = vec![
            kiss_coding::skills::Skill {
                name: "model".into(),
                description: "Conflicting skill".into(),
                file_path: "model/SKILL.md".into(),
                disable_model_invocation: false,
            },
            kiss_coding::skills::Skill {
                name: "review".into(),
                description: "Conflicting template skill".into(),
                file_path: "review/SKILL.md".into(),
                disable_model_invocation: false,
            },
        ];
        resources
            .prompt_templates
            .push(kiss_coding::prompts::PromptTemplate {
                name: "review".into(),
                description: String::new(),
                argument_hint: None,
                body: "template".into(),
                path: "review.md".into(),
            });

        let names = slash_invocable_skills(&resources)
            .into_iter()
            .map(|skill| skill.name)
            .collect::<Vec<_>>();
        assert!(!names.contains(&"model".to_string()));
        assert!(!names.contains(&"review".to_string()));
    }

    #[test]
    fn path_commands_accept_quoted_paths() {
        assert_eq!(
            parse_path_argument("\"folder with spaces/session.jsonl\" ignored"),
            Some("folder with spaces/session.jsonl".into())
        );
        assert_eq!(
            parse_path_argument("folder/session.jsonl ignored"),
            Some("folder/session.jsonl".into())
        );
        assert_eq!(
            parse_path_argument("~/x.html"),
            Some(kiss_ai::paths::expand_home("~/x.html"))
        );
        assert!(!parse_path_argument("~/x.html").unwrap().starts_with("~"));
    }

    #[test]
    fn accounts_commands_open_pickers_instead_of_usage_errors() {
        let mut app = test_app();
        let session = test_session(kiss_coding::SessionManager::in_memory(
            std::path::Path::new("/tmp/project"),
        ));
        let mut resources = test_resources();
        let picker = |app: &App| match app.picker.as_ref().map(|picker| &picker.kind) {
            Some(PickerKind::AccountProviders(providers, action)) => {
                Some((providers.clone(), *action))
            }
            _ => None,
        };

        for command in ["accounts", "accounts list"] {
            run_command_for_test(&mut app, &session, &mut resources, command);
            let (providers, action) = picker(&app).expect("provider picker");
            assert_eq!(action, AccountAction::Manage);
            for featured in FEATURED_ACCOUNT_PROVIDERS {
                assert!(providers.iter().any(|provider| provider == featured));
            }
        }

        // A missing or unknown provider opens the picker filtered by the text.
        run_command_for_test(&mut app, &session, &mut resources, "accounts add codex");
        assert_eq!(picker(&app).unwrap().1, AccountAction::Add);
        assert_eq!(app.picker.as_ref().unwrap().list.filter(), "codex");
        assert!(app.pending_account_add.is_none());

        // Selecting a provider in manage mode lists its accounts with an add row.
        let providers = {
            run_command_for_test(&mut app, &session, &mut resources, "accounts");
            picker(&app).unwrap().0
        };
        let index = providers.iter().position(|p| p == "meta").unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let kind = app.picker.take().unwrap().kind;
        apply_picker_selection(
            &mut app,
            &session,
            PickerSelection {
                kind,
                value: index,
                filter: String::new(),
            },
            false,
            &mut resources,
            &tx,
        );
        let picker = app.picker.as_ref().expect("accounts picker");
        assert!(
            matches!(&picker.kind, PickerKind::Accounts(provider, AccountAction::Manage) if provider == "meta")
        );
        assert!(
            picker
                .list
                .items
                .iter()
                .any(|item| item.value == ADD_ACCOUNT)
        );
    }

    #[test]
    fn account_helpers_resolve_providers_and_format_cooldowns() {
        assert_eq!(account_provider("openai-codex"), Some("openai-codex"));
        assert_eq!(account_provider("codex"), None);
        assert_eq!(format_duration_secs(42), "42s");
        assert_eq!(format_duration_secs(900), "15m");
        assert_eq!(format_duration_secs(7980), "2h13m");
        assert_eq!(format_duration_secs(7200), "2h");
    }

    #[test]
    fn adding_an_account_offers_only_per_account_login_methods() {
        let mut app = test_app();
        app.pending_account_add = Some(("google-vertex".into(), None));
        open_login_methods_picker(&mut app, "google-vertex");
        let Some(Picker {
            kind: PickerKind::LoginMethods(_, choices),
            ..
        }) = app.picker.as_ref()
        else {
            panic!("login methods picker");
        };
        assert!(!choices.iter().any(|choice| matches!(
            choice,
            LoginChoice::Method(kiss_ai::auth::LoginMethod::GoogleApplicationDefault)
        )));
        assert!(choices.iter().any(|choice| matches!(
            choice,
            LoginChoice::Method(kiss_ai::auth::LoginMethod::ApiKey)
        )));
    }

    #[test]
    fn pending_account_add_is_consumed_by_a_failed_login_for_that_provider() {
        let mut app = test_app();
        app.pending_account_add = Some(("openai-codex".into(), None));
        complete_account_add(&mut app, "anthropic", false);
        assert!(app.pending_account_add.is_some());
        complete_account_add(&mut app, "openai-codex", false);
        assert!(app.pending_account_add.is_none());
    }

    #[test]
    fn login_without_provider_opens_complete_provider_selector() {
        let mut app = test_app();
        let session = test_session(kiss_coding::SessionManager::in_memory(
            std::path::Path::new("/tmp/project"),
        ));
        let mut resources = test_resources();

        run_command_for_test(&mut app, &session, &mut resources, "login");
        let Some(Picker {
            kind: PickerKind::LoginProviders(providers),
            ..
        }) = app.picker.as_ref()
        else {
            panic!("login picker");
        };
        assert!(providers.contains(&"openai-codex".to_string()));
        assert!(providers.contains(&"anthropic".to_string()));
        assert!(providers.contains(&"typesafe".to_string()));
        assert!(providers.contains(&"liquid".to_string()));
        assert!(providers.contains(&"llama.cpp".to_string()));
    }

    #[test]
    fn openai_login_offers_subscription_and_api_key() {
        let mut app = test_app();
        let session = test_session(kiss_coding::SessionManager::in_memory(
            std::path::Path::new("/tmp/project"),
        ));
        let mut resources = test_resources();

        run_command_for_test(&mut app, &session, &mut resources, "login openai");
        let Some(Picker {
            kind: PickerKind::LoginMethods(provider, choices),
            ..
        }) = app.picker.as_ref()
        else {
            panic!("login method picker");
        };
        assert_eq!(provider, "openai");
        assert!(matches!(
            choices.first(),
            Some(LoginChoice::Method(
                kiss_ai::auth::LoginMethod::BrowserOAuth
            ))
        ));
        assert!(choices.iter().any(|choice| matches!(
            choice,
            LoginChoice::Method(kiss_ai::auth::LoginMethod::ApiKey)
        )));
    }

    #[test]
    fn export_jsonl_and_import_switch_active_session() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut manager =
            kiss_coding::SessionManager::create(&cwd, Some(temp.path().join("sessions"))).unwrap();
        manager
            .append_message(AgentMessage::user("portable"))
            .unwrap();
        let expected_id = manager.session_id().to_string();
        let session = test_session(manager);
        let mut app = test_app();
        let mut resources = test_resources();
        let exported = temp.path().join("exported.jsonl");

        run_command_for_test(
            &mut app,
            &session,
            &mut resources,
            &format!("export {}", exported.display()),
        );
        assert!(exported.is_file());

        let replacement = session.manager.lock().unwrap().create_sibling().unwrap();
        session.replace_manager(|_| Ok(replacement)).unwrap();
        assert_ne!(session.manager.lock().unwrap().session_id(), expected_id);
        run_command_for_test(
            &mut app,
            &session,
            &mut resources,
            &format!("import {}", exported.display()),
        );
        let mut picker = app.picker.take().expect("import confirmation");
        let value = picker.list.current().unwrap().value;
        let (tx, _rx) = mpsc::unbounded_channel();
        apply_picker_selection(
            &mut app,
            &session,
            PickerSelection {
                kind: picker.kind,
                value,
                filter: String::new(),
            },
            false,
            &mut resources,
            &tx,
        );
        let manager = session.manager.lock().unwrap();
        assert_ne!(manager.session_id(), expected_id);
        assert_ne!(manager.session_file(), Some(exported.as_path()));
        let messages = manager.build_session_context().messages;
        assert!(matches!(
            messages.first(),
            Some(AgentMessage::User(user)) if user.content.as_text() == "portable"
        ));
    }

    #[test]
    fn expands_native_home_paths_and_keeps_literal_names() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(kiss_ai::paths::expand_home("~"), home);
        assert_eq!(
            kiss_ai::paths::expand_home("~/sessions with spaces"),
            home.join("sessions with spaces")
        );
        if cfg!(windows) {
            assert_eq!(
                kiss_ai::paths::expand_home("~\\sessions with spaces"),
                home.join("sessions with spaces")
            );
        }
        assert_eq!(
            kiss_ai::paths::expand_home("~alice"),
            PathBuf::from("~alice")
        );
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_transcript_and_context() {
        let session = synthetic_session();
        let mut app = test_app();
        let paragraph = "A deterministic assistant paragraph has **styled text**, a source/path.rs name, and enough words to wrap across terminal rows.";
        for index in 0..360 {
            app.cells.push(Cell::AssistantFinal(format!(
                "## Result {index}\n\n{paragraph}\n\n{paragraph}"
            )));
        }
        let rendered_rows = app.render(100, &session).len();
        kiss_bench::measure(
            "app_transcript_full",
            11,
            2,
            &format!("{rendered_rows}_logical_rows"),
            || app.render(100, &session).len(),
        );

        let _active_run = session.prompt(vec![AgentMessage::user("busy")]);
        kiss_bench::measure(
            "app_spinner_full",
            11,
            2,
            &format!("{rendered_rows}_logical_rows_spinner_only"),
            || {
                app.spinner_frame = app.spinner_frame.wrapping_add(1);
                app.render(100, &session).len()
            },
        );

        let mut manager = kiss_coding::SessionManager::in_memory(Path::new("/synthetic"));
        let message = "context accounting benchmark text ".repeat(120);
        for index in 0..225 {
            manager
                .append_message(AgentMessage::user(format!("{index}: {message}")))
                .unwrap();
        }
        let context_session = test_session(manager);
        kiss_bench::measure(
            "session_context_usage_225",
            15,
            20,
            "225_messages_approximately_900kb",
            || context_session.context_usage(),
        );
    }

    #[test]
    #[ignore = "release-mode large transcript benchmark; KISS_PERF_MIB=2, 10, or 40"]
    fn benchmark_shared_transcript_pipeline() {
        use std::io::{self, Write};
        #[derive(Default)]
        struct Sink {
            bytes: usize,
            max_write: usize,
            hash: u64,
        }
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes += bytes.len();
                self.max_write = self.max_write.max(bytes.len());
                // Timing uses a counting sink; verify bytes separately below.
                if self.hash != 0 {
                    for byte in bytes {
                        self.hash = (self.hash ^ u64::from(*byte)).wrapping_mul(1099511628211);
                    }
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mib: usize = std::env::var("KISS_PERF_MIB")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        assert!(
            [0, 2, 10, 40].contains(&mib),
            "Use KISS_PERF_MIB=0, 2, 10, or 40; 0 selects one small cell."
        );
        let session = synthetic_session();
        let mut app = test_app();
        let paragraph = "A deterministic assistant paragraph has **styled text**, a source/path.rs name, and enough words to wrap across terminal rows.\n\n";
        let text = paragraph.repeat(8192 / paragraph.len() + 1);
        for _ in 0..(mib * 1024 * 1024).div_ceil(text.len()).max(1) {
            app.cells.push(Cell::AssistantFinal(text.clone()));
        }
        let rows = app.render(100, &session);
        let mut sink = Sink {
            hash: 14695981039346656037,
            ..Default::default()
        };
        kiss_tui::renderer::DiffRenderer::new()
            .render_shared_frame(&rows, 100, 40, &mut sink)
            .unwrap();
        println!(
            "KISS_OUTPUT\tmib={mib}\trows={}\tbytes={}\tmax_write={}\thash={}",
            rows.len(),
            sink.bytes,
            sink.max_write,
            sink.hash
        );
        kiss_bench::measure(
            "app_cached_transcript_large",
            15,
            2,
            &format!("{mib}mib_source"),
            || app.render(100, &session).len(),
        );
        kiss_bench::measure(
            "first_full_app_renderer",
            15,
            1,
            &format!("{mib}mib_cached_app_counting_sink_no_terminal_io"),
            || {
                let rows = app.render(100, &session);
                let mut sink = Sink::default();
                kiss_tui::renderer::DiffRenderer::new()
                    .render_shared_frame(&rows, 100, 40, &mut sink)
                    .unwrap();
                sink.bytes
            },
        );
        let mut renderer = kiss_tui::renderer::DiffRenderer::new();
        renderer
            .render_shared_frame(&rows, 100, 40, &mut Sink::default())
            .unwrap();
        let _active_run = session.prompt(vec![AgentMessage::user("busy")]);
        kiss_bench::measure(
            "spinner_app_renderer",
            15,
            2,
            &format!("{mib}mib_cached_app_counting_sink_no_terminal_io"),
            || {
                app.spinner_frame = app.spinner_frame.wrapping_add(1);
                let rows = app.render(100, &session);
                let mut sink = Sink::default();
                renderer
                    .render_shared_frame(&rows, 100, 40, &mut sink)
                    .unwrap();
                sink.bytes
            },
        );
    }

    #[test]
    #[ignore = "release-mode scalar session benchmark; KISS_PERF_MIB=2, 10, or 40"]
    fn benchmark_scalar_session_reads() {
        let mib: usize = std::env::var("KISS_PERF_MIB")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        assert!(
            [0, 2, 10, 40].contains(&mib),
            "Use KISS_PERF_MIB=0, 2, 10, or 40; 0 selects one message."
        );
        let mut manager = kiss_coding::SessionManager::in_memory(Path::new("/synthetic"));
        manager
            .append_model_change("anthropic", "test-model")
            .unwrap();
        manager
            .append_thinking_level_change(kiss_ai::ThinkingLevel::High)
            .unwrap();
        let records = (mib * 64).max(1);
        for _ in 0..records {
            manager
                .append_message(AgentMessage::user("x".repeat(16 * 1024)))
                .unwrap();
        }
        assert_eq!(manager.context_message_count(), records);
        assert_eq!(
            manager.session_settings(),
            (
                Some(("anthropic".into(), "test-model".into())),
                Some(kiss_ai::ThinkingLevel::High)
            )
        );
        kiss_bench::measure(
            "session_message_count",
            15,
            2,
            &format!("{mib}mib_{records}_messages"),
            || manager.context_message_count(),
        );
        kiss_bench::measure(
            "session_saved_settings",
            15,
            2,
            &format!("{mib}mib_{records}_messages"),
            || manager.session_settings(),
        );
    }
    #[tokio::test]
    async fn shell_command_merges_output_and_keeps_context_mode() {
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let task = CommandTask::with_id(0, &tx);
        let result = run_shell_command(
            Some("sh"),
            None,
            "printf stdout; printf stderr >&2; exit 7",
            temp.path(),
            true,
            &cancel,
            &task,
        )
        .await
        .unwrap();

        assert!(result.output.contains("stdout"));
        assert!(result.output.contains("stderr"));
        assert_eq!(result.exit_code, Some(7));
        assert!(result.exclude_from_context);
        assert!(!result.cancelled);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_command_can_be_cancelled() {
        // The second command ignores SIGTERM, and so does its `sleep` child.
        for command in [
            "printf started; sleep 30",
            "trap '' TERM; printf started; sleep 30",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let (tx, _rx) = mpsc::unbounded_channel();
            let task = CommandTask::with_id(0, &tx);
            let future = run_shell_command(
                Some("sh"),
                None,
                command,
                temp.path(),
                false,
                &cancel,
                &task,
            );
            tokio::pin!(future);
            tokio::select! {
                result = &mut future => panic!("shell ended before cancellation: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => cancel.cancel(),
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), future)
                .await
                .unwrap_or_else(|_| panic!("cancelled shell must stop: {command}"))
                .unwrap();
            assert!(result.cancelled, "{command}");
        }
    }

    fn press(
        app: &mut App,
        session: &Arc<kiss_coding::AgentSession>,
        resources: &mut InteractiveResources,
        event: InputEvent,
    ) -> Flow {
        let args = Args::parse_from(["kiss"]);
        let (tx, _rx) = mpsc::unbounded_channel();
        let (search_tx, _search_rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(search_tx);
        handle_input(
            app,
            session,
            &event,
            &args,
            resources,
            &mut file_search,
            &tx,
        )
    }

    fn apply_event(app: &mut App, session: &Arc<kiss_coding::AgentSession>, event: AgentEvent) {
        let (search_tx, _search_rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(search_tx);
        handle_session_event(
            app,
            SessionEvent::Agent(Box::new(event)),
            &mut file_search,
            session,
        );
    }

    fn apply_command_event(
        app: &mut App,
        session: &Arc<kiss_coding::AgentSession>,
        event: CommandEvent,
    ) {
        let (search_tx, _search_rx) = mpsc::unbounded_channel();
        let mut search = FileSearchService::new(search_tx);
        handle_command_event(app, event, &mut search, session);
    }

    fn visible_frame(app: &mut App, session: &Arc<kiss_coding::AgentSession>) -> String {
        kiss_tui::text::strip_ansi(&app.render(200, session).join("\n"))
    }

    #[test]
    fn cancelling_a_transcription_returns_the_space_key_to_the_editor() {
        let mut app = test_app();
        let session = synthetic_session();
        let mut resources = test_resources();
        app.voice_mode = VoiceMode::Tap;
        app.voice = VoiceState::Recording {
            recording: crate::voice::Recording::without_worker(),
            preview: String::new(),
        };
        stop_voice(&mut app);
        press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(key(Key::Escape)),
        );

        // A draft that starts with a slash always takes a literal space.
        app.editor.set_text("/voice");
        press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(KeyEvent::char(' ')),
        );
        assert_eq!(app.editor.text(), "/voice ");
    }

    #[tokio::test]
    async fn escape_stops_a_retry_wait() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let registry = kiss_ai::Registry::from_builtin();
        let model = registry.all().first().expect("built-in model").clone();
        let session = kiss_coding::AgentSession::new(
            kiss_coding::SessionManager::in_memory(Path::new("/synthetic")),
            Vec::new(),
            registry,
            {
                let mut settings = kiss_coding::Settings::default();
                settings.compaction.enabled = false;
                settings.retry.base_delay_ms = 60_000;
                settings
            },
            "test prompt".into(),
            model,
            ThinkingLevel::Off,
            None,
            Arc::new(move |event| {
                let _ = event_tx.send(event);
            }),
        );
        session.set_stream_fn(Some(Arc::new(|_, _, _| {
            let mut message = kiss_ai::AssistantMessage::empty("test", "test", "test");
            message.stop_reason = StopReason::Error;
            message.error_message = Some("HTTP 503".into());
            let (sink, stream) = kiss_ai::EventStream::channel();
            sink.error(message);
            stream
        })));
        let run = tokio::spawn({
            let session = session.clone();
            async move { session.prompt(vec![AgentMessage::user("hello")]).await }
        });

        let mut app = test_app();
        let mut resources = test_resources();
        let (search_tx, _search_rx) = mpsc::unbounded_channel();
        let mut file_search = FileSearchService::new(search_tx);
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                .await
                .expect("the session reaches its retry wait")
                .expect("the session stays open");
            let retrying = matches!(event, SessionEvent::Retry { .. });
            handle_session_event(&mut app, event, &mut file_search, &session);
            if retrying {
                break;
            }
        }
        press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(key(Key::Escape)),
        );
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("Esc must stop the retry wait")
            .unwrap();
    }

    #[test]
    fn parallel_tool_output_stays_with_its_own_call() {
        let mut app = test_app();
        let session = synthetic_session();
        for (id, command) in [("first", "ls"), ("second", "pwd")] {
            apply_event(
                &mut app,
                &session,
                AgentEvent::ToolExecutionStart {
                    tool_call_id: id.into(),
                    tool_name: "bash".into(),
                    args: serde_json::json!({ "command": command }),
                },
            );
        }
        apply_event(
            &mut app,
            &session,
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "first".into(),
                tool_name: "bash".into(),
                result: kiss_agent::ToolResult::text("output of ls"),
                is_error: false,
            },
        );
        let Cell::ToolCall { title, output, .. } = &app.cells[0] else {
            panic!("the first cell is a tool call");
        };
        assert!(title.contains("ls"), "{title:?}");
        assert_eq!(output, "output of ls");
        let Cell::ToolCall { output, done, .. } = &app.cells[1] else {
            panic!("the second cell is a tool call");
        };
        assert!(output.is_empty(), "{output:?}");
        assert!(!done);
    }

    #[test]
    fn a_notice_during_a_stream_does_not_freeze_the_answer() {
        let mut app = test_app();
        let session = synthetic_session();
        let mut resources = test_resources();
        let mut message = kiss_ai::AssistantMessage::empty("test", "test", "test");
        apply_event(
            &mut app,
            &session,
            AgentEvent::MessageStart {
                message: AgentMessage::Assistant(message.clone()),
            },
        );
        let delta = |delta: &str| AgentEvent::MessageUpdate {
            assistant_event: Box::new(AssistantEvent::TextDelta {
                content_index: 0,
                delta: delta.into(),
            }),
        };
        apply_event(&mut app, &session, delta("hel"));
        run_command_for_test(&mut app, &session, &mut resources, "help");
        apply_event(&mut app, &session, delta("lo"));
        assert!(matches!(&app.cells[0], Cell::AssistantStreaming(text) if text == "hello"));

        message.content = vec![ContentBlock::Text {
            text: "hello".into(),
            text_signature: None,
        }];
        message.stop_reason = StopReason::Stop;
        apply_event(
            &mut app,
            &session,
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(message),
            },
        );
        assert!(matches!(&app.cells[0], Cell::AssistantFinal(text) if text == "hello"));
    }

    #[tokio::test]
    async fn a_second_background_command_does_not_replace_the_first() {
        let mut app = test_app();
        let session = synthetic_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        start_shell_passthrough(&mut app, &session, "sleep 30".into(), &tx);
        start_shell_passthrough(&mut app, &session, "sleep 31".into(), &tx);
        let frame = visible_frame(&mut app, &session);
        assert!(frame.contains("running !sleep 30 (esc"), "{frame}");
        assert!(!frame.contains("running !sleep 31 (esc"), "{frame}");
        assert_eq!(app.editor.text(), "!sleep 31");

        // A cancelled command keeps the slot until its task reports back, so
        // its late finish event cannot clear a newer command.
        let mut resources = test_resources();
        press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(key(Key::Escape)),
        );
        app.editor.clear();
        start_shell_passthrough(&mut app, &session, "sleep 32".into(), &tx);
        let frame = visible_frame(&mut app, &session);
        assert!(frame.contains("stopping"), "{frame}");
        assert!(!frame.contains("running !sleep 32 (esc"), "{frame}");

        apply_command_event(
            &mut app,
            &session,
            CommandEvent::ShellFinished(Err("cancelled".into())),
        );
        start_shell_passthrough(&mut app, &session, "sleep 32".into(), &tx);
        assert!(visible_frame(&mut app, &session).contains("running !sleep 32 (esc"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_abandoned_command_cannot_finish_a_newer_command() {
        let mut app = test_app();
        let directory = tempfile::tempdir().unwrap();
        let session = test_session(kiss_coding::SessionManager::in_memory(directory.path()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        start_shell_passthrough(&mut app, &session, "sleep 30".into(), &tx);
        assert!(cancel_command(&mut app));
        assert!(cancel_command(&mut app));
        start_shell_passthrough(&mut app, &session, "sleep 31".into(), &tx);
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the cancelled command must report completion")
            .unwrap();
        apply_command_event(&mut app, &session, event);
        let frame = visible_frame(&mut app, &session);
        assert!(frame.contains("running !sleep 31 (esc"), "{frame}");
        cancel_command(&mut app);
    }

    #[test]
    fn a_new_session_request_keeps_an_active_prompt_in_its_original_session() {
        let directory = tempfile::tempdir().unwrap();
        let session = test_session(
            kiss_coding::SessionManager::create(
                directory.path(),
                Some(directory.path().join("sessions")),
            )
            .unwrap(),
        );
        let original = session.manager.lock().unwrap().session_id().to_string();
        let run = session.prompt(vec![AgentMessage::user("first")]);
        let mut app = test_app();
        let mut resources = test_resources();
        run_command_for_test(&mut app, &session, &mut resources, "new");
        assert_eq!(session.manager.lock().unwrap().session_id(), original);
        assert!(
            matches!(app.cells.last(), Some(Cell::Error(error)) if error.contains("active prompt"))
        );
        drop(run);
        run_command_for_test(&mut app, &session, &mut resources, "new");
        assert_ne!(session.manager.lock().unwrap().session_id(), original);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_completion_does_not_write_into_a_replacement_session() {
        let directory = tempfile::tempdir().unwrap();
        let session = test_session(
            kiss_coding::SessionManager::create(
                directory.path(),
                Some(directory.path().join("sessions")),
            )
            .unwrap(),
        );
        let mut app = test_app();
        let mut resources = test_resources();
        let (tx, mut rx) = mpsc::unbounded_channel();
        start_shell_passthrough(
            &mut app,
            &session,
            "printf started; while [ ! -e release-shell ]; do sleep 0.01; done".into(),
            &tx,
        );
        let started = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        apply_command_event(&mut app, &session, started);
        run_command_for_test(&mut app, &session, &mut resources, "new");
        std::fs::write(directory.path().join("release-shell"), "").unwrap();
        let finished = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        apply_command_event(&mut app, &session, finished);
        let messages = session
            .manager
            .lock()
            .unwrap()
            .build_session_context()
            .messages;
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, AgentMessage::BashExecution(_))),
            "a shell result belongs to the session that started it"
        );
    }

    #[test]
    fn a_workflow_report_on_an_idle_session_joins_the_history_without_a_queue() {
        let mut app = test_app();
        let session = synthetic_session();
        apply_command_event(
            &mut app,
            &session,
            CommandEvent::WorkflowFinished {
                name: "audit".into(),
                result: Ok("all clear".into()),
            },
        );
        assert!(session.reclaim_queued().is_empty());
        let context = session.manager.lock().unwrap().build_session_context();
        assert!(context.messages.iter().any(|message| {
            matches!(message, AgentMessage::User(user) if user.content.as_text().contains("all clear"))
        }));
    }

    #[tokio::test]
    async fn a_background_mcp_check_does_not_end_a_running_command() {
        let mut app = test_app();
        let session = synthetic_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        start_shell_passthrough(&mut app, &session, "sleep 30".into(), &tx);
        apply_command_event(
            &mut app,
            &session,
            CommandEvent::McpServerChecked(Box::new(McpPanelServer {
                name: "example".into(),
                entry: Default::default(),
                state: McpPanelState::Connected,
                tools: Vec::new(),
                authenticated: false,
                source: "KISS global".into(),
                scope: Some(kiss_mcp::config::ConfigScope::User),
            })),
        );
        assert!(visible_frame(&mut app, &session).contains("running !sleep 30 (esc"));
    }

    #[test]
    fn a_workflow_approval_is_answered_over_the_job_view() {
        let mut app = test_app();
        let session = synthetic_session();
        let mut resources = test_resources();
        app.job_view = Some(JobView::new(session.iterative_jobs().unwrap()));
        let (reply, mut answer) = tokio::sync::oneshot::channel();
        open_workflow_approval(&mut app, test_workflow_plan(), reply);
        let frame = visible_frame(&mut app, &session);
        assert!(frame.contains("Run it"), "{frame}");
        press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(key(Key::Enter)),
        );
        assert!(answer.try_recv().is_ok());
    }

    #[test]
    fn a_new_session_does_not_inherit_the_previous_result() {
        use crate::terminal_hosts::State;
        let mut app = test_app();
        let session = synthetic_session();
        let mut resources = test_resources();
        let mut message = kiss_ai::AssistantMessage::empty("test", "test", "test");
        message.stop_reason = StopReason::Error;
        apply_event(
            &mut app,
            &session,
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(message),
            },
        );
        assert_eq!(terminal_host_state(&app, &session), State::Error);
        run_command_for_test(&mut app, &session, &mut resources, "new");
        assert_eq!(terminal_host_state(&app, &session), State::Idle);
    }

    #[tokio::test]
    async fn login_during_a_running_command_keeps_the_account_being_added() {
        let mut app = test_app();
        let session = synthetic_session();
        let mut resources = test_resources();
        let (tx, _rx) = mpsc::unbounded_channel();
        start_shell_passthrough(&mut app, &session, "sleep 30".into(), &tx);
        app.pending_account_add = Some(("anthropic".into(), None));
        run_command_for_test(&mut app, &session, &mut resources, "login anthropic");
        assert_eq!(
            app.pending_account_add,
            Some(("anthropic".into(), None)),
            "the login in flight must still finish the account add"
        );
    }

    #[test]
    fn closing_the_login_picker_abandons_the_account_add() {
        let mut app = test_app();
        let session = synthetic_session();
        let mut resources = test_resources();
        app.pending_account_add = Some(("google-vertex".into(), None));
        open_login_methods_picker(&mut app, "google-vertex");
        press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(key(Key::Escape)),
        );
        assert_eq!(app.pending_account_add, None);
    }

    #[test]
    fn a_login_prompt_that_rejects_its_input_abandons_the_account_add() {
        let mut app = test_app();
        let session = synthetic_session();
        let mut resources = test_resources();
        app.pending_account_add = Some(("openai".into(), None));
        app.secret_prompt = Some(SecretPrompt {
            kind: SecretPromptKind::ApiKey("openai".into()),
            value: String::new(),
        });
        press(
            &mut app,
            &session,
            &mut resources,
            InputEvent::Key(key(Key::Enter)),
        );
        assert!(app.secret_prompt.is_none());
        assert_eq!(app.pending_account_add, None);
    }
}
#[test]
fn startup_editor_keeps_draft_until_setup_is_ready() {
    let mut editor = Editor::new(Theme::dark());
    assert_eq!(
        handle_startup_input(&mut editor, &InputEvent::Paste("fix tests".into())),
        StartupInput::Continue
    );
    assert_eq!(editor.text(), "fix tests");
    assert_eq!(
        handle_startup_input(
            &mut editor,
            &InputEvent::Key(KeyEvent {
                key: Key::Enter,
                ctrl: false,
                alt: false,
                shift: false,
            })
        ),
        StartupInput::Submit
    );
    assert_eq!(editor.text(), "fix tests");
}
