//! How an SDK caller describes the session they want.
//!
//! [`SessionOptions`] is the union of everything `kiss`'s command line can
//! configure about a session, expressed as data instead of flags. Its
//! [`SessionOptions::build`] method performs the same startup sequence as
//! `crates/kiss/src/setup.rs::build_startup`: load settings, load the model
//! catalog, choose a model, discover project resources, assemble the system
//! prompt, build the tools, open or create the session file, and construct the
//! `AgentSession` the rest of the SDK drives.

use crate::tools::{build_tools, select_tool_names};
use anyhow::{Context as _, Result};
use kiss_agent::{DynTool, StreamFn};
use kiss_ai::{Model, Registry, ThinkingLevel};
use kiss_coding::model_choice::{self, ModelRequest};
use kiss_coding::session::manager::SessionManager;
use kiss_coding::session_runner::{AgentSession, SessionEventSink};
use kiss_coding::settings::Settings;
use kiss_coding::system_prompt::{SystemPromptOptions, build_system_prompt};
use kiss_coding::{context_files, trust};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the session's history comes from and whether it is written to disk.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum SessionSource {
    /// Nothing is written to disk. This is the SDK default: a library should
    /// not silently create files under the user's home directory.
    #[default]
    InMemory,
    /// Restore entries held by the caller without writing them to disk.
    InMemoryEntries(Vec<kiss_coding::SessionEntry>),
    /// Create a new session file in the session directory.
    Create,
    /// Continue the most recent session for this working directory.
    ContinueRecent,
    /// Open an existing session file.
    Open(PathBuf),
    /// Copy an existing session file into a new one and continue there.
    Fork(PathBuf),
}

impl SessionSource {
    /// Parse the session option the language bindings take: `in-memory`
    /// (also when absent), `create`, `continue`, `open:<path>`, or
    /// `fork:<path>`.
    pub fn parse(spec: Option<&str>) -> Result<Self, String> {
        Ok(match spec {
            None | Some("in-memory") => SessionSource::InMemory,
            Some("create") => SessionSource::Create,
            Some("continue") => SessionSource::ContinueRecent,
            Some(spec) if let Some(path) = spec.strip_prefix("open:") => {
                SessionSource::Open(PathBuf::from(path))
            }
            Some(spec) if let Some(path) = spec.strip_prefix("fork:") => {
                SessionSource::Fork(PathBuf::from(path))
            }
            Some(other) => {
                return Err(format!(
                    "session must be 'in-memory', 'create', 'continue', 'open:<path>', or \
                     'fork:<path>', got {other:?}"
                ));
            }
        })
    }
}

// Keep the public `Eq` bound from earlier SDK releases. Session entries come
// from JSON-compatible history, where non-finite floating-point costs are not
// valid values.
impl Eq for SessionSource {}

/// Everything needed to build a [`crate::Session`].
#[derive(Clone)]
pub struct SessionOptions {
    /// Working directory for tools, project settings, and resource discovery.
    pub cwd: PathBuf,
    /// Model pattern, for example `anthropic/claude-opus-4-5` or `gpt-5`.
    pub model: Option<String>,
    /// Restrict model resolution to one provider.
    pub provider: Option<String>,
    /// Reasoning effort. Defaults to the settings value, then to `off`.
    pub thinking_level: Option<ThinkingLevel>,
    /// Credential for the selected provider, bypassing stored credentials.
    pub api_key: Option<String>,
    /// An alternative `models.json` catalog. Tests point this at a fake server.
    pub models_file: Option<PathBuf>,
    /// Allowlist of built-in tool names. `None` means the four defaults.
    pub tools: Option<Vec<String>>,
    /// Tool names to remove after the allowlist is applied.
    pub exclude_tools: Vec<String>,
    /// Disable every built-in tool.
    pub no_tools: bool,
    /// Extra tools implemented by the caller.
    pub custom_tools: Vec<DynTool>,
    /// MCP servers supplied by the embedding transport for this session only.
    pub mcp_servers: BTreeMap<String, kiss_mcp::ServerEntry>,
    /// Replace the generated system prompt entirely.
    pub system_prompt: Option<String>,
    /// Append to the generated system prompt.
    pub append_system_prompt: Option<String>,
    /// Where history comes from.
    pub session: SessionSource,
    /// Directory that holds session files.
    pub session_dir: Option<PathBuf>,
    /// Display name recorded in the session file.
    pub session_name: Option<String>,
    /// Replace the loaded settings wholesale.
    pub settings: Option<Settings>,
    /// Load project-local skills, prompt templates, and MCP configuration.
    /// Off by default because those files can execute project-controlled code.
    /// When neither this nor `ignore_project_files` is set, a saved trust
    /// decision for the folder applies.
    pub trust_project_files: bool,
    /// Never load project-local files, even when the folder is saved as
    /// trusted. `trust_project_files` wins when both are set.
    pub ignore_project_files: bool,
    /// Skip discovery of `AGENTS.md` and similar context files.
    pub no_context_files: bool,
    /// How many events the broadcast channel buffers per subscriber.
    pub event_capacity: usize,
    /// Replace the provider transport. Used by tests and custom transports.
    pub stream_fn: Option<StreamFn>,
}

