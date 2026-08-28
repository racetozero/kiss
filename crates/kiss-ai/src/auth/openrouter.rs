//! OpenRouter OAuth PKCE login.

use super::OAuthCredential;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use rand::RngCore as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub callback_host: String,
    pub timeout: Duration,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            authorize_url: "https://openrouter.ai/auth".into(),
            token_url: "https://openrouter.ai/api/v1/auth/keys".into(),
            callback_host: std::env::var("KISS_OAUTH_CALLBACK_HOST")
                .unwrap_or_else(|_| "127.0.0.1".into()),
            timeout: Duration::from_secs(5 * 60),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub authorization_url: String,
    verifier: String,
}

fn random_url_safe(length: usize) -> String {
    let mut bytes = vec![0_u8; length];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn create_pkce() -> (String, String) {
    let verifier = random_url_safe(64);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn start_authorization(
    config: &OAuthConfig,
    callback_url: &str,
) -> Result<PendingAuthorization> {
    let (verifier, challenge) = create_pkce();
    let mut url = Url::parse(&config.authorize_url)?;
    url.query_pairs_mut()
        .append_pair("callback_url", callback_url)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(PendingAuthorization {
        authorization_url: url.into(),
        verifier,
    })
}

fn authorization_code(input: &str) -> Result<String> {
    let value = input.trim();
    if let Ok(url) = Url::parse(value) {
        return url
            .query_pairs()
            .find(|(name, _)| name == "code")
            .map(|(_, value)| value.into_owned())
            .context("OpenRouter callback URL has no code");
    }
    if value.contains("code=") {
        let url = Url::parse(&format!("http://localhost/?{value}"))?;
        return authorization_code(url.as_str());
    }
    if value.is_empty() {
        anyhow::bail!("OpenRouter authorization code cannot be empty");
    }
    Ok(value.into())
}

pub async fn finish_authorization(
    config: &OAuthConfig,
    pending: &PendingAuthorization,
    input: &str,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let code = authorization_code(input)?;
    let response = tokio::select! {
        response = crate::stream::http_client()
            .post(&config.token_url)
            .header("accept", "application/json")
            .json(&json!({
                "code": code,
                "code_verifier": pending.verifier,
                "code_challenge_method": "S256",
            }))
            .send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("OpenRouter OAuth returned invalid JSON")?;
    if !status.is_success() {
        let detail = body["error_description"]
            .as_str()
            .or_else(|| body["message"].as_str())
            .or_else(|| body["error"].as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("OpenRouter OAuth key exchange failed (HTTP {status}): {detail}");
    }
    let access = body["key"]
        .as_str()
        .filter(|value| !value.is_empty())
        .context("OpenRouter OAuth response has no key")?;
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access: access.into(),
        refresh: String::new(),
        expires: i64::MAX,
        account_id: String::new(),
        available_model_ids: None,
    })
}

pub async fn login_browser(
    config: &OAuthConfig,
    cancel: &CancellationToken,
    show_url: impl FnOnce(&str),
) -> Result<OAuthCredential> {
    let listener = TcpListener::bind((config.callback_host.as_str(), 0)).await?;
    let address = listener.local_addr()?;
    let callback_path = format!("/oauth/callback/{}", uuid::Uuid::new_v4());
    let callback_url = format!(
        "http://{}:{}{}",
        config.callback_host,
        address.port(),
        callback_path
    );
    let pending = start_authorization(config, &callback_url)?;
    show_url(&pending.authorization_url);
    let deadline = tokio::time::sleep(config.timeout);
    tokio::pin!(deadline);
    loop {
        let (mut socket, _) = tokio::select! {
            result = listener.accept() => result?,
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
            _ = &mut deadline => anyhow::bail!("OpenRouter login timed out"),
        };
        let mut request = vec![0_u8; 8192];
        let length = socket.read(&mut request).await?;
        let target = String::from_utf8_lossy(&request[..length])
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .map(str::to_string);
        let parsed = target
            .as_deref()
            .and_then(|target| Url::parse(&format!("http://localhost{target}")).ok());
        let valid = parsed
            .as_ref()
            .is_some_and(|url| url.path() == callback_path);
        let result = if valid {
            finish_authorization(
                config,
                &pending,
                parsed.as_ref().expect("valid callback").as_str(),
                cancel,
            )
            .await
        } else {
            Err(anyhow::anyhow!("invalid callback path"))
        };
        let page = if result.is_ok() {
            "OpenRouter authentication is complete. You can close this window."
        } else {
            "The OpenRouter OAuth callback is invalid."
        };
        let status = if result.is_ok() {
            "200 OK"
        } else {
            "400 Bad Request"
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        socket.write_all(response.as_bytes()).await?;
        if result.is_ok() {
            return result;
        }
    }
}

pub async fn refresh(
    credential: &OAuthCredential,
    _config: &OAuthConfig,
) -> Result<OAuthCredential> {
    Ok(credential.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_url_has_pkce_and_callback() {
        let pending = start_authorization(
            &OAuthConfig::default(),
            "http://127.0.0.1:9999/oauth/callback",
        )
        .unwrap();
        let url = Url::parse(&pending.authorization_url).unwrap();
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            query["callback_url"],
            "http://127.0.0.1:9999/oauth/callback"
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!query["code_challenge"].is_empty());
    }

    #[test]
    fn manual_input_accepts_code_or_redirect_url() {
        assert_eq!(authorization_code("plain-code").unwrap(), "plain-code");
        assert_eq!(
            authorization_code("http://localhost/callback?code=url-code").unwrap(),
            "url-code"
        );
    }
}
