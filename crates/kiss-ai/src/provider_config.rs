//! Read and update the custom OpenAI-compatible provider catalog.

use anyhow::{Context as _, Result, bail};
use fs2::FileExt as _;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderApi {
    ChatCompletions,
    Responses,
    Codex,
}

impl ProviderApi {
    pub fn catalog_name(self) -> &'static str {
        match self {
            Self::ChatCompletions => "openai-completions",
            Self::Responses => "openai-responses",
            Self::Codex => "openai-codex-responses",
        }
    }
}

impl fmt::Display for ProviderApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ChatCompletions => "chat-completions",
            Self::Responses => "responses",
            Self::Codex => "codex",
        })
    }
}

impl FromStr for ProviderApi {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "chat-completions" | "completions" | "openai-completions" => Ok(Self::ChatCompletions),
            "responses" | "openai-responses" => Ok(Self::Responses),
            "codex" | "openai-codex-responses" => Ok(Self::Codex),
            _ => {
                bail!("unknown provider API '{value}' (use chat-completions, responses, or codex)")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AddProvider {
    pub id: String,
    pub name: Option<String>,
    pub base_url: String,
    pub api: ProviderApi,
    pub model_id: String,
    pub model_name: Option<String>,
    pub api_key_env: Option<String>,
    pub auth_provider: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub reasoning: bool,
    pub context_window: u64,
    pub max_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSummary {
    pub id: String,
    pub name: Option<String>,
    pub base_url: String,
    pub api: String,
    pub models: Vec<String>,
    pub api_key_env: Option<String>,
    pub auth_provider: Option<String>,
}

pub fn catalog_path() -> Result<PathBuf> {
    std::env::var_os("KISS_MODELS_FILE")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".kiss/agent/models.json")))
        .context("no home directory; set KISS_MODELS_FILE")
}

pub fn add(request: &AddProvider) -> Result<PathBuf> {
    let path = catalog_path()?;
    add_at(&path, request)?;
    Ok(path)
}

fn add_at(path: &Path, request: &AddProvider) -> Result<()> {
    validate(request)?;
    update_catalog(path, |root| {
        let providers = providers_mut(root)?;
        let mut provider = Map::new();
        provider.insert("baseUrl".into(), json!(request.base_url));
        provider.insert("api".into(), json!(request.api.catalog_name()));
        if let Some(name) = request.name.as_deref() {
            provider.insert("name".into(), json!(name));
        }
        if let Some(variable) = request.api_key_env.as_deref() {
            provider.insert("apiKey".into(), json!(format!("${variable}")));
        } else if let Some(auth_provider) = request.auth_provider.as_deref() {
            provider.insert("authProvider".into(), json!(auth_provider));
        } else if request.api == ProviderApi::Codex {
            provider.insert("authProvider".into(), json!("openai-codex"));
        } else {
            // The adapters require a credential value. A local server can
            // ignore this placeholder, while saved credentials still win.
            provider.insert("apiKey".into(), json!("not-needed"));
        }
        if !request.headers.is_empty() {
            provider.insert("headers".into(), json!(request.headers));
        }
        let mut model = Map::new();
        model.insert("id".into(), json!(request.model_id));
        if let Some(name) = request.model_name.as_deref() {
            model.insert("name".into(), json!(name));
        }
        model.insert("reasoning".into(), json!(request.reasoning));
        model.insert("contextWindow".into(), json!(request.context_window));
        model.insert("maxTokens".into(), json!(request.max_tokens));
        provider.insert("models".into(), Value::Array(vec![Value::Object(model)]));
        providers.insert(request.id.clone(), Value::Object(provider));
        Ok(())
    })
}

pub fn list() -> Result<Vec<ProviderSummary>> {
    let path = catalog_path()?;
    let root = read_catalog(&path)?;
    let Some(providers) = root.get("providers").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    for (id, value) in providers {
        let Some(provider) = value.as_object() else {
            continue;
        };
        output.push(ProviderSummary {
            id: id.clone(),
            name: string_field(provider, "name"),
            base_url: string_field(provider, "baseUrl").unwrap_or_default(),
            api: string_field(provider, "api").unwrap_or_default(),
            models: provider
                .get("models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|model| model.get("id").and_then(Value::as_str))
                .map(str::to_owned)
                .collect(),
            api_key_env: string_field(provider, "apiKey")
                .and_then(|value| value.strip_prefix('$').map(str::to_owned)),
            auth_provider: string_field(provider, "authProvider"),
        });
    }
    Ok(output)
}

pub fn remove(id: &str) -> Result<bool> {
    validate_id("provider", id)?;
    let path = catalog_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let mut removed = false;
    update_catalog(&path, |root| {
        removed = providers_mut(root)?.remove(id).is_some();
        Ok(())
    })?;
    Ok(removed)
}

fn validate(request: &AddProvider) -> Result<()> {
    validate_id("provider", &request.id)?;
    validate_id("model", &request.model_id)?;
    let url = url::Url::parse(&request.base_url).context("invalid provider base URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("provider base URL must be an HTTP or HTTPS URL with a host");
    }
    if request.context_window == 0 || request.max_tokens == 0 {
        bail!("context window and max tokens must be greater than zero");
    }
    if let Some(variable) = request.api_key_env.as_deref() {
        validate_env_name(variable)?;
    }
    if let Some(provider) = request.auth_provider.as_deref() {
        validate_id("authentication provider", provider)?;
    }
    if request.api_key_env.is_some() && request.auth_provider.is_some() {
        bail!("use either an API key environment variable or an authentication provider");
    }
    for (name, value) in &request.headers {
        if name.trim().is_empty() || value.contains(['\r', '\n']) {
            bail!("invalid HTTP header");
        }
    }
    Ok(())
}

fn validate_id(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        bail!("{kind} ID must contain only letters, numbers, '.', '-', '_', or ':'");
    }
    Ok(())
}

