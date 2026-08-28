//! Shared startup: resolve settings, trust, resources, model, session, and
//! build the AgentSession all modes drive.

use crate::args::Args;
use anyhow::{Context as _, Result};
use kiss_agent::DynTool;
use kiss_ai::{Model, Registry, ThinkingLevel};
use kiss_coding::context_files;
use kiss_coding::session::manager::{SessionManager, default_session_dir};
use kiss_coding::session_runner::{AgentSession, SessionEventSink};
use kiss_coding::settings::Settings;
use kiss_coding::skills::Skill;
use kiss_coding::system_prompt::{SystemPromptOptions, build_system_prompt};
use kiss_coding::trust;
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
}

pub struct ReloadedRuntime {
    pub settings: Settings,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<kiss_coding::prompts::PromptTemplate>,
    pub context_file_paths: Vec<PathBuf>,
    pub tools: Vec<DynTool>,
    pub system_prompt: String,
}

pub const DEFAULT_TOOLS: &[&str] = &["read", "write", "edit", "bash"];
pub const ALL_TOOLS: &[&str] = &["read", "write", "edit", "bash", "grep", "find", "ls", "mcp"];

pub fn selected_tool_names(args: &Args) -> Vec<String> {
    if args.no_tools {
        return Vec::new();
    }
    let mut names: Vec<String> = if let Some(allow) = &args.tools {
        Args::split_csv(&Some(allow.clone()))
    } else {
        DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect()
    };
    names.retain(|name| ALL_TOOLS.contains(&name.as_str()));
    for excluded in Args::split_csv(&args.exclude_tools) {
        names.retain(|n| *n != excluded);
    }
    names
}

pub fn build_tools(
    names: &[String],
    cwd: &std::path::Path,
    settings: &Settings,
    mcp: Option<&McpManager>,
) -> Vec<DynTool> {
    let mut tools: Vec<DynTool> = Vec::new();
    for name in names {
        let tool: Option<DynTool> = match name.as_str() {
            "read" => Some(Arc::new(kiss_agent::tools::read::ReadTool {
                cwd: cwd.to_path_buf(),
            })),
            "bash" => {
                let mut bash = kiss_agent::tools::bash::BashTool::new(cwd.to_path_buf());
                bash.shell_path = settings.shell_path.clone();
                bash.command_prefix = settings.shell_command_prefix.clone();
                Some(Arc::new(bash))
            }
            "edit" => Some(Arc::new(kiss_agent::tools::edit::EditTool {
                cwd: cwd.to_path_buf(),
            })),
            "write" => Some(Arc::new(kiss_agent::tools::write::WriteTool {
                cwd: cwd.to_path_buf(),
            })),
            "grep" => Some(Arc::new(kiss_coding::tools::grep::GrepTool {
                cwd: cwd.to_path_buf(),
            })),
            "find" => Some(Arc::new(kiss_coding::tools::find::FindTool {
                cwd: cwd.to_path_buf(),
            })),
            "ls" => Some(Arc::new(kiss_coding::tools::ls::LsTool {
                cwd: cwd.to_path_buf(),
            })),
            "mcp" => mcp
                .cloned()
                .map(kiss_mcp::McpTool::new)
                .map(|tool| Arc::new(tool) as DynTool),
            _ => None,
        };
        if let Some(t) = tool {
            tools.push(t);
        }
    }
    tools
}

