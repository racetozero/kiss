//! `kiss mcp` configuration, OAuth, and diagnostics commands.

use crate::args::{McpCommand, McpScope, McpServerArgs};
use anyhow::{Context as _, Result, bail};
use kiss_mcp::config::{self, AuthMode, AuthSetting, ConfigScope, OAuthConfig, OAuthGrantType};
use kiss_mcp::{McpManager, ServerEntry};
use serde::Serialize;
use std::io::Write as _;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

pub async fn run(command: &McpCommand) -> Result<i32> {
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
                        .map(|url| format!("HTTP {url}"))
                        .unwrap_or_else(|| {
                            format!("stdio {}", server.command.as_deref().unwrap_or(""))
                        });
                    let state = if server.disabled {
                        "disabled"
                    } else {
                        "enabled"
                    };
                    println!("{name}\t{state}\t{transport}");
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
            config::add_server(&loaded.paths, scope_value(*scope), name, entry)?;
            println!("Added MCP server `{name}` to {}.", scope_label(*scope));
        }
        McpCommand::Update {
            name,
            scope,
            server,
        } => {
            let base = loaded
                .config
                .mcp_servers
                .get(name)
                .cloned()
                .with_context(|| format!("MCP server `{name}` is not configured"))?;
            let entry = update_server(base, server)?;
            config::put_server(&loaded.paths, scope_value(*scope), name, entry)?;
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
            config::set_disabled(
                &loaded.paths,
                scope_value(*scope),
                name,
                disabled,
                loaded.config.mcp_servers.get(name),
            )?;
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
            let manager = McpManager::new(loaded)?;
            let cancel = CancellationToken::new();
            let tools = manager.list_tools(Some(name), &cancel).await?;
            println!(
                "Connected to MCP server `{name}`. Found {} tools.",
                tools.len()
            );
            manager.disconnect_all().await;
        }
    }
    Ok(0)
}

fn new_server(input: &McpServerArgs) -> Result<ServerEntry> {
    let mut server = ServerEntry::default();
    apply_server_args(&mut server, input, true)?;
    Ok(server)
}

fn update_server(mut server: ServerEntry, input: &McpServerArgs) -> Result<ServerEntry> {
    apply_server_args(&mut server, input, false)?;
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
        || input.redirect_uri.is_some();
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
    let url = server.url.as_deref().context("MCP URL is missing")?;
    let oauth = server.oauth.clone().unwrap_or_default();
    if oauth.grant_type == OAuthGrantType::ClientCredentials {
        return kiss_mcp::login_client_credentials(name, url, &oauth).await;
    }

    let challenge = tokio::time::timeout(
        Duration::from_secs(15),
        kiss_mcp::probe_oauth_challenge(server),
    )
    .await
    .context("MCP OAuth discovery timed out")??;
    let pending = kiss_mcp::begin_login(name, url, &oauth, challenge.as_deref()).await?;
    println!("Open this URL to sign in:\n{}", pending.authorization_url);

    if no_browser {
        let callback = prompt_line("Paste the full redirect URL: ")?;
        return kiss_mcp::finish_login(pending, &callback).await;
    }

    if let Some(listener) = callback_listener(&pending.redirect_uri).await? {
        if !crate::auth_flow::open_browser(&pending.authorization_url) {
            eprintln!("The browser did not open. Open the URL manually.");
        }
        let callback = receive_callback(listener, &pending.redirect_uri).await?;
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

pub(crate) async fn callback_listener(
    redirect_uri: &str,
) -> Result<Option<tokio::net::TcpListener>> {
    let uri = url::Url::parse(redirect_uri)?;
    let Some(host) = uri.host_str() else {
        return Ok(None);
    };
    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return Ok(None);
    }
    let port = uri
        .port_or_known_default()
        .context("OAuth redirect URI has no port")?;
    let address = if host == "::1" {
        format!("[::1]:{port}")
    } else {
        format!("127.0.0.1:{port}")
    };
    Ok(Some(tokio::net::TcpListener::bind(address).await?))
}

pub(crate) async fn receive_callback(
    listener: tokio::net::TcpListener,
    redirect_uri: &str,
) -> Result<String> {
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(600), listener.accept())
        .await
        .context("MCP OAuth callback timed out")??;
    let mut buffer = vec![0_u8; 16 * 1024];
    let length = socket.read(&mut buffer).await?;
    let request =
        std::str::from_utf8(&buffer[..length]).context("invalid OAuth callback request")?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("invalid OAuth callback request line")?;
    let base = url::Url::parse(redirect_uri)?;
    let callback = base.join(target)?.to_string();
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><title>KISS MCP login</title><p>Login is complete. You can close this page.</p>",
        )
        .await?;
    Ok(callback)
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
            auth: None,
            bearer_token: None,
            bearer_token_env: None,
            oauth_scope: None,
            client_id: None,
            client_secret: None,
            client_credentials: false,
            redirect_uri: None,
            timeout_ms: None,
            stdio: vec!["demo".to_string(), "--stdio".to_string()],
        };
        let server = new_server(&input).unwrap();
        assert_eq!(server.command.as_deref(), Some("demo"));
        assert_eq!(server.args, ["--stdio"]);
        assert_eq!(server.env["TOKEN"], "value");
    }
}
