//! Interactive mode: transcript + editor + footer over the differential
//! renderer, with slash commands, queueing, and pickers.

use crate::args::Args;
use crate::file_search::{FileSearchMatch, FileSearchQuery, FileSearchResult, FileSearchService};
use crate::setup::{Startup, build_startup, reload_runtime};
use crate::slash_commands;
use anyhow::{Context as _, Result};
use kiss_agent::{AgentEvent, AgentMessage};
use kiss_ai::{AssistantEvent, ContentBlock, StopReason, ThinkingLevel, Transport};
use kiss_coding::session_runner::SessionEvent;
use kiss_coding::settings::{MermaidRendering, QueueMode};
use kiss_tui::{
    Action, Component, DiffRenderer, Editor, EditorSubmission, InputDecoder, InputEvent, Key,
    KeyEvent, Keybindings, MarkdownRenderer, MermaidMode, SelectItem, SelectList,
    StreamingMarkdownCache, Terminal, Theme,
};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One rendered transcript cell.
enum Cell {
    User(String),
    AssistantStreaming(String),
    AssistantFinal(String),
    Thinking(String),
    ToolCall {
        title: String,
        output: String,
        is_error: bool,
        done: bool,
    },
    Notice(String),
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
    lines: Vec<String>,
    streaming: Option<StreamingMarkdownCache>,
}

struct App {
    theme: Theme,
    md: MarkdownRenderer,
    cells: Vec<Cell>,
    cell_render_cache: Vec<Option<CachedCellLines>>,
    editor: Editor,
    keybindings: Keybindings,
    startup_lines: Vec<String>,
    queue_note: Option<String>,
    working: bool,
    spinner_frame: usize,
    picker: Option<Picker>,
    command_menu: Option<CommandCompletion>,
    file_menu: Option<FileCompletion>,
    file_search_request: Option<u64>,
    file_search_pending: bool,
    secret_prompt: Option<SecretPrompt>,
    command_status: Option<String>,
    command_cancel: Option<CancellationToken>,
    ctrl_c_armed: bool,
    escape_armed: bool,
    hide_thinking: bool,
    expand_tools: bool,
    mermaid_mode: MermaidMode,
    git_branch: Option<String>,
    btw_panel: Option<BtwPanel>,
    btw_request_id: u64,
    recap: Option<String>,
    recap_loading: bool,
    recap_automatic: bool,
    recap_cancel: Option<CancellationToken>,
    recap_request_id: u64,
    last_user_activity: Instant,
    idle_recap_armed: bool,
    mcp_manager: Option<kiss_mcp::McpManager>,
    mcp_servers: Vec<McpPanelServer>,
    mcp_config_paths: Option<kiss_mcp::config::ConfigPaths>,
}

struct BtwPanel {
    request_id: u64,
    question: String,
    answer: Option<String>,
    error: Option<String>,
    cancel: Option<CancellationToken>,
}

