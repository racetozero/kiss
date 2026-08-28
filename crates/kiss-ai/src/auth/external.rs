//! Discovery and explicit import of credentials stored by other coding agents.

use super::OAuthCredential;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCredentialKind {
    ApiKey,
    OAuth,
}

#[derive(Debug, Clone)]
pub struct ExternalCredentialSource {
    pub id: String,
    pub application: String,
    pub provider: String,
    pub kind: ExternalCredentialKind,
    pub location: String,
    format: SourceFormat,
    path: Option<PathBuf>,
    entry_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceFormat {
    Codex,
    ClaudeCode,
    ClaudeKeychain,
    OpenCode,
    Pi,
    OpenClaw,
    Hermes,
}

#[derive(Debug, Clone)]
enum ImportedCredential {
    ApiKey(String),
    OAuth(OAuthCredential),
}

pub fn discover() -> Vec<ExternalCredentialSource> {
    let mut sources = dirs::home_dir()
        .as_deref()
        .map(discover_at)
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    if claude_keychain_secret(false).is_some() {
        sources.insert(
            usize::from(
                sources
                    .first()
                    .is_some_and(|source| source.format == SourceFormat::Codex),
            ),
            source(
                "Claude Code",
                "anthropic",
                ExternalCredentialKind::OAuth,
                "macOS Keychain: Claude Code-credentials".into(),
                SourceFormat::ClaudeKeychain,
                None,
                None,
            ),
        );
    }
    sources
}

pub fn discover_at(home: &Path) -> Vec<ExternalCredentialSource> {
    let mut sources = Vec::new();
    let codex = home.join(".codex/auth.json");
    if read_codex(&codex).is_ok() {
        sources.push(source(
            "OpenAI Codex",
            "openai-codex",
            ExternalCredentialKind::OAuth,
            codex.display().to_string(),
            SourceFormat::Codex,
            Some(codex),
            None,
        ));
    }

    let claude = home.join(".claude/.credentials.json");
    if read_claude(&claude).is_ok() {
        sources.push(source(
            "Claude Code",
            "anthropic",
            ExternalCredentialKind::OAuth,
            claude.display().to_string(),
            SourceFormat::ClaudeCode,
            Some(claude),
            None,
        ));
    }

    let mut shared = vec![
        (
            "OpenCode",
            home.join(".local/share/opencode/auth.json"),
            SourceFormat::OpenCode,
        ),
        ("Pi", home.join(".pi/agent/auth.json"), SourceFormat::Pi),
    ];
    shared.extend(
        openclaw_paths(home)
            .into_iter()
            .map(|path| ("OpenClaw", path, SourceFormat::OpenClaw)),
    );
    shared.push((
        "Hermes",
        home.join(".hermes/auth.json"),
        SourceFormat::Hermes,
    ));
    let mut seen = HashSet::new();
    for (application, path, format) in shared {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(map) = read_shared_map(&path, format) else {
            continue;
        };
        for (entry_key, entry) in map {
            let Some((provider, kind)) = classify_entry(format, &entry_key, &entry) else {
                continue;
            };
            sources.push(source(
                application,
                &provider,
                kind,
                path.display().to_string(),
                format,
                Some(path.clone()),
                Some(entry_key),
            ));
        }
    }
    sources
}

fn source(
    application: &str,
    provider: &str,
    kind: ExternalCredentialKind,
    location: String,
    format: SourceFormat,
    path: Option<PathBuf>,
    entry_key: Option<String>,
) -> ExternalCredentialSource {
    ExternalCredentialSource {
        id: format!(
            "{}:{}:{}",
            application.to_ascii_lowercase().replace(' ', "-"),
            provider,
            location
        ),
        application: application.into(),
        provider: provider.into(),
        kind,
        location,
        format,
        path,
        entry_key,
    }
}

fn openclaw_paths(home: &Path) -> Vec<PathBuf> {
    let mut paths = vec![home.join(".openclaw/agent/auth.json")];
    let agents = home.join(".openclaw/agents");
    let mut directories = vec![agents.join("main")];
    if let Ok(entries) = std::fs::read_dir(&agents) {
        let mut rest: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.file_name().is_some_and(|name| name != "main"))
            .collect();
        rest.sort();
        directories.extend(rest);
    }
    for directory in directories {
        paths.push(directory.join("agent/auth-profiles.json"));
        paths.push(directory.join("agent/auth.json"));
    }
    paths.push(home.join(".openclaw/credentials/oauth.json"));
    paths
}

