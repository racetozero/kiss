//! MCP OAuth 2.1 orchestration and persistent credential storage.

use crate::config::{ClientRegistration, OAuthConfig, OAuthGrantType};
use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationRequest,
    AuthorizationSession, ClientCredentialsConfig, CredentialStore, OAuthState, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:3118/callback";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CredentialFile {
    servers: BTreeMap<String, CredentialEnvelope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialEnvelope {
    server_url: String,
    credentials: StoredCredentials,
}

#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    server_name: String,
    server_url: String,
}

impl FileCredentialStore {
    pub fn discover(server_name: &str, server_url: &str) -> Result<Self> {
        let path = kiss_ai::paths::agent_dir()
            .context("no home directory")?
            .join("mcp-oauth.json");
        Ok(Self::new(path, server_name, server_url))
    }

    pub fn new(
        path: PathBuf,
        server_name: impl Into<String>,
        server_url: impl Into<String>,
    ) -> Self {
        Self {
            path,
            server_name: server_name.into(),
            server_url: server_url.into(),
        }
    }

    /// Read, change and write the file under its lock, so a save for one
    /// server cannot drop the credentials another server saved meanwhile.
    /// `change` returns its result and whether it changed the file.
    fn update<T>(
        &self,
        change: impl FnOnce(&mut CredentialFile) -> (T, bool),
    ) -> std::result::Result<T, AuthError> {
        kiss_ai::config_file::with_lock(&self.path, || {
            let mut file: CredentialFile = kiss_ai::config_file::read_json(&self.path)?;
            let (result, changed) = change(&mut file);
            if changed {
                kiss_ai::config_file::write_private_json(&self.path, &file)?;
            }
            Ok(result)
        })
        .map_err(|error| AuthError::InternalError(format!("{error:#}")))
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        self.update(|file| match file.servers.get(&self.server_name) {
            None => (None, false),
            Some(envelope) if envelope.server_url == self.server_url => {
                (Some(envelope.credentials.clone()), false)
            }
            // Credentials saved for another URL must not be sent to this one.
            Some(_) => {
                file.servers.remove(&self.server_name);
                (None, true)
            }
        })
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        self.update(|file| {
            file.servers.insert(
                self.server_name.clone(),
                CredentialEnvelope {
                    server_url: self.server_url.clone(),
                    credentials,
                },
            );
            ((), true)
        })
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        self.update(|file| ((), file.servers.remove(&self.server_name).is_some()))
    }
}

pub struct PendingLogin {
    state: OAuthState,
    pub authorization_url: String,
    pub redirect_uri: String,
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingLogin")
            .field("authorization_url", &self.authorization_url)
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

pub async fn begin_login(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    challenge: Option<&str>,
) -> Result<PendingLogin> {
    begin_login_with_path(server_name, server_url, oauth, challenge, None).await
}

async fn begin_login_with_path(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    challenge: Option<&str>,
    credential_path: Option<PathBuf>,
) -> Result<PendingLogin> {
    if oauth.grant_type != OAuthGrantType::AuthorizationCode {
        bail!("MCP server `{server_name}` does not use authorization_code login")
    }
    let redirect_uri = oauth
        .redirect_uri
        .clone()
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string());
    let mut manager = base_manager(server_name, server_url, oauth, credential_path).await?;
    let metadata = match &oauth.auth_server_metadata_url {
        Some(url) => fetch_metadata(server_name, url).await?,
        None => {
            manager
                .resolve_metadata_from_challenge(challenge)
                .await
                .context("discover the MCP OAuth authorization server")?
                .metadata
        }
    };
    // KISS opens the authorization URL with the platform URL handler, which
    // would also run `file:` URLs and custom protocol handlers.
    for (field, url) in [
        (
            "authorization_endpoint",
            Some(&metadata.authorization_endpoint),
        ),
        ("token_endpoint", Some(&metadata.token_endpoint)),
        (
            "registration_endpoint",
            metadata.registration_endpoint.as_ref(),
        ),
    ] {
        if let Some(url) = url
            && !url::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        {
            bail!(
                "the authorization server of MCP server `{server_name}` sent {field} {url:?}, which is not an http or https URL. KISS does not open it. Ask the server owner to fix the OAuth metadata, or set oauth.authServerMetadataUrl to a correct document"
            )
        }
    }
    let mut request = AuthorizationRequest::new(&redirect_uri)
        .with_client_name(oauth.client_name.as_deref().unwrap_or("KISS MCP Client"));
    let cimd_supported = metadata
        .additional_fields
        .get("client_id_metadata_document_supported")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let use_cimd = match oauth.client_registration {
        // rmcp silently falls back to dynamic registration without support.
        ClientRegistration::Cimd if !cimd_supported => bail!(
            "the authorization server of MCP server `{server_name}` does not support Client ID Metadata Documents. Set oauth.clientRegistration to \"auto\" or \"dcr\" to use dynamic client registration"
        ),
        ClientRegistration::Cimd => true,
        ClientRegistration::Auto => {
            cimd_supported && oauth.client_name.is_none() && oauth.redirect_uri.is_none()
        }
        ClientRegistration::Dcr => false,
    };
    if use_cimd && let Some(url) = &oauth.client_metadata_url {
        request = request.with_client_metadata_url(url);
    }
    manager.set_metadata(metadata);
    if let Some(scopes) = &oauth.scope {
        request = request.with_scopes(scopes.split_whitespace());
    }
    if let Some(client_id) = &oauth.client_id {
        request = request.with_preregistered_client(client_id);
    }
    if let Some(client_secret) = &oauth.client_secret {
        request = request.with_client_secret(client_secret);
    }
    // Build the session directly. `OAuthState::start_authorization` would
    // discover the metadata again and replace a configured document.
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, error)| error)
        .context("start MCP OAuth authorization")?;
    let state = OAuthState::Session(session);
    let authorization_url = add_authorization_params(
        &state.get_authorization_url().await?,
        &oauth.authorization_params,
    )?;
    Ok(PendingLogin {
        state,
        authorization_url,
        redirect_uri,
    })
}

