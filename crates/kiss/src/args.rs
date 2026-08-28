//! CLI argument parsing.

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum McpScope {
    User,
    Project,
}

#[derive(Debug, Clone, clap::Args)]
pub struct McpServerArgs {
    /// Streamable HTTP endpoint.
    #[arg(long, conflicts_with = "stdio")]
    pub url: Option<String>,

    /// Environment value for a stdio server. Repeat as KEY=VALUE.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// HTTP header. Repeat as KEY=VALUE.
    #[arg(long = "header", value_name = "KEY=VALUE")]
    pub headers: Vec<String>,

    /// Working directory for a stdio server.
    #[arg(long)]
    pub cwd: Option<String>,

    /// HTTP authentication mode: oauth, bearer, or none.
    #[arg(long, value_name = "MODE")]
    pub auth: Option<String>,

    /// Bearer token value. Environment expansion uses ${NAME}.
    #[arg(long, value_name = "TOKEN", conflicts_with = "bearer_token_env")]
    pub bearer_token: Option<String>,

    /// Environment variable that contains a bearer token.
    #[arg(long, value_name = "NAME")]
    pub bearer_token_env: Option<String>,

    /// OAuth scopes, separated by spaces.
    #[arg(long, value_name = "SCOPES")]
    pub oauth_scope: Option<String>,

    /// Preregistered OAuth client ID.
    #[arg(long)]
    pub client_id: Option<String>,

    /// OAuth client secret.
    #[arg(long)]
    pub client_secret: Option<String>,

    /// Use the OAuth client_credentials grant.
    #[arg(long)]
    pub client_credentials: bool,

    /// OAuth callback URI.
    #[arg(long)]
    pub redirect_uri: Option<String>,

    /// Request timeout in milliseconds.
    #[arg(long)]
    pub timeout_ms: Option<u64>,

    /// Stdio command and arguments. Put these after `--`.
    #[arg(last = true, value_name = "COMMAND", conflicts_with = "url")]
    pub stdio: Vec<String>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum McpCommand {
    /// List configured MCP servers.
    List {
        #[arg(long)]
        json: bool,
    },

    /// Show one MCP server without secrets.
    Get {
        name: String,
        #[arg(long)]
        json: bool,
    },

    /// Add an MCP server.
    Add {
        name: String,
        #[arg(long, value_enum, default_value = "user")]
        scope: McpScope,
        #[command(flatten)]
        server: Box<McpServerArgs>,
    },

    /// Update an MCP server.
    Update {
        name: String,
        #[arg(long, value_enum, default_value = "user")]
        scope: McpScope,
        #[command(flatten)]
        server: Box<McpServerArgs>,
    },

    /// Remove an MCP server from one scope.
    Remove {
        name: String,
        #[arg(long, value_enum, default_value = "user")]
        scope: McpScope,
    },

    /// Enable an MCP server.
    Enable {
        name: String,
        #[arg(long, value_enum, default_value = "user")]
        scope: McpScope,
    },

    /// Disable an MCP server.
    Disable {
        name: String,
        #[arg(long, value_enum, default_value = "user")]
        scope: McpScope,
    },

    /// Sign in to an OAuth MCP server.
    Login {
        name: String,
        /// Print the URL and accept a pasted redirect URL.
        #[arg(long)]
        no_browser: bool,
    },

    /// Remove saved OAuth credentials for an MCP server.
    Logout { name: String },

    /// Connect to a server and list its capabilities.
    Test { name: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Configure and use Model Context Protocol servers.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },

    /// Save credentials for a provider.
    Login {
        /// Provider ID. OpenAI Codex uses ChatGPT subscription login.
        #[arg(default_value = "openai-codex")]
        provider: String,

        /// Use a headless flow. OpenAI uses a device code; Anthropic accepts a pasted callback.
        #[arg(
            long = "device-auth",
            alias = "device-code",
            conflicts_with = "browser"
        )]
        device_auth: bool,

        /// Use browser login. This is the OAuth default.
        #[arg(long)]
        browser: bool,

