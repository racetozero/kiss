//! Shared startup: resolve settings, trust, resources, model, session, and
//! build the AgentSession all modes drive.

use crate::args::Args;
use anyhow::{Context as _, Result};
use kiss_agent::DynTool;
use kiss_ai::{Model, Registry, ThinkingLevel};
use kiss_coding::context_files;
use kiss_coding::model_choice::{self, ModelRequest};
use kiss_coding::session::manager::SessionManager;
use kiss_coding::session_runner::{AgentSession, SessionEventSink};
use kiss_coding::settings::Settings;
use kiss_coding::skills::Skill;
use kiss_coding::system_prompt::{SystemPromptOptions, build_system_prompt};
use kiss_mcp::McpManager;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Startup {
    pub session: Arc<AgentSession>,
    pub settings: Settings,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<kiss_coding::prompts::PromptTemplate>,
    pub context_file_paths: Vec<PathBuf>,
    pub enabled_models: Vec<Model>,
    pub initial_message: Option<String>,
    pub needs_login: bool,
    /// Problems found while starting that did not stop it, for the mode to
    /// show where it shows notices.
    pub warnings: Vec<String>,
}

pub struct ReloadedRuntime {
    pub settings: Settings,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<kiss_coding::prompts::PromptTemplate>,
    pub context_file_paths: Vec<PathBuf>,
    pub tools: Vec<DynTool>,
    pub system_prompt: String,
    /// Files skipped because they could not be read, for the mode to show.
    pub problems: Vec<String>,
}

// The tool table lives in kiss-sdk so the command-line program and every SDK
// surface agree on which tools exist and how they are built.
pub use kiss_sdk::tools::build_tools;

fn configured_tools(
    args: &Args,
    cwd: &std::path::Path,
    trusted: bool,
    settings: &Settings,
) -> Result<(Vec<String>, Option<McpManager>)> {
    let allow = args.tools.as_ref().map(|_| Args::split_csv(&args.tools));
    let mut names = kiss_sdk::tools::select_tool_names(
        allow.as_deref(),
        &Args::split_csv(&args.exclude_tools),
        args.no_tools,
        settings.default_tools.as_deref(),
    );
    if args.no_tools {
        return Ok((names, None));
    }
    if args.no_mcp {
        names.retain(|name| name != "mcp");
        return Ok((names, None));
    }
    let loaded = kiss_mcp::config::load(cwd, trusted)?;
    if loaded.enabled_server_count() == 0 {
        names.retain(|name| name != "mcp");
        return Ok((names, None));
    }
    if kiss_sdk::tools::includes_mcp_by_default(
        allow.as_deref(),
        &Args::split_csv(&args.exclude_tools),
        settings.default_tools.as_deref(),
    ) && !names.iter().any(|name| name == "mcp")
    {
        names.push("mcp".to_string());
    }
    let manager = names
        .iter()
        .any(|name| name == "mcp")
        .then(|| McpManager::new(loaded))
        .transpose()?;
    Ok((names, manager))
}

/// Reload the file-backed inputs used by an existing interactive session.
pub fn reload_runtime(args: &Args, cwd: &std::path::Path) -> Result<ReloadedRuntime> {
    let trusted = args.project_trusted(cwd);
    let mut settings = Settings::load(cwd, trusted);
    if args.experimental_context_file {
        settings.override_for_run(|settings| settings.experimental_context_file = true);
    }

    let context = if args.no_context_files {
        Vec::new()
    } else {
        context_files::discover(cwd)
    };
    let skill_paths: Vec<PathBuf> = args.skills.iter().map(PathBuf::from).collect();
    let skills = if args.no_skills {
        kiss_coding::skills::discover(cwd, false, &skill_paths)
            .into_iter()
            .filter(|skill| {
                skill_paths
                    .iter()
                    .any(|path| skill.file_path.starts_with(path) || &skill.file_path == path)
            })
            .collect()
    } else {
        kiss_coding::skills::discover(cwd, trusted, &skill_paths)
    };
    let template_paths: Vec<PathBuf> = args.prompt_templates.iter().map(PathBuf::from).collect();
    let (prompt_templates, problems) = if args.no_prompt_templates {
        (Vec::new(), Vec::new())
    } else {
        kiss_coding::prompts::discover(cwd, trusted, &template_paths)
    };

    let files = context_files::system_prompt_files(cwd, trusted);
    let (tool_names, mcp) = configured_tools(args, cwd, trusted, &settings)?;
    let append = files.append_after(args.append_system_prompt.as_deref());
    let custom = args.system_prompt.clone().or(files.replace);
    let system_prompt = build_system_prompt(&SystemPromptOptions {
        custom_prompt: custom.as_deref(),
        append: append.as_deref(),
        selected_tools: &tool_names,
        cwd: &cwd.display().to_string(),
        context_files: &context,
        skills: &skills,
    });
    let tools = build_tools(&tool_names, cwd, &settings, mcp.as_ref());

    Ok(ReloadedRuntime {
        settings,
        skills,
        prompt_templates,
        context_file_paths: context.into_iter().map(|item| item.path).collect(),
        tools,
        system_prompt,
        problems,
    })
}

