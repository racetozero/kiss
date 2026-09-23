//! Meta Muse subscription OAuth device-code login.

use super::OAuthCredential;
use super::device_code::{PollResult, poll};
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

const CLIENT_ID: &str = "1031625952748946";
const API_KEY_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub device_url: String,
    pub token_url: String,
    pub mint_url: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            device_url: "https://auth.meta.com/oidc/device/authorization/".into(),
            token_url: "https://auth.meta.com/oidc/device/token/".into(),
            mint_url: "https://api.meta.ai/muse-code/key".into(),
        }
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

async fn response_json(response: reqwest::Response) -> (reqwest::StatusCode, Value) {
    let status = response.status();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

fn required(body: &Value, name: &str) -> Result<String> {
    body[name]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("Meta OAuth response has no {name}"))
}

fn error_detail(body: &Value) -> String {
    ["error_description", "detail", "message", "error"]
        .into_iter()
        .find_map(|key| body[key].as_str().filter(|value| !value.trim().is_empty()))
        .map(|value| format!(": {value}"))
        .unwrap_or_default()
}

fn trusted_verification_url(value: &str) -> Result<String> {
    let url = Url::parse(value).context("Meta returned an invalid verification URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("Meta returned an untrusted verification URL");
    }
    Ok(url.into())
}

pub async fn start(
    config: &OAuthConfig,
    cancel: &CancellationToken,
) -> Result<DeviceAuthorization> {
    let request = crate::stream::http_client()
        .post(&config.device_url)
        .header("accept", "application/json")
        .form(&[("client_id", CLIENT_ID)])
        .timeout(Duration::from_secs(30));
    let response = tokio::select! {
        response = request.send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let (status, body) = response_json(response).await;
    if !status.is_success() {
        anyhow::bail!(
            "Meta device authorization failed (HTTP {status}){}",
            error_detail(&body)
        );
    }
    let verification = body["verification_uri_complete"]
        .as_str()
        .or_else(|| body["verification_uri"].as_str())
        .context("Meta OAuth response has no verification_uri")?;
    Ok(DeviceAuthorization {
        device_code: required(&body, "device_code")?,
        user_code: required(&body, "user_code")?,
        verification_uri: trusted_verification_url(verification)?,
        interval: Duration::from_secs(body["interval"].as_u64().unwrap_or(5).max(1)),
        expires_in: Duration::from_secs(body["expires_in"].as_u64().unwrap_or(900)),
    })
}

async fn mint(
    config: &OAuthConfig,
    identity_token: &str,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let request = crate::stream::http_client()
        .post(&config.mint_url)
        .bearer_auth(identity_token)
        .header("accept", "application/json")
        .header("x-api-version", "1.0.0")
        .json(&serde_json::json!({}))
        .timeout(Duration::from_secs(30));
    let response = tokio::select! {
        response = request.send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let (status, body) = response_json(response).await;
    if matches!(status.as_u16(), 401 | 403) {
        anyhow::bail!(
            "Meta session expired (HTTP {status}). Run `/login meta` to sign in again{}",
            error_detail(&body)
        );
    }
    if !status.is_success() {
        anyhow::bail!(
            "Meta API key mint failed (HTTP {status}){}",
            error_detail(&body)
        );
    }
    let access = body["api_key"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let Some(access) = access else {
        let setup = body["action_url"]
            .as_str()
            .and_then(|value| trusted_verification_url(value).ok())
            .map(|url| format!(" Complete setup at {url}"))
            .unwrap_or_default();
        anyhow::bail!("Meta did not issue an API key.{setup}");
    };
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access,
        refresh: identity_token.into(),
        expires: chrono::Utc::now().timestamp_millis() + API_KEY_LIFETIME_MS,
        account_id: String::new(),
        available_model_ids: None,
    })
}

pub async fn finish(
    config: &OAuthConfig,
    device: &DeviceAuthorization,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let identity_token = poll(device.interval, device.expires_in, true, cancel, || async {
        let request = crate::stream::http_client()
            .post(&config.token_url)
            .header("accept", "application/json")
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
                ("client_id", CLIENT_ID),
            ])
            .timeout(Duration::from_secs(30));
        let response = tokio::select! {
            response = request.send() => response?,
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
        };
        let (status, body) = response_json(response).await;
        if status.is_success() {
            return Ok(PollResult::Complete(required(&body, "access_token")?));
        }
        Ok(match body["error"].as_str() {
            Some("authorization_pending") => PollResult::Pending,
            Some("slow_down") => {
                PollResult::SlowDown(body["interval"].as_u64().map(Duration::from_secs))
            }
            Some("access_denied") => PollResult::Failed("Meta login was denied".into()),
            Some("expired_token") => {
                PollResult::Failed("Meta device authorization expired; restart login".into())
            }
            _ => PollResult::Failed(format!(
                "Meta device token request failed (HTTP {status}){}",
                error_detail(&body)
            )),
        })
    })
    .await?;
    mint(config, &identity_token, cancel).await
}

pub async fn refresh(
    credential: &OAuthCredential,
    config: &OAuthConfig,
) -> Result<OAuthCredential> {
    mint(config, &credential.refresh, &CancellationToken::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verification_url_rejects_non_http_schemes() {
        assert!(trusted_verification_url("https://auth.meta.com/device").is_ok());
        assert!(trusted_verification_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn errors_prefer_actionable_provider_details() {
        assert_eq!(
            error_detail(&json!({"detail":"finish account setup"})),
            ": finish account setup"
        );
    }
}
