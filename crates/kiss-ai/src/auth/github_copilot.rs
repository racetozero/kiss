//! GitHub Copilot OAuth device-code login.

use super::OAuthCredential;
use super::device_code::{PollResult, poll};
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const COPILOT_API_VERSION: &str = "2026-06-01";

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub domain: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            domain: "github.com".into(),
        }
    }
}

impl OAuthConfig {
    fn device_url(&self) -> String {
        format!("https://{}/login/device/code", self.domain)
    }

    fn access_url(&self) -> String {
        format!("https://{}/login/oauth/access_token", self.domain)
    }

    fn copilot_url(&self) -> String {
        format!("https://api.{}/copilot_internal/v2/token", self.domain)
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
        response = crate::stream::http_client()
            .post(url)
            .header("accept", "application/json")
            .header("user-agent", "GitHubCopilotChat/0.35.0")
            .form(fields)
            .send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("GitHub OAuth returned invalid JSON")?;
    Ok((status, body))
}

fn required(body: &Value, field: &str) -> Result<String> {
    body[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("GitHub OAuth response has no {field}"))
}

pub fn normalize_domain(input: &str) -> Result<String> {
    let value = input.trim();
    if value.is_empty() {
        return Ok("github.com".into());
    }
    let url = Url::parse(if value.contains("://") {
        value
    } else {
        return Url::parse(&format!("https://{value}"))?
            .host_str()
            .map(str::to_string)
            .context("GitHub Enterprise URL has no host");
    })?;
    url.host_str()
        .map(str::to_string)
        .context("GitHub Enterprise URL has no host")
}

pub async fn start(
    config: &OAuthConfig,
    cancel: &CancellationToken,
) -> Result<DeviceAuthorization> {
    let (status, body) = post_form(
        &config.device_url(),
        &[("client_id", CLIENT_ID), ("scope", "read:user")],
        cancel,
    )
    .await?;
    if !status.is_success() {
        anyhow::bail!("GitHub device authorization failed (HTTP {status})");
    }
    let verification = required(&body, "verification_uri")?;
    let url = Url::parse(&verification).context("GitHub returned an invalid verification URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("GitHub returned an untrusted verification URL");
    }
    Ok(DeviceAuthorization {
        device_code: required(&body, "device_code")?,
        user_code: required(&body, "user_code")?,
        verification_uri: url.into(),
        interval: Duration::from_secs(body["interval"].as_u64().unwrap_or(5).max(1)),
        expires_in: Duration::from_secs(body["expires_in"].as_u64().unwrap_or(900)),
    })
}

async fn copilot_token(
    config: &OAuthConfig,
    github_token: &str,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let response = tokio::select! {
        response = crate::stream::http_client()
            .get(config.copilot_url())
            .bearer_auth(github_token)
            .header("accept", "application/json")
            .header("user-agent", "GitHubCopilotChat/0.35.0")
            .header("editor-version", "vscode/1.107.0")
            .header("editor-plugin-version", "copilot-chat/0.35.0")
            .header("copilot-integration-id", "vscode-chat")
            .send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("Copilot token response has invalid JSON")?;
    if !status.is_success() {
        anyhow::bail!("Copilot token request failed (HTTP {status})");
    }
    let expires = body["expires_at"]
        .as_i64()
        .context("Copilot token response has no expires_at")?;
    let mut credential = OAuthCredential {
        kind: "oauth".into(),
        access: required(&body, "token")?,
        refresh: github_token.into(),
        expires: expires.saturating_mul(1000) - 5 * 60 * 1000,
        account_id: if config.domain != "github.com" {
            config.domain.clone()
        } else {
            String::new()
        },
        available_model_ids: None,
    };
    credential.available_model_ids = Some(sync_account_models(&credential, cancel).await?);
    Ok(credential)
}

fn copilot_base_url(token: &str, enterprise_domain: Option<&str>) -> String {
    if let Some(proxy_host) = token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))
    {
        return format!("https://{}", proxy_host.replacen("proxy.", "api.", 1));
    }
    enterprise_domain.map_or_else(
        || "https://api.individual.githubcopilot.com".into(),
        |domain| format!("https://copilot-api.{domain}"),
    )
}

fn parse_model_catalog(
    body: &Value,
    allow_policy_fallback: bool,
    known_ids: &std::collections::HashSet<String>,
) -> Result<(Vec<String>, Vec<String>)> {
    let data = body["data"]
        .as_array()
        .context("Copilot models response has no data array")?;
    let mut models = Vec::new();
    for item in data {
        let Some(id) = item["id"].as_str() else {
            continue;
        };
        if item["capabilities"]["supports"]["tool_calls"].as_bool() == Some(false) {
            continue;
        }
        models.push((
            id.to_string(),
            item["model_picker_enabled"].as_bool() == Some(true),
            item["policy"]["state"].as_str().map(str::to_string),
        ));
    }
    let picker_ids = models
        .iter()
        .filter(|(_, picker, policy)| *picker && policy.as_deref() != Some("disabled"))
        .map(|(id, _, _)| id.clone())
        .collect::<Vec<_>>();
    let use_fallback = allow_policy_fallback && picker_ids.is_empty();
    let available = if use_fallback {
        models
            .iter()
            .filter(|(_, _, policy)| policy.as_deref() == Some("enabled"))
            .map(|(id, _, _)| id.clone())
            .collect()
    } else {
        picker_ids
    };
    let policies = models
        .into_iter()
        .filter(|(id, picker, policy)| {
            policy.as_deref() == Some("unconfigured")
                && known_ids.contains(id)
                && (*picker || use_fallback)
        })
        .map(|(id, _, _)| id)
        .collect();
    Ok((available, policies))
}