/// The models Ctrl+P cycles through: `--models`, else the enabledModels
/// setting; none when neither names any.
pub fn enabled_models(args: &Args, settings: &Settings, registry: &Registry) -> Vec<Model> {
    let patterns = Args::split_csv(&args.models);
    let patterns = if patterns.is_empty() {
        settings.enabled_models.clone().unwrap_or_default()
    } else {
        patterns
    };
    if patterns.is_empty() {
        Vec::new()
    } else {
        registry.match_patterns(&patterns)
    }
}

fn model_request(args: &Args) -> ModelRequest<'_> {
    ModelRequest {
        pattern: args.model.as_deref(),
        provider: args.provider.as_deref(),
        api_key_given: args.api_key.is_some(),
        import_external_credentials: true,
    }
}

pub fn resolve_model(
    args: &Args,
    settings: &Settings,
    registry: &Registry,
) -> Result<(Model, Option<ThinkingLevel>)> {
    model_choice::choose(&model_request(args), settings, registry)?
    .context(
        "no model available: set an API key (e.g. ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY) or pass --model/--api-key",
    )
}

fn startup_model(
    args: &Args,
    registry: &Registry,
    interactive: bool,
    resolved: Result<(Model, Option<ThinkingLevel>)>,
) -> Result<(Model, Option<ThinkingLevel>, bool)> {
    if interactive && args.model.is_none() && resolved.is_err() {
        return Ok((
            registry
                .provider_default("openai-codex")
                .context("no fallback model available")?,
            None,
            true,
        ));
    }
    resolved.map(|(model, thinking)| (model, thinking, false))
}

/// How to open a session whose stored working directory is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingCwdChoice {
    /// Run it in the current directory once; the next open asks again.
    Continue,
    /// Copy it into the current directory as a new session.
    Fork,
    Cancel,
}

/// The choices for a session whose stored working directory is gone.
pub fn missing_cwd_choices(cwd: &std::path::Path) -> [(MissingCwdChoice, String); 3] {
    let cwd = kiss_ai::paths::abbreviate_home(&cwd.display().to_string());
    [
        (
            MissingCwdChoice::Continue,
            format!("Continue in {cwd} (asks again next time)"),
        ),
        (
            MissingCwdChoice::Fork,
            format!("Fork into {cwd} (new session ID, original kept)"),
        ),
        (MissingCwdChoice::Cancel, "Cancel".to_string()),
    ]
}

pub fn session_dir(args: &Args, settings: &Settings) -> PathBuf {
    kiss_coding::session_dir(args.session_dir.as_deref(), settings)
}