fn validate_external_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("read external credential metadata at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "external credential source is not a regular file: {}",
            path.display()
        );
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    validate_external_file(path)?;
    let bytes = std::fs::read(path)
        .with_context(|| format!("read external credentials at {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse external credentials at {}", path.display()))
}

fn jwt_claim(token: &str, claim: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice::<Value>(&decoded)
        .ok()?
        .get(claim)
        .cloned()
}

fn read_codex(path: &Path) -> Result<OAuthCredential> {
    let value = read_json(path)?;
    let tokens = &value["tokens"];
    let access = tokens["access_token"]
        .as_str()
        .context("Codex auth has no access token")?;
    let refresh = tokens["refresh_token"]
        .as_str()
        .context("Codex auth has no refresh token")?;
    if access.trim().is_empty() || refresh.trim().is_empty() {
        anyhow::bail!("Codex auth has an empty token");
    }
    let account_id = tokens["account_id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| super::openai_codex::decode_jwt_account_id(access))
        .context("Codex auth has no ChatGPT account ID")?;
    let expires = jwt_claim(access, "exp")
        .and_then(|value| value.as_i64())
        .unwrap_or_default()
        .saturating_mul(1000);
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access: access.into(),
        refresh: refresh.into(),
        expires,
        account_id,
        available_model_ids: None,
    })
}

fn expiry_millis(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number.as_i64().unwrap_or_default(),
        Some(Value::String(text)) => text
            .parse::<i64>()
            .ok()
            .or_else(|| {
                chrono::DateTime::parse_from_rfc3339(text)
                    .ok()
                    .map(|date| date.timestamp_millis())
            })
            .unwrap_or_default(),
        _ => 0,
    }
}

fn parse_claude_value(value: &Value) -> Result<OAuthCredential> {
    let oauth = value.get("claudeAiOauth").unwrap_or(value);
    let access = oauth["accessToken"]
        .as_str()
        .context("Claude Code auth has no access token")?;
    let refresh = oauth["refreshToken"]
        .as_str()
        .context("Claude Code auth has no refresh token")?;
    if access.trim().is_empty() || refresh.trim().is_empty() {
        anyhow::bail!("Claude Code auth has an empty token");
    }
    Ok(OAuthCredential {
        kind: "oauth".into(),
        access: access.into(),
        refresh: refresh.into(),
        expires: expiry_millis(oauth.get("expiresAt")),
        account_id: String::new(),
        available_model_ids: None,
    })
}

fn read_claude(path: &Path) -> Result<OAuthCredential> {
    parse_claude_value(&read_json(path)?)
}

