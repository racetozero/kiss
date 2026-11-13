//! Browser authentication shared by CLI and interactive mode.

use anyhow::{Context as _, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

pub async fn login_browser(
    provider: &str,
    cancel: &CancellationToken,
    show_url: impl FnOnce(&str),
) -> Result<()> {
    let credential = match provider {
        "openai" => {
            let id = kiss_coding::settings::Settings::get_or_create_device_id()?;
            kiss_ai::auth::openai_codex::login_browser(
                &kiss_ai::auth::openai_codex::OAuthConfig::chatgpt(Some(id)),
                cancel,
                show_url,
            )
            .await?
        }
        "openai-codex" => {
            kiss_ai::auth::openai_codex::login_browser(&Default::default(), cancel, show_url)
                .await?
        }
        "anthropic" => {
            kiss_ai::auth::anthropic::login_browser(&Default::default(), cancel, show_url).await?
        }
        "cursor" => {
            kiss_ai::auth::cursor::login_browser(&Default::default(), cancel, show_url).await?
        }
        "openrouter" => {
            kiss_ai::auth::openrouter::login_browser(&Default::default(), cancel, show_url).await?
        }
        "radius" => {
            kiss_ai::auth::radius::login_browser(&Default::default(), cancel, show_url).await?
        }
        _ => anyhow::bail!("{provider} does not provide browser authentication"),
    };
    kiss_ai::auth::store_oauth(provider, credential)
}

pub fn open_browser(url: &str) -> bool {
    // The platform URL handler would also run `file:` URLs and custom
    // protocol handlers. Open only web URLs.
    if !url::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https")) {
        return false;
    }
    #[cfg(target_os = "windows")]
    if std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some()
        || std::env::var("SESSIONNAME").is_ok_and(|name| name.eq_ignore_ascii_case("Services"))
    {
        return false;
    }
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return false;
        }
        std::process::Command::new("xdg-open")
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        // Avoid cmd.exe: it treats '&' in authentication URLs as command separators.
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return false;

    command.arg(url).spawn().is_ok()
}

/// Start an MCP server's OAuth sign-in: refuse a server that uses a
/// provider login (`login_command` names how to sign in to that provider),
/// finish a client-credentials grant directly, and otherwise discover the
/// authorization server and return the pending browser sign-in.
pub async fn begin_mcp_login(
    name: &str,
    server: &kiss_mcp::ServerEntry,
    login_command: &str,
    cancel: &CancellationToken,
) -> Result<Option<kiss_mcp::PendingLogin>> {
    if let Some(provider) = server.auth_provider() {
        anyhow::bail!(
            "MCP server `{name}` uses your `{provider}` login and has no separate OAuth sign-in. Run {login_command} {provider} instead"
        )
    }
    let url = server
        .url
        .as_deref()
        .with_context(|| format!("MCP server `{name}` is a stdio server and does not use OAuth"))?;
    let oauth = server.oauth.clone().unwrap_or_default();
    if oauth.grant_type == kiss_mcp::OAuthGrantType::ClientCredentials {
        kiss_mcp::login_client_credentials(name, url, &oauth).await?;
        return Ok(None);
    }
    let challenge = tokio::select! {
        result = tokio::time::timeout(
            Duration::from_secs(15),
            kiss_mcp::probe_oauth_challenge(server),
        ) => result.context("MCP OAuth discovery timed out")??,
        _ = cancel.cancelled() => anyhow::bail!("cancelled"),
    };
    Ok(Some(
        kiss_mcp::begin_login(name, url, &oauth, challenge.as_deref()).await?,
    ))
}

/// A listener for an OAuth redirect to this machine, or `None` when the
/// redirect goes elsewhere and the user must paste it.
pub async fn callback_listener(redirect_uri: &str) -> Result<Option<tokio::net::TcpListener>> {
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

pub async fn receive_callback(
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

#[cfg(test)]
mod tests {
    #[test]
    fn browser_opens_only_web_urls() {
        for url in [
            "file:///Applications/Calculator.app",
            "javascript:alert(1)",
            "x-custom:run",
            "/etc/passwd",
        ] {
            assert!(!super::open_browser(url), "{url}");
        }
    }
}