impl std::fmt::Debug for SessionOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOptions")
            .field("cwd", &self.cwd)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("thinking_level", &self.thinking_level)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("models_file", &self.models_file)
            .field("tools", &self.tools)
            .field("exclude_tools", &self.exclude_tools)
            .field("no_tools", &self.no_tools)
            .field("custom_tools", &self.custom_tools.len())
            .field("mcp_servers", &self.mcp_servers.len())
            .field("session", &self.session)
            .field("session_dir", &self.session_dir)
            .field("session_name", &self.session_name)
            .field("trust_project_files", &self.trust_project_files)
            .field("ignore_project_files", &self.ignore_project_files)
            .field("no_context_files", &self.no_context_files)
            .field("event_capacity", &self.event_capacity)
            .field("stream_fn", &self.stream_fn.is_some())
            .finish()
    }
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            model: None,
            provider: None,
            thinking_level: None,
            api_key: None,
            models_file: None,
            tools: None,
            exclude_tools: Vec::new(),
            no_tools: false,
            custom_tools: Vec::new(),
            mcp_servers: BTreeMap::new(),
            system_prompt: None,
            append_system_prompt: None,
            session: SessionSource::default(),
            session_dir: None,
            session_name: None,
            settings: None,
            trust_project_files: false,
            ignore_project_files: false,
            no_context_files: false,
            event_capacity: 1024,
            stream_fn: None,
        }
    }
}

/// The pieces `SessionOptions::build` produced, before the SDK wraps them.
pub struct BuiltSession {
    pub session: Arc<AgentSession>,
    pub model: Model,
    pub tool_names: Vec<String>,
}

impl SessionOptions {
    /// Perform startup and construct the underlying harness session.
    pub fn build(&self, sink: SessionEventSink) -> Result<BuiltSession> {
        let cwd = self.cwd.clone();
        std::fs::create_dir_all(&cwd)
            .with_context(|| format!("create working directory {}", cwd.display()))?;

        let (trusted, settings) = self.trust_and_settings();

        let registry = Registry::load(self.models_file.as_deref());
        let chosen = model_choice::choose(&self.model_request(), &settings, &registry)?.context(
            "no model available: pass `model`, or configure a credential such as ANTHROPIC_API_KEY",
        )?;

        let context = if self.no_context_files {
            Vec::new()
        } else {
            context_files::discover(&cwd)
        };
        let skills = kiss_coding::skills::discover(&cwd, trusted, &[]);

        let mut tool_names = select_tool_names(
            self.tools.as_deref(),
            &self.exclude_tools,
            self.no_tools,
            settings.default_tools.as_deref(),
        );
        let mcp = self.load_mcp(
            &cwd,
            trusted,
            &mut tool_names,
            settings.default_tools.as_deref(),
        )?;
        let files = context_files::system_prompt_files(&cwd, trusted);
        let append = files.append_after(self.append_system_prompt.as_deref());
        let custom = self.system_prompt.clone().or(files.replace);
        let system_prompt = build_system_prompt(&SystemPromptOptions {
            custom_prompt: custom.as_deref(),
            append: append.as_deref(),
            selected_tools: &tool_names,
            cwd: &cwd.display().to_string(),
            context_files: &context,
            skills: &skills,
        });

        let mut tools = build_tools(&tool_names, &cwd, &settings, mcp.as_ref());
        tools.extend(self.custom_tools.iter().cloned());
        let mut all_names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
        all_names.dedup();

        let mut manager = self.open_manager(&cwd, &settings)?;
        if let Some(name) = &self.session_name {
            manager.append_session_info(name)?;
        }

        let (model, thinking) = model_choice::for_session(
            &self.model_request(),
            self.thinking_level,
            chosen,
            &manager,
            &settings,
            &registry,
        );

        let api_key_override = self
            .api_key
            .clone()
            .map(|key| (model.provider.clone(), key));
        let session = AgentSession::new_with_subagents_allowed(
            manager,
            tools,
            registry,
            settings,
            system_prompt,
            model.clone(),
            thinking,
            api_key_override,
            sink,
            !self.no_tools,
        );
        session.set_stream_fn(self.stream_fn.clone());

        Ok(BuiltSession {
            session,
            model,
            tool_names: all_names,
        })
    }

    fn load_mcp(
        &self,
        cwd: &Path,
        trusted: bool,
        tool_names: &mut Vec<String>,
        defaults: Option<&[String]>,
    ) -> Result<Option<kiss_mcp::McpManager>> {
        if self.no_tools {
            return Ok(None);
        }
        let mut loaded = kiss_mcp::config::load(cwd, trusted)?;
        for (name, server) in &self.mcp_servers {
            server.validate(name)?;
            loaded
                .config
                .mcp_servers
                .insert(name.clone(), server.clone());
        }
        if loaded.enabled_server_count() == 0 {
            tool_names.retain(|name| name != "mcp");
            return Ok(None);
        }
        let explicitly_excluded = self
            .exclude_tools
            .iter()
            .any(|pattern| crate::tools::tool_pattern_matches(pattern, "mcp"));
        // An SDK tool list is exact. Only the CLI keeps MCP beside `--tools`.
        if self.tools.is_none()
            && defaults.is_none()
            && !explicitly_excluded
            && !tool_names.iter().any(|name| name == "mcp")
        {
            tool_names.push("mcp".to_string());
        }
        if !tool_names.iter().any(|name| name == "mcp") {
            return Ok(None);
        }
        Ok(Some(kiss_mcp::McpManager::new(loaded)?))
    }