#[cfg(target_os = "macos")]
fn claude_keychain_secret(include_secret: bool) -> Option<String> {
    let mut command = std::process::Command::new("/usr/bin/security");
    command.args(["find-generic-password", "-s", "Claude Code-credentials"]);
    if include_secret {
        command.arg("-w");
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    if include_secret {
        Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
    } else {
        Some(String::new())
    }
}

fn read_shared_map(path: &Path, format: SourceFormat) -> Result<BTreeMap<String, Value>> {
    let value = read_json(path)?;
    match format {
        SourceFormat::OpenCode | SourceFormat::Pi => Ok(value
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default()),
        SourceFormat::OpenClaw => Ok(flatten_openclaw(&value)),
        SourceFormat::Hermes => Ok(flatten_hermes(&value)),
        _ => anyhow::bail!("not a shared credential store"),
    }
}

fn flatten_openclaw(value: &Value) -> BTreeMap<String, Value> {
    let Some(object) = value.as_object() else {
        return BTreeMap::new();
    };
    let Some(profiles) = object.get("profiles").and_then(Value::as_object) else {
        return object
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    };
    let mut map = BTreeMap::new();
    for (profile_id, entry) in profiles {
        let provider = entry["provider"]
            .as_str()
            .or_else(|| profile_id.split(':').next());
        let Some(provider) = provider else { continue };
        let default = profile_id.ends_with(":default") || !profile_id.contains(':');
        if default || !map.contains_key(provider) {
            map.insert(provider.to_string(), entry.clone());
        }
    }
    map
}

fn flatten_hermes(value: &Value) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    if let Some(providers) = value["providers"].as_object() {
        map.extend(
            providers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    if let Some(pool) = value["credential_pool"].as_object() {
        for (provider, entries) in pool {
            if let Some(entry) = entries.as_array().and_then(|entries| entries.first()) {
                map.insert(provider.clone(), entry.clone());
            }
        }
    }
    map
}

fn shared_api_key(format: SourceFormat, entry: &Value) -> Option<String> {
    if let Some(value) = entry.as_str() {
        return Some(value.to_string());
    }
    let object = entry.as_object()?;
    let raw = match format {
        SourceFormat::OpenCode if object.get("type")?.as_str()? == "api" => {
            object.get("key")?.as_str()?
        }
        SourceFormat::Pi | SourceFormat::OpenClaw if object.get("type")?.as_str()? == "api_key" => {
            object.get("key")?.as_str()?
        }
        SourceFormat::Hermes if object.get("auth_type")?.as_str()? == "api_key" => {
            object.get("access_token")?.as_str()?
        }
        _ => return None,
    }
    .trim();
    if raw.is_empty() || raw.starts_with('!') {
        return None;
    }
    Some(
        std::env::var(raw)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| raw.to_string()),
    )
}

fn shared_oauth(format: SourceFormat, entry: &Value) -> Option<OAuthCredential> {
    let object = entry.as_object()?;
    let (access, refresh, expires) = match format {
        SourceFormat::OpenCode | SourceFormat::Pi | SourceFormat::OpenClaw => {
            if object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind != "oauth")
            {
                return None;
            }
            (
                object.get("access")?.as_str()?,
                object.get("refresh")?.as_str()?,
                object.get("expires")?.as_i64()?,
            )
        }
        SourceFormat::Hermes => {
            if object
                .get("auth_type")
                .and_then(Value::as_str)
                .is_some_and(|kind| !kind.starts_with("oauth"))
            {
                return None;
            }
            let expires = object
                .get("expires_at_ms")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| expiry_millis(object.get("expires_at")));
            (
                object.get("access_token")?.as_str()?,
                object.get("refresh_token")?.as_str()?,
                expires,
            )
        }
        _ => return None,
    };
    if access.trim().is_empty() || refresh.trim().is_empty() {
        return None;
    }
    Some(OAuthCredential {
        kind: "oauth".into(),
        access: access.into(),
        refresh: refresh.into(),
        expires,
        account_id: object
            .get("enterpriseUrl")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        available_model_ids: object.get("availableModelIds").and_then(|value| {
            value.as_array().map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
        }),
    })
}

fn canonical_provider(entry_key: &str, kind: ExternalCredentialKind) -> Option<String> {
    let provider = match (entry_key, kind) {
        ("openai-codex" | "openai_codex" | "openai", ExternalCredentialKind::OAuth) => {
            "openai-codex"
        }
        ("anthropic" | "claude", _) => "anthropic",
        ("github-copilot" | "copilot", _) => "github-copilot",
        ("google" | "gemini", ExternalCredentialKind::ApiKey) => "google",
        ("azure" | "azure-openai", _) => "azure-openai-responses",
        ("togetherai" | "together-ai", _) => "together",
        ("moonshot", _) => "moonshotai",
        ("kimi", _) => "kimi-coding",
        ("zhipu" | "zhipuai", _) => "zai",
        ("hf", _) => "huggingface",
        ("ai-gateway", _) => "vercel-ai-gateway",
        (other, _) => other,
    };
    crate::registry::BUILTIN_PROVIDER_IDS
        .contains(&provider)
        .then(|| provider.to_string())
}

fn classify_entry(
    format: SourceFormat,
    entry_key: &str,
    entry: &Value,
) -> Option<(String, ExternalCredentialKind)> {
    let kind = if shared_oauth(format, entry).is_some() {
        ExternalCredentialKind::OAuth
    } else if shared_api_key(format, entry).is_some() {
        ExternalCredentialKind::ApiKey
    } else {
        return None;
    };
    canonical_provider(entry_key, kind).map(|provider| (provider, kind))
}

