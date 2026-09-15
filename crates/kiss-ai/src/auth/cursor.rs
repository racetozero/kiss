//! Cursor deep-link PKCE authentication.

use super::OAuthCredential;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use rand::RngCore as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub login_url: String,
    pub poll_url: String,
    pub refresh_url: String,
    pub poll_interval: Duration,
    pub timeout: Duration,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            login_url: "https://cursor.com/loginDeepControl".into(),
            poll_url: "https://api2.cursor.sh/auth/poll".into(),
            refresh_url: "https://api2.cursor.sh/auth/exchange_user_api_key".into(),
            poll_interval: Duration::from_secs(2),
            timeout: Duration::from_secs(10 * 60),
        }
    }
}

#[derive(Debug, Clone)]
struct PendingAuthorization {
    url: String,
    verifier: String,
    id: String,
}

fn start_authorization(config: &OAuthConfig) -> Result<PendingAuthorization> {
    let mut bytes = [0_u8; 96];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let id = uuid::Uuid::new_v4().to_string();
    let mut url = Url::parse(&config.login_url)?;
    url.query_pairs_mut()
        .append_pair("challenge", &challenge)
        .append_pair("uuid", &id)
        .append_pair("mode", "login")
        .append_pair("redirectTarget", "cli");
    Ok(PendingAuthorization {
        url: url.into(),
        verifier,
        id,
    })
}

pub async fn login_browser(
    config: &OAuthConfig,
    cancel: &CancellationToken,
    show_url: impl FnOnce(&str),
) -> Result<OAuthCredential> {
    let pending = start_authorization(config)?;
    show_url(&pending.url);
    let deadline = tokio::time::sleep(config.timeout);
    tokio::pin!(deadline);
    let mut failures = 0;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {}
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
            _ = &mut deadline => anyhow::bail!("Cursor login timed out"),
        }
        let response = tokio::select! {
            response = crate::stream::http_client()
                .get(&config.poll_url)
                .query(&[("uuid", pending.id.as_str()), ("verifier", pending.verifier.as_str())])
                .send() => response,
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                failures += 1;
                if failures >= 3 {
                    return Err(error).context("Cursor authentication polling failed");
                }
                continue;
            }
        };
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            failures = 0;
            continue;
        }
        let status = response.status();
        let body = response.json::<Value>().await.unwrap_or_default();
        if !status.is_success() {
            failures += 1;
            if failures >= 3 {
                anyhow::bail!("Cursor authentication polling returned HTTP {status}");
            }
            continue;
        }
        let access = body["accessToken"]
            .as_str()
            .filter(|value| !value.is_empty())
            .context("Cursor authentication returned no access token")?;
        let refresh = body["refreshToken"]
            .as_str()
            .filter(|value| !value.is_empty())
            .context("Cursor authentication returned no refresh token")?;
        return Ok(OAuthCredential {
            kind: "oauth".into(),
            access: access.into(),
            refresh: refresh.into(),
            expires: token_expiry(access),
            account_id: String::new(),
            available_model_ids: None,
        });
    }
}

pub async fn refresh(
    credential: &OAuthCredential,
    config: &OAuthConfig,
) -> Result<OAuthCredential> {
    let response = crate::stream::http_client()
        .post(&config.refresh_url)
        .bearer_auth(&credential.refresh)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await?;
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Cursor token refresh returned HTTP {status}");
    }
    let access = body["accessToken"]
        .as_str()
        .filter(|value| !value.is_empty())
        .context("Cursor token refresh returned no access token")?;
    let refresh = body["refreshToken"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(&credential.refresh);
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access: access.into(),
        refresh: refresh.into(),
        expires: token_expiry(access),
        account_id: credential.account_id.clone(),
        available_model_ids: credential.available_model_ids.clone(),
    })
}

fn token_expiry(token: &str) -> i64 {
    let expiry = token
        .split('.')
        .nth(1)
        .and_then(|payload| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .ok()
        })
        .and_then(|payload| serde_json::from_slice::<Value>(&payload).ok())
        .and_then(|payload| payload["exp"].as_i64())
        .and_then(|seconds| seconds.checked_mul(1000));
    expiry.unwrap_or_else(|| chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000) - 5 * 60 * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_url_has_cursor_pkce_fields() {
        let pending = start_authorization(&OAuthConfig::default()).unwrap();
        let url = Url::parse(&pending.url).unwrap();
        let query = url
            .query_pairs()
            .into_owned()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(query["uuid"], pending.id);
        assert_eq!(query["mode"], "login");
        assert_eq!(query["redirectTarget"], "cli");
        assert!(!query["challenge"].is_empty());
    }

    #[test]
    fn jwt_expiry_uses_exp_claim_with_safety_margin() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":2000000000}"#);
        assert_eq!(token_expiry(&format!("x.{payload}.y")), 1_999_999_700_000);
    }
}
