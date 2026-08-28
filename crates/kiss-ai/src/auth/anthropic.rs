//! Anthropic OAuth for Claude Pro and Max subscriptions.

use super::OAuthCredential;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use rand::RngCore as _;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

const CLIENT_ID_BASE64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub redirect_uri: String,
    pub callback_host: String,
    pub timeout: Duration,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            authorize_url: AUTHORIZE_URL.into(),
            token_url: TOKEN_URL.into(),
            redirect_uri: REDIRECT_URI.into(),
            callback_host: std::env::var("KISS_OAUTH_CALLBACK_HOST")
                .unwrap_or_else(|_| "127.0.0.1".into()),
            timeout: Duration::from_secs(5 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAuthorization {
    pub authorization_url: String,
    verifier: String,
    state: String,
    redirect_uri: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

fn client_id() -> String {
    String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(CLIENT_ID_BASE64)
            .expect("Anthropic OAuth client ID is valid base64"),
    )
    .expect("Anthropic OAuth client ID is UTF-8")
}

fn random_url_safe(byte_count: usize) -> String {
    let mut bytes = vec![0_u8; byte_count];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn create_pkce() -> (String, String) {
    let verifier = random_url_safe(64);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn start_authorization(config: &OAuthConfig) -> Result<PendingAuthorization> {
    let (verifier, challenge) = create_pkce();
    // Anthropic accepts the verifier as state. This also lets the manual flow
    // validate a returned state without keeping a second secret.
    let state = verifier.clone();
    let mut url = Url::parse(&config.authorize_url)?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", &client_id())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &config.redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    Ok(PendingAuthorization {
        authorization_url: url.into(),
        verifier,
        state,
        redirect_uri: config.redirect_uri.clone(),
    })
}

fn parse_authorization_input(input: &str) -> Result<(String, Option<String>)> {
    let value = input.trim();
    if value.is_empty() {
        anyhow::bail!("authorization code cannot be empty");
    }
    if let Ok(url) = Url::parse(value) {
        let code = url
            .query_pairs()
            .find(|(name, _)| name == "code")
            .map(|(_, value)| value.into_owned())
            .context("callback URL has no authorization code")?;
        let state = url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .map(|(_, value)| value.into_owned());
        return Ok((code, state));
    }
    if let Some((code, state)) = value.split_once('#') {
        return Ok((code.to_string(), Some(state.to_string())));
    }
    if value.contains("code=") {
        let url = Url::parse(&format!("http://localhost/?{value}"))?;
        return parse_authorization_input(url.as_str());
    }
    Ok((value.to_string(), None))
}

async fn cancellable_send(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<reqwest::Response> {
    tokio::select! {
        response = request.send() => Ok(response?),
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    }
}

fn credential_from_token(token: TokenResponse) -> OAuthCredential {
    OAuthCredential {
        kind: "oauth".into(),
        access: token.access_token,
        refresh: token.refresh_token,
        // Match Pi and refresh five minutes before the server expiry.
        expires: chrono::Utc::now().timestamp_millis() + token.expires_in.saturating_mul(1000)
            - 5 * 60 * 1000,
        account_id: String::new(),
        available_model_ids: None,
    }
}

async fn token_request(
    config: &OAuthConfig,
    body: serde_json::Value,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let response = cancellable_send(
        crate::stream::http_client()
            .post(&config.token_url)
            .header("accept", "application/json")
            .json(&body),
        cancel,
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("Anthropic token {operation} failed ({status})");
    }
    let token = response
        .json::<TokenResponse>()
        .await
        .with_context(|| format!("Anthropic token {operation} response has invalid fields"))?;
    Ok(credential_from_token(token))
}

pub async fn finish_authorization(
    config: &OAuthConfig,
    pending: &PendingAuthorization,
    input: &str,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let (code, state) = parse_authorization_input(input)?;
    if state.as_deref().is_some_and(|state| state != pending.state) {
        anyhow::bail!("OAuth state mismatch");
    }
    token_request(
        config,
        json!({
            "grant_type": "authorization_code",
            "client_id": client_id(),
            "code": code,
            "state": state.unwrap_or_else(|| pending.state.clone()),
            "redirect_uri": pending.redirect_uri,
            "code_verifier": pending.verifier,
        }),
        "exchange",
        cancel,
    )
    .await
}

/// Start a browser login and wait for the verified loopback callback.
pub async fn login_browser(
    config: &OAuthConfig,
    cancel: &CancellationToken,
    show_url: impl FnOnce(&str),
) -> Result<OAuthCredential> {
    let redirect = Url::parse(&config.redirect_uri).context("invalid Anthropic callback URL")?;
    let port = redirect
        .port_or_known_default()
        .context("Anthropic callback URL has no port")?;
    let callback_path = redirect.path().to_string();
    let listener = TcpListener::bind((config.callback_host.as_str(), port))
        .await
        .with_context(|| format!("listen for Anthropic OAuth callback on port {port}"))?;
    let pending = start_authorization(config)?;
    show_url(&pending.authorization_url);

    let deadline = tokio::time::sleep(config.timeout);
    tokio::pin!(deadline);
    loop {
        let (mut socket, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
            _ = &mut deadline => anyhow::bail!("Anthropic browser login timed out"),
        };
        let mut request = vec![0_u8; 8192];
        let count = socket.read(&mut request).await?;
        let request = String::from_utf8_lossy(&request[..count]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1));
        let parsed =
            target.and_then(|target| Url::parse(&format!("http://localhost{target}")).ok());
        let input = parsed.as_ref().map(Url::as_str).unwrap_or_default();
        let valid_path = parsed
            .as_ref()
            .is_some_and(|url| url.path() == callback_path);
        let credential = if valid_path {
            finish_authorization(config, &pending, input, cancel).await
        } else {
            Err(anyhow::anyhow!("invalid callback path"))
        };
        let accepted = credential.is_ok();
        let (status, page) = if accepted {
            (
                "200 OK",
                "Anthropic authentication is complete. You can close this window.",
            )
        } else {
            (
                "400 Bad Request",
                "The Anthropic OAuth callback is invalid.",
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        socket.write_all(response.as_bytes()).await?;
        if accepted {
            return credential;
        }
    }
}

pub async fn refresh(
    credential: &OAuthCredential,
    config: &OAuthConfig,
) -> Result<OAuthCredential> {
    let cancel = CancellationToken::new();
    token_request(
        config,
        json!({
            "grant_type": "refresh_token",
            "client_id": client_id(),
            "refresh_token": credential.refresh,
        }),
        "refresh",
        &cancel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    #[test]
    fn authorization_url_has_pi_scopes_and_pkce() {
        let pending = start_authorization(&OAuthConfig::default()).unwrap();
        let url = Url::parse(&pending.authorization_url).unwrap();
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["state"], pending.state);
        assert!(query["scope"].contains("user:inference"));
        assert!(query["scope"].contains("user:sessions:claude_code"));
    }

    #[test]
    fn manual_input_accepts_url_code_hash_and_plain_code() {
        assert_eq!(
            parse_authorization_input("https://localhost/callback?code=one&state=two").unwrap(),
            ("one".into(), Some("two".into()))
        );
        assert_eq!(
            parse_authorization_input("one#two").unwrap(),
            ("one".into(), Some("two".into()))
        );
        assert_eq!(
            parse_authorization_input("one").unwrap(),
            ("one".into(), None)
        );
    }

    async fn read_request(socket: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 2048];
        loop {
            let count = socket.read(&mut chunk).await.unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..count]);
            let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    async fn token_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}/token"), task)
    }

    #[tokio::test]
    async fn manual_flow_exchanges_json_and_builds_refreshable_credential() {
        let (token_url, server) = token_server(
            "200 OK",
            r#"{"access_token":"access-one","refresh_token":"refresh-one","expires_in":3600}"#,
        )
        .await;
        let config = OAuthConfig {
            token_url,
            ..Default::default()
        };
        let pending = start_authorization(&config).unwrap();
        let credential = finish_authorization(
            &config,
            &pending,
            "authorization-one",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(credential.access, "access-one");
        assert_eq!(credential.refresh, "refresh-one");
        assert!(!credential.is_expired());
        let request = server.await.unwrap();
        assert!(request.contains("authorization-one"));
        assert!(request.contains("code_verifier"));
    }

    #[tokio::test]
    async fn state_mismatch_stops_before_token_exchange() {
        let pending = start_authorization(&OAuthConfig::default()).unwrap();
        let input = "http://localhost:53692/callback?code=one&state=wrong";
        let error = finish_authorization(
            &OAuthConfig::default(),
            &pending,
            input,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("state mismatch"));
    }

    #[tokio::test]
    async fn token_errors_do_not_echo_response_secrets() {
        let (token_url, server) = token_server(
            "400 Bad Request",
            r#"{"error":"bad","access_token":"must-not-leak"}"#,
        )
        .await;
        let credential = OAuthCredential {
            kind: "oauth".into(),
            access: "old".into(),
            refresh: "refresh-old".into(),
            expires: 0,
            account_id: String::new(),
            available_model_ids: None,
        };
        let error = refresh(
            &credential,
            &OAuthConfig {
                token_url,
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(!error.contains("must-not-leak"));
        server.await.unwrap();
    }
}