pub async fn finish_login(mut pending: PendingLogin, callback_url: &str) -> Result<()> {
    pending
        .state
        .handle_callback_url(callback_url.trim())
        .await
        .context("complete MCP OAuth authorization")
}

pub async fn login_client_credentials(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
) -> Result<()> {
    if oauth.grant_type != OAuthGrantType::ClientCredentials {
        bail!("MCP server `{server_name}` does not use client_credentials")
    }
    let client_id = oauth
        .client_id
        .clone()
        .context("client_credentials needs clientId")?;
    let client_secret = oauth
        .client_secret
        .clone()
        .context("client_credentials needs clientSecret")?;
    let scopes = oauth
        .scope
        .as_deref()
        .map(|value| value.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    let mut state =
        OAuthState::Unauthorized(new_manager(server_name, server_url, oauth, None).await?);
    state
        .authenticate_client_credentials(ClientCredentialsConfig::ClientSecret {
            client_id,
            client_secret,
            scopes,
            resource: Some(server_url.to_string()),
        })
        .await
        .context("authenticate MCP client credentials")
}

pub async fn new_manager(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    credential_path: Option<PathBuf>,
) -> Result<AuthorizationManager> {
    let mut manager = base_manager(server_name, server_url, oauth, credential_path).await?;
    if let Some(url) = &oauth.auth_server_metadata_url {
        manager.set_metadata(fetch_metadata(server_name, url).await?);
    }
    Ok(manager)
}

async fn base_manager(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    credential_path: Option<PathBuf>,
) -> Result<AuthorizationManager> {
    crate::ensure_tls_crypto_provider();
    let mut manager = AuthorizationManager::new(server_url)
        .await
        .context("create MCP OAuth manager")?;
    manager.set_allow_missing_issuer(oauth.skip_issuer_metadata_validation);
    let store = match credential_path {
        Some(path) => FileCredentialStore::new(path, server_name, server_url),
        None => FileCredentialStore::discover(server_name, server_url)?,
    };
    manager.set_credential_store(store);
    Ok(manager)
}

/// Fetch a configured RFC 8414 or OpenID Connect metadata document. It is
/// trusted as configured, so its issuer is not compared with the server URL.
async fn fetch_metadata(server_name: &str, url: &str) -> Result<AuthorizationMetadata> {
    let response = kiss_ai::stream::http_client()
        .get(url)
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .with_context(|| {
            format!("fetch oauth.authServerMetadataUrl {url} for MCP server `{server_name}`")
        })?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "oauth.authServerMetadataUrl {url} for MCP server `{server_name}` returned HTTP {status}. Check the URL of the authorization server metadata document"
        )
    }
    response.json().await.with_context(|| {
        format!(
            "oauth.authServerMetadataUrl {url} for MCP server `{server_name}` is not an authorization server metadata document with authorization_endpoint and token_endpoint"
        )
    })
}

