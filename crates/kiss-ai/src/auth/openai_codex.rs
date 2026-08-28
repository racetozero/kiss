//! OpenAI Codex OAuth for ChatGPT subscription access.

use super::OAuthCredential;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use rand::RngCore as _;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE_URL: &str = "https://auth.openai.com";
const BROWSER_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub auth_base_url: String,
    pub client_id: String,
    pub browser_redirect_uri: String,
    pub device_redirect_uri: String,
    pub device_timeout: Duration,
    pub minimum_poll_interval: Duration,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            auth_base_url: AUTH_BASE_URL.into(),
            client_id: CLIENT_ID.into(),
            browser_redirect_uri: BROWSER_REDIRECT_URI.into(),
            device_redirect_uri: DEVICE_REDIRECT_URI.into(),
            device_timeout: Duration::from_secs(15 * 60),
            minimum_poll_interval: Duration::from_secs(1),
        }
    }
}

impl OAuthConfig {
    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.auth_base_url.trim_end_matches('/'), path)
    }

    pub fn device_verification_uri(&self) -> String {
        self.endpoint("/codex/device")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: Duration,
    pub verification_uri: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
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

fn authorization_url(config: &OAuthConfig, state: &str, challenge: &str) -> Result<String> {
    let mut url = Url::parse(&config.endpoint("/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &config.browser_redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "kiss");
    Ok(url.into())
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

fn credential_from_token(token: TokenResponse) -> Result<OAuthCredential> {
    let account_id = decode_jwt_account_id(&token.access_token)
        .context("OpenAI Codex token has no ChatGPT account ID")?;
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access: token.access_token,
        refresh: token.refresh_token,
        expires: chrono::Utc::now().timestamp_millis() + token.expires_in.saturating_mul(1000),
        account_id,
        available_model_ids: None,
    })
}

async fn read_token_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<TokenResponse> {
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("OpenAI Codex token {operation} failed ({status})");
    }
    response
        .json::<TokenResponse>()
        .await
        .with_context(|| format!("OpenAI Codex token {operation} response has invalid fields"))
}

async fn exchange_code(
    config: &OAuthConfig,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let response = cancellable_send(
        crate::stream::http_client()
            .post(config.endpoint("/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", config.client_id.as_str()),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
            ]),
        cancel,
    )
    .await?;
    credential_from_token(read_token_response(response, "exchange").await?)
}

/// Start a browser login. The callback runs after the local listener starts.
/// It can print the URL and open a browser.
pub async fn login_browser(
    config: &OAuthConfig,
    cancel: &CancellationToken,
    show_url: impl FnOnce(&str),
) -> Result<OAuthCredential> {
    let redirect = Url::parse(&config.browser_redirect_uri).context("invalid callback URL")?;
    let port = redirect
        .port_or_known_default()
        .context("callback URL has no port")?;
    let callback_path = redirect.path().to_string();
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("listen for OAuth callback on port {port}"))?;

    let (verifier, challenge) = create_pkce();
    let state = random_url_safe(24);
    let url = authorization_url(config, &state, &challenge)?;
    show_url(&url);

    let deadline = tokio::time::sleep(config.device_timeout);
    tokio::pin!(deadline);
    let code = loop {
        let (mut socket, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
            _ = &mut deadline => anyhow::bail!("browser login timed out"),
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
        let valid_path = parsed
            .as_ref()
            .is_some_and(|url| url.path() == callback_path);
        let returned_state = parsed.as_ref().and_then(|url| {
            url.query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
        });
        let code = parsed.as_ref().and_then(|url| {
            url.query_pairs()
                .find(|(key, _)| key == "code")
                .map(|(_, value)| value.into_owned())
        });
        let accepted = valid_path && returned_state.as_deref() == Some(&state) && code.is_some();
        let (status, page) = if accepted {
            (
                "200 OK",
                "OpenAI authentication is complete. You can close this window.",
            )
        } else {
            ("400 Bad Request", "The OAuth callback is invalid.")
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        socket.write_all(response.as_bytes()).await?;
        socket.shutdown().await?;
        if accepted {
            break code.expect("accepted callback has a code");
        }
    };

    exchange_code(
        config,
        &code,
        &verifier,
        &config.browser_redirect_uri,
        cancel,
    )
    .await
}

pub async fn start_device_authorization(
    config: &OAuthConfig,
    cancel: &CancellationToken,
) -> Result<DeviceAuthorization> {
    let response = cancellable_send(
        crate::stream::http_client()
            .post(config.endpoint("/api/accounts/deviceauth/usercode"))
            .json(&json!({ "client_id": config.client_id })),
        cancel,
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "OpenAI Codex device code request failed ({status}): {}",
            crate::truncate_err(&body)
        );
    }
    let body: Value = response.json().await?;
    let device_auth_id = body["device_auth_id"]
        .as_str()
        .context("device code response has no device_auth_id")?;
    let user_code = body["user_code"]
        .as_str()
        .context("device code response has no user_code")?;
    let interval_seconds = body["interval"]
        .as_f64()
        .or_else(|| {
            body["interval"]
                .as_str()
                .and_then(|value| value.parse().ok())
        })
        .context("device code response has no valid interval")?;
    if !interval_seconds.is_finite() || interval_seconds < 0.0 {
        anyhow::bail!("device code response has an invalid interval");
    }
    Ok(DeviceAuthorization {
        device_auth_id: device_auth_id.into(),
        user_code: user_code.into(),
        interval: Duration::from_secs_f64(interval_seconds).max(config.minimum_poll_interval),
        verification_uri: config.device_verification_uri(),
    })
}

