//! xAI OAuth device-code login.

use super::OAuthCredential;
use super::device_code::{PollResult, poll};
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub device_url: String,
    pub token_url: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            device_url: "https://auth.x.ai/oauth2/device/code".into(),
            token_url: "https://auth.x.ai/oauth2/token".into(),
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

async fn post_form(
    url: &str,
    fields: &[(&str, &str)],
    cancel: &CancellationToken,
) -> Result<(reqwest::StatusCode, Value)> {
    let response = tokio::select! {
        response = crate::stream::http_client().post(url).header("accept", "application/json").form(fields).send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("xAI OAuth returned invalid JSON")?;
    Ok((status, body))
}

fn required(body: &Value, name: &str) -> Result<String> {
    body[name]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("xAI OAuth response has no {name}"))
}

fn trusted_verification_url(value: &str) -> Result<String> {
    let url = Url::parse(value).context("xAI returned an invalid verification URL")?;
    if url.scheme() != "https" {
        anyhow::bail!("xAI returned an untrusted verification URL");
    }
    Ok(url.into())
}

fn credential(body: &Value, old_refresh: Option<&str>) -> Result<OAuthCredential> {
    let expires = body["expires_in"].as_i64().unwrap_or(3600);
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access: required(body, "access_token")?,
        refresh: body["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| old_refresh.map(str::to_string))
            .context("xAI OAuth response has no refresh_token")?,
        expires: chrono::Utc::now().timestamp_millis() + expires.saturating_mul(1000)
            - 5 * 60 * 1000,
        account_id: String::new(),
        available_model_ids: None,
    })
}

pub async fn start(
    config: &OAuthConfig,
    cancel: &CancellationToken,
) -> Result<DeviceAuthorization> {
    let (status, body) = post_form(
        &config.device_url,
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("referrer", "pi"),
        ],
        cancel,
    )
    .await?;
    if !status.is_success() {
        anyhow::bail!("xAI device authorization failed (HTTP {status})");
    }
    let verification = body["verification_uri_complete"]
        .as_str()
        .or_else(|| body["verification_uri"].as_str())
        .context("xAI OAuth response has no verification_uri")?;
    Ok(DeviceAuthorization {
        device_code: required(&body, "device_code")?,
        user_code: required(&body, "user_code")?,
        verification_uri: trusted_verification_url(verification)?,
        interval: Duration::from_secs(body["interval"].as_u64().unwrap_or(5).max(1)),
        expires_in: Duration::from_secs(body["expires_in"].as_u64().unwrap_or(900)),
    })
}

pub async fn finish(
    config: &OAuthConfig,
    device: &DeviceAuthorization,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    poll(device.interval, device.expires_in, true, cancel, || async {
        let (status, body) = post_form(
            &config.token_url,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", &device.device_code),
            ],
            cancel,
        )
        .await?;
        if status.is_success() {
            return Ok(PollResult::Complete(credential(&body, None)?));
        }
        Ok(match body["error"].as_str() {
            Some("authorization_pending") => PollResult::Pending,
            Some("slow_down") => PollResult::SlowDown(None),
            Some("access_denied" | "authorization_denied") => {
                PollResult::Failed("xAI device authorization was denied".into())
            }
            Some("expired_token") => PollResult::Failed("xAI device code expired".into()),
            error => PollResult::Failed(format!(
                "xAI device login failed (HTTP {status}): {}",
                error.unwrap_or("unknown error")
            )),
        })
    })
    .await
}

pub async fn refresh(
    credential_value: &OAuthCredential,
    config: &OAuthConfig,
) -> Result<OAuthCredential> {
    let cancel = CancellationToken::new();
    let (status, body) = post_form(
        &config.token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", &credential_value.refresh),
        ],
        &cancel,
    )
    .await?;
    if !status.is_success() {
        anyhow::bail!("xAI token refresh failed (HTTP {status})");
    }
    credential(&body, Some(&credential_value.refresh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_refresh_can_keep_the_old_refresh_token() {
        let value = credential(
            &json!({"access_token":"new", "expires_in":3600}),
            Some("old-refresh"),
        )
        .unwrap();
        assert_eq!(value.access, "new");
        assert_eq!(value.refresh, "old-refresh");
    }

    #[test]
    fn verification_url_must_use_https() {
        assert!(trusted_verification_url("https://accounts.x.ai/device").is_ok());
        assert!(trusted_verification_url("http://accounts.x.ai/device").is_err());
    }
}
