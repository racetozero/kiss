//! `kiss mcp` configuration, OAuth, and diagnostics commands.

use crate::args::{McpCommand, McpScope, McpServerArgs};
use anyhow::{Context as _, Result, bail};
use kiss_mcp::config::{
    self, AuthMode, AuthSetting, ClientRegistration, ConfigScope, OAuthConfig, OAuthGrantType,
};
use kiss_mcp::{McpManager, ServerEntry};
use serde::Serialize;
use std::io::Write as _;
use tokio_util::sync::CancellationToken;

pub async fn run(command: &McpCommand, trust_override: Option<bool>) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let loaded = config::load(&cwd, true)?;
    match command {
        McpCommand::List { json } => {
            if *json {
                let servers = loaded
                    .config
                    .mcp_servers
                    .iter()
                    .map(|(name, server)| {
                        serde_json::json!({
                            "name": name,
                            "server": server.redacted(),
                            "sources": loaded.source_labels(name),
                        })
                    })
                    .collect::<Vec<_>>();
                print_json(&servers)?;
            } else if loaded.config.mcp_servers.is_empty() {
                println!("No MCP servers are configured.");
            } else {
                for (name, server) in &loaded.config.mcp_servers {
                    let transport = server
                        .url
                        .as_deref()
                        .map(|url| format!("HTTP {}", config::redact_url(url)))
                        .unwrap_or_else(|| {
                            format!("stdio {}", server.command.as_deref().unwrap_or(""))
                        });
                    let state = if server.disabled {
                        "disabled"
                    } else {
                        "enabled"
                    };
                    match server.description.as_deref() {
                        Some(description) => {
                            println!("{name}\t{state}\t{transport}\t{description}")
                        }
                        None => println!("{name}\t{state}\t{transport}"),
                    }
                }
            }
        }
        McpCommand::Get { name, json } => {
            let server = loaded
                .config
                .mcp_servers
                .get(name)
                .with_context(|| format!("MCP server `{name}` is not configured"))?;
            let value = serde_json::json!({
                "name": name,
                "server": server.redacted(),
                "sources": loaded.source_labels(name),
            });
            if *json {
                print_json(&value)?;
            } else {
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
        }
        McpCommand::Add {
            name,
            scope,
            server,
        } => {
            let entry = new_server(server)?;
            reject_project_provider_auth(name, *scope, &entry)?;
            config::add_server(&loaded.paths, scope_value(*scope), name, entry)?;
            println!("Added MCP server `{name}` to {}.", scope_label(*scope));
        }
        McpCommand::Update {
            name,
            scope,
            server,
        } => {
            let merged = loaded.config.mcp_servers.get(name);
            config::edit_server(&loaded.paths, scope_value(*scope), name, merged, |entry| {
                apply_server_args(entry, server, false)?;
                reject_project_provider_auth(name, *scope, entry)
            })?;
            println!("Updated MCP server `{name}` in {}.", scope_label(*scope));
        }
        McpCommand::Remove { name, scope } => {
            if config::remove_server(&loaded.paths, scope_value(*scope), name)? {
                println!("Removed MCP server `{name}` from {}.", scope_label(*scope));
            } else {
                println!(
                    "MCP server `{name}` is not present in {}.",
                    scope_label(*scope)
                );
            }
        }
        McpCommand::Enable { name, scope } | McpCommand::Disable { name, scope } => {
            let disabled = matches!(command, McpCommand::Disable { .. });
            let merged = loaded.config.mcp_servers.get(name);
            config::edit_server(&loaded.paths, scope_value(*scope), name, merged, |entry| {
                entry.disabled = disabled;
                Ok(())
            })?;
            println!(
                "{} MCP server `{name}` in {}.",
                if disabled { "Disabled" } else { "Enabled" },
                scope_label(*scope)
            );
        }
        McpCommand::Login { name, no_browser } => {
            login(&loaded, name, *no_browser).await?;
            println!("Saved OAuth credentials for MCP server `{name}`.");
        }
        McpCommand::Logout { name } => {
            let server = http_server(&loaded, name)?;
            let url = server.url.as_deref().context("MCP URL is missing")?;
            if kiss_mcp::logout(name, url).await? {
                println!("Removed OAuth credentials for MCP server `{name}`.");
            } else {
                println!("No OAuth credentials are saved for MCP server `{name}`.");
            }
        }
        McpCommand::Test { name } => {
            // Testing starts the server, so a project's own servers run only
            // when the project is trusted.
            let trusted = kiss_coding::trust::resolve(&cwd, trust_override);
            if !trusted
                && loaded.config.mcp_servers.contains_key(name)
                && !config::load(&cwd, false)?
                    .config
                    .mcp_servers
                    .contains_key(name)
            {
                anyhow::bail!(
                    "MCP server `{name}` comes from this project's configuration, and this project is not trusted. Run `kiss --approve mcp test {name}` to start it once, or trust the project with /trust in kiss."
                );
            }
            let manager = McpManager::new(config::load(&cwd, trusted)?)?;
            let cancel = CancellationToken::new();
            let tools = manager.list_tools(Some(name), &cancel).await?.tools;
            println!(
                "Connected to MCP server `{name}`. Found {} tools.",
                tools.len()
            );
            manager.disconnect_all().await;
        }
    }
    Ok(0)
}