fn read_source(source: &ExternalCredentialSource) -> Result<ImportedCredential> {
    match source.format {
        SourceFormat::Codex => Ok(ImportedCredential::OAuth(read_codex(
            source.path.as_deref().context("Codex source has no path")?,
        )?)),
        SourceFormat::ClaudeCode => Ok(ImportedCredential::OAuth(read_claude(
            source
                .path
                .as_deref()
                .context("Claude source has no path")?,
        )?)),
        SourceFormat::ClaudeKeychain => {
            #[cfg(target_os = "macos")]
            {
                let secret = claude_keychain_secret(true)
                    .context("Claude Code Keychain credential is unavailable")?;
                let value: Value = serde_json::from_str(&secret)
                    .context("Claude Code Keychain credential has invalid JSON")?;
                Ok(ImportedCredential::OAuth(parse_claude_value(&value)?))
            }
            #[cfg(not(target_os = "macos"))]
            anyhow::bail!("Claude Code Keychain import is available only on macOS")
        }
        format => {
            let path = source
                .path
                .as_deref()
                .context("shared source has no path")?;
            let key = source
                .entry_key
                .as_deref()
                .context("shared source has no provider key")?;
            let map = read_shared_map(path, format)?;
            let entry = map
                .get(key)
                .context("external credential entry is no longer present")?;
            if let Some(oauth) = shared_oauth(format, entry) {
                let oauth = if source.provider == "openai-codex" && oauth.account_id.is_empty() {
                    OAuthCredential {
                        account_id: super::openai_codex::decode_jwt_account_id(&oauth.access)
                            .context("imported Codex token has no ChatGPT account ID")?,
                        ..oauth
                    }
                } else {
                    oauth
                };
                Ok(ImportedCredential::OAuth(oauth))
            } else if let Some(key) = shared_api_key(format, entry) {
                Ok(ImportedCredential::ApiKey(key))
            } else {
                anyhow::bail!("external credential entry is no longer valid")
            }
        }
    }
}

/// Import one discovered source into Kiss's private auth store.
pub fn import(source: &ExternalCredentialSource) -> Result<()> {
    let path = super::auth_file_path().context("no home directory")?;
    import_at(source, &path)
}

fn import_at(source: &ExternalCredentialSource, path: &Path) -> Result<()> {
    let entry = match read_source(source)? {
        ImportedCredential::ApiKey(key) => super::AuthEntry::Key(key),
        ImportedCredential::OAuth(credential) => super::AuthEntry::OAuth(credential),
    };
    super::update_auth_file_at(path, |file| {
        file.entries.insert(source.provider.clone(), entry);
        Ok(())
    })
}

/// Import one unambiguous external credential without replacing Kiss state.
pub fn auto_import_unique(provider: &str) -> Result<Option<String>> {
    let sources = discover();
    auto_import_unique_from_sources(provider, &sources)
}

