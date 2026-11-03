//! Anthropic workload identity federation: exchange a workload's OIDC
//! identity token for a short-lived Anthropic access token (RFC 7523).

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::time::{Duration, Instant};

/// Local placeholder that lists Anthropic models before the first exchange.
pub(super) const MARKER: &str = "anthropic-workload-identity-federation";
/// Prefix that tells the Anthropic adapter to send an OAuth bearer token.
pub const TOKEN_PREFIX: &str = "anthropic-wif:";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const MAX_IDENTITY_TOKEN_BYTES: usize = 16 * 1024;
/// Exchange again when less than this time remains.
const REFRESH_MARGIN: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    base_url: String,
    federation_rule_id: String,
    organization_id: String,
    identity_token_file: String,
    service_account_id: Option<String>,
    workspace_id: Option<String>,
}

impl Config {
    fn from_env() -> Option<Self> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        Some(Self {
            base_url: var("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            federation_rule_id: var("ANTHROPIC_FEDERATION_RULE_ID")?,
            organization_id: var("ANTHROPIC_ORGANIZATION_ID")?,
            identity_token_file: var("ANTHROPIC_IDENTITY_TOKEN_FILE")?,
            service_account_id: var("ANTHROPIC_SERVICE_ACCOUNT_ID"),
            workspace_id: var("ANTHROPIC_WORKSPACE_ID"),
        })
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: f64,
}

struct CachedToken {
    config: Config,
    token: String,
    expires_at: Instant,
}

/// Whether the three required federation variables are set.
pub(super) fn configured() -> bool {
    Config::from_env().is_some()
}

/// A prefixed access token, exchanged again shortly before it expires.
pub(super) async fn access_token() -> Result<String> {
    let config = Config::from_env().context(
        "Anthropic workload identity federation needs ANTHROPIC_FEDERATION_RULE_ID, ANTHROPIC_ORGANIZATION_ID, and ANTHROPIC_IDENTITY_TOKEN_FILE",
    )?;
    static CACHE: tokio::sync::Mutex<Option<CachedToken>> = tokio::sync::Mutex::const_new(None);
    let mut cache = CACHE.lock().await;
    if let Some(cached) = cache.as_ref()
        && cached.config == config
        && cached.expires_at > Instant::now() + REFRESH_MARGIN
    {
        return Ok(format!("{TOKEN_PREFIX}{}", cached.token));
    }
    let (token, expires_in) = exchange(&config).await?;
    *cache = Some(CachedToken {
        config,
        token: token.clone(),
        expires_at: Instant::now() + expires_in,
    });
    Ok(format!("{TOKEN_PREFIX}{token}"))
}

async fn exchange(config: &Config) -> Result<(String, Duration)> {
    let base_url = config.base_url.trim_end_matches('/');
    let parsed = url::Url::parse(base_url).with_context(|| {
        format!("ANTHROPIC_BASE_URL '{base_url}' is not a valid URL. Use an https URL such as {DEFAULT_BASE_URL}")
    })?;
    let loopback = matches!(
        parsed.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        anyhow::bail!(
            "Anthropic workload identity federation sends the identity token to {base_url}, which is not https. Use an https ANTHROPIC_BASE_URL, or http only on localhost"
        );
    }
    let path = &config.identity_token_file;
    let assertion = tokio::fs::read_to_string(path)
        .await
        .with_context(|| {
            format!("read the Anthropic identity token file {path} (ANTHROPIC_IDENTITY_TOKEN_FILE). Make sure the workload writes its OIDC token there")
        })?
        .trim()
        .to_string();
    if assertion.is_empty() {
        anyhow::bail!(
            "the Anthropic identity token file {path} is empty. Make sure the workload writes its OIDC token there"
        );
    }
    if assertion.len() > MAX_IDENTITY_TOKEN_BYTES {
        anyhow::bail!(
            "the Anthropic identity token in {path} is {} KiB, but the limit is 16 KiB. Point ANTHROPIC_IDENTITY_TOKEN_FILE at the OIDC token only",
            assertion.len().div_ceil(1024)
        );
    }
    let mut body = serde_json::json!({
        "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
        "assertion": assertion,
        "federation_rule_id": config.federation_rule_id,
        "organization_id": config.organization_id,
    });
    if let Some(id) = &config.service_account_id {
        body["service_account_id"] = id.as_str().into();
    }
    if let Some(id) = &config.workspace_id {
        body["workspace_id"] = id.as_str().into();
    }
    let url = format!("{base_url}/v1/oauth/token");
    let response = crate::stream::http_client()
        .post(&url)
        .header(
            "anthropic-beta",
            "oauth-2025-04-20,oidc-federation-2026-04-01",
        )
        .header("user-agent", concat!("kiss/", env!("CARGO_PKG_VERSION")))
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("reach the Anthropic token endpoint {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let hint = if status.as_u16() == 401 && config.workspace_id.is_none() {
            " Make sure the federation rule matches the identity token. If the rule covers more than one workspace, set ANTHROPIC_WORKSPACE_ID."
        } else if status.as_u16() == 401 {
            " Make sure the federation rule matches the identity token."
        } else {
            ""
        };
        anyhow::bail!(
            "Anthropic token exchange returned HTTP {status}: {}.{hint}",
            crate::truncate_err(&text)
        );
    }
    let token: TokenResponse = response
        .json()
        .await
        .context("parse the Anthropic token exchange response")?;
    if token.access_token.is_empty() || !token.expires_in.is_finite() || token.expires_in <= 0.0 {
        anyhow::bail!(
            "the Anthropic token exchange response has no usable access_token and expires_in"
        );
    }
    Ok((
        token.access_token,
        Duration::from_secs_f64(token.expires_in),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn exchange_posts_the_identity_token_and_reads_the_access_token() {
        crate::stream::ensure_tls_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let body =
                r#"{"access_token":"sk-ant-oat-test","expires_in":3600,"token_type":"Bearer"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("token");
        std::fs::write(&token_file, "header.payload.signature\n").unwrap();
        let config = Config {
            base_url: format!("http://127.0.0.1:{}", address.port()),
            federation_rule_id: "fdrl_1".into(),
            organization_id: "org_1".into(),
            identity_token_file: token_file.display().to_string(),
            service_account_id: None,
            workspace_id: Some("wrkspc_1".into()),
        };
        let (token, expires_in) = exchange(&config).await.unwrap();
        assert_eq!(token, "sk-ant-oat-test");
        assert_eq!(expires_in, Duration::from_secs(3600));

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/oauth/token "));
        assert!(request.contains("anthropic-beta: oauth-2025-04-20,oidc-federation-2026-04-01"));
        let body: serde_json::Value =
            serde_json::from_str(&request[request.find("\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(body["assertion"], "header.payload.signature");
        assert_eq!(body["federation_rule_id"], "fdrl_1");
        assert_eq!(body["workspace_id"], "wrkspc_1");
        assert!(body.get("service_account_id").is_none());
    }

    #[tokio::test]
    async fn exchange_refuses_plain_http_off_loopback() {
        let config = Config {
            base_url: "http://example.com".into(),
            federation_rule_id: "fdrl_1".into(),
            organization_id: "org_1".into(),
            identity_token_file: "/nonexistent".into(),
            service_account_id: None,
            workspace_id: None,
        };
        let error = exchange(&config).await.unwrap_err().to_string();
        assert!(error.contains("not https"), "{error}");
    }
}