fn reject_project_provider_auth(name: &str, scope: McpScope, server: &ServerEntry) -> Result<()> {
    if scope == McpScope::Project
        && let Some(provider) = server.auth_provider()
    {
        bail!(
            "MCP server `{name}` uses the `{provider}` login token, which a project config must not send. Add it with --scope user"
        )
    }
    Ok(())
}

fn new_server(input: &McpServerArgs) -> Result<ServerEntry> {
    let mut server = ServerEntry::default();
    apply_server_args(&mut server, input, true)?;
    Ok(server)
}

fn apply_server_args(
    server: &mut ServerEntry,
    input: &McpServerArgs,
    creating: bool,
) -> Result<()> {
    if let Some(url) = &input.url {
        server.url = Some(url.clone());
        server.command = None;
        server.args.clear();
    } else if !input.stdio.is_empty() {
        server.command = Some(input.stdio[0].clone());
        server.args = input.stdio[1..].to_vec();
        server.url = None;
        server.headers.clear();
        server.auth = None;
        server.oauth = None;
        server.bearer_token = None;
        server.bearer_token_env = None;
    } else if creating {
        bail!("MCP server needs --url URL or a stdio command after `--`")
    }
    if !input.env.is_empty() {
        server.env = config::parse_pairs(&input.env, "MCP environment value")?;
    }
    if !input.headers.is_empty() {
        server.headers = config::parse_pairs(&input.headers, "MCP header")?;
    }
    if let Some(cwd) = &input.cwd {
        server.cwd = Some(cwd.clone());
    }
    if let Some(description) = &input.description {
        server.description = Some(description.clone()).filter(|value| !value.trim().is_empty());
    }
    if let Some(provider) = &input.auth_provider {
        server.auth = Some(AuthSetting::Provider {
            provider: provider.clone(),
        });
        server.oauth = None;
        server.bearer_token = None;
        server.bearer_token_env = None;
    }
    if let Some(auth) = &input.auth {
        server.auth = Some(match auth.as_str() {
            "oauth" => AuthSetting::Mode(AuthMode::OAuth),
            "bearer" => AuthSetting::Mode(AuthMode::Bearer),
            "none" => AuthSetting::Disabled(false),
            _ => bail!("MCP auth must be oauth, bearer, or none"),
        });
    }
    if let Some(token) = &input.bearer_token {
        server.bearer_token = Some(token.clone());
        server.bearer_token_env = None;
    }
    if let Some(name) = &input.bearer_token_env {
        server.bearer_token_env = Some(name.clone());
        server.bearer_token = None;
    }
    let oauth_changed = input.oauth_scope.is_some()
        || input.client_id.is_some()
        || input.client_secret.is_some()
        || input.client_credentials
        || input.redirect_uri.is_some()
        || input.oauth_client_name.is_some()
        || input.auth_server_metadata_url.is_some()
        || input.client_metadata_url.is_some()
        || input.oauth_client_registration.is_some();
    if oauth_changed || matches!(server.auth, Some(AuthSetting::Mode(AuthMode::OAuth))) {
        let oauth = server.oauth.get_or_insert_with(OAuthConfig::default);
        if let Some(scope) = &input.oauth_scope {
            oauth.scope = Some(scope.clone());
        }
        if let Some(client_id) = &input.client_id {
            oauth.client_id = Some(client_id.clone());
        }
        if let Some(client_secret) = &input.client_secret {
            oauth.client_secret = Some(client_secret.clone());
        }
        if input.client_credentials {
            oauth.grant_type = OAuthGrantType::ClientCredentials;
        }
        if let Some(redirect_uri) = &input.redirect_uri {
            oauth.redirect_uri = Some(redirect_uri.clone());
        }
        if let Some(client_name) = &input.oauth_client_name {
            oauth.client_name = Some(client_name.clone());
        }
        if let Some(url) = &input.auth_server_metadata_url {
            oauth.auth_server_metadata_url = Some(url.clone());
        }
        if let Some(url) = &input.client_metadata_url {
            oauth.client_metadata_url = Some(url.clone());
        }
        if let Some(mode) = &input.oauth_client_registration {
            oauth.client_registration = match mode.as_str() {
                "dcr" => ClientRegistration::Dcr,
                "cimd" => ClientRegistration::Cimd,
                _ => ClientRegistration::Auto,
            };
        }
    }
    if let Some(timeout) = input.timeout_ms {
        if timeout == 0 {
            bail!("MCP timeout must be more than zero")
        }
        server.request_timeout_ms = Some(timeout);
    }
    Ok(())
}

