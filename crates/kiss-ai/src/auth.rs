//! Provider credentials. Stored credentials override environment variables,
//! and custom catalog placeholders are the last fallback.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

pub mod anthropic;
mod device_code;
pub mod external;
pub mod github_copilot;
pub mod kimi_coding;
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
    pub account_id: String,
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
    provider == "anthropic"
        && std::env::var("ANTHROPIC_OAUTH_TOKEN")
            .is_ok_and(|value| !value.is_empty() && value == access_token)
}

pub fn is_bearer_access_token(provider: &str, access_token: &str) -> bool {
    (provider == "anthropic"
        && std::env::var("ANTHROPIC_AUTH_TOKEN")
            .is_ok_and(|value| !value.is_empty() && value == access_token))
        || (provider != "anthropic" && is_oauth_access_token(provider, access_token))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredAuthKind {
    ApiKey,
    OAuth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    BrowserOAuth,
    DeviceOAuth,
    ManualOAuth,
    ApiKey,
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
        "anthropic" => vec![BrowserOAuth, ManualOAuth, ApiKey],
        "github-copilot" | "kimi-coding" | "xai" => vec![DeviceOAuth, ApiKey],
        "openrouter" => vec![BrowserOAuth, ManualOAuth, ApiKey],
        "radius" => vec![BrowserOAuth, DeviceOAuth, ApiKey],
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
        "azure-openai-responses" => &["AZURE_OPENAI_API_KEY"],
        "baseten" => &["BASETEN_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "cloudflare-ai-gateway" => &["CLOUDFLARE_AI_GATEWAY_API_KEY"],
        "cloudflare-workers-ai" => &["CLOUDFLARE_API_TOKEN"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        "google" => &["GEMINI_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "llama.cpp" => &["LLAMA_API_KEY"],
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
        "together" => &["TOGETHER_API_KEY"],
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

pub fn auth_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kiss/agent/auth.json"))
}

fn read_auth_file_at(path: &Path) -> AuthFile {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => AuthFile::default(),
    }
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
    let value = read_auth_file_at(path);
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
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("auth path has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create auth directory {}", parent.display()))?;

    let temp_name = format!(
        ".auth.json.tmp-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let temp_path = parent.join(temp_name);
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .with_context(|| format!("create temporary auth file {}", temp_path.display()))?;
        let text = serde_json::to_string_pretty(auth)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temp_path, path)
            .with_context(|| format!("replace auth file {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        update_auth_cache(path, auth);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn update_auth_file_at<T>(
    path: &Path,
    update: impl FnOnce(&mut AuthFile) -> Result<T>,
) -> Result<T> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("auth path has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create auth directory {}", parent.display()))?;
    let lock_path = parent.join(".auth.json.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let lock = options
        .open(&lock_path)
        .with_context(|| format!("open auth lock {}", lock_path.display()))?;
    fs2::FileExt::lock_exclusive(&lock)
        .with_context(|| format!("lock auth file {}", path.display()))?;
    let mut auth = read_auth_file_at(path);
    let result = update(&mut auth)?;
    write_auth_file_at(path, &auth)?;
    fs2::FileExt::unlock(&lock).with_context(|| format!("unlock auth file {}", path.display()))?;
    Ok(result)
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
    let file = read_auth_file();
    if let Some(entry) = file.entries.get(provider) {
        return Some(entry.access_token().to_string());
    }
    for variable in env_var_names(provider) {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    if provider == "google-vertex" && has_google_application_credentials() {
        return Some("google-application-default-credentials".into());
    }
    declared.get(provider).cloned()
}

/// Resolve a local credential, then try one automatic external import.
pub fn resolve_api_key(provider: &str, declared: &BTreeMap<String, String>) -> Option<String> {
    if let Some(key) = resolve_api_key_local(provider, declared) {
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
pub async fn resolve_api_key_async(
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
                    "openai-codex"
                        | "anthropic"
                        | "github-copilot"
                        | "kimi-coding"
                        | "xai"
                        | "radius"
                ) && credential.is_expired() =>
            {
                static REFRESH_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
                    std::sync::OnceLock::new();
                let _guard = REFRESH_LOCK
                    .get_or_init(|| tokio::sync::Mutex::new(()))
                    .lock()
                    .await;
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
                let refreshed = match provider {
                    "openai-codex" => openai_codex::refresh(&current, &Default::default()).await?,
                    "anthropic" => anthropic::refresh(&current, &Default::default()).await?,
                    "github-copilot" => {
                        github_copilot::refresh(&current, &Default::default()).await?
                    }
                    "kimi-coding" => kimi_coding::refresh(&current, &Default::default()).await?,
                    "xai" => xai::refresh(&current, &Default::default()).await?,
                    "radius" => radius::refresh(&current, &Default::default()).await?,
                    _ => unreachable!(),
                };
                let access = refreshed.access.clone();
                update_auth_file(|file| {
                    file.entries
                        .insert(provider.to_string(), AuthEntry::OAuth(refreshed));
                    Ok(())
                })?;
                return Ok(Some(access));
            }
            other => return Ok(Some(other.access_token().to_string())),
        }
    }
    for variable in env_var_names(provider) {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
        {
            return Ok(Some(value));
        }
    }
    if external::auto_import_unique(provider)?.is_some()
        && let Some(entry) = read_auth_file().entries.get(provider)
    {
        return Ok(Some(entry.access_token().to_string()));
    }
    if provider == "google-vertex" && has_google_application_credentials() {
        return Ok(Some(google_application_access_token().await?));
    }
    Ok(declared.get(provider).cloned())
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
            dirs::home_dir()
                .map(|home| home.join(".config/gcloud/application_default_credentials.json"))
        });
    path.is_some_and(|path| path.is_file())
}

async fn google_application_access_token() -> Result<String> {
    let output = tokio::process::Command::new("gcloud")
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

pub fn store_api_key(provider: &str, key: &str) -> Result<()> {
    update_auth_file(|file| {
        file.entries
            .insert(provider.to_string(), AuthEntry::Key(key.to_string()));
        Ok(())
    })
}

pub fn store_oauth(provider: &str, credential: OAuthCredential) -> Result<()> {
    update_auth_file(|file| {
        file.entries
            .insert(provider.to_string(), AuthEntry::OAuth(credential));
        Ok(())
    })
}

/// Remove a stored credential. Returns true when an entry existed.
pub fn remove_api_key(provider: &str) -> Result<bool> {
    let path = auth_file_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    update_auth_file_at(&path, |file| Ok(file.entries.remove(provider).is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_old_and_new_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"old":"one","detailed":{"key":"two"},"oauth":{"type":"oauth","access":"three","refresh":"four","expires":9,"accountId":"acct"}}"#,
        )
        .unwrap();
        let auth = read_auth_file_at(&path);
        assert_eq!(auth.entries["old"].access_token(), "one");
        assert_eq!(auth.entries["detailed"].access_token(), "two");
        assert_eq!(auth.entries["oauth"].access_token(), "three");
    }

    #[test]
    fn atomic_write_replaces_file_and_keeps_valid_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent/auth.json");
        let mut auth = AuthFile::default();
        auth.entries
            .insert("openai".into(), AuthEntry::Key("secret".into()));
        write_auth_file_at(&path, &auth).unwrap();
        let saved = read_auth_file_at(&path);
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
        let auth = read_auth_file_at(&path);
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
        for provider in ["github-copilot", "kimi-coding", "xai"] {
            assert!(login_methods(provider).contains(&LoginMethod::DeviceOAuth));
        }
        assert!(login_methods("openrouter").contains(&LoginMethod::BrowserOAuth));
        assert!(login_methods("openrouter").contains(&LoginMethod::ManualOAuth));
        assert_eq!(
            login_methods("radius")[..2],
            [LoginMethod::BrowserOAuth, LoginMethod::DeviceOAuth]
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
                }),
            );
        }
        write_auth_file_at(&path, &auth).unwrap();
        kiss_bench::measure("auth_read_30", 15, 100, "30_oauth_entries", || {
            read_auth_file_at(&path).entries.len()
        });
        read_auth_file_cached_at(&path);
        kiss_bench::measure("auth_cached_30", 15, 1_000, "30_oauth_entries", || {
            read_auth_file_cached_at(&path).entries.len()
        });
    }
}
