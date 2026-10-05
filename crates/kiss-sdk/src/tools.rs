//! The one table that maps a built-in tool name to its implementation.
//!
//! Both the command-line program (`crates/kiss/src/setup.rs`) and the SDK build
//! their tool list from here, so "which tools does `kiss` have" has exactly one
//! answer no matter how the agent was started.

use kiss_agent::DynTool;
use kiss_coding::settings::Settings;
use kiss_mcp::McpManager;
use std::path::Path;
use std::sync::Arc;

/// Tools enabled when the caller does not choose.
pub const DEFAULT_TOOLS: &[&str] = &["read", "write", "edit", "bash"];

/// Every built-in tool name the agent recognizes.
pub const ALL_TOOLS: &[&str] = &["read", "write", "edit", "bash", "grep", "find", "ls", "mcp"];

/// Resolve a requested tool list into concrete tools.
///
/// Names that are not in [`ALL_TOOLS`] are ignored rather than rejected, which
/// keeps a configuration written for a newer build usable on an older one.
pub fn build_tools(
    names: &[String],
    cwd: &Path,
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
                if let Some(path) = settings
                    .shell_path
                    .as_ref()
                    .filter(|path| !path.trim().is_empty())
                {
                    bash.shell_path = Some(path.clone());
                }
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
        if let Some(tool) = tool {
            tools.push(tool);
        }
    }
    tools
}

/// Apply an allowlist and an exclusion list to the built-in tool names.
pub fn select_tool_names(
    allow: Option<&[String]>,
    exclude: &[String],
    no_tools: bool,
    defaults: Option<&[String]>,
) -> Vec<String> {
    if no_tools {
        return Vec::new();
    }
    let mut names: Vec<String> = match allow {
        Some(allow) => allow.to_vec(),
        None => {
            let entries = defaults.unwrap_or_default();
            let mut names: Vec<String> = entries
                .iter()
                .filter(|name| !name.starts_with(['+', '-']))
                .cloned()
                .collect();
            if defaults.is_none() || (!entries.is_empty() && names.is_empty()) {
                names = DEFAULT_TOOLS.iter().map(|name| name.to_string()).collect();
            }
            for entry in entries {
                if let Some(name) = entry.strip_prefix('+') {
                    if !name.is_empty() && !names.iter().any(|existing| existing == name) {
                        names.push(name.to_string());
                    }
                } else if let Some(name) = entry.strip_prefix('-') {
                    names.retain(|existing| existing != name);
                }
            }
            names
        }
    };
    names.retain(|name| ALL_TOOLS.contains(&name.as_str()));
    names.retain(|name| !exclude.iter().any(|excluded| excluded == name));
    let mut seen = std::collections::HashSet::new();
    names.retain(|name| seen.insert(name.clone()));
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_command_line_program() {
        assert_eq!(select_tool_names(None, &[], false, None), DEFAULT_TOOLS);
    }

    #[test]
    fn exclusions_apply_after_the_allowlist() {
        let allow = ["read".to_string(), "grep".to_string(), "bash".to_string()];
        let exclude = ["bash".to_string()];
        assert_eq!(
            select_tool_names(Some(&allow), &exclude, false, None),
            ["read", "grep"]
        );
    }

    #[test]
    fn unknown_names_are_dropped() {
        let allow = ["read".to_string(), "teleport".to_string()];
        assert_eq!(select_tool_names(Some(&allow), &[], false, None), ["read"]);
    }

    #[test]
    fn no_tools_wins() {
        let allow = ["read".to_string()];
        assert!(select_tool_names(Some(&allow), &[], true, None).is_empty());
    }

    #[test]
    fn configured_defaults_and_cli_selection_have_ordered_precedence() {
        let defaults = ["-bash", "+grep", "+grep", "+bash", "-write"].map(str::to_string);
        assert_eq!(
            select_tool_names(None, &[], false, Some(&defaults)),
            ["read", "edit", "grep", "bash"]
        );
        let defaults = ["read", "+grep"].map(str::to_string);
        let exclude = ["grep".to_string()];
        assert_eq!(
            select_tool_names(None, &exclude, false, Some(&defaults)),
            ["read"]
        );
        let allow = ["ls".to_string()];
        assert_eq!(
            select_tool_names(Some(&allow), &[], false, Some(&defaults)),
            ["ls"]
        );
        assert!(select_tool_names(None, &[], false, Some(&[])).is_empty());
        assert!(select_tool_names(Some(&allow), &[], true, Some(&defaults)).is_empty());
    }
}