fn validate_env_name(value: &str) -> Result<()> {
    let mut bytes = value.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("API key environment variable has an invalid name");
    }
    Ok(())
}

fn string_field(object: &Map<String, Value>, field: &str) -> Option<String> {
    object.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn providers_mut(root: &mut Value) -> Result<&mut Map<String, Value>> {
    let object = root
        .as_object_mut()
        .context("custom model catalog root must be a JSON object")?;
    let providers = object
        .entry("providers")
        .or_insert_with(|| Value::Object(Map::new()));
    providers
        .as_object_mut()
        .context("custom model catalog 'providers' must be a JSON object")
}

fn read_catalog(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("parse custom model catalog {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(json!({ "providers": {} }))
        }
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn update_catalog(path: &Path, update: impl FnOnce(&mut Value) -> Result<()>) -> Result<()> {
    let parent = path
        .parent()
        .context("custom model catalog has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create provider directory {}", parent.display()))?;
    let lock_path = parent.join(".models.json.lock");
    let mut lock_options = std::fs::OpenOptions::new();
    lock_options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        lock_options.mode(0o600);
    }
    let lock = lock_options
        .open(&lock_path)
        .with_context(|| format!("open provider lock {}", lock_path.display()))?;
    lock.lock_exclusive()?;

    let result = (|| {
        let mut root = read_catalog(path)?;
        update(&mut root)?;
        let temporary = parent.join(format!(".models.json.tmp-{}", std::process::id()));
        let write_result = (|| -> Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            serde_json::to_writer_pretty(&mut file, &root)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        write_result
    })();
    let unlock_result = lock.unlock();
    result?;
    unlock_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AddProvider {
        AddProvider {
            id: "codex-lb".into(),
            name: Some("CodexLB".into()),
            base_url: "http://127.0.0.1:2455/backend-api/codex".into(),
            api: ProviderApi::Codex,
            model_id: "gpt-5.6-sol".into(),
            model_name: None,
            api_key_env: None,
            auth_provider: None,
            headers: BTreeMap::new(),
            reasoning: true,
            context_window: 128_000,
            max_tokens: 16_384,
        }
    }

    #[test]
    fn add_preserves_unknown_data_and_loads_codex_lb() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"future":true,"providers":{"keep":{"baseUrl":"http://localhost","api":"openai-responses","models":[],"custom":1}}}"#,
        )
        .unwrap();
        add_at(&path, &request()).unwrap();

        let root = read_catalog(&path).unwrap();
        assert_eq!(root["future"], true);
        assert_eq!(root["providers"]["keep"]["custom"], 1);
        let registry = crate::Registry::load(Some(&path));
        let (model, _) = registry
            .resolve("codex-lb/gpt-5.6-sol", None)
            .expect("CodexLB model");
        assert_eq!(model.api, "openai-codex-responses");
        assert_eq!(model.base_url, "http://127.0.0.1:2455/backend-api/codex");
        assert_eq!(registry.credential_provider("codex-lb"), "openai-codex");
    }

    #[test]
    fn environment_key_is_stored_as_a_reference() {
        let mut request = request();
        request.api_key_env = Some("CODEX_LB_API_KEY".into());
        request.auth_provider = None;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.json");
        update_catalog(&path, |root| {
            let providers = providers_mut(root)?;
            providers.insert(
                request.id.clone(),
                json!({
                    "baseUrl": request.base_url,
                    "api": request.api.catalog_name(),
                    "apiKey": format!("${}", request.api_key_env.as_deref().unwrap()),
                    "models": [{"id": request.model_id}]
                }),
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(
            read_catalog(&path).unwrap()["providers"]["codex-lb"]["apiKey"],
            "$CODEX_LB_API_KEY"
        );
    }
}
