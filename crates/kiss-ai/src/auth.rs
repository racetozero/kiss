//! Provider credentials. Stored credentials override environment variables,
//! and custom catalog placeholders are the last fallback.

use crate::ResolvedCredential;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

pub mod accounts;
pub mod anthropic;
pub mod anthropic_federation;
mod azure;
pub mod cursor;
mod device_code;
pub mod external;
pub mod github_copilot;
pub mod kimi_coding;
pub mod meta;
pub mod openai_codex;
pub mod openrouter;
pub mod radius;
pub mod xai;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredential {
    #[serde(rename = "type")]
    pub kind: String,
    pub access: String,
    pub refresh: String,
    /// Expiration time as Unix milliseconds.
    pub expires: i64,
    #[serde(default)]
    pub account_id: String,
    /// Issued public client ID for OpenAI's direct ChatGPT token flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_model_ids: Option<Vec<String>>,
}

impl OAuthCredential {
    pub fn is_expired(&self) -> bool {
        // Refresh one minute early so a token does not expire during a turn.
        self.expires <= chrono::Utc::now().timestamp_millis() + 60_000
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(flatten)]
    entries: BTreeMap<String, AuthEntry>,
}

#[derive(Default)]
struct AuthCache {
    path: Option<PathBuf>,
    signature: Option<(u64, SystemTime)>,
    value: AuthFile,
}