async fn login(loaded: &kiss_mcp::LoadedConfig, name: &str, no_browser: bool) -> Result<()> {
    let server = http_server(loaded, name)?;
    let Some(pending) =
        crate::auth_flow::begin_mcp_login(name, server, "kiss login", &CancellationToken::new())
            .await?
    else {
        return Ok(());
    };
    println!("Open this URL to sign in:\n{}", pending.authorization_url);

    if no_browser {
        let callback = prompt_line("Paste the full redirect URL: ")?;
        return kiss_mcp::finish_login(pending, &callback).await;
    }

    if let Some(listener) = crate::auth_flow::callback_listener(&pending.redirect_uri).await? {
        if !crate::auth_flow::open_browser(&pending.authorization_url) {
            eprintln!("The browser did not open. Open the URL manually.");
        }
        let callback = crate::auth_flow::receive_callback(listener, &pending.redirect_uri).await?;
        kiss_mcp::finish_login(pending, &callback).await
    } else {
        if !crate::auth_flow::open_browser(&pending.authorization_url) {
            eprintln!("The browser did not open. Open the URL manually.");
        }
        let callback = prompt_line("Paste the full redirect URL: ")?;
        kiss_mcp::finish_login(pending, &callback).await
    }
}

fn http_server<'a>(loaded: &'a kiss_mcp::LoadedConfig, name: &str) -> Result<&'a ServerEntry> {
    let server = loaded
        .config
        .mcp_servers
        .get(name)
        .with_context(|| format!("MCP server `{name}` is not configured"))?;
    if server.url.is_none() {
        bail!("MCP server `{name}` is a stdio server and does not use OAuth")
    }
    Ok(server)
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut value = String::new();
    std::io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_string())
}

fn scope_value(scope: McpScope) -> ConfigScope {
    match scope {
        McpScope::User => ConfigScope::User,
        McpScope::Project => ConfigScope::Project,
    }
}

fn scope_label(scope: McpScope) -> &'static str {
    match scope {
        McpScope::User => "the user configuration",
        McpScope::Project => ".mcp.json",
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_stdio_server_from_trailing_command() {
        let input = McpServerArgs {
            url: None,
            env: vec!["TOKEN=value".to_string()],
            headers: Vec::new(),
            cwd: None,
            description: None,
            auth: None,
            auth_provider: None,
            bearer_token: None,
            bearer_token_env: None,
            oauth_scope: None,
            client_id: None,
            client_secret: None,
            client_credentials: false,
            redirect_uri: None,
            oauth_client_name: None,
            auth_server_metadata_url: None,
            client_metadata_url: None,
            oauth_client_registration: None,
            timeout_ms: None,
            stdio: vec!["demo".to_string(), "--stdio".to_string()],
        };
        let server = new_server(&input).unwrap();
        assert_eq!(server.command.as_deref(), Some("demo"));
        assert_eq!(server.args, ["--stdio"]);
        assert_eq!(server.env["TOKEN"], "value");
    }

    #[test]
    fn http_flags_set_description_oauth_options_and_provider_auth() {
        use clap::Parser as _;
        let parse = |extra: &[&str]| {
            let mut argv = vec![
                "kiss",
                "mcp",
                "add",
                "demo",
                "--url",
                "https://mcp.example.com",
            ];
            argv.extend_from_slice(extra);
            match crate::args::Args::try_parse_from(argv).unwrap().command {
                Some(crate::args::Command::Mcp {
                    command: McpCommand::Add { server, .. },
                }) => new_server(&server).unwrap(),
                other => panic!("unexpected command {other:?}"),
            }
        };
        let server = parse(&[
            "--description",
            "Team tickets",
            "--auth-server-metadata-url",
            "https://auth.example.com/.well-known/oauth-authorization-server",
            "--client-metadata-url",
            "https://example.com/kiss/client.json",
        ]);
        assert_eq!(server.description.as_deref(), Some("Team tickets"));
        let oauth = server.oauth.unwrap();
        assert_eq!(oauth.client_registration, ClientRegistration::Auto);
        let server = parse(&[
            "--client-metadata-url",
            "https://example.com/kiss/client.json",
            "--oauth-client-registration",
            "cimd",
        ]);
        assert_eq!(
            server.oauth.unwrap().client_registration,
            ClientRegistration::Cimd
        );
        assert!(oauth.auth_server_metadata_url.is_some());

        let server = parse(&["--auth-provider", "radius"]);
        assert_eq!(server.auth_provider(), Some("radius"));
        assert!(reject_project_provider_auth("demo", McpScope::Project, &server).is_err());
        assert!(reject_project_provider_auth("demo", McpScope::User, &server).is_ok());
    }
}