enum PickerKind {
    Model,
    Thinking,
    ScopedModels,
    Session(Vec<PathBuf>, bool),
    Tree,
    TreeSummary(String),
    Fork,
    LoginProviders(Vec<String>),
    LoginMethods(String, Vec<LoginChoice>),
    Logout(Vec<String>),
    Settings,
    Trust,
    ImportConfirm(PathBuf),
    Llama(Vec<LlamaModel>),
    McpServers,
    McpActions(String, Vec<McpPanelAction>),
    McpTools(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpPanelState {
    Checking,
    Connected,
    NeedsAuthentication,
    Failed,
    Disabled,
}

#[derive(Debug, Clone)]
struct McpPanelServer {
    name: String,
    entry: kiss_mcp::ServerEntry,
    state: McpPanelState,
    tools: Vec<kiss_mcp::CachedTool>,
    authenticated: bool,
    source: String,
    scope: kiss_mcp::config::ConfigScope,
    error: Option<String>,
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
    SessionRename(PathBuf),
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
    LlamaModels(std::result::Result<Vec<LlamaModel>, String>),
    LlamaActionFinished(std::result::Result<String, String>),
    ShellOutput(String),
    ShellFinished(std::result::Result<ShellRunResult, String>),
    ShareFinished(std::result::Result<String, String>),
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
}

#[derive(Debug)]
struct ShellRunResult {
    command: String,
    output: String,
    exit_code: Option<i32>,
    cancelled: bool,
    truncated: bool,
    exclude_from_context: bool,
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const AUTO_RECAP_IDLE: Duration = Duration::from_secs(5 * 60);
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(33);
const MAX_READY_EVENTS_PER_TICK: usize = 256;

#[derive(Debug, PartialEq, Eq)]
enum CommandMenuAction {
    Handled,
    Submit(EditorSubmission),
}

impl App {
    fn render(&mut self, width: usize, session: &Arc<kiss_coding::AgentSession>) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        lines.extend(self.startup_lines.iter().cloned());

        if self.cell_render_cache.len() < self.cells.len() {
            self.cell_render_cache
                .resize_with(self.cells.len(), || None);
        } else {
            self.cell_render_cache.truncate(self.cells.len());
        }

        for (cell_index, cell) in self.cells.iter().enumerate() {
            if !lines.is_empty() {
                lines.push(String::new());
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
                lines.extend(cached.lines.iter().cloned());
                continue;
            }
            if let Cell::AssistantStreaming(text) = cell {
                let mut streaming = self.cell_render_cache[cell_index]
                    .take()
                    .and_then(|cached| cached.streaming)
                    .unwrap_or_default();
                let rendered = streaming.render(&self.md, text, width, self.mermaid_mode);
                lines.extend(rendered.iter().cloned());
                self.cell_render_cache[cell_index] = Some(CachedCellLines {
                    width,
                    key,
                    lines: rendered,
                    streaming: Some(streaming),
                });
                continue;
            }
            let cell_start = lines.len();
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
            self.cell_render_cache[cell_index] = Some(CachedCellLines {
                width,
                key,
                lines: lines[cell_start..].to_vec(),
                streaming: None,
            });
        }

        lines.push(String::new());
        if self.working {
            let spin = SPINNER[self.spinner_frame % SPINNER.len()];
            lines.push(self.theme.fg(
                "accent",
                &format!("{spin} working… (esc or ctrl+c to cancel)"),
            ));
        }
        if let Some(status) = &self.command_status {
            let spin = SPINNER[self.spinner_frame % SPINNER.len()];
            lines.push(self.theme.fg(
                "accent",
                &format!("{spin} {status} (esc or ctrl+c to cancel)"),
            ));
        }
        if let Some(note) = &self.queue_note {
            lines.push(self.theme.fg("muted", note));
        }
        if let Some(recap) = &self.recap {
            lines.push(self.theme.fg("muted", &format!("※ recap: {recap}")));
        } else if self.recap_loading {
            let spin = SPINNER[self.spinner_frame % SPINNER.len()];
            lines.push(self.theme.fg("muted", &format!("{spin} creating recap...")));
        }

        if let Some(panel) = &self.btw_panel {
            lines.push(self.theme.fg("accent", &self.theme.bold("BTW")));
            for line in kiss_tui::text::wrap_text(&panel.question, width.saturating_sub(2)) {
                lines.push(self.theme.fg("muted", &format!("  {line}")));
            }
            if let Some(answer) = &panel.answer {
                lines.push(String::new());
                lines.extend(self.md.render(answer, width));
                lines.push(self.theme.fg("dim", "esc or enter closes"));
            } else if let Some(error) = &panel.error {
                lines.push(self.theme.fg("error", &format!("✗ {error}")));
                lines.push(self.theme.fg("dim", "esc or enter closes"));
            } else {
                let spin = SPINNER[self.spinner_frame % SPINNER.len()];
                lines.push(self.theme.fg("accent", &format!("{spin} answering...")));
                lines.push(self.theme.fg("dim", "esc cancels"));
            }
        } else if let Some(prompt) = &self.secret_prompt {
            let title = match &prompt.kind {
                SecretPromptKind::ApiKey(provider) => format!("API key for {provider}"),
                SecretPromptKind::ProviderConfig(provider) => match provider.as_str() {
                    "google-vertex" => "Google API_KEY|PROJECT|LOCATION".into(),
                    "cloudflare-workers-ai" => "Cloudflare API_TOKEN|ACCOUNT_ID".into(),
                    "cloudflare-ai-gateway" => "Cloudflare API_KEY|ACCOUNT_ID|GATEWAY_ID".into(),
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
                SecretPromptKind::SessionRename(_) => "New session name".into(),
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
            if let Some(menu) = &mut self.command_menu {
                lines.extend(menu.list.render_compact(width, ""));
            } else if let Some(menu) = &mut self.file_menu {
                lines.extend(menu.list.render_compact(width, ""));
                if self.file_search_pending {
                    lines.push(self.theme.fg("dim", "  Searching files..."));
                }
            } else if self.file_search_pending {
                lines.push(self.theme.fg("dim", "  Indexing files..."));
            }
            lines.extend(self.footer(width, session));
        }
        lines
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
            shorten_path(&manager.cwd().display().to_string())
        );
        drop(manager);
        let left = format!(
            "↑{} ↓{} R{} W{} · {pct:.1}% ctx · ${:.3}",
            format_tokens(totals.input),
            format_tokens(totals.output),
            format_tokens(totals.cache_read),
            format_tokens(totals.cache_write),
            totals.cost.total,
        );
        let right = format!("({}) {}{thinking}", model.provider, model.id);
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
        Cell::ToolCall {
            title,
            output,
            is_error,
            done,
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

fn shorten_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let path_value = Path::new(path);
        if let Ok(rest) = path_value.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.to_string()
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

fn should_start_idle_recap(app: &App, settings: &kiss_coding::Settings, now: Instant) -> bool {
    settings.auto_recap_enabled()
        && app.idle_recap_armed
        && !app.working
        && !app.recap_loading
        && app.btw_panel.is_none()
        && app.picker.is_none()
        && app.secret_prompt.is_none()
        && app.command_status.is_none()
        && now.saturating_duration_since(app.last_user_activity) >= AUTO_RECAP_IDLE
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
    if app.recap_loading && app.recap_automatic {
        if let Some(cancel) = app.recap_cancel.take() {
            cancel.cancel();
        }
        app.recap_loading = false;
        app.recap_automatic = false;
        app.recap_request_id = app.recap_request_id.wrapping_add(1);
    }
}

fn tool_title(name: &str, args: &serde_json::Value) -> String {
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

pub async fn run(args: &Args) -> Result<i32> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let sink = {
        let tx = event_tx.clone();
        Arc::new(move |event: SessionEvent| {
            let _ = tx.send(event);
        })
    };
    Terminal::install_panic_hook();
    let mut terminal = Terminal::new()?;
    let mut renderer = DiffRenderer::new();
    let mut decoder = InputDecoder::default();
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

    let (width, height) = Terminal::size();
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
    let startup = 'startup: loop {
        tokio::select! {
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
            Some(bytes) = stdin_rx.recv() => {
                let mut events = Vec::new();
                decoder.feed(&bytes, &mut events);
                for event in events {
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
                let (width, height) = Terminal::size();
                let lines = provisional_lines(&mut provisional_editor, &provisional_theme, width);
                let mut out = std::io::stdout().lock();
                renderer.render_frame(&lines, width, height, &mut out)?;
                out.flush()?;
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
    } = startup;
    let mut resources = InteractiveResources {
        settings: settings.clone(),
        skills,
        prompt_templates,
        context_file_paths,
        enabled_models,
    };

    let theme = match settings.theme.as_deref() {
        Some("light") => Theme::light(),
        _ => Theme::dark(),
    };
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
    let mut keybindings = Keybindings::default();
    keybindings.load_overrides();

    let mut startup_lines: Vec<String> = Vec::new();
    if !settings.quiet_startup {
        startup_lines.push(theme.fg(
            "accent",
            &theme.bold(&format!("kiss v{}", env!("CARGO_PKG_VERSION"))),
        ));
        startup_lines.push(theme.fg(
            "dim",
            "enter send · shift+tab effort · esc cancel · ctrl+d exit · / commands",
        ));
        if !resources.context_file_paths.is_empty() {
            let names: Vec<String> = resources
                .context_file_paths
                .iter()
                .map(|p| shorten_path(&p.display().to_string()))
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

    let mut app = App {
        md: MarkdownRenderer::new(theme.clone()),
        editor: provisional_editor,
        theme,
        cells: Vec::new(),
        cell_render_cache: Vec::new(),
        keybindings,
        startup_lines,
        queue_note: None,
        working: false,
        spinner_frame: 0,
        picker: None,
        command_menu: None,
        file_menu: None,
        file_search_request: None,
        file_search_pending: false,
        secret_prompt: None,
        command_status: None,
        command_cancel: None,
        ctrl_c_armed: false,
        escape_armed: false,
        hide_thinking: settings.hide_thinking_block,
        expand_tools: false,
        mermaid_mode: mermaid_mode(settings.markdown.mermaid),
        git_branch: None,
        btw_panel: None,
        btw_request_id: 0,
        recap: None,
        recap_loading: false,
        recap_automatic: false,
        recap_cancel: None,
        recap_request_id: 0,
        last_user_activity: Instant::now(),
        idle_recap_armed: true,
        mcp_manager: None,
        mcp_servers: Vec::new(),
        mcp_config_paths: None,
    };
    if let Some(indent) = &settings.markdown.code_block_indent {
        app.md.code_indent = indent.clone();
    }
    app.editor.placeholder = "Ask anything. / commands, $ skills, @ files, ! shell.".into();
    update_thinking_border(&mut app, session.thinking_level());
    refresh_git_branch(&mut app, &session);

    // Kick off initial message if provided.
    if let Some(message) = initial_message {
        let session = session.clone();
        app.working = true;
        app.cells.push(Cell::User(message.clone()));
        tokio::spawn(async move {
            session.prompt(vec![AgentMessage::user(message)]).await;
        });
    }

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    let mut dirty = true;
    let mut next_render_at = Instant::now();
    let mut running_task: Option<tokio::task::JoinHandle<()>> = None;
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<CommandEvent>();
    let (file_search_tx, mut file_search_rx) = mpsc::unbounded_channel::<FileSearchResult>();
    let mut file_search = FileSearchService::new(file_search_tx);
    file_search.warm(session.manager.lock().unwrap().cwd().to_path_buf());

    'main: loop {
        let render_is_active = app.working
            || app.command_status.is_some()
            || app.btw_panel.is_some()
            || app.recap_loading;
        if dirty && (!render_is_active || Instant::now() >= next_render_at) {
            let (width, height) = Terminal::size();
            let lines = app.render(width, &session);
            let mut out = std::io::stdout().lock();
            renderer.render_frame(&lines, width, height, &mut out)?;
            out.flush()?;
            dirty = false;
            next_render_at = Instant::now() + MIN_FRAME_INTERVAL;
        }

        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_render_at)),
                if dirty && render_is_active && Instant::now() < next_render_at => {}
            _ = ticker.tick() => {
                if app.working || app.command_status.is_some() || app.btw_panel.is_some() || app.recap_loading {
                    app.spinner_frame += 1;
                    dirty = true;
                }
                if let Some(task) = &running_task
                    && task.is_finished()
                {
                    running_task = None;
                    app.working = false;
                    dirty = true;
                }
                if should_start_idle_recap(&app, &resources.settings, Instant::now())
                    && session_has_conversation(&session)
                {
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
                handle_command_event(&mut app, event, &mut resources, &mut file_search, &session);
                for _ in 1..MAX_READY_EVENTS_PER_TICK {
                    let Ok(event) = command_rx.try_recv() else { break };
                    handle_command_event(&mut app, event, &mut resources, &mut file_search, &session);
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
            Some(bytes) = stdin_rx.recv() => {
                let mut events = Vec::new();
                decoder.feed(&bytes, &mut events);
                for event in events {
                    let flow = handle_input(
                        &mut app,
                        &session,
                        &event,
                        args,
                        &mut resources,
                        &mut running_task,
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
            shorten_path(&file.display().to_string())
        );
        println!("resume with: kiss -c");
    }
    Ok(0)
}

enum Flow {
    Continue,
    Quit,
}

fn handle_session_event(
    app: &mut App,
    event: SessionEvent,
    file_search: &mut FileSearchService,
    session: &Arc<kiss_coding::AgentSession>,
) {
    match event {
        SessionEvent::Agent(agent_event) => match *agent_event {
            AgentEvent::AgentStart => app.working = true,
            AgentEvent::AgentEnd { .. } => {
                app.working = false;
                // Solidify any streaming cell.
                if let Some(Cell::AssistantStreaming(text)) = app.cells.last() {
                    let text = text.clone();
                    *app.cells.last_mut().unwrap() = Cell::AssistantFinal(text);
                }
            }
            AgentEvent::MessageStart { message } => match &message {
                AgentMessage::User(u) => {
                    // Steering/follow-up injections appear mid-run; the
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
                    if let Some(Cell::AssistantStreaming(text)) = app.cells.last_mut() {
                        text.push_str(&delta);
                    }
                }
                AssistantEvent::TextEnd { content, .. } => {
                    if let Some(Cell::AssistantStreaming(text)) = app.cells.last_mut()
                        && text.is_empty()
                    {
                        *text = content;
                    }
                }
                AssistantEvent::ThinkingStart { .. } => {
                    if let Some(position) = app
                        .cells
                        .iter()
                        .rposition(|cell| matches!(cell, Cell::AssistantStreaming(_)))
                        && (position == 0 || !matches!(app.cells[position - 1], Cell::Thinking(_)))
                    {
                        app.cells.insert(position, Cell::Thinking(String::new()));
                    }
                }
                AssistantEvent::ThinkingDelta { delta, .. } => {
                    if let Some(position) = app
                        .cells
                        .iter()
                        .rposition(|cell| matches!(cell, Cell::AssistantStreaming(_)))
                    {
                        if position > 0
                            && let Cell::Thinking(thinking) = &mut app.cells[position - 1]
                        {
                            thinking.push_str(&delta);
                        } else {
                            app.cells.insert(position, Cell::Thinking(delta));
                        }
                    }
                }
                AssistantEvent::ThinkingEnd { content, .. } => {
                    if let Some(Cell::Thinking(thinking)) = app
                        .cells
                        .iter_mut()
                        .rev()
                        .take(2)
                        .find(|cell| matches!(cell, Cell::Thinking(_)))
                    {
                        *thinking = content;
                    }
                }
                _ => {}
            },
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(a),
            } => {
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
                if let Some(Cell::AssistantStreaming(_)) = app.cells.last() {
                    *app.cells.last_mut().unwrap() = Cell::AssistantFinal(a.text());
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                app.cells.push(Cell::ToolCall {
                    title: tool_title(&tool_name, &args),
                    output: String::new(),
                    is_error: false,
                    done: false,
                });
            }
            AgentEvent::ToolExecutionUpdate { partial, .. } => {
                if let Some(Cell::ToolCall { output, .. }) = app
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|c| matches!(c, Cell::ToolCall { done: false, .. }))
                {
                    *output = partial.output_text();
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } => {
                if let Some(Cell::ToolCall {
                    output,
                    is_error: err_slot,
                    done,
                    ..
                }) = app
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|c| matches!(c, Cell::ToolCall { done: false, .. }))
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
    }
}

fn handle_command_event(
    app: &mut App,
    event: CommandEvent,
    resources: &mut InteractiveResources,
    file_search: &mut FileSearchService,
    session: &Arc<kiss_coding::AgentSession>,
) {
    match event {
        CommandEvent::BrowserLoginUrl { url, opened } => {
            let message = if opened {
                "finish authentication in the browser".to_string()
            } else {
                format!("the browser did not open; open this URL: {url}")
            };
            app.cells.push(Cell::Notice(message));
        }
        CommandEvent::BrowserLoginFinished { provider, result } => {
            app.command_status = None;
            app.command_cancel = None;
            match result {
                Ok(()) => {
                    if provider == "github-copilot"
                        && let Some(ids) = kiss_ai::auth::stored_oauth_model_ids(&provider)
                    {
                        resources
                            .enabled_models
                            .retain(|model| model.provider != provider || ids.contains(&model.id));
                    }
                    app.cells
                        .push(Cell::Notice(format!("logged in to {provider}")));
                }
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("login failed for {provider}: {error}"))),
            }
        }
        CommandEvent::DeviceLoginNotice {
            provider,
            url,
            code,
            opened,
        } => {
            app.command_status = Some(format!("waiting for {provider} device approval"));
            app.cells.push(Cell::Notice(format!(
                "{} {url} and enter code {code}",
                if opened { "Use" } else { "Open" }
            )));
        }
        CommandEvent::LlamaModels(result) => {
            app.command_status = None;
            app.command_cancel = None;
            match result {
                Ok(models) if models.is_empty() => app
                    .cells
                    .push(Cell::Notice("the llama.cpp router has no models".into())),
                Ok(models) => open_llama_picker(app, models),
                Err(error) => app.cells.push(Cell::Error(error)),
            }
        }
        CommandEvent::LlamaActionFinished(result) => {
            app.command_status = None;
            app.command_cancel = None;
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
                if output.len() > 50_000 {
                    output.truncate(50_000);
                }
            }
        }
        CommandEvent::ShellFinished(result) => {
            reset_file_search(app, file_search);
            file_search.refresh_all();
            refresh_git_branch(app, session);
            app.command_status = None;
            app.command_cancel = None;
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
            app.command_status = None;
            app.command_cancel = None;
            match result {
                Ok(url) => app
                    .cells
                    .push(Cell::Notice(format!("shared session: {url}"))),
                Err(error) => app.cells.push(Cell::Error(error)),
            }
        }
        CommandEvent::TreeNavigationFinished(result) => {
            app.command_status = None;
            app.command_cancel = None;
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
            panel.cancel = None;
            match result {
                Ok(response) => panel.answer = Some(response.text),
                Err(error) => panel.error = Some(error),
            }
        }
        CommandEvent::RecapFinished {
            request_id,
            automatic,
            result,
        } => {
            if app.recap_request_id != request_id {
                return;
            }
            app.recap_loading = false;
            app.recap_cancel = None;
            app.recap_automatic = false;
            match result {
                Ok(response) => app.recap = Some(normalize_recap(&response.text)),
                Err(error) if error.to_ascii_lowercase().contains("cancel") => {}
                Err(error) if automatic => app
                    .cells
                    .push(Cell::Notice(format!("automatic recap failed: {error}"))),
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("recap failed: {error}"))),
            }
        }
        CommandEvent::McpServerChecked(server) => {
            let server = *server;
            app.command_status = None;
            app.command_cancel = None;
            if let Some(current) = app
                .mcp_servers
                .iter_mut()
                .find(|current| current.name == server.name)
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
                format!("the browser did not open; open this URL for MCP server {name}: {url}")
            }));
        }
        CommandEvent::McpActionFinished {
            name,
            action,
            result,
        } => {
            app.command_status = None;
            app.command_cancel = None;
            match result {
                Ok(message) => app.cells.push(Cell::Notice(message)),
                Err(error) => app.cells.push(Cell::Error(format!(
                    "MCP server {name} {action} failed: {error}"
                ))),
            }
        }
    }
}

fn command_items(
    prompt_templates: &[kiss_coding::prompts::PromptTemplate],
    skills: &[kiss_coding::skills::Skill],
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
            let copilot_models = kiss_ai::auth::stored_oauth_model_ids("github-copilot");
            session
                .registry
                .all()
                .iter()
                .filter(|model| account_allows_model(model, copilot_models.as_deref()))
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
        "login" => kiss_ai::registry::BUILTIN_PROVIDER_IDS
            .iter()
            .copied()
            .chain(std::iter::once("llama.cpp"))
            .map(|provider| {
                (
                    provider.to_string(),
                    Some("provider authentication".into()),
                    provider.to_string(),
                    provider.to_string(),
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
    if editor.cursor().0 != 0 {
        return None;
    }
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
    if !preceding.is_empty()
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
) {
    let text = app.editor.text();
    if let Some(query) = command_query(&text) {
        let (items, mut replacements) = command_items(prompt_templates, skills);
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
    app.file_menu = None;
}

fn path_token_boundary(character: char) -> bool {
    matches!(character, ' ' | '\t' | '"' | '\'' | '=')
}

fn file_completion_prefix(editor: &Editor) -> Option<String> {
    let text = editor.current_line_before_cursor();
    if let Some(start) = text.rfind("@\"")
        && (start == 0
            || text[..start]
                .chars()
                .next_back()
                .is_some_and(path_token_boundary))
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
    let token = &text[start..];
    token.starts_with('@').then(|| token.to_string())
}

fn file_completion_label(path: &str) -> String {
    let path = path.trim_end_matches('/');
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn reset_file_search(app: &mut App, file_search: &mut FileSearchService) {
    file_search.cancel();
    app.file_menu = None;
    app.file_search_request = None;
    app.file_search_pending = false;
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
    app.file_search_pending = true;
}

fn apply_file_search_result(app: &mut App, result: FileSearchResult) {
    if app.file_search_request != Some(result.request_id)
        || file_completion_prefix(&app.editor).as_deref() != Some(result.prefix.as_str())
    {
        return;
    }
    app.file_search_request = None;
    app.file_search_pending = false;
    if result.values.is_empty() {
        app.file_menu = None;
        return;
    }
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
    let title = if result.index_limited {
        "Files (500000-entry index limit)"
    } else {
        "Files"
    };
    let mut list = SelectList::new(title, items, app.theme.clone());
    list.max_visible = 8;
    app.file_menu = Some(FileCompletion {
        prefix: result.prefix,
        list,
        values: result.values,
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
        Key::Tab | Key::Enter if app.file_search_pending => true,
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
    let skills = if display.starts_with('$') || display.starts_with("/skill:") {
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

fn queue_user_message(
    session: &Arc<kiss_coding::AgentSession>,
    display_text: String,
    model_text: String,
    follow_up: bool,
) {
    let stored_text = stored_user_text(&display_text, &model_text);
    let message = AgentMessage::user(stored_text);
    if follow_up {
        session.queue_follow_up(message);
    } else {
        session.queue_steering(message);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_input(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    event: &InputEvent,
    args: &Args,
    resources: &mut InteractiveResources,
    running_task: &mut Option<tokio::task::JoinHandle<()>>,
    file_search: &mut FileSearchService,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) -> Flow {
    note_user_activity(app);

    if app.btw_panel.is_some() {
        if let InputEvent::Key(key) = event
            && matches!(key.key, Key::Escape | Key::Enter)
        {
            if let Some(panel) = app.btw_panel.take()
                && let Some(cancel) = panel.cancel
            {
                cancel.cancel();
            }
            app.btw_request_id = app.btw_request_id.wrapping_add(1);
        }
        return Flow::Continue;
    }

    if app.secret_prompt.is_some() {
        return handle_secret_prompt(app, session, event, command_tx);
    }

    // Picker input intercepts everything.
    if app.picker.is_some() {
        if let InputEvent::Key(key) = event {
            return handle_picker_key(app, session, key, resources, command_tx);
        }
        return Flow::Continue;
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
                    run_slash_command(
                        app,
                        session,
                        command,
                        args,
                        resources,
                        running_task,
                        command_tx,
                    )
                }
            }
        };
    }

    if let InputEvent::Key(key) = event
        && key.key == Key::Escape
        && app.file_search_pending
    {
        reset_file_search(app, file_search);
        return Flow::Continue;
    }

    if let InputEvent::Key(key) = event {
        // Ctrl+C interrupts active work. At idle it clears once and exits on
        // a second press, matching Pi's editor behavior.
        if *key == KeyEvent::ctrl('c') {
            if let Some(cancel) = app.command_cancel.take() {
                cancel.cancel();
                app.command_status = None;
                app.cells.push(Cell::Notice("command cancelled".into()));
                return Flow::Continue;
            }
            if app.working {
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
            sync_command_menu(app, session, &resources.prompt_templates, &resources.skills);
            sync_file_menu(app, session, file_search);
            return Flow::Continue;
        }

        if key.key == Key::Escape {
            if let Some(cancel) = app.command_cancel.take() {
                cancel.cancel();
                app.command_status = None;
                app.cells.push(Cell::Notice("command cancelled".into()));
                return Flow::Continue;
            }
            if app.working {
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
                open_model_picker(app, session);
                return Flow::Continue;
            }
            Some(Action::CycleThinking) => {
                if !session.model().reasoning {
                    app.cells
                        .push(Cell::Notice("this model has no thinking levels".into()));
                    return Flow::Continue;
                }
                let levels = session.model().supported_thinking_levels();
                let current = session.thinking_level();
                let pos = levels.iter().position(|l| *l == current).unwrap_or(0);
                session.set_thinking_level(levels[(pos + 1) % levels.len()]);
                update_thinking_border(app, session.thinking_level());
                return Flow::Continue;
            }
            Some(Action::ToggleThinking) => {
                app.hide_thinking = !app.hide_thinking;
                return Flow::Continue;
            }
            Some(Action::ExpandTools) => {
                app.expand_tools = !app.expand_tools;
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
                    if app.working {
                        queue_user_message(session, display_text, model_text, true);
                    } else {
                        submit_with_display(app, session, display_text, model_text, running_task);
                    }
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
                if app.working {
                    queue_user_message(session, display_text, model_text, false);
                } else {
                    submit_with_display(app, session, display_text, model_text, running_task);
                }
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
            return run_slash_command(
                app,
                session,
                command,
                args,
                resources,
                running_task,
                command_tx,
            );
        }
        if let Some(shell) = text.strip_prefix('!') {
            if shell.trim_start_matches('!').trim().is_empty() {
                if app.working {
                    queue_user_message(session, display_text, text, false);
                } else {
                    submit_with_display(app, session, display_text, text, running_task);
                }
            } else {
                start_shell_passthrough(app, session, shell.to_string(), command_tx);
            }
            return Flow::Continue;
        }
        if app.working {
            queue_user_message(session, display_text, text, false);
        } else {
            submit_with_display(app, session, display_text, text, running_task);
        }
    } else {
        sync_command_menu(app, session, &resources.prompt_templates, &resources.skills);
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
        InputEvent::Paste(text) => {
            if let Some(prompt) = &mut app.secret_prompt {
                prompt.value.push_str(text.trim());
            }
        }
        InputEvent::Key(key) => match key.key {
            Key::Escape => {
                let login = app.secret_prompt.as_ref().is_some_and(|prompt| {
                    matches!(
                        &prompt.kind,
                        SecretPromptKind::ApiKey(_)
                            | SecretPromptKind::ProviderConfig(_)
                            | SecretPromptKind::AnthropicManual(_)
                            | SecretPromptKind::OpenRouterManual(_)
                            | SecretPromptKind::GitHubEnterpriseDomain
                            | SecretPromptKind::GoogleApplicationDefault
                            | SecretPromptKind::AwsProfile
                    )
                });
                app.secret_prompt = None;
                app.cells.push(Cell::Notice(if login {
                    "login cancelled".into()
                } else {
                    "input cancelled".into()
                }));
            }
            Key::Backspace => {
                if let Some(prompt) = &mut app.secret_prompt {
                    prompt.value.pop();
                }
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
                        SecretPromptKind::SessionRename(_) => "session name cannot be empty",
                        _ => "authentication input cannot be empty",
                    };
                    app.cells.push(Cell::Error(message.into()));
                } else {
                    match prompt.kind {
                        SecretPromptKind::ApiKey(provider) => {
                            match kiss_ai::auth::store_api_key(&provider, key) {
                                Ok(()) => app
                                    .cells
                                    .push(Cell::Notice(format!("saved API key for {provider}"))),
                                Err(error) => app.cells.push(Cell::Error(format!(
                                    "could not save API key: {error:#}"
                                ))),
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
                                _ => Err(anyhow::anyhow!("invalid value format for {provider}")),
                            };
                            match configured {
                                Ok(()) => app.cells.push(Cell::Notice(format!(
                                    "saved authentication for {provider}; run /reload to apply endpoint settings"
                                ))),
                                Err(error) => app.cells.push(Cell::Error(format!(
                                    "could not save authentication: {error:#}"
                                ))),
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
                            let cancel = CancellationToken::new();
                            app.command_cancel = Some(cancel.clone());
                            app.command_status = Some("finishing Anthropic login".into());
                            let tx = command_tx.clone();
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
                            let cancel = CancellationToken::new();
                            app.command_cancel = Some(cancel.clone());
                            app.command_status = Some("finishing OpenRouter login".into());
                            let tx = command_tx.clone();
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
                        SecretPromptKind::SessionRename(path) => {
                            let current = session
                                .manager
                                .lock()
                                .unwrap()
                                .session_file()
                                .is_some_and(|current| current == path);
                            let renamed = if current {
                                session
                                    .manager
                                    .lock()
                                    .unwrap()
                                    .append_session_info(key)
                                    .map(|_| ())
                            } else {
                                kiss_coding::SessionManager::open(&path).and_then(|mut manager| {
                                    manager.append_session_info(key).map(|_| ())
                                })
                            };
                            match renamed {
                                Ok(()) => app
                                    .cells
                                    .push(Cell::Notice(format!("renamed session to {key}"))),
                                Err(error) => app.cells.push(Cell::Error(format!(
                                    "could not rename session: {error:#}"
                                ))),
                            }
                        }
                    }
                }
            }
            Key::Char(character) if !key.ctrl && !key.alt => {
                if let Some(prompt) = &mut app.secret_prompt {
                    prompt.value.push(character);
                }
            }
            _ => {}
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
        app.editor.set_text(&text);
    }
}

fn update_thinking_border(app: &mut App, level: ThinkingLevel) {
    app.editor.border_color_token = if app.editor.text().starts_with('!') {
        "bashMode"
    } else {
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
    .into();
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
    let source = if enabled_models.is_empty() {
        session.registry.all()
    } else {
        enabled_models
    };
    let copilot_models = kiss_ai::auth::stored_oauth_model_ids("github-copilot");
    let models = source
        .iter()
        .filter(|model| account_allows_model(model, copilot_models.as_deref()))
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

fn submit(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    text: String,
    running_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    submit_with_display(app, session, text.clone(), text, running_task);
}

fn submit_with_display(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    display_text: String,
    model_text: String,
    running_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let stored_text = stored_user_text(&display_text, &model_text);
    app.cells.push(Cell::User(display_text));
    app.working = true;
    let session = session.clone();
    *running_task = Some(tokio::spawn(async move {
        session.prompt(vec![AgentMessage::user(stored_text)]).await;
    }));
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
    let cwd = session.manager.lock().unwrap().cwd().to_path_buf();
    let settings = session.settings();
    let shell = settings
        .shell_path
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "bash".into());
    let command = settings
        .shell_command_prefix
        .filter(|value| !value.trim().is_empty())
        .map(|prefix| format!("{prefix} {command}"))
        .unwrap_or(command);
    let cancel = CancellationToken::new();
    app.command_cancel = Some(cancel.clone());
    app.command_status = Some(format!("running !{command}"));
    app.cells.push(Cell::BashExecution {
        command: command.clone(),
        output: String::new(),
        exclude_from_context: exclude,
    });
    let tx = command_tx.clone();
    let session = session.clone();
    tokio::spawn(async move {
        let result = run_shell_command(&shell, &command, &cwd, exclude, &cancel, &tx).await;
        if let Ok(result) = &result {
            let message = AgentMessage::BashExecution(kiss_agent::BashExecutionMessage {
                command: result.command.clone(),
                output: result.output.clone(),
                exit_code: result.exit_code,
                cancelled: result.cancelled,
                truncated: result.truncated,
                full_output_path: None,
                exclude_from_context: result.exclude_from_context,
                timestamp: kiss_ai::now_ms(),
            });
            let _ = session.manager.lock().unwrap().append_message(message);
        }
        let _ = tx.send(CommandEvent::ShellFinished(result));
    });
}

async fn run_shell_command(
    shell: &str,
    command: &str,
    cwd: &std::path::Path,
    exclude_from_context: bool,
    cancel: &CancellationToken,
    tx: &mpsc::UnboundedSender<CommandEvent>,
) -> std::result::Result<ShellRunResult, String> {
    let mut process = tokio::process::Command::new(shell);
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.as_std_mut().process_group(0);
    }
    let mut child = process.spawn().map_err(|error| error.to_string())?;
    let mut stdout = child.stdout.take().ok_or("shell stdout is not available")?;
    let mut stderr = child.stderr.take().ok_or("shell stderr is not available")?;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut cancelled = false;
    let mut output = String::new();
    let mut buffer_out = [0_u8; 4096];
    let mut buffer_err = [0_u8; 4096];

    while !stdout_done || !stderr_done {
        tokio::select! {
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    // The child is the leader of its process group.
                    unsafe { libc::kill(-(pid as i32), libc::SIGTERM); }
                }
                let _ = child.kill().await;
            }
            read = stdout.read(&mut buffer_out), if !stdout_done => {
                match read {
                    Ok(0) => stdout_done = true,
                    Ok(length) => append_shell_output(&mut output, &buffer_out[..length], tx),
                    Err(error) => return Err(error.to_string()),
                }
            }
            read = stderr.read(&mut buffer_err), if !stderr_done => {
                match read {
                    Ok(0) => stderr_done = true,
                    Ok(length) => append_shell_output(&mut output, &buffer_err[..length], tx),
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
    }
    let status = child.wait().await.map_err(|error| error.to_string())?;
    let truncated = output.len() > 50_000;
    if truncated {
        output.truncate(50_000);
    }
    Ok(ShellRunResult {
        command: command.to_string(),
        output,
        exit_code: status.code(),
        cancelled,
        truncated,
        exclude_from_context,
    })
}

fn append_shell_output(
    output: &mut String,
    bytes: &[u8],
    tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let chunk = String::from_utf8_lossy(bytes).into_owned();
    if output.len() < 50_001 {
        output.push_str(&chunk);
    }
    let _ = tx.send(CommandEvent::ShellOutput(chunk));
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
    app.picker = Some(Picker {
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
    app.picker = Some(Picker {
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
    let cancel = CancellationToken::new();
    app.command_cancel = Some(cancel.clone());
    app.command_status = Some(if summarize {
        "summarizing branch".into()
    } else {
        "moving in session tree".into()
    });
    let session = session.clone();
    let tx = command_tx.clone();
    tokio::spawn(async move {
        let result = session
            .navigate_tree(&target_id, summarize, custom_instructions, cancel)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = tx.send(CommandEvent::TreeNavigationFinished(result));
    });
}

fn open_model_picker(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    open_model_picker_with_filter(app, session, None);
}

fn open_model_picker_with_filter(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    filter: Option<&str>,
) {
    let copilot_models = kiss_ai::auth::stored_oauth_model_ids("github-copilot");
    let items: Vec<SelectItem> = session
        .registry
        .all()
        .iter()
        .enumerate()
        .filter(|(_, model)| account_allows_model(model, copilot_models.as_deref()))
        .map(|(value, model)| SelectItem {
            label: format!("{}/{}", model.provider, model.id),
            detail: Some(model.display_name().to_string()),
            value,
        })
        .collect();
    let mut list = SelectList::new(
        "Select model (enter uses, shift+enter saves default)",
        items,
        app.theme.clone(),
    );
    list.max_visible = 12;
    if let Some(filter) = filter {
        list.set_filter(filter.to_string());
    }
    app.picker = Some(Picker {
        kind: PickerKind::Model,
        list,
    });
}

const THINKING_LEVELS: [ThinkingLevel; 7] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Xhigh,
    ThinkingLevel::Max,
];

fn open_thinking_picker(app: &mut App, session: &Arc<kiss_coding::AgentSession>) {
    if !session.model().reasoning {
        app.cells
            .push(Cell::Notice("this model has no thinking levels".into()));
        return;
    }
    let current = session.thinking_level();
    let supported = session.model().supported_thinking_levels();
    let items = THINKING_LEVELS
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
        "Thinking level (enter uses, shift+enter saves default)",
        items,
        app.theme.clone(),
    );
    list.selected = supported
        .iter()
        .position(|level| *level == current)
        .unwrap_or(0);
    app.picker = Some(Picker {
        kind: PickerKind::Thinking,
        list,
    });
}

fn open_scoped_models_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
) {
    app.picker = Some(scoped_models_picker(app, session, resources));
}

fn scoped_models_picker(
    app: &App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
) -> Picker {
    let copilot_models = kiss_ai::auth::stored_oauth_model_ids("github-copilot");
    let items = session
        .registry
        .all()
        .iter()
        .enumerate()
        .filter(|(_, model)| account_allows_model(model, copilot_models.as_deref()))
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
    app.picker = Some(picker);
}

fn account_allows_model(model: &kiss_ai::Model, copilot_models: Option<&[String]>) -> bool {
    model.provider != "github-copilot"
        || copilot_models.is_none_or(|ids| ids.iter().any(|id| id == &model.id))
}

fn open_session_picker(app: &mut App, session: &Arc<kiss_coding::AgentSession>, global: bool) {
    let manager = session.manager.lock().unwrap();
    let listings = if global {
        kiss_coding::SessionManager::list_all(manager.session_dir())
    } else {
        kiss_coding::SessionManager::list(manager.cwd(), manager.session_dir())
    };
    let current = manager.session_file().map(PathBuf::from);
    drop(manager);
    match listings {
        Ok(listings) if !listings.is_empty() => {
            let paths: Vec<PathBuf> = listings.iter().map(|item| item.path.clone()).collect();
            let items = listings
                .iter()
                .enumerate()
                .map(|(value, listing)| SelectItem {
                    label: listing
                        .name
                        .clone()
                        .or_else(|| listing.first_message.clone())
                        .unwrap_or_else(|| listing.id.clone()),
                    detail: Some(format!(
                        "{} entries · {}{}",
                        listing.entry_count,
                        shorten_path(&listing.cwd),
                        if current.as_ref() == Some(&listing.path) {
                            " · current"
                        } else {
                            ""
                        }
                    )),
                    value,
                })
                .collect();
            let mut list = SelectList::new(
                format!(
                    "Resume {} sessions (ctrl+g scope · ctrl+r rename)",
                    if global { "all" } else { "project" }
                ),
                items,
                app.theme.clone(),
            );
            list.max_visible = 14;
            app.picker = Some(Picker {
                kind: PickerKind::Session(paths, global),
                list,
            });
        }
        Ok(_) => app.cells.push(Cell::Notice("no saved sessions".into())),
        Err(error) => app
            .cells
            .push(Cell::Error(format!("could not list sessions: {error:#}"))),
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
    app.picker = Some(Picker {
        kind: PickerKind::Fork,
        list,
    });
}

fn open_login_picker(app: &mut App) {
    open_login_picker_with_filter(app, None);
}

fn open_login_picker_with_filter(app: &mut App, filter: Option<&str>) {
    let mut providers: Vec<String> = kiss_ai::registry::BUILTIN_PROVIDER_IDS
        .iter()
        .map(|provider| (*provider).to_string())
        .collect();
    providers.push("llama.cpp".into());
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
    app.picker = Some(Picker {
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
    let mut choices = kiss_ai::auth::login_methods(provider)
        .into_iter()
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
    app.picker = Some(Picker {
        kind: PickerKind::LoginMethods(provider.into(), choices),
        list,
    });
}

fn open_logout_picker(app: &mut App) {
    let providers = kiss_ai::auth::stored_provider_ids();
    if providers.is_empty() {
        app.cells.push(Cell::Notice(
            "no stored credentials; environment variables are unchanged".into(),
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
    app.picker = Some(Picker {
        kind: PickerKind::Logout(providers),
        list: SelectList::new("Remove provider authentication", items, app.theme.clone()),
    });
}

fn open_settings_picker(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
) {
    app.picker = Some(settings_picker(app, session, resources));
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
            value: 0,
        },
        SelectItem {
            label: "Theme".into(),
            detail: Some(theme.into()),
            value: 1,
        },
        SelectItem {
            label: "Thinking blocks".into(),
            detail: Some(if settings.hide_thinking_block {
                "hidden".into()
            } else {
                "visible".into()
            }),
            value: 2,
        },
        SelectItem {
            label: "Transport".into(),
            detail: Some(format_transport(settings.transport).into()),
            value: 3,
        },
        SelectItem {
            label: "Steering delivery".into(),
            detail: Some(format_queue_mode(settings.steering_mode).into()),
            value: 4,
        },
        SelectItem {
            label: "Follow-up delivery".into(),
            detail: Some(format_queue_mode(settings.follow_up_mode).into()),
            value: 5,
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
            value: 6,
        },
        SelectItem {
            label: "Automatic retry".into(),
            detail: Some(if settings.retry.enabled { "on" } else { "off" }.into()),
            value: 7,
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
            value: 8,
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
            value: 9,
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
            value: 10,
        },
        SelectItem {
            label: "Mermaid rendering".into(),
            detail: Some(settings.markdown.mermaid.as_str().into()),
            value: 11,
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
            value: 12,
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
            value: 13,
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
    app.picker = Some(picker);
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
    app.picker = Some(Picker {
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
    app.picker = Some(Picker {
        kind: PickerKind::Llama(models),
        list,
    });
}

fn mcp_state_label(state: McpPanelState) -> &'static str {
    match state {
        McpPanelState::Checking => "◌ checking",
        McpPanelState::Connected => "✔ connected",
        McpPanelState::NeedsAuthentication => "△ needs authentication",
        McpPanelState::Failed => "✗ failed",
        McpPanelState::Disabled => "◯ disabled",
    }
}

fn redact_mcp_url(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return value.to_string();
    };
    let pairs = url
        .query_pairs()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            let secret = ["key", "token", "secret", "auth", "password"]
                .iter()
                .any(|part| lower.contains(part));
            (
                name.into_owned(),
                if secret {
                    "[REDACTED]".into()
                } else {
                    value.into_owned()
                },
            )
        })
        .collect::<Vec<_>>();
    if !pairs.is_empty() {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    url.to_string().replace("%5BREDACTED%5D", "[REDACTED]")
}

fn mcp_can_authenticate(server: &McpPanelServer) -> bool {
    server.entry.auth_mode() == Some(kiss_mcp::AuthMode::OAuth)
        || server.state == McpPanelState::NeedsAuthentication
}

fn mcp_server_intro(theme: &Theme, server: &McpPanelServer, width: usize) -> Vec<String> {
    let mut lines = vec![theme.fg(
        "accent",
        &theme.bold(&kiss_tui::text::truncate_to_width(
            &format!("{} MCP Server", server.name),
            width,
        )),
    )];
    lines.push(format!("Status:  {}", mcp_state_label(server.state)));
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
            &format!("URL:     {}", redact_mcp_url(url)),
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
    if let Some(error) = &server.error {
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
                mcp_state_label(server.state).to_string()
            } else {
                format!(
                    "{} · {} tools",
                    mcp_state_label(server.state),
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
    app.picker = Some(Picker {
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
        message = message.replace(url, &redact_mcp_url(url));
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
        Ok(Ok(tools)) => {
            server.state = McpPanelState::Connected;
            server.tools = tools;
            server.error = None;
        }
        Ok(Err(error)) => {
            let message = format!("{error:#}");
            server.state = if mcp_error_needs_authentication(&message) {
                McpPanelState::NeedsAuthentication
            } else {
                McpPanelState::Failed
            };
            server.error = Some(concise_mcp_error(&message, &server.entry));
        }
        Err(_) => {
            server.state = McpPanelState::Failed;
            server.error = Some("connection timed out".into());
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
    let cli_trust = if args.approve {
        Some(true)
    } else if args.no_approve {
        Some(false)
    } else {
        None
    };
    let bootstrap = kiss_coding::Settings::load(&cwd, false);
    let trusted = kiss_coding::trust::resolve_non_interactive(
        &cwd,
        cli_trust,
        bootstrap.default_project_trust,
    );
    let loaded = match kiss_mcp::config::load(&cwd, trusted) {
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
            "no MCP servers are configured; use `kiss mcp add --help` to add one".into(),
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
        .map(|(name, entry)| {
            let project = loaded
                .sources
                .get(name)
                .is_some_and(|sources| sources.iter().any(|source| source.project));
            McpPanelServer {
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
                scope: if project {
                    kiss_mcp::config::ConfigScope::Project
                } else {
                    kiss_mcp::config::ConfigScope::User
                },
                error: None,
            }
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
    app.picker = Some(Picker {
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
    app.picker = Some(Picker {
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
    let Some(server) = app
        .mcp_servers
        .iter_mut()
        .find(|server| server.name == name)
    else {
        return;
    };
    server.state = McpPanelState::Checking;
    server.error = None;
    let server = server.clone();
    show_mcp_server_picker(app);
    app.command_status = Some(format!("reconnecting MCP server {name}"));
    let tx = command_tx.clone();
    tokio::spawn(async move {
        let _ = manager.disconnect(&server.name).await;
        let server = check_mcp_server(&manager, server).await;
        let _ = tx.send(CommandEvent::McpServerChecked(Box::new(server)));
    });
}

fn start_mcp_authentication(
    app: &mut App,
    server: McpPanelServer,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let Some(url) = server.entry.url.clone() else {
        app.cells.push(Cell::Error(format!(
            "MCP server {} uses stdio and does not use OAuth",
            server.name
        )));
        return;
    };
    let name = server.name.clone();
    let oauth = server.entry.oauth.clone().unwrap_or_default();
    let cancel = CancellationToken::new();
    app.command_cancel = Some(cancel.clone());
    app.command_status = Some(format!("authenticating MCP server {name}"));
    app.picker = None;
    let tx = command_tx.clone();
    tokio::spawn(async move {
        let result: Result<String> = async {
            if oauth.grant_type == kiss_mcp::OAuthGrantType::ClientCredentials {
                kiss_mcp::login_client_credentials(&name, &url, &oauth).await?;
                return Ok(format!("authenticated MCP server {name}"));
            }
            let challenge = tokio::select! {
                result = tokio::time::timeout(
                    Duration::from_secs(15),
                    kiss_mcp::probe_oauth_challenge(&server.entry),
                ) => result.context("MCP OAuth discovery timed out")??,
                _ = cancel.cancelled() => anyhow::bail!("cancelled"),
            };
            let pending = kiss_mcp::begin_login(&name, &url, &oauth, challenge.as_deref()).await?;
            let listener = crate::mcp_cli::callback_listener(&pending.redirect_uri)
                .await?
                .context("MCP OAuth redirect is not local; use `kiss mcp login --no-browser`")?;
            let opened = crate::auth_flow::open_browser(&pending.authorization_url);
            let _ = tx.send(CommandEvent::McpLoginUrl {
                name: name.clone(),
                url: pending.authorization_url.clone(),
                opened,
            });
            let callback = tokio::select! {
                result = crate::mcp_cli::receive_callback(listener, &pending.redirect_uri) => result?,
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
    app.command_status = Some(format!("clearing authentication for MCP server {name}"));
    app.picker = None;
    let tx = command_tx.clone();
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
    match kiss_mcp::config::set_disabled(
        paths,
        server.scope,
        &server.name,
        disabled,
        Some(&server.entry),
    ) {
        Ok(()) => {
            server.entry.disabled = disabled;
            server.state = if disabled {
                McpPanelState::Disabled
            } else {
                McpPanelState::Checking
            };
            app.cells.push(Cell::Notice(format!(
                "{} MCP server {}; run /reload to apply the tool change",
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
    if *key == KeyEvent::ctrl('g')
        && let PickerKind::Session(_, global) = &picker.kind
    {
        let next = !*global;
        open_session_picker(app, session, next);
        return Flow::Continue;
    }
    if *key == KeyEvent::ctrl('r')
        && let PickerKind::Session(paths, _) = &picker.kind
    {
        let path = picker
            .list
            .current()
            .and_then(|item| paths.get(item.value))
            .cloned();
        if let Some(path) = path {
            app.picker = None;
            app.secret_prompt = Some(SecretPrompt {
                kind: SecretPromptKind::SessionRename(path),
                value: String::new(),
            });
        }
        return Flow::Continue;
    }
    let activates_settings = matches!(&picker.kind, PickerKind::Settings)
        && picker.list.filter.is_empty()
        && key.key == Key::Char(' ')
        && !key.ctrl
        && !key.alt;
    if key.key == Key::Enter || activates_settings {
        let mut picker = app.picker.take().expect("picker is present");
        let value = picker.list.current().map(|item| item.value);
        let filter = std::mem::take(&mut picker.list.filter);
        if let Some(value) = value {
            apply_picker_selection(
                app,
                session,
                PickerSelection {
                    kind: picker.kind,
                    value,
                    filter,
                },
                key.shift,
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
        Key::Backspace => {
            let mut f = picker.list.filter.clone();
            f.pop();
            picker.list.set_filter(f);
        }
        Key::Char(c) if !key.ctrl && !key.alt => {
            let mut f = picker.list.filter.clone();
            f.push(c);
            picker.list.set_filter(f);
        }
        _ => {}
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
        PickerKind::Model => {
            if let Some(model) = session.registry.all().get(value) {
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
            if let Some(level) = THINKING_LEVELS.get(value).copied() {
                session.set_thinking_level(level);
                update_thinking_border(app, level);
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
        PickerKind::Session(paths, _) => {
            if let Some(path) = paths.get(value) {
                switch_session(app, session, path);
            }
        }
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
                let fork = session
                    .manager
                    .lock()
                    .unwrap()
                    .fork_active_branch(selected_id.as_deref(), false);
                match fork {
                    Ok(manager) => {
                        session.replace_manager(manager);
                        app.cells.clear();
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
        PickerKind::LoginMethods(provider, choices) => {
            if let Some(choice) = choices.get(value) {
                match choice {
                    LoginChoice::Method(method) => {
                        start_login_method(app, &provider, *method, command_tx)
                    }
                    LoginChoice::External(source) => {
                        match kiss_ai::auth::external::import(source) {
                            Ok(()) => app.cells.push(Cell::Notice(format!(
                                "imported {provider} credentials from {}",
                                source.application
                            ))),
                            Err(error) => app
                                .cells
                                .push(Cell::Error(format!("credential import failed: {error:#}"))),
                        }
                    }
                }
            }
        }
        PickerKind::Logout(providers) => {
            if let Some(provider) = providers.get(value) {
                match kiss_ai::auth::remove_api_key(provider) {
                    Ok(true) => app.cells.push(Cell::Notice(format!(
                        "removed stored credentials for {provider}"
                    ))),
                    Ok(false) => app.cells.push(Cell::Notice(format!(
                        "no stored credentials for {provider}"
                    ))),
                    Err(error) => app
                        .cells
                        .push(Cell::Error(format!("logout failed: {error:#}"))),
                }
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
                    "saved project trust as {}; run /reload to apply it",
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

fn save_interactive_settings(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    resources: &InteractiveResources,
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
    match value {
        0 => {
            let levels = [
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh,
                ThinkingLevel::Max,
            ];
            let current = session.thinking_level();
            let position = levels
                .iter()
                .position(|level| *level == current)
                .unwrap_or(0);
            let next = levels[(position + 1) % levels.len()];
            session.set_thinking_level(next);
            resources.settings.default_thinking_level = Some(next.as_str().into());
            update_thinking_border(app, next);
        }
        1 => {
            let light = resources.settings.theme.as_deref() != Some("light");
            resources.settings.theme = Some(if light { "light" } else { "dark" }.into());
            app.apply_theme(if light { Theme::light() } else { Theme::dark() });
            update_thinking_border(app, session.thinking_level());
        }
        2 => {
            resources.settings.hide_thinking_block = !resources.settings.hide_thinking_block;
            app.hide_thinking = resources.settings.hide_thinking_block;
        }
        3 => {
            resources.settings.transport = match resources.settings.transport {
                Transport::Auto => Transport::Sse,
                Transport::Sse => Transport::WebSocket,
                Transport::WebSocket => Transport::WebSocketCached,
                Transport::WebSocketCached => Transport::Auto,
            };
        }
        4 => {
            resources.settings.steering_mode = match resources.settings.steering_mode {
                QueueMode::All => QueueMode::OneAtATime,
                QueueMode::OneAtATime => QueueMode::All,
            };
        }
        5 => {
            resources.settings.follow_up_mode = match resources.settings.follow_up_mode {
                QueueMode::All => QueueMode::OneAtATime,
                QueueMode::OneAtATime => QueueMode::All,
            };
        }
        6 => resources.settings.compaction.enabled = !resources.settings.compaction.enabled,
        7 => resources.settings.retry.enabled = !resources.settings.retry.enabled,
        8 => resources.settings.quiet_startup = !resources.settings.quiet_startup,
        9 => {
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
        10 => {
            resources.settings.enable_skill_commands =
                Some(!resources.settings.enable_skill_commands.unwrap_or(true));
        }
        11 => {
            resources.settings.markdown.mermaid = match resources.settings.markdown.mermaid {
                MermaidRendering::Streaming => MermaidRendering::Final,
                MermaidRendering::Final => MermaidRendering::Off,
                MermaidRendering::Off => MermaidRendering::Streaming,
            };
            app.mermaid_mode = mermaid_mode(resources.settings.markdown.mermaid);
        }
        12 => {
            resources.settings.auto_recap = Some(!resources.settings.auto_recap_enabled());
            app.idle_recap_armed = resources.settings.auto_recap_enabled();
            app.last_user_activity = Instant::now();
        }
        13 => {
            resources.settings.subagents.enabled = !resources.settings.subagents.enabled;
        }
        _ => return,
    }
    save_interactive_settings(app, session, resources);
}

fn switch_session(app: &mut App, session: &Arc<kiss_coding::AgentSession>, path: &std::path::Path) {
    match kiss_coding::SessionManager::open(path) {
        Ok(manager) => {
            session.replace_manager(manager);
            app.cells = session_cells(session);
            app.cells.push(Cell::Notice(format!(
                "resumed {}",
                shorten_path(&path.display().to_string())
            )));
            update_thinking_border(app, session.thinking_level());
            refresh_git_branch(app, session);
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
                    "google-vertex" | "cloudflare-workers-ai" | "cloudflare-ai-gateway"
                ) {
                    SecretPromptKind::ProviderConfig(provider.into())
                } else {
                    SecretPromptKind::ApiKey(provider.into())
                },
                value: String::new(),
            });
        }
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
                Err(error) => app.cells.push(Cell::Error(format!(
                    "could not start Anthropic login: {error:#}"
                ))),
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
                Err(error) => app.cells.push(Cell::Error(format!(
                    "could not start OpenRouter login: {error:#}"
                ))),
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
        _ => app.cells.push(Cell::Error(format!(
            "{} is not available for {provider}",
            method.label()
        ))),
    }
}

fn start_provider_browser_login(
    app: &mut App,
    provider: &str,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) {
    let provider = provider.to_string();
    let cancel = CancellationToken::new();
    app.command_cancel = Some(cancel.clone());
    app.command_status = Some(format!("logging in to {provider}"));
    let tx = command_tx.clone();
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
    let cancel = CancellationToken::new();
    app.command_cancel = Some(cancel.clone());
    app.command_status = Some(format!("starting device login for {provider}"));
    let tx = command_tx.clone();
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
    let cancel = CancellationToken::new();
    app.command_cancel = Some(cancel.clone());
    app.command_status = Some("reading llama.cpp models".into());
    let tx = command_tx.clone();
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
    let cancel = CancellationToken::new();
    app.command_cancel = Some(cancel.clone());
    app.command_status = Some(format!("{action}ing {}", model.id));
    let tx = command_tx.clone();
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

fn run_slash_command(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    command: &str,
    args: &Args,
    resources: &mut InteractiveResources,
    running_task: &mut Option<tokio::task::JoinHandle<()>>,
    command_tx: &mpsc::UnboundedSender<CommandEvent>,
) -> Flow {
    let mut parts = command.splitn(2, ' ');
    let name = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim().to_string();

    match name {
        "quit" | "exit" => return Flow::Quit,
        "help" | "hotkeys" => {
            app.cells.push(Cell::Notice(
                "Input\n  Enter send · Shift+Enter or Alt+Enter newline · / commands and skills · $ skills · @ files · ! shell · !! shell outside context\nModels and effort\n  Shift+Tab effort · Ctrl+L select model · Ctrl+P next model · Ctrl+Shift+P previous model\nSession\n  Ctrl+D exit · Esc cancel · Ctrl+C interrupt or clear · double Esc tree\nDisplay and queues\n  Ctrl+O tools · Ctrl+T thinking · Ctrl+X copy · Ctrl+Enter follow-up · Alt+Up dequeue".into(),
            ));
        }
        "model" => {
            if rest.is_empty() {
                open_model_picker(app, session);
            } else if let Some((model, thinking)) = session.registry.resolve(&rest, None) {
                let copilot_models = kiss_ai::auth::stored_oauth_model_ids("github-copilot");
                if account_allows_model(&model, copilot_models.as_deref()) {
                    session.set_model(model);
                    if let Some(thinking) = thinking {
                        session.set_thinking_level(thinking);
                        update_thinking_border(app, thinking);
                    }
                } else {
                    app.cells.push(Cell::Error(format!(
                        "model {rest} is not available for this GitHub Copilot account"
                    )));
                }
            } else {
                open_model_picker_with_filter(app, session, Some(&rest));
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
                update_thinking_border(app, level);
                app.cells.push(Cell::Notice(format!(
                    "thinking level set to {}",
                    level.as_str()
                )));
            } else {
                app.cells.push(Cell::Error(format!(
                    "invalid thinking level: {rest}; use off, minimal, low, medium, high, xhigh, or max"
                )));
            }
        }
        "scoped-models" => open_scoped_models_picker(app, session, resources),
        "settings" => open_settings_picker(app, session, resources),
        "mcp" => open_mcp_picker(app, session, args, command_tx),
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
                resources.settings.auto_recap = Some(true);
                app.idle_recap_armed = true;
                app.last_user_activity = Instant::now();
                save_interactive_settings(app, session, resources);
                app.cells.push(Cell::Notice("automatic recap is on".into()));
            }
            "off" => {
                resources.settings.auto_recap = Some(false);
                app.idle_recap_armed = false;
                if app.recap_loading && app.recap_automatic {
                    if let Some(cancel) = app.recap_cancel.take() {
                        cancel.cancel();
                    }
                    app.recap_loading = false;
                    app.recap_automatic = false;
                    app.recap_request_id = app.recap_request_id.wrapping_add(1);
                }
                save_interactive_settings(app, session, resources);
                app.cells
                    .push(Cell::Notice("automatic recap is off".into()));
            }
            _ => app
                .cells
                .push(Cell::Notice("usage: /recap [now|on|off]".into())),
        },
        "tree" => open_tree_picker(app, session),
        "fork" => open_fork_picker(app, session),
        "clone" => {
            if session.manager.lock().unwrap().leaf_id().is_none() {
                app.cells.push(Cell::Notice("nothing to clone yet".into()));
                return Flow::Continue;
            }
            let fork = {
                let manager = session.manager.lock().unwrap();
                manager.fork_active_branch(manager.leaf_id(), true)
            };
            match fork {
                Ok(manager) => {
                    session.replace_manager(manager);
                    app.cells = session_cells(session);
                    app.cells
                        .push(Cell::Notice("cloned to a new session".into()));
                    refresh_git_branch(app, session);
                }
                Err(error) => app
                    .cells
                    .push(Cell::Error(format!("clone failed: {error:#}"))),
            }
        }
        "trust" => open_trust_picker(app, session),
        "login" => {
            if rest.is_empty() {
                open_login_picker(app);
            } else {
                let provider = kiss_ai::registry::BUILTIN_PROVIDER_IDS
                    .iter()
                    .copied()
                    .chain(std::iter::once("llama.cpp"))
                    .find(|provider| provider.eq_ignore_ascii_case(&rest));
                if let Some(provider) = provider {
                    open_login_methods_picker(app, provider);
                } else {
                    open_login_picker_with_filter(app, Some(&rest));
                }
            }
        }
        "logout" => {
            if rest.is_empty() {
                open_logout_picker(app);
            } else {
                match kiss_ai::auth::remove_api_key(&rest) {
                    Ok(true) => app.cells.push(Cell::Notice(format!(
                        "removed stored credentials for {rest}"
                    ))),
                    Ok(false) => app
                        .cells
                        .push(Cell::Notice(format!("no stored credentials for {rest}"))),
                    Err(error) => app
                        .cells
                        .push(Cell::Error(format!("logout failed: {error:#}"))),
                }
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
                let _ = session.manager.lock().unwrap().append_session_info(&rest);
                app.cells
                    .push(Cell::Notice(format!("session named: {rest}")));
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
            let session = session.clone();
            let instructions = if rest.is_empty() { None } else { Some(rest) };
            app.working = true;
            *running_task = Some(tokio::spawn(async move {
                session.compact(instructions, false).await;
            }));
        }
        "copy" => {
            copy_last_response(app);
        }
        "changelog" => app.cells.push(Cell::Notice(
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../CHANGELOG.md")).into(),
        )),
        "new" => {
            let manager = session.manager.lock().unwrap().create_sibling();
            match manager {
                Ok(new_manager) => {
                    session.replace_manager(new_manager);
                    app.cells.clear();
                    app.cells.push(Cell::Notice("started a new session".into()));
                    refresh_git_branch(app, session);
                }
                Err(e) => app
                    .cells
                    .push(Cell::Error(format!("could not create session: {e:#}"))),
            }
        }
        "resume" => open_session_picker(app, session, false),
        "import" => {
            if let Some(path) = parse_path_argument(&rest) {
                let path = expand_user_path(&path);
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
                app.picker = Some(Picker {
                    kind: PickerKind::ImportConfirm(path),
                    list: SelectList::new("Confirm session import", items, app.theme.clone()),
                });
            } else {
                app.cells
                    .push(Cell::Notice("usage: /import <file.jsonl>".into()));
            }
        }
        "export" => {
            let manager = session.manager.lock().unwrap();
            let target = parse_path_argument(&rest)
                .unwrap_or_else(|| format!("session-{}.html", manager.session_id()));
            match crate::export::export_session(&manager, std::path::Path::new(&target)) {
                Ok(()) => app
                    .cells
                    .push(Cell::Notice(format!("exported to {target}"))),
                Err(e) => app.cells.push(Cell::Error(format!("export failed: {e:#}"))),
            }
        }
        "share" => start_share_session(app, session, command_tx),
        "reload" => reload_interactive(app, session, args, resources),
        "llama" => start_llama_list(app, command_tx),
        other => {
            // Prompt templates and skills as commands.
            if let Some(template) = resources
                .prompt_templates
                .iter()
                .find(|template| template.name == other)
            {
                let args: Vec<&str> = rest.split_whitespace().collect();
                let expanded = kiss_coding::prompts::expand(&template.body, &args);
                if app.working {
                    session.queue_steering(AgentMessage::user(expanded));
                } else {
                    submit(app, session, expanded, running_task);
                }
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
                    Ok(Some(model_text)) if app.working => {
                        queue_user_message(session, display_text, model_text, false)
                    }
                    Ok(Some(model_text)) => {
                        submit_with_display(app, session, display_text, model_text, running_task)
                    }
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
    if let Some(panel) = app.btw_panel.take()
        && let Some(cancel) = panel.cancel
    {
        cancel.cancel();
    }
    app.btw_request_id = app.btw_request_id.wrapping_add(1);
    let request_id = app.btw_request_id;
    let cancel = CancellationToken::new();
    app.btw_panel = Some(BtwPanel {
        request_id,
        question: question.clone(),
        answer: None,
        error: None,
        cancel: Some(cancel.clone()),
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
    if let Some(cancel) = app.recap_cancel.take() {
        cancel.cancel();
    }
    app.recap_request_id = app.recap_request_id.wrapping_add(1);
    let request_id = app.recap_request_id;
    let cancel = CancellationToken::new();
    let previous = app.recap.clone();
    app.recap_loading = true;
    app.recap_automatic = automatic;
    app.recap_cancel = Some(cancel.clone());
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

fn expand_user_path(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

fn parse_path_argument(value: &str) -> Option<String> {
    let value = value.trim_start();
    let first = value.chars().next()?;
    if matches!(first, '\'' | '"') {
        let rest = &value[first.len_utf8()..];
        let end = rest.find(first)?;
        return Some(rest[..end].to_string());
    }
    value.split_whitespace().next().map(str::to_string)
}

fn import_session(
    app: &mut App,
    session: &Arc<kiss_coding::AgentSession>,
    source: &std::path::Path,
) {
    let (fallback_cwd, session_dir) = {
        let manager = session.manager.lock().unwrap();
        (
            manager.cwd().to_path_buf(),
            manager.session_dir().to_path_buf(),
        )
    };
    let imported_cwd = kiss_coding::SessionManager::open(source)
        .ok()
        .map(|manager| PathBuf::from(&manager.header().cwd))
        .filter(|path| path.is_dir());
    let cwd = imported_cwd.unwrap_or_else(|| {
        app.cells.push(Cell::Notice(
            "the imported working directory is unavailable; using the current project".into(),
        ));
        fallback_cwd
    });
    match kiss_coding::SessionManager::fork_from(source, &cwd, Some(session_dir)) {
        Ok(manager) => {
            session.replace_manager(manager);
            app.cells = session_cells(session);
            app.cells.push(Cell::Notice(format!(
                "imported {} into a new Kiss session",
                shorten_path(&source.display().to_string())
            )));
            update_thinking_border(app, session.thinking_level());
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
    let (id, jsonl, target) = {
        let manager = session.manager.lock().unwrap();
        let id = manager.session_id().to_string();
        let jsonl = match manager.to_jsonl() {
            Ok(jsonl) => jsonl,
            Err(error) => {
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
        if let Err(error) = crate::export::export_html(&manager, &target) {
            app.cells.push(Cell::Error(format!(
                "could not prepare session share: {error:#}"
            )));
            return;
        }
        (id, jsonl, target)
    };
    let cancel = CancellationToken::new();
    app.command_cancel = Some(cancel.clone());
    app.command_status = Some("sharing session".into());
    let tx = command_tx.clone();
    tokio::spawn(async move {
        let result = share_session_async(&id, &jsonl, &target, &cancel)
            .await
            .map_err(|error| format!("session share failed: {error:#}"));
        let _ = std::fs::remove_file(&target);
        let _ = tx.send(CommandEvent::ShareFinished(result));
    });
}

async fn share_session_async(
    id: &str,
    jsonl: &str,
    html_path: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<String> {
    if let Some(token) = kiss_ai::auth::resolve_api_key_async("radius", &Default::default()).await?
    {
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

    let patterns = {
        let command_line = Args::split_csv(&args.models);
        if command_line.is_empty() {
            reloaded.settings.enabled_models.clone().unwrap_or_default()
        } else {
            command_line
        }
    };
    resources.enabled_models = if patterns.is_empty() {
        Vec::new()
    } else {
        session.registry.match_patterns(&patterns)
    };
    resources.skills = reloaded.skills;
    resources.prompt_templates = reloaded.prompt_templates;
    resources.context_file_paths = reloaded.context_file_paths;
    resources.settings = reloaded.settings.clone();
    session.reload_runtime(reloaded.settings, reloaded.system_prompt, reloaded.tools);

    let theme = if resources.settings.theme.as_deref() == Some("light") {
        Theme::light()
    } else {
        Theme::dark()
    };
    app.apply_theme(theme);
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
    app.keybindings = Keybindings::default();
    app.keybindings.load_overrides();
    update_thinking_border(app, session.thinking_level());
    refresh_git_branch(app, session);
    app.cells.push(Cell::Notice(format!(
        "reloaded {} skills, {} prompts, and {} context files",
        resources.skills.len(),
        resources.prompt_templates.len(),
        resources.context_file_paths.len()
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use std::path::PathBuf;

    fn test_app() -> App {
        let theme = Theme::dark();
        App {
            md: MarkdownRenderer::new(theme.clone()),
            editor: Editor::new(theme.clone()),
            theme,
            cells: Vec::new(),
            cell_render_cache: Vec::new(),
            keybindings: Keybindings::default(),
            startup_lines: Vec::new(),
            queue_note: None,
            working: false,
            spinner_frame: 0,
            picker: None,
            command_menu: None,
            file_menu: None,
            file_search_request: None,
            file_search_pending: false,
            secret_prompt: None,
            command_status: None,
            command_cancel: None,
            ctrl_c_armed: false,
            escape_armed: false,
            hide_thinking: false,
            expand_tools: false,
            mermaid_mode: MermaidMode::Streaming,
            git_branch: None,
            btw_panel: None,
            btw_request_id: 0,
            recap: None,
            recap_loading: false,
            recap_automatic: false,
            recap_cancel: None,
            recap_request_id: 0,
            last_user_activity: Instant::now(),
            idle_recap_armed: true,
            mcp_manager: None,
            mcp_servers: Vec::new(),
            mcp_config_paths: None,
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
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
        sync_command_menu(app, &session, &[], &[]);
    }

    fn test_session(manager: kiss_coding::SessionManager) -> Arc<kiss_coding::AgentSession> {
        let registry = kiss_ai::Registry::from_builtin();
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
        let mut task = None;
        let (tx, _rx) = mpsc::unbounded_channel();
        run_slash_command(app, session, command, &args, resources, &mut task, &tx)
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
        let settings = kiss_coding::Settings::default();
        let now = Instant::now();
        app.last_user_activity = now - AUTO_RECAP_IDLE;
        assert!(should_start_idle_recap(&app, &settings, now));

        app.working = true;
        assert!(!should_start_idle_recap(&app, &settings, now));
        app.working = false;
        app.idle_recap_armed = false;
        assert!(!should_start_idle_recap(&app, &settings, now));

        app.idle_recap_armed = true;
        let mut disabled = settings;
        disabled.auto_recap = Some(false);
        assert!(!should_start_idle_recap(&app, &disabled, now));
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
        let redacted = redact_mcp_url("https://example.com/mcp?apiKey=secret&region=ca");
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
            scope: kiss_mcp::config::ConfigScope::User,
            error: None,
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
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
        let mut app = test_app();
        let mut resources = test_resources();

        run_command_for_test(&mut app, &session, &mut resources, "model missing-model");
        let picker = app.picker.as_ref().expect("filtered model picker");
        assert!(matches!(picker.kind, PickerKind::Model));
        assert_eq!(picker.list.filter, "missing-model");

        run_command_for_test(&mut app, &session, &mut resources, "login missing-provider");
        let picker = app.picker.as_ref().expect("filtered login picker");
        assert!(matches!(picker.kind, PickerKind::LoginProviders(_)));
        assert_eq!(picker.list.filter, "missing-provider");
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
    fn rebuilt_settings_picker_keeps_filter_selection_and_new_value() {
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
        let mut app = test_app();
        let mut resources = test_resources();
        open_settings_picker(&mut app, &session, &resources);

        let picker = app.picker.as_mut().expect("settings picker");
        picker.list.set_filter("automatic".into());
        assert!(picker.list.select_value(12));

        resources.settings.auto_recap = Some(false);
        reopen_settings_picker(&mut app, &session, &resources, "automatic".into(), 12);

        let picker = app.picker.as_mut().expect("rebuilt settings picker");
        assert_eq!(picker.list.filter, "automatic");
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
    fn settings_picker_shows_the_opt_in_subagent_toggle() {
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
        let mut app = test_app();
        let mut resources = test_resources();

        open_settings_picker(&mut app, &session, &resources);
        let picker = app.picker.as_mut().expect("settings picker");
        assert!(picker.list.select_value(13));
        assert_eq!(picker.list.current().unwrap().label, "Subagents");
        assert_eq!(
            picker.list.current().unwrap().detail.as_deref(),
            Some("off")
        );

        resources.settings.subagents.enabled = true;
        reopen_settings_picker(&mut app, &session, &resources, String::new(), 13);
        let picker = app.picker.as_mut().unwrap();
        assert_eq!(picker.list.current().map(|item| item.value), Some(13));
        assert_eq!(picker.list.current().unwrap().detail.as_deref(), Some("on"));
    }

    #[test]
    fn rebuilt_scoped_models_picker_keeps_filter_and_selection() {
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
        let mut app = test_app();
        let mut resources = test_resources();
        let selected = session.registry.all().first().expect("model").clone();
        let selected_value = 0;

        resources.enabled_models.push(selected);
        reopen_scoped_models_picker(
            &mut app,
            &session,
            &resources,
            String::new(),
            selected_value,
        );

        let picker = app.picker.as_mut().expect("scoped model picker");
        assert_eq!(picker.list.current().map(|item| item.value), Some(0));
        assert!(
            picker
                .list
                .current()
                .is_some_and(|item| item.label.starts_with("[x] "))
        );
    }

    #[test]
    fn saved_default_model_stays_in_an_active_scope() {
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
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
        assert!(app.editor.history.is_empty());
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
        assert!(app.editor.history.is_empty());
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
        assert!(app.file_search_pending);
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

    #[test]
    fn stale_file_search_result_does_not_replace_the_current_menu() {
        let mut app = test_app();
        app.editor.set_text("read @new");
        app.file_search_request = Some(2);
        app.file_search_pending = true;

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
        assert!(app.file_search_pending);
    }

    #[test]
    fn context_usage_cache_is_invalidated_by_session_changes() {
        let mut manager = kiss_coding::SessionManager::in_memory(Path::new("/synthetic"));
        manager
            .append_message(AgentMessage::user("small message"))
            .unwrap();
        let session = test_session(manager);
        let initial = session.context_usage().0;
        assert_eq!(session.context_usage().0, initial);

        session
            .manager
            .lock()
            .unwrap()
            .append_message(AgentMessage::user("larger message ".repeat(500)))
            .unwrap();

        assert!(session.context_usage().0 > initial);
    }

    #[test]
    fn delta_only_updates_build_the_streaming_cells() {
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
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

        let labels: Vec<String> = command_items(&templates, &skills)
            .0
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert!(labels.contains(&"review".to_string()));
        assert!(labels.contains(&"release".to_string()));
    }

    #[test]
    fn dollar_completion_inserts_skills_without_submitting() {
        let mut app = test_app();
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
        let skills = vec![kiss_coding::skills::Skill {
            name: "release".into(),
            description: "Prepare a release".into(),
            file_path: "release/SKILL.md".into(),
            disable_model_invocation: false,
        }];
        app.editor.set_text("$rel");
        sync_command_menu(&mut app, &session, &[], &skills);

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
        assert_eq!(app.editor.text(), "$release ");
    }

    #[test]
    fn slash_skill_completion_uses_direct_name_and_allows_a_chain() {
        let mut app = test_app();
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
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
        sync_command_menu(&mut app, &session, &[], &skills);
        assert_eq!(
            handle_command_menu_key(&mut app, &key(Key::Enter)),
            Some(CommandMenuAction::Handled)
        );
        assert_eq!(app.editor.text(), "/review ");

        app.editor.insert("/te");
        sync_command_menu(&mut app, &session, &[], &skills);
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
        assert!(providers.contains(&"llama.cpp".to_string()));
    }

    #[test]
    fn api_key_login_uses_masked_prompt() {
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
            Some(LoginChoice::Method(kiss_ai::auth::LoginMethod::ApiKey))
        ));
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
        session.replace_manager(replacement);
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
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_transcript_and_context() {
        let session = test_session(kiss_coding::SessionManager::in_memory(Path::new(
            "/synthetic",
        )));
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

        app.working = true;
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

    #[tokio::test]
    async fn shell_command_merges_output_and_keeps_context_mode() {
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = run_shell_command(
            "sh",
            "printf stdout; printf stderr >&2; exit 7",
            temp.path(),
            true,
            &cancel,
            &tx,
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
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let future = run_shell_command(
            "sh",
            "printf started; sleep 30",
            temp.path(),
            false,
            &cancel,
            &tx,
        );
        tokio::pin!(future);
        tokio::select! {
            result = &mut future => panic!("shell ended before cancellation: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => cancel.cancel(),
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), future)
            .await
            .expect("cancelled shell must stop")
            .unwrap();
        assert!(result.cancelled);
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