    /// The folder new and resumed sessions of these options are saved in:
    /// `session_dir`, else the sessionDir setting read under the same project
    /// trust that `build` uses.
    pub fn sessions_directory(&self) -> PathBuf {
        let (_, settings) = self.trust_and_settings();
        kiss_coding::session_dir(
            self.session_dir.as_deref().and_then(Path::to_str),
            &settings,
        )
    }

    fn trust_and_settings(&self) -> (bool, Settings) {
        let trusted = trust::resolve(
            &self.cwd,
            if self.trust_project_files {
                Some(true)
            } else if self.ignore_project_files {
                Some(false)
            } else {
                None
            },
        );
        let settings = match &self.settings {
            Some(settings) => settings.clone(),
            None => Settings::load(&self.cwd, trusted),
        };
        (trusted, settings)
    }

    fn model_request(&self) -> ModelRequest<'_> {
        ModelRequest {
            pattern: self.model.as_deref(),
            provider: self.provider.as_deref(),
            api_key_given: self.api_key.is_some(),
            import_external_credentials: false,
        }
    }

    fn open_manager(&self, cwd: &Path, settings: &Settings) -> Result<SessionManager> {
        let dir =
            kiss_coding::session_dir(self.session_dir.as_deref().and_then(Path::to_str), settings);
        Ok(match &self.session {
            SessionSource::InMemory => SessionManager::in_memory(cwd),
            SessionSource::InMemoryEntries(entries) => {
                SessionManager::in_memory_with_entries(cwd, entries.clone())
            }
            SessionSource::Create => SessionManager::create(cwd, Some(dir))?,
            SessionSource::ContinueRecent => SessionManager::continue_recent(cwd, Some(dir))?,
            SessionSource::Open(path) => SessionManager::open(path)?,
            SessionSource::Fork(path) => SessionManager::fork_from(path, cwd, Some(dir))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_session_writes_nothing_to_disk() {
        assert_eq!(SessionOptions::default().session, SessionSource::InMemory);
    }

    #[test]
    fn externally_managed_entries_restore_in_memory() {
        fn requires_eq<T: Eq>() {}
        requires_eq::<SessionSource>();

        let cwd = Path::new("/work");
        let mut original = SessionManager::in_memory(cwd);
        original
            .append_message(kiss_agent::AgentMessage::user("restored"))
            .unwrap();
        let options = SessionOptions {
            cwd: cwd.into(),
            session: SessionSource::InMemoryEntries(original.entries().to_vec()),
            ..Default::default()
        };
        let restored = options.open_manager(cwd, &Settings::default()).unwrap();
        assert_eq!(restored.build_session_context().messages.len(), 1);
        assert!(!restored.is_persisted());
    }

    #[test]
    fn a_missing_model_pattern_is_an_error_naming_the_pattern() {
        let directory = tempfile::tempdir().unwrap();
        let options = SessionOptions {
            cwd: directory.path().to_path_buf(),
            model: Some("definitely/not-a-model".into()),
            ..Default::default()
        };
        let Err(error) = options.build(Arc::new(|_| {})) else {
            panic!("a model pattern that matches nothing must fail the build");
        };
        assert!(
            error.to_string().contains("definitely/not-a-model"),
            "{error}"
        );
    }

    #[test]
    fn session_mcp_servers_enable_the_mcp_tool_without_writing_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let mut options = SessionOptions {
            cwd: directory.path().to_path_buf(),
            ..Default::default()
        };
        options.mcp_servers.insert(
            "editor".into(),
            kiss_mcp::ServerEntry {
                command: Some("test-mcp".into()),
                ..Default::default()
            },
        );

        let mut names = select_tool_names(None, &[], false, None);
        let manager = options
            .load_mcp(directory.path(), false, &mut names, None)
            .unwrap()
            .expect("MCP manager");

        assert!(names.iter().any(|name| name == "mcp"));
        assert!(manager.config().config.mcp_servers.contains_key("editor"));
        assert!(!directory.path().join(".mcp.json").exists());
        for defaults in [
            vec![],
            vec!["read".into()],
            vec!["-mcp".into()],
            vec!["+mcp".into()],
        ] {
            let mut names = select_tool_names(None, &[], false, Some(&defaults));
            let manager = options
                .load_mcp(directory.path(), false, &mut names, Some(&defaults))
                .unwrap();
            let expected = defaults == ["+mcp"];
            assert_eq!(manager.is_some(), expected);
            assert_eq!(
                names.iter().filter(|name| *name == "mcp").count(),
                usize::from(expected)
            );
        }
    }
}