pub async fn build_startup(
    args: &Args,
    interactive: bool,
    sink: SessionEventSink,
) -> Result<Startup> {
    let cwd = std::env::current_dir()?;
    let ReloadedRuntime {
        settings,
        skills,
        prompt_templates,
        context_file_paths,
        tools,
        system_prompt,
        problems,
    } = reload_runtime(args, &cwd)?;

    let mut registry = Registry::load(None);
    // Fetch only the account model lists this run can use.
    let mut warnings = registry
        .refresh_dynamic(|provider| {
            args.provider.as_deref() == Some(provider)
                || settings.default_provider.as_deref() == Some(provider)
                || args.model.as_deref().is_some_and(|model| {
                    model
                        .strip_prefix(provider)
                        .is_some_and(|rest| rest.starts_with('/'))
                })
        })
        .await;
    warnings.extend(problems);
    warnings.extend(settings.problems.iter().cloned());
    warnings.extend(registry.problems.iter().cloned());

    let (model, pattern_thinking, needs_login) = startup_model(
        args,
        &registry,
        interactive,
        resolve_model(args, &settings, &registry),
    )?;

    // Session.
    let dir = session_dir(args, &settings);
    let manager = if args.no_session {
        SessionManager::in_memory(&cwd)
    } else if let Some(reference) = &args.session {
        let path = SessionManager::resolve_reference(&dir, reference)?;
        let manager = SessionManager::open(&path)?;
        // Interactive mode asks to continue or fork when the screen starts.
        if let Some(stored) = manager.missing_cwd().filter(|_| !interactive) {
            anyhow::bail!(
                "The session's working directory {} no longer exists. Current working directory: {}. Run kiss --session {reference} in interactive mode to continue or fork it here, or run kiss --fork {reference} to copy it here as a new session.",
                stored.display(),
                cwd.display()
            );
        }
        manager
    } else if let Some(reference) = &args.fork {
        SessionManager::fork_from(
            &SessionManager::resolve_reference(&dir, reference)?,
            &cwd,
            Some(dir.clone()),
        )?
    } else if args.continue_recent {
        SessionManager::continue_recent(&cwd, Some(dir.clone()))?
    } else {
        SessionManager::create(&cwd, Some(dir.clone()))?
    };
    let mut manager = manager;
    if let Some(name) = &args.name {
        manager.append_session_info(name)?;
    }

    let (model, thinking) = model_choice::for_session(
        &model_request(args),
        args.thinking.as_deref().and_then(ThinkingLevel::parse),
        (model, pattern_thinking),
        &manager,
        &settings,
        &registry,
    );

    let enabled_models = enabled_models(args, &settings, &registry);

    let api_key_override = args
        .api_key
        .clone()
        .map(|key| (model.provider.clone(), key));
    let session = AgentSession::new_with_subagents_allowed(
        manager,
        tools,
        registry,
        settings.clone(),
        system_prompt,
        model,
        thinking,
        api_key_override,
        sink,
        !args.no_tools,
    );

    // Initial message: positional args + @files + piped stdin (print mode).
    let mut parts: Vec<String> = Vec::new();
    for arg in &args.messages {
        if let Some(file) = arg.strip_prefix('@') {
            let path = cwd.join(file);
            match std::fs::read_to_string(&path) {
                Ok(content) => parts.push(format!("<file path=\"{file}\">\n{content}\n</file>")),
                Err(e) => anyhow::bail!("could not read @{file}: {e}"),
            }
        } else {
            parts.push(arg.clone());
        }
    }
    let initial_message = if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    };

    Ok(Startup {
        session,
        settings,
        skills,
        prompt_templates,
        context_file_paths,
        enabled_models,
        initial_message,
        needs_login,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn changed_pi_provider_defaults_are_preferred() {
        let registry = Registry::from_builtin();
        assert_eq!(registry.provider_default("xai").unwrap().id, "grok-4.7");
        assert_eq!(
            registry.provider_default("meta").unwrap().id,
            "muse-spark-1.3"
        );
        for (provider, id) in [
            ("openai", "gpt-6.1-sol"),
            ("openai-codex", "gpt-6.1-sol"),
            ("fireworks", "accounts/fireworks/models/kimi-k3"),
            ("together", "moonshotai/Kimi-K3"),
            ("opencode-go", "kimi-k3.1-pro"),
            ("anthropic", "claude-opus-5-5"),
        ] {
            assert_eq!(registry.provider_default(provider).unwrap().id, id);
        }
    }

    #[test]
    fn fresh_interactive_startup_allows_login_without_credentials() {
        let registry = Registry::from_builtin();
        let args = Args::parse_from(["kiss"]);
        let (model, _, needs_login) = startup_model(
            &args,
            &registry,
            true,
            Err(anyhow::anyhow!("no credentials")),
        )
        .unwrap();
        assert_eq!(model.provider, "openai-codex");
        assert!(needs_login);
        assert!(
            startup_model(
                &args,
                &registry,
                false,
                Err(anyhow::anyhow!("no credentials"))
            )
            .is_err()
        );
        let args = Args::parse_from(["kiss", "--model", "bad-model"]);
        assert!(startup_model(&args, &registry, true, Err(anyhow::anyhow!("bad model"))).is_err());
    }
}