pub async fn has_credentials(server_name: &str, server_url: &str) -> Result<bool> {
    Ok(FileCredentialStore::discover(server_name, server_url)?
        .load()
        .await?
        .is_some())
}

pub async fn logout(server_name: &str, server_url: &str) -> Result<bool> {
    let store = FileCredentialStore::discover(server_name, server_url)?;
    let existed = store.load().await?.is_some();
    store.clear().await?;
    Ok(existed)
}

fn add_authorization_params(url: &str, params: &BTreeMap<String, String>) -> Result<String> {
    if params.is_empty() {
        return Ok(url.to_string());
    }
    let reserved: BTreeSet<&str> = [
        "client_id",
        "redirect_uri",
        "response_type",
        "scope",
        "state",
        "code_challenge",
        "code_challenge_method",
        "resource",
    ]
    .into_iter()
    .collect();
    let mut parsed = url::Url::parse(url)?;
    {
        let mut query = parsed.query_pairs_mut();
        for (key, value) in params {
            if reserved.contains(key.as_str()) {
                bail!("OAuth authorizationParams cannot replace flow-owned `{key}`")
            }
            query.append_pair(key, value);
        }
    }
    Ok(parsed.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_util::sync::CancellationToken;

    fn credentials() -> StoredCredentials {
        StoredCredentials::new("client".to_string(), None, vec![], None)
    }

    #[tokio::test]
    async fn credential_store_is_bound_to_server_url() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let first = FileCredentialStore::new(path.clone(), "demo", "https://one.example/mcp");
        first.save(credentials()).await.unwrap();
        assert!(first.load().await.unwrap().is_some());

        let changed = FileCredentialStore::new(path, "demo", "https://two.example/mcp");
        assert!(changed.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_clears_only_one_server() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let first = FileCredentialStore::new(path.clone(), "one", "https://one.example/mcp");
        let second = FileCredentialStore::new(path, "two", "https://two.example/mcp");
        first.save(credentials()).await.unwrap();
        second.save(credentials()).await.unwrap();
        first.clear().await.unwrap();
        assert!(first.load().await.unwrap().is_none());
        assert!(second.load().await.unwrap().is_some());
    }

    #[test]
    fn provider_params_cannot_replace_pkce_state() {
        let mut params = BTreeMap::new();
        params.insert("state".to_string(), "attacker".to_string());
        assert!(add_authorization_params("https://example.com/auth?state=safe", &params).is_err());
    }

    async fn start_oauth_server() -> (String, CancellationToken) {
        crate::ensure_tls_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let task_base = base.clone();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = task_cancel.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _)) = accepted else {
                    break;
                };
                let base = task_base.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 4096];
                    let header_end = loop {
                        let count = socket.read(&mut buffer).await.unwrap_or(0);
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        if let Some(position) =
                            request.windows(4).position(|item| item == b"\r\n\r\n")
                        {
                            break position + 4;
                        }
                    };
                    let request_head = String::from_utf8_lossy(&request[..header_end]);
                    let target = request_head
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, content_type, body) =
                        if target == "/.well-known/oauth-authorization-server" {
                            (
                                "200 OK",
                                "application/json",
                                serde_json::to_vec(&json!({
                                    "issuer": base,
                                    "authorization_endpoint": format!("{base}/authorize"),
                                    "token_endpoint": format!("{base}/token"),
                                    "response_types_supported": ["code"],
                                    "code_challenge_methods_supported": ["S256"]
                                }))
                                .unwrap(),
                            )
                        } else if target == "/configured-metadata" {
                            (
                            "200 OK",
                            "application/json",
                            serde_json::to_vec(&json!({
                                "issuer": base,
                                "authorization_endpoint": format!("{base}/configured-authorize"),
                                "token_endpoint": format!("{base}/token"),
                                "response_types_supported": ["code"],
                                "code_challenge_methods_supported": ["S256"],
                                "client_id_metadata_document_supported": true
                            }))
                            .unwrap(),
                        )
                        } else if target == "/file-metadata" {
                            (
                                "200 OK",
                                "application/json",
                                serde_json::to_vec(&json!({
                                    "issuer": base,
                                    "authorization_endpoint": "file:///Applications/Calculator.app",
                                    "token_endpoint": format!("{base}/token"),
                                    "response_types_supported": ["code"],
                                    "code_challenge_methods_supported": ["S256"]
                                }))
                                .unwrap(),
                            )
                        } else if target == "/token" {
                            (
                                "200 OK",
                                "application/json",
                                serde_json::to_vec(&json!({
                                    "access_token": "local-access-token",
                                    "token_type": "Bearer",
                                    "expires_in": 3600,
                                    "refresh_token": "local-refresh-token"
                                }))
                                .unwrap(),
                            )
                        } else {
                            ("404 Not Found", "text/plain", b"not found".to_vec())
                        };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (base, cancel)
    }

    #[tokio::test]
    async fn login_refuses_oauth_metadata_urls_that_are_not_web_urls() {
        let temp = tempfile::tempdir().unwrap();
        let (base, server_cancel) = start_oauth_server().await;
        let oauth = OAuthConfig {
            client_id: Some("kiss-test-client".to_string()),
            auth_server_metadata_url: Some(format!("{base}/file-metadata")),
            ..Default::default()
        };
        let error = begin_login_with_path(
            "demo",
            &format!("{base}/mcp"),
            &oauth,
            None,
            Some(temp.path().join("oauth.json")),
        )
        .await
        .err()
        .unwrap();
        let error = format!("{error:#}");
        assert!(error.contains("authorization_endpoint"), "{error}");
        assert!(error.contains("not an http or https URL"), "{error}");
        server_cancel.cancel();
    }

    #[tokio::test]
    async fn authorization_code_flow_uses_pkce_rejects_wrong_state_and_saves_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let (base, server_cancel) = start_oauth_server().await;
        let server_url = format!("{base}/mcp");
        let oauth = OAuthConfig {
            client_id: Some("kiss-test-client".to_string()),
            scope: Some("tools.read".to_string()),
            ..Default::default()
        };

        let wrong = begin_login_with_path("demo", &server_url, &oauth, None, Some(path.clone()))
            .await
            .unwrap();
        let authorization = url::Url::parse(&wrong.authorization_url).unwrap();
        let params = authorization.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
        assert!(!params.get("code_challenge").unwrap().is_empty());
        assert_eq!(params.get("scope").unwrap(), "tools.read");
        let wrong_callback = format!("{}?code=local-code&state=wrong-state", wrong.redirect_uri);
        assert!(finish_login(wrong, &wrong_callback).await.is_err());

        let pending = begin_login_with_path("demo", &server_url, &oauth, None, Some(path.clone()))
            .await
            .unwrap();
        let authorization = url::Url::parse(&pending.authorization_url).unwrap();
        let state = authorization
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let issuer = url::form_urlencoded::byte_serialize(base.as_bytes()).collect::<String>();
        let callback = format!(
            "{}?code=local-code&state={state}&iss={issuer}",
            pending.redirect_uri
        );
        finish_login(pending, &callback).await.unwrap();

        let store = FileCredentialStore::new(path, "demo", &server_url);
        let saved = store.load().await.unwrap().unwrap();
        assert!(saved.token_response.is_some());
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        server_cancel.cancel();
    }

    #[tokio::test]
    async fn configured_metadata_and_client_metadata_document_drive_login() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let (base, server_cancel) = start_oauth_server().await;
        let server_url = format!("{base}/mcp");
        let document = "https://example.com/kiss/client.json";

        // Discovered metadata does not advertise CIMD support.
        let cimd = OAuthConfig {
            client_registration: ClientRegistration::Cimd,
            client_metadata_url: Some(document.to_string()),
            ..Default::default()
        };
        let error = begin_login_with_path("demo", &server_url, &cimd, None, Some(path.clone()))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("does not support Client ID Metadata Documents"),
            "{error}"
        );

        // The configured document replaces discovery and supports CIMD.
        let configured = OAuthConfig {
            auth_server_metadata_url: Some(format!("{base}/configured-metadata")),
            ..cimd
        };
        // `auto`, the default, uses a supported document the same way.
        for configured in [
            configured.clone(),
            OAuthConfig {
                client_registration: ClientRegistration::Auto,
                ..configured
            },
        ] {
            let pending =
                begin_login_with_path("demo", &server_url, &configured, None, Some(path.clone()))
                    .await
                    .unwrap();
            let authorization = url::Url::parse(&pending.authorization_url).unwrap();
            assert_eq!(authorization.path(), "/configured-authorize");
            let params = authorization.query_pairs().collect::<BTreeMap<_, _>>();
            assert_eq!(params.get("client_id").unwrap(), document);
        }
        server_cancel.cancel();
    }
}