/// Poll a device login and exchange its authorization code for credentials.
pub async fn finish_device_authorization(
    config: &OAuthConfig,
    device: &DeviceAuthorization,
    cancel: &CancellationToken,
) -> Result<OAuthCredential> {
    let deadline = tokio::time::Instant::now() + config.device_timeout;
    let mut interval = device.interval.max(config.minimum_poll_interval);
    loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("device login timed out");
        }
        let response = cancellable_send(
            crate::stream::http_client()
                .post(config.endpoint("/api/accounts/deviceauth/token"))
                .json(&json!({
                    "device_auth_id": device.device_auth_id,
                    "user_code": device.user_code,
                })),
            cancel,
        )
        .await?;
        let status = response.status();
        if status.is_success() {
            let body: Value = response.json().await?;
            let code = body["authorization_code"]
                .as_str()
                .context("device token response has no authorization_code")?;
            let verifier = body["code_verifier"]
                .as_str()
                .context("device token response has no code_verifier")?;
            return exchange_code(config, code, verifier, &config.device_redirect_uri, cancel)
                .await;
        }

        let pending_status = status.as_u16() == 403 || status.as_u16() == 404;
        let body = response.text().await.unwrap_or_default();
        let error_code = serde_json::from_str::<Value>(&body).ok().and_then(|value| {
            value["error"]
                .as_str()
                .map(str::to_string)
                .or_else(|| value["error"]["code"].as_str().map(str::to_string))
        });
        match error_code.as_deref() {
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("deviceauth_authorization_pending") => {}
            _ if pending_status => {}
            _ => anyhow::bail!(
                "OpenAI Codex device login failed ({status}): {}",
                crate::truncate_err(&body)
            ),
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::select! {
            _ = tokio::time::sleep(interval.min(remaining)) => {},
            _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
        }
    }
}

pub async fn refresh(
    credential: &OAuthCredential,
    config: &OAuthConfig,
) -> Result<OAuthCredential> {
    let cancel = CancellationToken::new();
    let response = cancellable_send(
        crate::stream::http_client()
            .post(config.endpoint("/oauth/token"))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", credential.refresh.as_str()),
                ("client_id", config.client_id.as_str()),
            ]),
        &cancel,
    )
    .await?;
    credential_from_token(read_token_response(response, "refresh").await?)
}

