//! MCP OAuth 2.1 orchestration and persistent credential storage.

use crate::config::{OAuthConfig, OAuthGrantType};
use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use fs2::FileExt as _;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, ClientCredentialsConfig,
    CredentialStore, OAuthState, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

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
        let path = dirs::home_dir()
            .context("no home directory")?
            .join(".kiss/agent/mcp-oauth.json");
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

    fn read(&self) -> Result<CredentialFile> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CredentialFile::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.path.display()));
            }
        };
        serde_json::from_str(&text).with_context(|| format!("parse {}", self.path.display()))
    }

    fn write(&self, credentials: &CredentialFile) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("credential path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let lock_path = self.path.with_extension("json.lock");
        let lock = secure_open(&lock_path)?;
        lock.lock_exclusive()?;
        let temporary = self
            .path
            .with_extension(format!("json.tmp.{}", std::process::id()));
        let result = (|| -> Result<()> {
            let mut output = secure_create(&temporary)?;
            serde_json::to_writer_pretty(&mut output, credentials)?;
            output.write_all(b"\n")?;
            output.sync_all()?;
            std::fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        let _ = std::fs::remove_file(temporary);
        let _ = fs2::FileExt::unlock(&lock);
        result
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        let mut file = self
            .read()
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        let Some(envelope) = file.servers.get(&self.server_name) else {
            return Ok(None);
        };
        if envelope.server_url == self.server_url {
            return Ok(Some(envelope.credentials.clone()));
        }
        file.servers.remove(&self.server_name);
        self.write(&file)
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        Ok(None)
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let mut file = self
            .read()
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        file.servers.insert(
            self.server_name.clone(),
            CredentialEnvelope {
                server_url: self.server_url.clone(),
                credentials,
            },
        );
        self.write(&file)
            .map_err(|error| AuthError::InternalError(error.to_string()))
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        let mut file = self
            .read()
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        if file.servers.remove(&self.server_name).is_some() {
            self.write(&file)
                .map_err(|error| AuthError::InternalError(error.to_string()))?;
        }
        Ok(())
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
    let mut state = OAuthState::Unauthorized(
        new_manager(server_name, server_url, oauth, credential_path).await?,
    );
    let mut request = AuthorizationRequest::new(&redirect_uri)
        .with_client_name(oauth.client_name.as_deref().unwrap_or("KISS MCP Client"));
    if let Some(scopes) = &oauth.scope {
        request = request.with_scopes(scopes.split_whitespace());
    }
    if let Some(client_id) = &oauth.client_id {
        request = request.with_preregistered_client(client_id);
    }
    if let Some(client_secret) = &oauth.client_secret {
        request = request.with_client_secret(client_secret);
    }
    if let Some(challenge) = challenge {
        request = request.with_challenge(challenge);
    }
    state
        .start_authorization(request)
        .await
        .context("start MCP OAuth authorization")?;
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

fn secure_open(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn secure_create(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
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
}