/// Import one unambiguous source from a discovery result shared by a caller.
pub fn auto_import_unique_from_sources(
    provider: &str,
    sources: &[ExternalCredentialSource],
) -> Result<Option<String>> {
    if super::stored_auth_kind(provider).is_some() {
        return Ok(None);
    }
    let mut matches = sources.iter().filter(|source| source.provider == provider);
    let Some(source) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Ok(None);
    }
    let entry = match read_source(source)? {
        ImportedCredential::ApiKey(key) => super::AuthEntry::Key(key),
        ImportedCredential::OAuth(credential) => super::AuthEntry::OAuth(credential),
    };
    let path = super::auth_file_path().context("no home directory")?;
    let imported = super::update_auth_file_at(&path, |file| {
        if file.entries.contains_key(provider) {
            return Ok(false);
        }
        file.entries.insert(provider.to_string(), entry);
        Ok(true)
    })?;
    Ok(imported.then(|| source.application.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, value: Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn fake_codex_token(account: &str) -> String {
        let payload = json!({
            "exp": 2_000_000_000,
            "https://api.openai.com/auth": {"chatgpt_account_id": account}
        });
        format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn discovers_native_and_shared_sources_without_secret_fields() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        write(
            &home.join(".codex/auth.json"),
            json!({"tokens": {
                "access_token": fake_codex_token("acct-one"),
                "refresh_token": "codex-refresh",
                "account_id": "acct-one"
            }}),
        );
        write(
            &home.join(".claude/.credentials.json"),
            json!({"claudeAiOauth": {
                "accessToken": "claude-access",
                "refreshToken": "claude-refresh",
                "expiresAt": 2_000_000_000_000_i64
            }}),
        );
        write(
            &home.join(".pi/agent/auth.json"),
            json!({
                "openrouter": {"type":"api_key", "key":"router-secret"},
                "anthropic": {"type":"oauth", "access":"pi-access", "refresh":"pi-refresh", "expires":2_000_000_000_000_i64}
            }),
        );
        let sources = discover_at(home);
        assert!(
            sources
                .iter()
                .any(|source| source.application == "OpenAI Codex")
        );
        assert!(
            sources
                .iter()
                .any(|source| source.application == "Claude Code")
        );
        assert!(sources.iter().any(|source| source.provider == "openrouter"));
        let debug = format!("{sources:?}");
        assert!(!debug.contains("router-secret"));
        assert!(!debug.contains("claude-access"));
    }

    #[test]
    fn reads_codex_and_claude_credentials_for_explicit_import() {
        let directory = tempfile::tempdir().unwrap();
        let codex = directory.path().join("codex.json");
        write(
            &codex,
            json!({"tokens": {
                "access_token": fake_codex_token("acct-two"),
                "refresh_token": "refresh-two"
            }}),
        );
        let credential = read_codex(&codex).unwrap();
        assert_eq!(credential.account_id, "acct-two");
        assert_eq!(credential.refresh, "refresh-two");

        let claude = directory.path().join("claude.json");
        write(
            &claude,
            json!({
                "accessToken":"access-three",
                "refreshToken":"refresh-three",
                "expiresAt":"2030-01-01T00:00:00Z"
            }),
        );
        let credential = read_claude(&claude).unwrap();
        assert_eq!(credential.access, "access-three");
        assert!(credential.expires > 1_800_000_000_000_i64);
    }

    #[test]
    fn flattens_openclaw_and_hermes_stores() {
        let openclaw = json!({"profiles": {
            "openrouter:one": {"type":"api_key", "provider":"openrouter", "key":"one"},
            "openrouter:default": {"type":"api_key", "provider":"openrouter", "key":"default"}
        }});
        assert_eq!(flatten_openclaw(&openclaw)["openrouter"]["key"], "default");
        let hermes = json!({
            "providers": {"anthropic": {"auth_type":"api_key", "access_token":"old"}},
            "credential_pool": {"anthropic": [{"auth_type":"api_key", "access_token":"new"}]}
        });
        assert_eq!(flatten_hermes(&hermes)["anthropic"]["access_token"], "new");
    }

    #[test]
    fn pi_copilot_import_keeps_account_model_scope() {
        let credential = shared_oauth(
            SourceFormat::Pi,
            &json!({
                "type":"oauth",
                "access":"copilot-access",
                "refresh":"github-access",
                "expires":2_000_000_000_000_i64,
                "enterpriseUrl":"company.ghe.com",
                "availableModelIds":["gpt-5.6-sol"]
            }),
        )
        .unwrap();
        assert_eq!(credential.account_id, "company.ghe.com");
        assert_eq!(
            credential.available_model_ids,
            Some(vec!["gpt-5.6-sol".into()])
        );
    }

    #[test]
    fn explicit_import_copies_only_the_selected_credential() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        write(
            &home.join(".pi/agent/auth.json"),
            json!({
                "openrouter": {"type":"api_key", "key":"router-value"},
                "anthropic": {"type":"api_key", "key":"anthropic-value"}
            }),
        );
        let source = discover_at(home)
            .into_iter()
            .find(|source| source.provider == "openrouter")
            .unwrap();
        let destination = home.join(".kiss/agent/auth.json");
        import_at(&source, &destination).unwrap();
        let saved = super::super::read_auth_file_at(&destination);
        assert_eq!(saved.entries.len(), 1);
        assert_eq!(saved.entries["openrouter"].access_token(), "router-value");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_external_credentials() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        let link = directory.path().join("link.json");
        write(&target, json!({}));
        symlink(&target, &link).unwrap();
        assert!(read_json(&link).is_err());
    }
}
