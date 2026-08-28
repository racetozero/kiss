//! Radius gateway OAuth browser and device-code login.

use super::OAuthCredential;
use super::device_code::{PollResult, poll};
use anyhow::{Context as _, Result};
use base64::Engine as _;
use rand::RngCore as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

const CLIENT_ID: &str = "pi-gateway";
const SCOPE: &str = "gateway offline_access";

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub gateway: String,
    pub callback_host: String,
    pub callback_port: u16,
    pub timeout: Duration,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            gateway: std::env::var("RADIUS_GATEWAY")
                .unwrap_or_else(|_| "https://radius.pi.dev".into()),
            callback_host: std::env::var("KISS_OAUTH_CALLBACK_HOST")
                .unwrap_or_else(|_| "127.0.0.1".into()),
            callback_port: 1456,
            timeout: Duration::from_secs(5 * 60),
        }
    }
}

impl OAuthConfig {
    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.gateway.trim_end_matches('/'), path)
    }

    fn redirect_uri(&self) -> String {
        format!(
            "http://{}:{}/oauth/callback",
            self.callback_host, self.callback_port
        )
    }
}

#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
    pub expires_in: Duration,
}

#[derive(Debug)]
struct OAuthResponseError {
    status: reqwest::StatusCode,
    code: Option<String>,
    detail: String,
}

impl std::fmt::Display for OAuthResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Radius OAuth failed (HTTP {}): {}",
            self.status, self.detail
        )
    }
}

impl std::error::Error for OAuthResponseError {}

async fn token_request(
    config: &OAuthConfig,
    fields: &[(&str, &str)],
    cancel: &CancellationToken,
) -> std::result::Result<OAuthCredential, OAuthResponseError> {
    let response = tokio::select! {
        response = crate::stream::http_client()
            .post(config.endpoint("/v1/oauth/token"))
            .header("accept", "application/json")
            .form(fields)
            .send() => response.map_err(|error| OAuthResponseError { status: reqwest::StatusCode::INTERNAL_SERVER_ERROR, code: None, detail: error.to_string() })?,
        _ = cancel.cancelled() => return Err(OAuthResponseError { status: reqwest::StatusCode::REQUEST_TIMEOUT, code: None, detail: "login cancelled".into() }),
    };
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or_default();
    if !status.is_success() {
        let code = body["error"].as_str().map(str::to_string);
        let detail = body["error_description"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("unknown error")
            .to_string();
        return Err(OAuthResponseError {
            status,
            code,
            detail,
        });
    }
    let access = body["access_token"]
        .as_str()
        .filter(|value| !value.is_empty())
        .context("Radius OAuth response has no access_token")
        .map_err(|error| OAuthResponseError {
            status,
            code: None,
            detail: error.to_string(),
        })?;
    let refresh = body["refresh_token"]
        .as_str()
        .filter(|value| !value.is_empty())
        .context("Radius OAuth response has no refresh_token")
        .map_err(|error| OAuthResponseError {
            status,
            code: None,
            detail: error.to_string(),
        })?;
    let expires = body["expires_in"].as_i64().unwrap_or(3600);
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access: access.into(),
        refresh: refresh.into(),
        expires: chrono::Utc::now().timestamp_millis() + expires.saturating_mul(1000) - 60_000,
        account_id: String::new(),
        available_model_ids: None,
    })
}

pub async fn start_device(
    config: &OAuthConfig,
    cancel: &CancellationToken,
) -> Result<DeviceAuthorization> {
    let response = tokio::select! {
        response = crate::stream::http_client()
            .post(config.endpoint("/v1/oauth/device"))
            .header("accept", "application/json")
            .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
            .send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Radius device authorization failed (HTTP {status})");
    }
    Ok(DeviceAuthorization {
        device_code: body["device_code"]
            .as_str()
            .context("Radius device response has no device_code")?
            .into(),
        user_code: body["user_code"]
            .as_str()
            .context("Radius device response has no user_code")?
            .into(),
        verification_uri: body["verification_uri"]
            .as_str()
            .context("Radius device response has no verification_uri")?
            .into(),
        interval: Duration::from_secs(body["interval"].as_u64().unwrap_or(5).max(1)),
        expires_in: Duration::from_secs(body["expires_in"].as_u64().unwrap_or(900)),
    })
}

