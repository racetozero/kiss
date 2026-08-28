//! Kimi Code subscription OAuth device-code login.

use super::OAuthCredential;
use super::device_code::{PollResult, poll};
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub oauth_host: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            oauth_host: std::env::var("KIMI_CODE_OAUTH_HOST")
                .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
                .unwrap_or_else(|_| "https://auth.kimi.com".into()),
        }
    }
}

impl OAuthConfig {
    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.oauth_host.trim_end_matches('/'), path)
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
        .context("Kimi Code OAuth returned invalid JSON")?;
    Ok((status, body))
}

fn required(body: &Value, field: &str) -> Result<String> {
    body[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("Kimi Code OAuth response has no {field}"))
}

fn credential(body: &Value) -> Result<OAuthCredential> {
    let expires = body["expires_in"]
        .as_i64()
        .context("Kimi Code OAuth response has no expires_in")?;
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access: required(body, "access_token")?,
        refresh: required(body, "refresh_token")?,
        expires: chrono::Utc::now().timestamp_millis() + expires.saturating_mul(1000),
        account_id: String::new(),
        available_model_ids: None,
    })
}

fn trusted_url(value: &str) -> Result<String> {
    let url = Url::parse(value).context("Kimi Code returned an invalid verification URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("Kimi Code returned an untrusted verification URL");
    }
    Ok(url.into())
}

pub async fn start(
    config: &OAuthConfig,
    cancel: &CancellationToken,
) -> Result<DeviceAuthorization> {
    let (status, body) = post_form(
        &config.endpoint("/api/oauth/device_authorization"),
        &[("client_id", CLIENT_ID)],
        cancel,
    )
    .await?;
    if !status.is_success() {
        anyhow::bail!("Kimi Code device authorization failed (HTTP {status})");
    }
    let verification = body["verification_uri_complete"]
        .as_str()
        .or_else(|| body["verification_uri"].as_str())
        .context("Kimi Code OAuth response has no verification URI")?;
    Ok(DeviceAuthorization {
        device_code: required(&body, "device_code")?,
        user_code: required(&body, "user_code")?,
        verification_uri: trusted_url(verification)?,
        interval: Duration::from_secs(body["interval"].as_u64().unwrap_or(5).max(1)),
        expires_in: Duration::from_secs(body["expires_in"].as_u64().unwrap_or(15 * 60)),
    })
}

pub async fn finish(
    config: &OAuthConfig,
    device: &DeviceAuthorization,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    poll(device.interval, device.expires_in, true, cancel, || async {
        let (status, body) = post_form(
            &config.endpoint("/api/oauth/token"),
            &[
                ("client_id", CLIENT_ID),
                ("device_code", &device.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
            cancel,
        )
        .await?;
        if status.is_success() {
            return Ok(PollResult::Complete(credential(&body)?));
        }
        Ok(match body["error"].as_str() {
            Some("authorization_pending") => PollResult::Pending,
            Some("slow_down") => {
                PollResult::SlowDown(body["interval"].as_u64().map(Duration::from_secs))
            }
            Some("expired_token") => {
                PollResult::Failed("Kimi Code device authorization expired".into())
            }
            Some("access_denied") => PollResult::Failed("Kimi Code login was denied".into()),
            error => PollResult::Failed(format!(
                "Kimi Code device login failed (HTTP {status}): {}",
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
    let url = config.endpoint("/api/oauth/token");
    let mut delay = Duration::from_secs(1);
    for attempt in 0..=3 {
        if attempt > 0 {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        let (status, body) = post_form(
            &url,
            &[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", &credential_value.refresh),
            ],
            &cancel,
        )
        .await?;
        if status.is_success() {
            return credential(&body);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || body["error"] == "invalid_grant"
        {
            anyhow::bail!("Kimi Code token refresh is unauthorized");
        }
        if status != reqwest::StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
            anyhow::bail!("Kimi Code token refresh failed (HTTP {status})");
        }
    }
    anyhow::bail!("Kimi Code token refresh failed after retries")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_response_requires_refreshable_fields() {
        let value = credential(&json!({
            "access_token":"access",
            "refresh_token":"refresh",
            "expires_in":3600
        }))
        .unwrap();
        assert_eq!(value.access, "access");
        assert_eq!(value.refresh, "refresh");
        assert!(credential(&json!({"access_token":"access"})).is_err());
    }
}