fn copilot_request_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header("accept", "application/json")
        .header("user-agent", "GitHubCopilotChat/0.35.0")
        .header("editor-version", "vscode/1.107.0")
        .header("editor-plugin-version", "copilot-chat/0.35.0")
        .header("copilot-integration-id", "vscode-chat")
}

async fn sync_account_models(
    credential: &OAuthCredential,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let enterprise = (!credential.account_id.is_empty()).then_some(credential.account_id.as_str());
    let base_url = copilot_base_url(&credential.access, enterprise);
    let response = tokio::select! {
        response = copilot_request_headers(
            crate::stream::http_client()
                .get(format!("{base_url}/models"))
                .bearer_auth(&credential.access)
                .header("x-github-api-version", COPILOT_API_VERSION),
        ).send() => response?,
        _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
    };
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("Copilot models response has invalid JSON")?;
    if !status.is_success() {
        anyhow::bail!("Copilot models request failed (HTTP {status})");
    }
    let known_ids = crate::Registry::from_builtin()
        .all()
        .iter()
        .filter(|model| model.provider == "github-copilot")
        .map(|model| model.id.clone())
        .collect();
    let (mut available, policies) = parse_model_catalog(
        &body,
        base_url == "https://api.individual.githubcopilot.com",
        &known_ids,
    )?;
    for model_id in policies {
        match enable_account_model(&base_url, &credential.access, &model_id, cancel).await? {
            Some(true) => available.push(model_id),
            Some(false) => {}
            None => break,
        }
    }
    available.sort();
    available.dedup();
    Ok(available)
}

async fn enable_account_model(
    base_url: &str,
    access_token: &str,
    model_id: &str,
    cancel: &CancellationToken,
) -> Result<Option<bool>> {
    let encoded = url::form_urlencoded::byte_serialize(model_id.as_bytes()).collect::<String>();
    let started = std::time::Instant::now();
    for retry in 0..=2 {
        let response = tokio::select! {
            response = copilot_request_headers(
                crate::stream::http_client()
                    .post(format!("{base_url}/models/{encoded}/policy"))
                    .bearer_auth(access_token)
                    .header("openai-intent", "chat-policy")
                    .header("x-interaction-type", "chat-policy")
                    .json(&serde_json::json!({"state":"enabled"})),
            ).send() => response?,
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
        };
        if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(Some(response.status().is_success()));
        }
        if retry == 2 {
            return Ok(None);
        }
        let delay = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok())
            .map(|seconds| std::time::Duration::from_secs_f64(seconds.max(0.0)))
            .unwrap_or_else(|| std::time::Duration::from_millis(500 * (1 << retry)));
        if started.elapsed() + delay >= std::time::Duration::from_secs(5) {
            return Ok(None);
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
        }
    }
    Ok(None)
}

pub async fn finish(
    config: &OAuthConfig,
    device: &DeviceAuthorization,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let github_token = poll(device.interval, device.expires_in, true, cancel, || async {
        let (status, body) = post_form(
            &config.access_url(),
            &[
                ("client_id", CLIENT_ID),
                ("device_code", &device.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
            cancel,
        )
        .await?;
        if status.is_success() && body["access_token"].is_string() {
            return Ok(PollResult::Complete(required(&body, "access_token")?));
        }
        Ok(match body["error"].as_str() {
            Some("authorization_pending") => PollResult::Pending,
            Some("slow_down") => {
                PollResult::SlowDown(body["interval"].as_u64().map(Duration::from_secs))
            }
            Some(error) => PollResult::Failed(format!("GitHub device login failed: {error}")),
            None => PollResult::Failed(format!("GitHub device login failed (HTTP {status})")),
        })
    })
    .await?;
    copilot_token(config, &github_token, cancel).await
}

pub async fn refresh(
    credential: &OAuthCredential,
    _config: &OAuthConfig,
) -> Result<OAuthCredential> {
    let config = OAuthConfig {
        domain: if credential.account_id.is_empty() {
            "github.com".into()
        } else {
            credential.account_id.clone()
        },
    };
    copilot_token(&config, &credential.refresh, &CancellationToken::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enterprise_domain_accepts_urls_and_hosts() {
        assert_eq!(normalize_domain("").unwrap(), "github.com");
        assert_eq!(
            normalize_domain("https://company.ghe.com/path").unwrap(),
            "company.ghe.com"
        );
        assert_eq!(
            normalize_domain("company.ghe.com").unwrap(),
            "company.ghe.com"
        );
    }

    #[test]
    fn model_catalog_filters_tools_and_finds_policies() {
        let known = ["enabled", "needs-policy"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let (available, policies) = parse_model_catalog(
            &json!({"data":[
                {"id":"enabled","model_picker_enabled":true,"policy":{"state":"enabled"}},
                {"id":"needs-policy","model_picker_enabled":true,"policy":{"state":"unconfigured"}},
                {"id":"no-tools","model_picker_enabled":true,"capabilities":{"supports":{"tool_calls":false}}}
            ]}),
            false,
            &known,
        )
        .unwrap();
        assert_eq!(available, ["enabled", "needs-policy"]);
        assert_eq!(policies, ["needs-policy"]);
    }
}