pub(crate) fn decode_jwt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value[JWT_CLAIM_PATH]["chatgpt_account_id"]
        .as_str()
        .filter(|account| !account.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    fn fake_access_token(account: &str) -> String {
        let payload = json!({ (JWT_CLAIM_PATH): { "chatgpt_account_id": account } });
        format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn reads_account_id_from_url_safe_jwt() {
        assert_eq!(
            decode_jwt_account_id(&fake_access_token("acct-test")).as_deref(),
            Some("acct-test")
        );
        assert!(decode_jwt_account_id("bad-token").is_none());
    }

    #[test]
    fn browser_url_uses_state_and_pkce() {
        let config = OAuthConfig::default();
        let url = Url::parse(&authorization_url(&config, "state", "challenge").unwrap()).unwrap();
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["state"], "state");
        assert_eq!(query["code_challenge"], "challenge");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["originator"], "kiss");
    }

    #[test]
    fn token_response_builds_refreshable_credential() {
        let credential = credential_from_token(TokenResponse {
            access_token: fake_access_token("acct-one"),
            refresh_token: "refresh".into(),
            expires_in: 3600,
        })
        .unwrap();
        assert_eq!(credential.account_id, "acct-one");
        assert_eq!(credential.kind, "oauth");
        assert!(!credential.is_expired());
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

    async fn respond(socket: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    }

    async fn mock_server(
        responses: Vec<(&'static str, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut paths = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                paths.push(
                    request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_string(),
                );
                respond(&mut socket, status, &body).await;
            }
            paths
        });
        (format!("http://{address}"), task)
    }

    fn test_config(base: String) -> OAuthConfig {
        OAuthConfig {
            auth_base_url: base,
            device_timeout: Duration::from_secs(2),
            minimum_poll_interval: Duration::from_millis(1),
            ..Default::default()
        }
    }

    fn token_json(account: &str) -> String {
        json!({
            "access_token": fake_access_token(account),
            "refresh_token": "refresh-new",
            "expires_in": 3600
        })
        .to_string()
    }

    #[tokio::test]
    async fn device_flow_polls_and_exchanges_code() {
        let (base, server) = mock_server(vec![
            (
                "200 OK",
                json!({"device_auth_id":"device-one","user_code":"ABCD-EFGH","interval":0})
                    .to_string(),
            ),
            ("403 Forbidden", "{}".into()),
            (
                "200 OK",
                json!({"authorization_code":"code-one","code_verifier":"verifier-one"}).to_string(),
            ),
            ("200 OK", token_json("acct-device")),
        ])
        .await;
        let config = test_config(base);
        let cancel = CancellationToken::new();
        let device = start_device_authorization(&config, &cancel).await.unwrap();
        assert_eq!(device.user_code, "ABCD-EFGH");
        let credential = finish_device_authorization(&config, &device, &cancel)
            .await
            .unwrap();
        assert_eq!(credential.account_id, "acct-device");
        assert_eq!(
            server.await.unwrap(),
            [
                "/api/accounts/deviceauth/usercode",
                "/api/accounts/deviceauth/token",
                "/api/accounts/deviceauth/token",
                "/oauth/token"
            ]
        );
    }

    #[tokio::test]
    async fn browser_flow_validates_callback_and_exchanges_code() {
        let (base, server) = mock_server(vec![("200 OK", token_json("acct-browser"))]).await;
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let callback_port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut config = test_config(base);
        config.browser_redirect_uri = format!("http://localhost:{callback_port}/auth/callback");
        let cancel = CancellationToken::new();
        let (url_tx, url_rx) = tokio::sync::oneshot::channel();
        let login_config = config.clone();
        let login_cancel = cancel.clone();
        let login = tokio::spawn(async move {
            login_browser(&login_config, &login_cancel, |url| {
                url_tx.send(url.to_string()).unwrap();
            })
            .await
        });
        let authorization_url = Url::parse(&url_rx.await.unwrap()).unwrap();
        let state = authorization_url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .into_owned();
        let target = format!("/auth/callback?code=browser-code&state={state}");
        let mut socket = TcpStream::connect(("127.0.0.1", callback_port))
            .await
            .unwrap();
        socket
            .write_all(
                format!(
                    "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{callback_port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200 OK"));
        let credential = login.await.unwrap().unwrap();
        assert_eq!(credential.account_id, "acct-browser");
        assert_eq!(server.await.unwrap(), ["/oauth/token"]);
    }

    #[tokio::test]
    async fn token_endpoint_errors_do_not_echo_response_secrets() {
        let (base, server) = mock_server(vec![(
            "400 Bad Request",
            r#"{"error":"bad","access_token":"must-not-leak"}"#.into(),
        )])
        .await;
        let credential = OAuthCredential {
            kind: "oauth".into(),
            access: fake_access_token("acct-old"),
            refresh: "refresh-old".into(),
            expires: 0,
            account_id: "acct-old".into(),
            available_model_ids: None,
        };
        let error = refresh(&credential, &test_config(base))
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("must-not-leak"));
        server.await.unwrap();
    }
}
