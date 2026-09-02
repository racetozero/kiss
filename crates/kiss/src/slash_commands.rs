//! User-visible slash commands mirrored from Pi's interactive command list.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

impl SlashCommand {
    const fn new(
        name: &'static str,
        description: &'static str,
        argument_hint: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            description,
            argument_hint,
        }
    }
}

/// Pi core commands at the v0.84.4 release commit.
pub(crate) const PI_CORE_SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand::new("settings", "Open settings menu", None),
    SlashCommand::new(
        "model",
        "Select model (opens selector UI)",
        Some("<provider/model>"),
    ),
    SlashCommand::new("tree", "Navigate session tree (switch branches)", None),
    SlashCommand::new("thinking", "Set thinking level", Some("<level>")),
    SlashCommand::new(
        "scoped-models",
        "Enable/disable models for Ctrl+P cycling",
        None,
    ),
    SlashCommand::new(
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
        None,
    ),
    SlashCommand::new(
        "import",
        "Import and resume a session from a JSONL file",
        None,
    ),
    SlashCommand::new("share", "Share session as a secret GitHub gist", None),
    SlashCommand::new("copy", "Copy last agent message to clipboard", None),
    SlashCommand::new("name", "Set session display name", None),
    SlashCommand::new("session", "Show session info and stats", None),
    SlashCommand::new("changelog", "Show changelog entries", None),
    SlashCommand::new("hotkeys", "Show all keyboard shortcuts", None),
    SlashCommand::new(
        "fork",
        "Create a new fork from a previous user message",
        None,
    ),
    SlashCommand::new(
        "clone",
        "Duplicate the current session at the current position",
        None,
    ),
    SlashCommand::new(
        "trust",
        "Save project trust decision for future sessions",
        None,
    ),
    SlashCommand::new(
        "login",
        "Configure provider authentication",
        Some("<provider>"),
    ),
    SlashCommand::new("logout", "Remove provider authentication", None),
    SlashCommand::new("new", "Start a new session", None),
    SlashCommand::new("compact", "Manually compact the session context", None),
    SlashCommand::new("resume", "Resume a different session", None),
    SlashCommand::new(
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
        None,
    ),
    SlashCommand::new("quit", "Quit Kiss", None),
];

/// Pi ships this command through its built-in llama.cpp extension.
pub(crate) const LLAMA_SLASH_COMMAND: SlashCommand =
    SlashCommand::new("llama", "Manage llama.cpp router models", None);

/// KISS-local commands that preserve Pi's core command inventory.
pub(crate) const KISS_SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand::new("mcp", "Manage MCP servers", None),
    SlashCommand::new(
        "btw",
        "Ask a quick read-only side question",
        Some("<question>"),
    ),
    SlashCommand::new(
        "recap",
        "Generate or configure the one-line session recap",
        Some("[now|on|off]"),
    ),
    SlashCommand::new(
        "workflow",
        "Run one task as a dynamic workflow",
        Some("<prompt>"),
    ),
    SlashCommand::new("workflows", "Browse and manage workflow runs", None),
];

pub(crate) fn commands() -> impl Iterator<Item = &'static SlashCommand> {
    PI_CORE_SLASH_COMMANDS
        .iter()
        .chain(std::iter::once(&LLAMA_SLASH_COMMAND))
        .chain(KISS_SLASH_COMMANDS.iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_names_match_tracked_pi_order() {
        let names: Vec<&str> = PI_CORE_SLASH_COMMANDS
            .iter()
            .map(|command| command.name)
            .collect();
        assert_eq!(
            names,
            [
                "settings",
                "model",
                "tree",
                "thinking",
                "scoped-models",
                "export",
                "import",
                "share",
                "copy",
                "name",
                "session",
                "changelog",
                "hotkeys",
                "fork",
                "clone",
                "trust",
                "login",
                "logout",
                "new",
                "compact",
                "resume",
                "reload",
                "quit",
            ]
        );
    }

    #[test]
    fn core_catalog_matches_pi_v0_84_4() {
        let actual = PI_CORE_SLASH_COMMANDS
            .iter()
            .map(|command| (command.name, command.description, command.argument_hint))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                ("settings", "Open settings menu", None),
                (
                    "model",
                    "Select model (opens selector UI)",
                    Some("<provider/model>")
                ),
                ("tree", "Navigate session tree (switch branches)", None),
                ("thinking", "Set thinking level", Some("<level>")),
                (
                    "scoped-models",
                    "Enable/disable models for Ctrl+P cycling",
                    None
                ),
                (
                    "export",
                    "Export session (HTML default, or specify path: .html/.jsonl)",
                    None
                ),
                (
                    "import",
                    "Import and resume a session from a JSONL file",
                    None
                ),
                ("share", "Share session as a secret GitHub gist", None),
                ("copy", "Copy last agent message to clipboard", None),
                ("name", "Set session display name", None),
                ("session", "Show session info and stats", None),
                ("changelog", "Show changelog entries", None),
                ("hotkeys", "Show all keyboard shortcuts", None),
                (
                    "fork",
                    "Create a new fork from a previous user message",
                    None
                ),
                (
                    "clone",
                    "Duplicate the current session at the current position",
                    None
                ),
                (
                    "trust",
                    "Save project trust decision for future sessions",
                    None
                ),
                (
                    "login",
                    "Configure provider authentication",
                    Some("<provider>")
                ),
                ("logout", "Remove provider authentication", None),
                ("new", "Start a new session", None),
                ("compact", "Manually compact the session context", None),
                ("resume", "Resume a different session", None),
                (
                    "reload",
                    "Reload keybindings, extensions, skills, prompts, themes, and context files",
                    None
                ),
                ("quit", "Quit Kiss", None),
            ]
        );
    }

    #[test]
    fn user_visible_surface_includes_shipped_llama_command() {
        let names: Vec<&str> = commands().map(|command| command.name).collect();
        assert_eq!(
            &names[names.len() - 6..],
            ["llama", "mcp", "btw", "recap", "workflow", "workflows"]
        );
        assert_eq!(
            names.len(),
            PI_CORE_SLASH_COMMANDS.len() + 1 + KISS_SLASH_COMMANDS.len()
        );
    }
}