static AUTH_CACHE: OnceLock<Mutex<AuthCache>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum AuthEntry {
    Key(String),
    Detailed {
        key: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    OAuth(OAuthCredential),
}

impl AuthEntry {
    fn access_token(&self) -> &str {
        match self {
            AuthEntry::Key(key) | AuthEntry::Detailed { key, .. } => key,
            AuthEntry::OAuth(credential) => &credential.access,
        }
    }
}

/// Return true only when the resolved secret came from an OAuth source.
/// This avoids identifying credentials from a token prefix.
pub fn is_oauth_access_token(provider: &str, access_token: &str) -> bool {
    let file = read_auth_file();
    if matches!(
        file.entries.get(provider),
        Some(AuthEntry::OAuth(credential)) if credential.access == access_token
    ) {
        return true;
    }
    (provider == "cursor"
        && std::env::var("CURSOR_ACCESS_TOKEN")
            .is_ok_and(|value| !value.is_empty() && value == access_token))
        || provider == "anthropic"
            && std::env::var("ANTHROPIC_OAUTH_TOKEN")
                .is_ok_and(|value| !value.is_empty() && value == access_token)
}

pub fn is_bearer_access_token(provider: &str, access_token: &str) -> bool {
    matches!(provider, "databricks-unity-gateway" | "snowflake-cortex")
        || (provider == "anthropic" && access_token.starts_with(anthropic_federation::TOKEN_PREFIX))
        || (provider == "anthropic"
            && std::env::var("ANTHROPIC_AUTH_TOKEN")
                .is_ok_and(|value| !value.is_empty() && value == access_token))
        || (provider != "anthropic" && is_oauth_access_token(provider, access_token))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredAuthKind {
    ApiKey,
    AzureEntraId,
    OAuth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    BrowserOAuth,
    DeviceOAuth,
    ManualOAuth,
    ApiKey,
    AzureEntraId,
    GoogleApplicationDefault,
    AwsProfile,
    AwsAmbient,
}

impl LoginMethod {
    pub fn label(self) -> &'static str {
        match self {
            LoginMethod::BrowserOAuth => "Sign in with browser",
            LoginMethod::DeviceOAuth => "Sign in with device code",
            LoginMethod::ManualOAuth => "Sign in and paste callback",
            LoginMethod::ApiKey => "Enter API key",
            LoginMethod::AzureEntraId => "Microsoft Entra ID",
            LoginMethod::GoogleApplicationDefault => "Google Application Default Credentials",
            LoginMethod::AwsProfile => "AWS profile",
            LoginMethod::AwsAmbient => "AWS default credential chain",
        }
    }
}

/// Authentication methods that the selected provider can use.
pub fn login_methods(provider: &str) -> Vec<LoginMethod> {
    use LoginMethod::*;
    match provider {
        "openai-codex" => vec![BrowserOAuth, DeviceOAuth],
        "openai" => vec![BrowserOAuth, ApiKey],
        "anthropic" => vec![BrowserOAuth, ManualOAuth, ApiKey],
        "cursor" => vec![BrowserOAuth, ApiKey],
        "github-copilot" | "kimi-coding" | "meta" | "xai" => vec![DeviceOAuth, ApiKey],
        "openrouter" => vec![BrowserOAuth, ManualOAuth, ApiKey],
        "radius" => vec![BrowserOAuth, DeviceOAuth, ApiKey],
        azure::PROVIDER => vec![ApiKey, AzureEntraId],
        "google-vertex" => vec![ApiKey, GoogleApplicationDefault],
        "amazon-bedrock" => vec![ApiKey, AwsProfile, AwsAmbient],
        _ => vec![ApiKey],
    }
}

pub fn env_var_names(provider: &str) -> &'static [&'static str] {
    match provider {
        "amazon-bedrock" => &["AWS_BEARER_TOKEN_BEDROCK"],
        "ant-ling" => &["ANT_LING_API_KEY"],
        "anthropic" => &[
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ],
        azure::PROVIDER => azure::env_var_names(),
        "baseten" => &["BASETEN_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "cloudflare-ai-gateway" => &["CLOUDFLARE_AI_GATEWAY_API_KEY"],
        "cloudflare-workers-ai" => &[
            "CLOUDFLARE_API_TOKEN",
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_AUTH_TOKEN",
        ],
        "cursor" => &["CURSOR_ACCESS_TOKEN"],
        "databricks-unity-gateway" => &["DATABRICKS_TOKEN"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        "google" => &["GEMINI_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "llama.cpp" => &["LLAMA_API_KEY"],
        "liquid" => &["LIQUID_API_KEY"],
        "meta" => &["META_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "moonshotai" => &["MOONSHOT_API_KEY"],
        "moonshotai-cn" => &["MOONSHOT_API_KEY"],
        "nvidia" => &["NVIDIA_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "opencode" => &["OPENCODE_API_KEY"],
        "opencode-go" => &["OPENCODE_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "qwen-token-plan" => &["QWEN_TOKEN_PLAN_API_KEY"],
        "qwen-token-plan-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        "qwen-token-plan-individual" => &["QWEN_TOKEN_PLAN_API_KEY"],
        "radius" => &["RADIUS_API_KEY"],
        "snowflake-cortex" => &["SNOWFLAKE_PAT", "SNOWFLAKE_TOKEN"],
        "together" => &["TOGETHER_API_KEY"],
        "typesafe" => &["TYPESAFE_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "xiaomi" => &["XIAOMI_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"],
        _ => &[],
    }
}

/// Check a URL that a sign-in server sent before KISS opens or prints it.
/// The platform URL handler would also run `file:` URLs and custom protocol
/// handlers, so only `http` and `https` are accepted. Parsing percent-encodes
/// control characters, so the result is safe in a terminal hyperlink.
pub(crate) fn trusted_http_url(value: &str, server: &str) -> Result<String> {
    match url::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(url.into()),
        _ => anyhow::bail!(
            "{server} sent the sign-in URL {value:?}, which is not an http or https URL. KISS does not open it. Try the login again later, or report the problem to {server}"
        ),
    }
}

pub fn auth_file_path() -> Option<PathBuf> {
    crate::paths::agent_dir().map(|dir| dir.join("auth.json"))
}

/// Read `auth.json`. An unreadable or invalid file is an error, so writers
/// never replace logins they could not parse.
fn read_auth_file_at(path: &Path) -> Result<AuthFile> {
    let mut auth: AuthFile = crate::config_file::read_json(path)?;
    // Logins saved under an old provider ID belong to the current ID.
    // The next write saves them under the current ID.
    for (old, new) in crate::registry::RENAMED_PROVIDER_IDS {
        if let Some(entry) = auth.entries.remove(*old) {
            auth.entries.entry((*new).into()).or_insert(entry);
        }
    }
    Ok(auth)
}

fn auth_file_signature(path: &Path) -> Option<(u64, SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

fn read_auth_file_cached_at(path: &Path) -> AuthFile {
    let signature = auth_file_signature(path);
    let cache = AUTH_CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap();
    if cache.path.as_deref() == Some(path) && cache.signature == signature {
        return cache.value.clone();
    }
    let value = read_auth_file_at(path).unwrap_or_default();
    cache.path = Some(path.to_path_buf());
    cache.signature = signature;
    cache.value = value.clone();
    value
}

fn update_auth_cache(path: &Path, value: &AuthFile) {
    let cache = AUTH_CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap();
    cache.path = Some(path.to_path_buf());
    cache.signature = auth_file_signature(path);
    cache.value = value.clone();
}

fn read_auth_file() -> AuthFile {
    auth_file_path()
        .as_deref()
        .map(read_auth_file_cached_at)
        .unwrap_or_default()
}

fn write_auth_file_at(path: &Path, auth: &AuthFile) -> Result<()> {
    crate::config_file::write_private_json(path, auth)?;
    update_auth_cache(path, auth);
    Ok(())
}

fn update_auth_file_at<T>(
    path: &Path,
    update: impl FnOnce(&mut AuthFile) -> Result<T>,
) -> Result<T> {
    crate::config_file::with_lock(path, || {
        accounts::recover_at(path)?;
        accounts::recover_add_at(path)?;
        let mut auth = read_auth_file_at(path)?;
        let result = update(&mut auth)?;
        write_auth_file_at(path, &auth)?;
        Ok(result)
    })
}

fn update_auth_file<T>(update: impl FnOnce(&mut AuthFile) -> Result<T>) -> Result<T> {
    let path = auth_file_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    update_auth_file_at(&path, update)
}

/// Resolve a credential without network access. An OAuth access token is
/// returned even when it needs refresh. Use `resolve_api_key_async` before an
/// API request.
pub fn resolve_api_key_local(
    provider: &str,
    declared: &BTreeMap<String, String>,
) -> Option<String> {
    resolve_credential_local(provider, declared).map(ResolvedCredential::into_value)
}

/// Whether the provider can be used without asking the user: a local secret
/// resolves, or a login (Microsoft Entra ID) that obtains its token at
/// request time is saved.
pub fn is_signed_in_local(provider: &str, declared: &BTreeMap<String, String>) -> bool {
    resolve_credential_local(provider, declared).is_some()
        || (provider == azure::PROVIDER
            && read_auth_file()
                .entries
                .get(provider)
                .is_some_and(azure::is_entra_marker))
}

pub fn resolve_credential_local(
    provider: &str,
    declared: &BTreeMap<String, String>,
) -> Option<ResolvedCredential> {
    if provider == azure::PROVIDER {
        return azure::resolve_local(declared);
    }
    let file = read_auth_file();
    if let Some(entry) = file.entries.get(provider) {
        return Some(match entry {
            AuthEntry::Key(key) | AuthEntry::Detailed { key, .. } => {
                if matches!(provider, "databricks-unity-gateway" | "snowflake-cortex") {
                    ResolvedCredential::bearer(key.clone())
                } else {
                    ResolvedCredential::api_key(key.clone())
                }
            }
            AuthEntry::OAuth(credential) => ResolvedCredential::bearer(&credential.access),
        });
    }
    for variable in env_var_names(provider) {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
        {
            return Some(
                if matches!(provider, "databricks-unity-gateway" | "snowflake-cortex") {
                    ResolvedCredential::bearer(value)
                } else {
                    ResolvedCredential::api_key(value)
                },
            );
        }
    }
    if provider == "google-vertex" && has_google_application_credentials() {
        return Some(ResolvedCredential::api_key(
            "google-application-default-credentials",
        ));
    }
    if provider == "anthropic" && anthropic_federation::configured() {
        return Some(ResolvedCredential::bearer(anthropic_federation::MARKER));
    }
    declared.get(provider).and_then(|value| {
        value
            .strip_prefix('$')
            .map(|variable| {
                std::env::var(variable)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(|value| {
                        if matches!(provider, "databricks-unity-gateway" | "snowflake-cortex") {
                            ResolvedCredential::bearer(value)
                        } else {
                            ResolvedCredential::api_key(value)
                        }
                    })
            })
            .unwrap_or_else(|| {
                Some(
                    if matches!(provider, "databricks-unity-gateway" | "snowflake-cortex") {
                        ResolvedCredential::bearer(value.clone())
                    } else {
                        ResolvedCredential::api_key(value.clone())
                    },
                )
            })
    })
}

/// Resolve a local credential, then try one automatic external import.
pub fn resolve_api_key(provider: &str, declared: &BTreeMap<String, String>) -> Option<String> {
    if let Some(key) =
        resolve_credential_local(provider, declared).map(ResolvedCredential::into_value)
    {
        return Some(key);
    }
    if external::auto_import_unique(provider)
        .ok()
        .flatten()
        .is_some()
    {
        return resolve_api_key_local(provider, declared);
    }
    None
}

/// Resolve a credential and refresh an expired provider OAuth token.
pub async fn resolve_credential_async(
    provider: &str,
    declared: &BTreeMap<String, String>,
) -> Result<Option<ResolvedCredential>> {
    if provider == azure::PROVIDER {
        return azure::resolve_async(declared).await;
    }
    let value = resolve_api_key_async_generic(provider, declared).await?;
    Ok(value.map(|value| {
        if is_bearer_access_token(provider, &value) {
            ResolvedCredential::bearer(value)
        } else {
            ResolvedCredential::api_key(value)
        }
    }))
}

pub async fn resolve_api_key_async(
    provider: &str,
    declared: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    Ok(resolve_credential_async(provider, declared)
        .await?
        .map(ResolvedCredential::into_value))
}

async fn resolve_api_key_async_generic(
    provider: &str,
    declared: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let mut file = read_auth_file();
    if let Some(entry) = file.entries.get(provider).cloned() {
        match entry {
            AuthEntry::Detailed { key, .. }
                if provider == "google-vertex"
                    && key == "google-application-default-credentials" =>
            {
                return Ok(Some(google_application_access_token().await?));
            }
            AuthEntry::OAuth(credential)
                if matches!(
                    provider,
                    "openai"
                        | "openai-codex"
                        | "anthropic"
                        | "github-copilot"
                        | "kimi-coding"
                        | "meta"
                        | "xai"
                        | "radius"
                        | "cursor"
                ) && credential.is_expired() =>
            {
                // One refresh at a time for each provider, so a stalled
                // refresh does not hold up the others.
                type Locks = Mutex<BTreeMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>;
                static REFRESH_LOCKS: OnceLock<Locks> = OnceLock::new();
                let lock = REFRESH_LOCKS
                    .get_or_init(Default::default)
                    .lock()
                    .unwrap()
                    .entry(provider.to_string())
                    .or_default()
                    .clone();
                let _guard = lock.lock().await;
                // Another request can complete the refresh while this request
                // waits for the lock. Read the file again before network I/O.
                file = read_auth_file();
                let current = match file.entries.get(provider).cloned() {
                    Some(AuthEntry::OAuth(current)) => current,
                    Some(other) => return Ok(Some(other.access_token().to_string())),
                    None => credential,
                };
                if !current.is_expired() {
                    return Ok(Some(current.access));
                }
                let refresh = async {
                    anyhow::Ok(match provider {
                        "openai-codex" => {
                            openai_codex::refresh(&current, &Default::default()).await?
                        }
                        "openai" => {
                            openai_codex::refresh(
                                &current,
                                &openai_codex::OAuthConfig::chatgpt(None),
                            )
                            .await?
                        }
                        "anthropic" => anthropic::refresh(&current, &Default::default()).await?,
                        "github-copilot" => {
                            github_copilot::refresh(&current, &Default::default()).await?
                        }
                        "kimi-coding" => {
                            kimi_coding::refresh(&current, &Default::default()).await?
                        }
                        "meta" => meta::refresh(&current, &Default::default()).await?,
                        "xai" => xai::refresh(&current, &Default::default()).await?,
                        "radius" => radius::refresh(&current, &Default::default()).await?,
                        "cursor" => cursor::refresh(&current, &Default::default()).await?,
                        _ => unreachable!(),
                    })
                };
                // A refresh that never answers must not hold every request
                // for this provider.
                let refreshed = tokio::time::timeout(std::time::Duration::from_secs(30), refresh)
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "the {provider} token refresh did not answer within 30 seconds. Check your connection, or sign in again with `kiss login {provider}`"
                        )
                    })??;
                let access = refreshed.access.clone();
                let path = auth_file_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
                accounts::save_refreshed_at(
                    &path,
                    provider,
                    &AuthEntry::OAuth(current),
                    AuthEntry::OAuth(refreshed),
                )?;
                return Ok(Some(access));
            }
            _ => {}
        }
    }
    // The sources and their order are the local resolver's, so a provider
    // that counts as signed in sends the same credential. Only the markers
    // that stand for a token fetched at request time are replaced here.
    let local = match resolve_credential_local(provider, declared) {
        Some(credential) => Some(credential),
        None if external::auto_import_unique(provider)?.is_some() => {
            resolve_credential_local(provider, declared)
        }
        None => None,
    };
    Ok(match local.map(ResolvedCredential::into_value) {
        Some(value)
            if provider == "google-vertex" && value == "google-application-default-credentials" =>
        {
            Some(google_application_access_token().await?)
        }
        Some(value) if provider == "anthropic" && value == anthropic_federation::MARKER => {
            Some(anthropic_federation::access_token().await?)
        }
        other => other,
    })
}

fn has_google_application_credentials() -> bool {
    let has_project = provider_env("google-vertex", "GOOGLE_CLOUD_PROJECT")
        .or_else(|| provider_env("google-vertex", "GCLOUD_PROJECT"))
        .is_some();
    let has_location = provider_env("google-vertex", "GOOGLE_CLOUD_LOCATION").is_some();
    if !has_project || !has_location {
        return false;
    }
    let path = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS")
        .map(PathBuf::from)
        .or_else(|| {
            let directory = if cfg!(windows) {
                std::env::var_os("APPDATA").map(PathBuf::from)
            } else {
                dirs::home_dir().map(|home| home.join(".config"))
            };
            directory.map(|directory| directory.join("gcloud/application_default_credentials.json"))
        });
    path.is_some_and(|path| path.is_file())
}

async fn google_application_access_token() -> Result<String> {
    let output = tokio::process::Command::new(if cfg!(windows) {
        "gcloud.cmd"
    } else {
        "gcloud"
    })
    .args(["auth", "application-default", "print-access-token"])
    .output()
    .await
    .context("run gcloud for Google Application Default Credentials")?;
    if !output.status.success() {
        anyhow::bail!("gcloud could not create a Google access token");
    }
    let token = String::from_utf8(output.stdout)?.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("gcloud returned an empty Google access token");
    }
    Ok(format!("vertex-oauth:{token}"))
}

pub fn stored_auth_kind(provider: &str) -> Option<StoredAuthKind> {
    match read_auth_file().entries.get(provider) {
        Some(AuthEntry::OAuth(_)) => Some(StoredAuthKind::OAuth),
        Some(entry) if provider == azure::PROVIDER && azure::is_entra_marker(entry) => {
            Some(StoredAuthKind::AzureEntraId)
        }
        Some(AuthEntry::Key(_) | AuthEntry::Detailed { .. }) => Some(StoredAuthKind::ApiKey),
        None => None,
    }
}

/// Account-specific model IDs returned by a provider OAuth login.
pub fn stored_oauth_model_ids(provider: &str) -> Option<Vec<String>> {
    match read_auth_file().entries.get(provider) {
        Some(AuthEntry::OAuth(credential)) => credential.available_model_ids.clone(),
        _ => None,
    }
}

pub fn stored_provider_ids() -> Vec<String> {
    read_auth_file().entries.into_keys().collect()
}

pub fn stored_credential_env(provider: &str) -> BTreeMap<String, String> {
    match read_auth_file().entries.get(provider) {
        Some(AuthEntry::Detailed { env, .. }) => env.clone(),
        _ => BTreeMap::new(),
    }
}

/// Read provider configuration from the process first, then the saved entry.
pub fn provider_env(provider: &str, name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            stored_credential_env(provider)
                .get(name)
                .filter(|value| !value.trim().is_empty())
                .cloned()
        })
}

pub fn store_api_key_with_env(
    provider: &str,
    key: &str,
    env: BTreeMap<String, String>,
) -> Result<()> {
    update_auth_file(|file| {
        file.entries.insert(
            provider.to_string(),
            AuthEntry::Detailed {
                key: key.to_string(),
                env,
            },
        );
        Ok(())
    })
}

pub fn store_gateway_credential(provider: &str, key: &str, base_url: &str) -> Result<()> {
    if key.trim().is_empty() {
        anyhow::bail!(
            "the {provider} token is empty. Enter a non-empty bearer token and retry login"
        );
    }
    let (variable, normalized) = normalize_gateway_base_url(provider, base_url)?;
    store_api_key_with_env(
        provider,
        key.trim(),
        BTreeMap::from([(variable.to_string(), normalized)]),
    )
}

fn normalize_gateway_base_url(provider: &str, base_url: &str) -> Result<(&'static str, String)> {
    let mut url = url::Url::parse(base_url.trim()).with_context(|| {
        format!(
            "the {provider} base URL '{base_url}' is not an absolute URL. Use an HTTP or HTTPS URL with a host"
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        anyhow::bail!(
            "the {provider} base URL '{base_url}' is not valid. Use an HTTP or HTTPS URL with a host"
        );
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!(
            "the {provider} base URL '{base_url}' contains credentials, a query, or a fragment. Remove those parts and retry login"
        );
    }

    let (variable, accepted) = match provider {
        "databricks-unity-gateway" => {
            if url.path() != "/" && !url.path().is_empty() {
                anyhow::bail!(
                    "the Databricks workspace URL '{base_url}' contains a path. Use only the workspace root, such as https://workspace.cloud.databricks.com, and retry login"
                );
            }
            url.set_path("");
            (
                "DATABRICKS_HOST",
                "an HTTP or HTTPS Databricks workspace root",
            )
        }
        "snowflake-cortex" => {
            let path = url.path().trim_end_matches('/').to_string();
            if path.is_empty() {
                url.set_path("/api/v2/cortex");
            } else if path == "/api/v2/cortex" {
                url.set_path(&path);
            } else if path.eq_ignore_ascii_case("/api/v2/aigateways/SNOWFLAKE") {
                url.set_path("/api/v2/aigateways/SNOWFLAKE");
            } else {
                anyhow::bail!(
                    "the Snowflake URL '{base_url}' has an unsupported path. Use the account root, /api/v2/cortex, or /api/v2/aigateways/SNOWFLAKE, and retry login"
                );
            }
            (
                "SNOWFLAKE_CORTEX_BASE_URL",
                "a Snowflake account root, Cortex root, or AI Gateway root",
            )
        }
        _ => anyhow::bail!(
            "provider '{provider}' does not use gateway URL login. Omit --base-url and use its supported login method"
        ),
    };
    let normalized = url.as_str().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        anyhow::bail!("the {provider} base URL is empty. Enter {accepted} and retry login");
    }
    Ok((variable, normalized))
}

pub fn store_api_key(provider: &str, key: &str) -> Result<()> {
    update_auth_file(|file| {
        file.entries
            .insert(provider.to_string(), AuthEntry::Key(key.to_string()));
        Ok(())
    })
}

pub fn store_azure_entra_id() -> Result<()> {
    store_api_key_with_env(azure::PROVIDER, azure::ENTRA_MARKER, BTreeMap::new())
}

pub fn store_oauth(provider: &str, credential: OAuthCredential) -> Result<()> {
    update_auth_file(|file| {
        file.entries
            .insert(provider.to_string(), AuthEntry::OAuth(credential));
        Ok(())
    })
}

/// Remove a stored credential. Returns true when an entry existed.
/// Every account in the provider's multi-account pool is removed as well.
pub fn remove_api_key(provider: &str) -> Result<bool> {
    let path = auth_file_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    accounts::forget_at(&path, provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_providers_use_standard_environment_variables() {
        assert_eq!(env_var_names("typesafe"), &["TYPESAFE_API_KEY"]);
        assert_eq!(login_methods("typesafe"), vec![LoginMethod::ApiKey]);
        assert_eq!(env_var_names("liquid"), &["LIQUID_API_KEY"]);
        assert_eq!(login_methods("liquid"), vec![LoginMethod::ApiKey]);
        assert!(env_var_names("cloudflare-workers-ai").contains(&"CLOUDFLARE_API_TOKEN"));
    }

    #[test]
    fn sign_in_urls_must_be_web_urls() {
        assert_eq!(
            trusted_http_url("https://github.com/login/device", "GitHub").unwrap(),
            "https://github.com/login/device"
        );
        // Control characters are percent-encoded, so the URL cannot end a
        // terminal hyperlink early.
        assert_eq!(
            trusted_http_url("https://example.com/a\u{1b}]8;;b", "Radius").unwrap(),
            "https://example.com/a%1B]8;;b"
        );
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "vscode://x",
            "not a url",
        ] {
            let error = trusted_http_url(url, "Radius").unwrap_err().to_string();
            assert!(error.contains("not an http or https URL"), "{error}");
        }
    }

    #[test]
    fn reads_old_and_new_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"old":"one","detailed":{"key":"two"},"oauth":{"type":"oauth","access":"three","refresh":"four","expires":9,"accountId":"acct"}}"#,
        )
        .unwrap();
        let auth = read_auth_file_at(&path).unwrap();
        assert_eq!(auth.entries["old"].access_token(), "one");
        assert_eq!(auth.entries["detailed"].access_token(), "two");
        assert_eq!(auth.entries["oauth"].access_token(), "three");

        // An Azure login saved under the old provider ID belongs to `azure`.
        std::fs::write(&path, r#"{"azure-openai-responses":"old-azure"}"#).unwrap();
        let auth = read_auth_file_at(&path).unwrap();
        assert_eq!(auth.entries["azure"].access_token(), "old-azure");
        assert!(!auth.entries.contains_key("azure-openai-responses"));
        std::fs::write(
            &path,
            r#"{"azure-openai-responses":"old-azure","azure":"new-azure"}"#,
        )
        .unwrap();
        assert_eq!(
            read_auth_file_at(&path).unwrap().entries["azure"].access_token(),
            "new-azure"
        );
    }

    #[test]
    fn atomic_write_replaces_file_and_keeps_valid_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent/auth.json");
        let mut auth = AuthFile::default();
        auth.entries
            .insert("openai".into(), AuthEntry::Key("secret".into()));
        write_auth_file_at(&path, &auth).unwrap();
        let saved = read_auth_file_at(&path).unwrap();
        assert_eq!(saved.entries["openai"].access_token(), "secret");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn cached_auth_read_reloads_after_an_external_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        std::fs::write(&path, r#"{"openai":"first"}"#).unwrap();
        assert_eq!(
            read_auth_file_cached_at(&path).entries["openai"].access_token(),
            "first"
        );

        std::fs::write(&path, r#"{"openai":"second-value"}"#).unwrap();
        assert_eq!(
            read_auth_file_cached_at(&path).entries["openai"].access_token(),
            "second-value"
        );
    }

    #[test]
    fn an_update_refuses_to_replace_an_auth_file_it_cannot_parse() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let unknown = r#"{"openai":{"type":"from-a-newer-kiss"}}"#;
        std::fs::write(&path, unknown).unwrap();
        let error = update_auth_file_at(&path, |auth| {
            auth.entries
                .insert("anthropic".into(), AuthEntry::Key("key".into()));
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("is not valid"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), unknown);
    }

    #[test]
    fn locked_updates_do_not_lose_other_provider_entries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent/auth.json");
        std::thread::scope(|scope| {
            for index in 0..12 {
                let path = path.clone();
                scope.spawn(move || {
                    update_auth_file_at(&path, |auth| {
                        auth.entries.insert(
                            format!("provider-{index}"),
                            AuthEntry::Key(format!("key-{index}")),
                        );
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        let auth = read_auth_file_at(&path).unwrap();
        assert_eq!(auth.entries.len(), 12);
        for index in 0..12 {
            assert_eq!(
                auth.entries[&format!("provider-{index}")].access_token(),
                format!("key-{index}")
            );
        }
    }

    #[test]
    fn pi_oauth_providers_expose_their_login_methods() {
        assert_eq!(
            login_methods("openai-codex"),
            vec![LoginMethod::BrowserOAuth, LoginMethod::DeviceOAuth]
        );
        assert!(login_methods("anthropic").contains(&LoginMethod::ManualOAuth));
        for provider in ["github-copilot", "kimi-coding", "meta", "xai"] {
            assert!(login_methods(provider).contains(&LoginMethod::DeviceOAuth));
        }
        assert_eq!(env_var_names("meta"), &["META_API_KEY"]);
        assert!(login_methods("openrouter").contains(&LoginMethod::BrowserOAuth));
        assert!(login_methods("openrouter").contains(&LoginMethod::ManualOAuth));
        assert_eq!(
            login_methods("cursor"),
            vec![LoginMethod::BrowserOAuth, LoginMethod::ApiKey]
        );
        assert_eq!(
            login_methods("radius")[..2],
            [LoginMethod::BrowserOAuth, LoginMethod::DeviceOAuth]
        );
        assert_eq!(
            login_methods("azure"),
            vec![LoginMethod::ApiKey, LoginMethod::AzureEntraId]
        );
    }

    #[test]
    fn azure_login_exposes_both_credential_sources() {
        assert_eq!(
            env_var_names("azure"),
            &["AZURE_OPENAI_API_KEY", "AZURE_OPENAI_AUTH_TOKEN"]
        );
    }

    #[test]
    fn gateway_providers_use_bearer_tokens_and_standard_environment_variables() {
        assert_eq!(
            env_var_names("databricks-unity-gateway"),
            &["DATABRICKS_TOKEN"]
        );
        assert_eq!(
            env_var_names("snowflake-cortex"),
            &["SNOWFLAKE_PAT", "SNOWFLAKE_TOKEN"]
        );
        assert!(is_bearer_access_token("databricks-unity-gateway", "token"));
        assert!(is_bearer_access_token("snowflake-cortex", "pat"));
    }

    #[test]
    fn azure_databricks_workspace_uses_the_cloud_neutral_host_setting() {
        let (variable, url) = normalize_gateway_base_url(
            "databricks-unity-gateway",
            "https://adb-1234567890123456.7.azuredatabricks.net/",
        )
        .unwrap();
        assert_eq!(variable, "DATABRICKS_HOST");
        assert_eq!(url, "https://adb-1234567890123456.7.azuredatabricks.net");
    }

    #[test]
    fn snowflake_account_and_gateway_urls_use_documented_roots() {
        assert_eq!(
            normalize_gateway_base_url(
                "snowflake-cortex",
                "https://account.snowflakecomputing.com"
            )
            .unwrap()
            .1,
            "https://account.snowflakecomputing.com/api/v2/cortex"
        );
        assert_eq!(
            normalize_gateway_base_url(
                "snowflake-cortex",
                "https://account.snowflakecomputing.com/api/v2/aigateways/snowflake/"
            )
            .unwrap()
            .1,
            "https://account.snowflakecomputing.com/api/v2/aigateways/SNOWFLAKE"
        );
    }

    #[test]
    fn local_resolution_uses_declared_keys_without_external_discovery() {
        let declared = BTreeMap::from([("test-provider".to_string(), "test-key".to_string())]);
        assert_eq!(
            resolve_api_key_local("test-provider", &declared).as_deref(),
            Some("test-key")
        );
    }

    #[tokio::test]
    async fn a_request_sends_the_value_of_a_declared_variable_key() {
        let variable = "KISS_TEST_DECLARED_KEY_VARIABLE";
        // SAFETY: the variable name is unique to this test.
        unsafe { std::env::set_var(variable, "from-env") };
        let declared = BTreeMap::from([("test-provider".to_string(), format!("${variable}"))]);
        assert_eq!(
            resolve_api_key_async("test-provider", &declared)
                .await
                .unwrap()
                .as_deref(),
            Some("from-env")
        );
        assert_eq!(
            resolve_api_key_local("test-provider", &declared).as_deref(),
            Some("from-env")
        );
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_auth_file_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let mut auth = AuthFile::default();
        for index in 0..30 {
            auth.entries.insert(
                format!("provider-{index}"),
                AuthEntry::OAuth(OAuthCredential {
                    kind: "oauth".into(),
                    access: "access-token-value".repeat(8),
                    refresh: "refresh-token-value".repeat(8),
                    expires: 2_000_000_000_000,
                    account_id: format!("account-{index}"),
                    available_model_ids: None,
                    client_id: None,
                }),
            );
        }
        write_auth_file_at(&path, &auth).unwrap();
        kiss_bench::measure("auth_read_30", 15, 100, "30_oauth_entries", || {
            read_auth_file_at(&path).unwrap().entries.len()
        });
        read_auth_file_cached_at(&path);
        kiss_bench::measure("auth_cached_30", 15, 1_000, "30_oauth_entries", || {
            read_auth_file_cached_at(&path).entries.len()
        });
    }
}