pub async fn finish_device(
    config: &OAuthConfig,
    device: &DeviceAuthorization,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    poll(
        device.interval,
        device.expires_in,
        false,
        cancel,
        || async {
            match token_request(
                config,
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", CLIENT_ID),
                    ("device_code", &device.device_code),
                ],
                cancel,
            )
            .await
            {
                Ok(value) => Ok(PollResult::Complete(value)),
                Err(error) => Ok(match error.code.as_deref() {
                    Some("authorization_pending") => PollResult::Pending,
                    Some("slow_down") => PollResult::SlowDown(None),
                    Some("expired_token") => {
                        PollResult::Failed("Radius device authorization expired".into())
                    }
                    Some("access_denied") => {
                        PollResult::Failed("Radius device authorization was denied".into())
                    }
                    _ => return Err(error.into()),
                }),
            }
        },
    )
    .await
}

fn create_pkce() -> (String, String) {
    let mut bytes = [0_u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub async fn login_browser(
    config: &OAuthConfig,
    cancel: &CancellationToken,
    show_url: impl FnOnce(&str),
) -> Result<OAuthCredential> {
    let discovery = tokio::select! {
        response = crate::stream::http_client().get(config.endpoint("/v1/oauth")).header("accept", "application/json").send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let status = discovery.status();
    let body = discovery.json::<Value>().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("could not load Radius OAuth config (HTTP {status})");
    }
    let authorization_endpoint = body["authorizationEndpoint"]
        .as_str()
        .context("Radius OAuth config has no authorizationEndpoint")?;
    let listener = TcpListener::bind((config.callback_host.as_str(), config.callback_port)).await?;
    let (verifier, challenge) = create_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let redirect_uri = config.redirect_uri();
    let mut url = Url::parse(authorization_endpoint)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("handoff", "url")
        .append_pair("state", &state);
    show_url(url.as_str());
    let deadline = tokio::time::sleep(config.timeout);
    tokio::pin!(deadline);
    loop {
        let (mut socket, _) = tokio::select! {
            result = listener.accept() => result?,
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
            _ = &mut deadline => anyhow::bail!("Radius OAuth login timed out"),
        };
        let mut request = vec![0_u8; 8192];
        let length = socket.read(&mut request).await?;
        let target = String::from_utf8_lossy(&request[..length])
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .map(str::to_string);
        let callback = target
            .as_deref()
            .and_then(|target| Url::parse(&format!("http://localhost{target}")).ok());
        let valid = callback.as_ref().is_some_and(|url| {
            url.path() == "/oauth/callback"
                && url
                    .query_pairs()
                    .any(|(name, value)| name == "state" && value == state)
        });
        let code = callback.as_ref().and_then(|url| {
            url.query_pairs()
                .find(|(name, _)| name == "code")
                .map(|(_, value)| value.into_owned())
        });
        let accepted = valid && code.is_some();
        let page = if accepted {
            "Radius authentication is complete. You can close this window."
        } else {
            "The Radius OAuth callback is invalid."
        };
        let response_status = if accepted {
            "200 OK"
        } else {
            "400 Bad Request"
        };
        let response = format!(
            "HTTP/1.1 {response_status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        socket.write_all(response.as_bytes()).await?;
        if let Some(code) = code.filter(|_| accepted) {
            return token_request(
                config,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", CLIENT_ID),
                    ("redirect_uri", &redirect_uri),
                    ("code", &code),
                    ("code_verifier", &verifier),
                ],
                cancel,
            )
            .await
            .map_err(Into::into);
        }
    }
}

pub async fn refresh(
    credential: &OAuthCredential,
    config: &OAuthConfig,
) -> Result<OAuthCredential> {
    token_request(
        config,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", &credential.refresh),
        ],
        &CancellationToken::new(),
    )
    .await
    .map_err(Into::into)
}
