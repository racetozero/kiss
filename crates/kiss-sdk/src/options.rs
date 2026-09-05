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
use kiss_coding::session::manager::{SessionManager, default_session_dir};
use kiss_coding::session_runner::{AgentSession, SessionEventSink};
use kiss_coding::settings::Settings;
use kiss_coding::system_prompt::{SystemPromptOptions, build_system_prompt};
use kiss_coding::{context_files, trust};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the session's history comes from and whether it is written to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SessionSource {
    /// Nothing is written to disk. This is the SDK default: a library should
    /// not silently create files under the user's home directory.
    #[default]
    InMemory,
    /// Create a new session file in the session directory.
    Create,
    /// Continue the most recent session for this working directory.
    ContinueRecent,
    /// Open an existing session file.
    Open(PathBuf),
    /// Copy an existing session file into a new one and continue there.
    Fork(PathBuf),
}

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
    pub trust_project_files: bool,
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
            .field("session", &self.session)
            .field("session_dir", &self.session_dir)
            .field("session_name", &self.session_name)
            .field("trust_project_files", &self.trust_project_files)
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
            system_prompt: None,
            append_system_prompt: None,
            session: SessionSource::default(),
            session_dir: None,
            session_name: None,
            settings: None,
            trust_project_files: false,
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

        let trusted = self.trust_project_files
            || trust::resolve_non_interactive(
                &cwd,
                None,
                Settings::load(&cwd, false).default_project_trust,
            );
        let settings = match &self.settings {
            Some(settings) => settings.clone(),
            None => Settings::load(&cwd, trusted),
        };

        let registry = Registry::load(self.models_file.as_deref());
        let (model, pattern_thinking) = self.resolve_model(&registry)?;
        let thinking = self
            .thinking_level
            .or(pattern_thinking)
            .or_else(|| {
                settings
                    .default_thinking_level
                    .as_deref()
                    .and_then(ThinkingLevel::parse)
            })
            .unwrap_or(ThinkingLevel::Off);

        let context = if self.no_context_files {
            Vec::new()
        } else {
            context_files::discover(&cwd)
        };
        let skills = kiss_coding::skills::discover(&cwd, trusted, &[]);

        let tool_names =
            select_tool_names(self.tools.as_deref(), &self.exclude_tools, self.no_tools);
        let mcp = self.load_mcp(&cwd, trusted, &tool_names)?;
        let files = context_files::system_prompt_files(&cwd);
        let custom = self.system_prompt.clone().or(files.replace);
        let append = match (&self.append_system_prompt, &files.append) {
            (Some(first), Some(second)) => Some(format!("{first}\n\n{second}")),
            (Some(value), None) => Some(value.clone()),
            (None, Some(value)) => Some(value.clone()),
            (None, None) => None,
        };
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

        // A resumed session may name a model the caller did not ask for.
        let (model, thinking) = if self.model.is_none() {
            let context = manager.build_session_context();
            let restored = context
                .model
                .as_ref()
                .and_then(|(provider, id)| {
                    registry.resolve(id, Some(provider)).map(|(model, _)| model)
                })
                .unwrap_or(model);
            (restored, context.thinking_level.unwrap_or(thinking))
        } else {
            (model, thinking)
        };

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
        tool_names: &[String],
    ) -> Result<Option<kiss_mcp::McpManager>> {
        if !tool_names.iter().any(|name| name == "mcp") {
            return Ok(None);
        }
        let loaded = kiss_mcp::config::load(cwd, trusted)?;
        if loaded.enabled_server_count() == 0 {
            return Ok(None);
        }
        Ok(Some(kiss_mcp::McpManager::new(loaded)?))
    }

    fn resolve_model(&self, registry: &Registry) -> Result<(Model, Option<ThinkingLevel>)> {
        if let Some(pattern) = &self.model {
            return registry
                .resolve(pattern, self.provider.as_deref())
                .with_context(|| format!("no model matches pattern '{pattern}'"));
        }
        if let Some(provider) = &self.provider
            && let Some(model) = registry.all().iter().find(|m| &m.provider == provider)
        {
            return Ok((model.clone(), None));
        }
        for model in registry.all() {
            if kiss_ai::auth::resolve_api_key_local(&model.provider, &registry.declared_keys)
                .is_some()
            {
                return Ok((model.clone(), None));
            }
        }
        anyhow::bail!(
            "no model available: pass `model`, or configure a credential such as ANTHROPIC_API_KEY"
        )
    }

    fn open_manager(&self, cwd: &Path, settings: &Settings) -> Result<SessionManager> {
        let dir = self
            .session_dir
            .clone()
            .or_else(|| settings.session_dir.as_ref().map(PathBuf::from))
            .unwrap_or_else(default_session_dir);
        Ok(match &self.session {
            SessionSource::InMemory => SessionManager::in_memory(cwd),
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
    fn a_missing_model_pattern_is_an_error_naming_the_pattern() {
        let options = SessionOptions {
            model: Some("definitely/not-a-model".into()),
            ..Default::default()
        };
        let registry = Registry::from_builtin();
        let error = options.resolve_model(&registry).unwrap_err();
        assert!(
            error.to_string().contains("definitely/not-a-model"),
            "{error}"
        );
    }
}