        /// Store this API key without an interactive prompt.
        #[arg(long, value_name = "KEY")]
        api_key: Option<String>,
    },

    /// Remove saved credentials for a provider.
    Logout {
        #[arg(default_value = "openai-codex")]
        provider: String,
    },

    /// Show credential sources without showing secrets.
    Auth {
        /// `import`, or a provider to show. By default, show all Pi providers.
        action_or_provider: Option<String>,

        /// Provider to import when the first argument is `import`.
        provider: Option<String>,

        /// Import without an interactive confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "kiss",
    version,
    about = "kiss: a fast, minimal terminal coding agent",
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Messages and @file attachments.
    #[arg(value_name = "MESSAGES")]
    pub messages: Vec<String>,

    /// Print response and exit (non-interactive).
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Output mode: json emits one event per line.
    #[arg(long, value_name = "MODE")]
    pub mode: Option<String>,

    /// Export a session file to HTML: --export <in> [out]
    #[arg(long, num_args = 1..=2, value_name = "FILE")]
    pub export: Option<Vec<String>>,

    // --- model options ---
    /// Provider, such as anthropic, openai, or google.
    #[arg(long)]
    pub provider: Option<String>,

    /// Model pattern or id; supports provider/id and :<thinking> suffix.
    #[arg(long)]
    pub model: Option<String>,

    /// API key override for the selected provider.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Thinking level: off, minimal, low, medium, high, xhigh, max.
    #[arg(long)]
    pub thinking: Option<String>,

    /// Comma-separated patterns for Ctrl+P model cycling.
    #[arg(long)]
    pub models: Option<String>,

    /// List available models, optionally filtered.
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "SEARCH")]
    pub list_models: Option<String>,

    // --- session options ---
    /// Continue most recent session.
    #[arg(short = 'c', long = "continue")]
    pub continue_recent: bool,

    /// Browse and select a session.
    #[arg(short = 'r', long)]
    pub resume: bool,

    /// Use a specific session file or partial session id.
    #[arg(long)]
    pub session: Option<String>,

    /// Fork a session file or partial id into a new session.
    #[arg(long)]
    pub fork: Option<String>,

    /// Custom session storage directory.
    #[arg(long)]
    pub session_dir: Option<String>,

    /// Ephemeral mode; do not save the session.
    #[arg(long)]
    pub no_session: bool,

    /// Set session display name at startup.
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    // --- tool options ---
    /// Allowlist specific tools (comma separated).
    #[arg(short = 't', long)]
    pub tools: Option<String>,

    /// Disable specific tools (comma separated).
    #[arg(long = "exclude-tools", short_alias = 'x')]
    pub exclude_tools: Option<String>,

    /// Disable all tools.
    #[arg(long = "no-tools")]
    pub no_tools: bool,

    // --- resource options ---
    /// Load a skill file or directory (repeatable).
    #[arg(long = "skill")]
    pub skills: Vec<String>,

    /// Disable skill discovery.
    #[arg(long)]
    pub no_skills: bool,

    /// Load a prompt template (repeatable).
    #[arg(long = "prompt-template")]
    pub prompt_templates: Vec<String>,

    /// Disable prompt template discovery.
    #[arg(long)]
    pub no_prompt_templates: bool,

    /// Load a theme file (repeatable).
    #[arg(long = "theme")]
    pub themes: Vec<String>,

    /// Disable theme discovery.
    #[arg(long)]
    pub no_themes: bool,

    /// Disable AGENTS.md / CLAUDE.md discovery.
    #[arg(long = "no-context-files", short_alias = 'C')]
    pub no_context_files: bool,

    // --- system prompt ---
    /// Replace the default system prompt.
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Append to the system prompt.
    #[arg(long)]
    pub append_system_prompt: Option<String>,

    // --- trust ---
    /// Trust project-local files for this run.
    #[arg(short = 'a', long)]
    pub approve: bool,

    /// Ignore project-local files for this run.
    #[arg(long = "no-approve")]
    pub no_approve: bool,

    /// Force verbose startup output.
    #[arg(long)]
    pub verbose: bool,
}

impl Args {
    pub fn split_csv(value: &Option<String>) -> Vec<String> {
        value
            .as_deref()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_device_login() {
        let args =
            Args::try_parse_from(["kiss", "login", "openai-codex", "--device-code"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Login {
                provider,
                device_auth: true,
                ..
            }) if provider == "openai-codex"
        ));
    }

    #[test]
    fn keeps_prompt_arguments() {
        let args = Args::try_parse_from(["kiss", "fix", "the", "tests"]).unwrap();
        assert_eq!(args.messages, ["fix", "the", "tests"]);
        assert!(args.command.is_none());
    }

    #[test]
    fn parses_external_auth_import() {
        let args = Args::try_parse_from(["kiss", "auth", "import", "anthropic", "--yes"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Auth {
                action_or_provider: Some(action),
                provider: Some(provider),
                yes: true,
            }) if action == "import" && provider == "anthropic"
        ));
    }

    #[test]
    fn end_of_options_keeps_flag_shaped_prompt_text() {
        let args =
            Args::try_parse_from(["kiss", "--print", "--", "--provider", "openai", "-c"]).unwrap();
        assert!(args.print);
        assert_eq!(args.provider, None);
        assert!(!args.continue_recent);
        assert_eq!(args.messages, ["--provider", "openai", "-c"]);
    }

    #[test]
    fn parses_mcp_crud_and_headless_login() {
        let add = Args::try_parse_from([
            "kiss",
            "mcp",
            "add",
            "demo",
            "--scope",
            "project",
            "--",
            "demo-server",
            "--stdio",
        ])
        .unwrap();
        assert!(matches!(
            add.command,
            Some(Command::Mcp {
                command: McpCommand::Add {
                    name,
                    scope: McpScope::Project,
                    server,
                }
            }) if name == "demo" && server.stdio == ["demo-server", "--stdio"]
        ));

        let login = Args::try_parse_from(["kiss", "mcp", "login", "demo", "--no-browser"]).unwrap();
        assert!(matches!(
            login.command,
            Some(Command::Mcp {
                command: McpCommand::Login {
                    name,
                    no_browser: true,
                }
            }) if name == "demo"
        ));
    }
}