fn configured_tools(
    args: &Args,
    cwd: &std::path::Path,
    trusted: bool,
) -> Result<(Vec<String>, Option<McpManager>)> {
    let mut names = selected_tool_names(args);
    if args.no_tools {
        return Ok((names, None));
    }
    let loaded = kiss_mcp::config::load(cwd, trusted)?;
    if loaded.enabled_server_count() == 0 {
        names.retain(|name| name != "mcp");
        return Ok((names, None));
    }
    let explicitly_excluded = Args::split_csv(&args.exclude_tools)
        .iter()
        .any(|name| name == "mcp");
    if args.tools.is_none() && !explicitly_excluded {
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
    let cli_trust = if args.approve {
        Some(true)
    } else if args.no_approve {
        Some(false)
    } else {
        None
    };
    let bootstrap = Settings::load(cwd, false);
    let trusted = trust::resolve_non_interactive(cwd, cli_trust, bootstrap.default_project_trust);
    let settings = Settings::load(cwd, trusted);

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
    let prompt_templates = if args.no_prompt_templates {
        Vec::new()
    } else {
        kiss_coding::prompts::discover(cwd, trusted, &template_paths)
    };

    let files = context_files::system_prompt_files(cwd);
    let (tool_names, mcp) = configured_tools(args, cwd, trusted)?;
    let custom = args.system_prompt.clone().or(files.replace);
    let append = match (&args.append_system_prompt, &files.append) {
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
    let tools = build_tools(&tool_names, cwd, &settings, mcp.as_ref());

    Ok(ReloadedRuntime {
        settings,
        skills,
        prompt_templates,
        context_file_paths: context.into_iter().map(|item| item.path).collect(),
        tools,
        system_prompt,
    })
}

pub fn resolve_model(
    args: &Args,
    settings: &Settings,
    registry: &Registry,
) -> Result<(Model, Option<ThinkingLevel>)> {
    // CLI flag > settings default > first model with an available key.
    if let Some(pattern) = &args.model {
        return registry
            .resolve(pattern, args.provider.as_deref())
            .with_context(|| format!("no model matches pattern '{pattern}'"));
    }
    if let (Some(provider), Some(model)) = (&settings.default_provider, &settings.default_model)
        && let Some(found) = registry.resolve(model, Some(provider))
    {
        return Ok(found);
    }
    if let Some(provider) = args
        .provider
        .as_deref()
        .or(settings.default_provider.as_deref())
        && let Some(model) = registry.all().iter().find(|m| m.provider == provider)
    {
        return Ok((model.clone(), None));
    }
    if args.api_key.is_some() {
        let provider = args.provider.as_deref().unwrap_or("anthropic");
        if let Some(model) = registry.all().iter().find(|m| m.provider == provider) {
            return Ok((model.clone(), None));
        }
    }
    for model in registry.all() {
        if kiss_ai::auth::resolve_api_key_local(&model.provider, &registry.declared_keys).is_some()
        {
            return Ok((model.clone(), None));
        }
    }
    // External discovery can query the macOS Keychain. Run it once only after
    // the fast local scan finds no usable credential.
    let external = kiss_ai::auth::external::discover();
    let mut checked = std::collections::HashSet::new();
    for model in registry.all() {
        if !checked.insert(model.provider.as_str()) {
            continue;
        }
        if kiss_ai::auth::external::auto_import_unique_from_sources(&model.provider, &external)
            .ok()
            .flatten()
            .is_some()
            && kiss_ai::auth::resolve_api_key_local(&model.provider, &registry.declared_keys)
                .is_some()
        {
            return Ok((model.clone(), None));
        }
    }
    anyhow::bail!(
        "no model available: set an API key (e.g. ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY) or pass --model/--api-key"
    )
}

pub fn session_dir(args: &Args, settings: &Settings) -> PathBuf {
    args.session_dir
        .clone()
        .or_else(|| std::env::var("KISS_SESSION_DIR").ok())
        .or_else(|| settings.session_dir.clone())
        .map(|p| {
            if let Some(rest) = p.strip_prefix("~/") {
                dirs::home_dir()
                    .map(|h| h.join(rest))
                    .unwrap_or_else(|| PathBuf::from(&p))
            } else {
                PathBuf::from(p)
            }
        })
        .unwrap_or_else(default_session_dir)
}

fn locate_session(reference: &str, dir: &std::path::Path) -> Result<PathBuf> {
    let as_path = PathBuf::from(reference);
    if as_path.is_file() {
        return Ok(as_path);
    }
    SessionManager::find_by_id(dir, reference)?
        .with_context(|| format!("no session matches '{reference}'"))
}

pub async fn build_startup(
    args: &Args,
    interactive: bool,
    sink: SessionEventSink,
) -> Result<Startup> {
    let cwd = std::env::current_dir()?;
    let cli_trust = if args.approve {
        Some(true)
    } else if args.no_approve {
        Some(false)
    } else {
        None
    };
    // Interactive trust prompting is handled by the caller before this via
    // saved decisions; default flow matches non-interactive resolution.
    let bootstrap_settings = Settings::load(&cwd, false);
    let trusted =
        trust::resolve_non_interactive(&cwd, cli_trust, bootstrap_settings.default_project_trust);
    let settings = Settings::load(&cwd, trusted);

    let mut registry = Registry::load(None);
    let radius_selected = args.provider.as_deref() == Some("radius")
        || args
            .model
            .as_deref()
            .is_some_and(|model| model.starts_with("radius/"))
        || settings.default_provider.as_deref() == Some("radius");
    if radius_selected {
        registry.refresh_radius().await;
    }

    let (model, cli_thinking) = resolve_model(args, &settings, &registry)?;
    let thinking = args
        .thinking
        .as_deref()
        .and_then(ThinkingLevel::parse)
        .or(cli_thinking)
        .or_else(|| {
            settings
                .default_thinking_level
                .as_deref()
                .and_then(ThinkingLevel::parse)
        })
        .unwrap_or(ThinkingLevel::Off);

    // Resources.
    let context = if args.no_context_files {
        Vec::new()
    } else {
        context_files::discover(&cwd)
    };
    let skill_paths: Vec<PathBuf> = args.skills.iter().map(PathBuf::from).collect();
    let skills = if args.no_skills {
        kiss_coding::skills::discover(&cwd, false, &skill_paths)
            .into_iter()
            .filter(|s| {
                skill_paths
                    .iter()
                    .any(|p| s.file_path.starts_with(p) || &s.file_path == p)
            })
            .collect()
    } else {
        kiss_coding::skills::discover(&cwd, trusted, &skill_paths)
    };
    let template_paths: Vec<PathBuf> = args.prompt_templates.iter().map(PathBuf::from).collect();
    let prompt_templates = if args.no_prompt_templates {
        Vec::new()
    } else {
        kiss_coding::prompts::discover(&cwd, trusted, &template_paths)
    };

    // System prompt.
    let files = context_files::system_prompt_files(&cwd);
    let (tool_names, mcp) = configured_tools(args, &cwd, trusted)?;
    let custom = args.system_prompt.clone().or(files.replace);
    let append = match (&args.append_system_prompt, &files.append) {
        (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
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

    // Session.
    let dir = session_dir(args, &settings);
    let manager = if args.no_session {
        SessionManager::in_memory(&cwd)
    } else if let Some(reference) = &args.session {
        SessionManager::open(&locate_session(reference, &dir)?)?
    } else if let Some(reference) = &args.fork {
        SessionManager::fork_from(&locate_session(reference, &dir)?, &cwd, Some(dir.clone()))?
    } else if args.continue_recent {
        SessionManager::continue_recent(&cwd, Some(dir.clone()))?
    } else {
        SessionManager::create(&cwd, Some(dir.clone()))?
    };
    let mut manager = manager;
    if let Some(name) = &args.name {
        manager.append_session_info(name)?;
    }

    // Session context may override model/thinking (resumed sessions).
    let session_context = manager.build_session_context();
    let (model, thinking) = if args.model.is_none() {
        let restored_model = session_context
            .model
            .as_ref()
            .and_then(|(p, id)| registry.resolve(id, Some(p)).map(|(m, _)| m))
            .unwrap_or(model);
        (
            restored_model,
            session_context.thinking_level.unwrap_or(thinking),
        )
    } else {
        (model, thinking)
    };

    let tools = build_tools(&tool_names, &cwd, &settings, mcp.as_ref());
    let enabled_models = {
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
    };

    let api_key_override = args
        .api_key
        .clone()
        .map(|key| (model.provider.clone(), key));
    let session = AgentSession::new(
        manager,
        tools,
        registry,
        settings.clone(),
        system_prompt,
        model,
        thinking,
        api_key_override,
        sink,
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

    let _ = interactive;
    Ok(Startup {
        session,
        settings,
        skills,
        prompt_templates,
        context_file_paths: context.into_iter().map(|c| c.path).collect(),
        enabled_models,
        initial_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn default_tools_match_pi() {
        let args = Args::parse_from(["kiss"]);
        assert_eq!(selected_tool_names(&args), DEFAULT_TOOLS);
    }

    #[test]
    fn explicit_tools_can_enable_optional_builtins() {
        let args = Args::parse_from(["kiss", "--tools", "read,grep"]);
        assert_eq!(selected_tool_names(&args), ["read", "grep"]);
    }

    #[test]
    fn exclusions_apply_to_default_tools() {
        let args = Args::parse_from(["kiss", "--exclude-tools", "bash"]);
        assert_eq!(selected_tool_names(&args), ["read", "write", "edit"]);
    }

    #[test]
    fn no_tools_disables_every_builtin() {
        let args = Args::parse_from(["kiss", "--no-tools"]);
        assert!(selected_tool_names(&args).is_empty());
    }
}
