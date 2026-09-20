//! Model catalog: built-in entries plus a `~/.kiss/agent/models.json`
//! overlay for custom providers (Ollama, vLLM, proxies, ...).

use crate::model::{Model, ModelCost, OpenAICompat, PromptCache};
use crate::types::ThinkingLevel;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Generated model data verified against `@earendil-works/pi-ai` 0.86.0.
const BUILTIN_PROVIDER_CATALOGS: &[&str] = &[
    include_str!("../data/providers/amazon-bedrock.json"),
    include_str!("../data/providers/ant-ling.json"),
    include_str!("../data/providers/anthropic.json"),
    include_str!("../data/providers/azure-openai-responses.json"),
    include_str!("../data/providers/baseten.json"),
    include_str!("../data/providers/cerebras.json"),
    include_str!("../data/providers/cloudflare-ai-gateway.json"),
    include_str!("../data/providers/cloudflare-workers-ai.json"),
    include_str!("../data/providers/cursor.json"),
    include_str!("../data/providers/databricks-unity-gateway.json"),
    include_str!("../data/providers/deepseek.json"),
    include_str!("../data/providers/fireworks.json"),
    include_str!("../data/providers/github-copilot.json"),
    include_str!("../data/providers/google-vertex.json"),
    include_str!("../data/providers/google.json"),
    include_str!("../data/providers/groq.json"),
    include_str!("../data/providers/huggingface.json"),
    include_str!("../data/providers/kimi-coding.json"),
    include_str!("../data/providers/minimax-cn.json"),
    include_str!("../data/providers/minimax.json"),
    include_str!("../data/providers/mistral.json"),
    include_str!("../data/providers/moonshotai-cn.json"),
    include_str!("../data/providers/moonshotai.json"),
    include_str!("../data/providers/nvidia.json"),
    include_str!("../data/providers/openai-codex.json"),
    include_str!("../data/providers/openai.json"),
    include_str!("../data/providers/opencode-go.json"),
    include_str!("../data/providers/opencode.json"),
    include_str!("../data/providers/openrouter.json"),
    include_str!("../data/providers/qwen-token-plan-cn.json"),
    include_str!("../data/providers/qwen-token-plan-individual.json"),
    include_str!("../data/providers/qwen-token-plan.json"),
    include_str!("../data/providers/radius.json"),
    include_str!("../data/providers/snowflake-cortex.json"),
    include_str!("../data/providers/together.json"),
    include_str!("../data/providers/vercel-ai-gateway.json"),
    include_str!("../data/providers/xai.json"),
    include_str!("../data/providers/xiaomi-token-plan-ams.json"),
    include_str!("../data/providers/xiaomi-token-plan-cn.json"),
    include_str!("../data/providers/xiaomi-token-plan-sgp.json"),
    include_str!("../data/providers/xiaomi.json"),
    include_str!("../data/providers/zai-coding-cn.json"),
    include_str!("../data/providers/zai.json"),
];

pub const BUILTIN_PROVIDER_IDS: &[&str] = &[
    "amazon-bedrock",
    "ant-ling",
    "anthropic",
    "azure-openai-responses",
    "baseten",
    "cerebras",
    "cloudflare-ai-gateway",
    "cloudflare-workers-ai",
    "cursor",
    "databricks-unity-gateway",
    "deepseek",
    "fireworks",
    "github-copilot",
    "google",
    "google-vertex",
    "groq",
    "huggingface",
    "kimi-coding",
    "minimax",
    "minimax-cn",
    "mistral",
    "moonshotai",
    "moonshotai-cn",
    "nvidia",
    "openai",
    "openai-codex",
    "opencode",
    "opencode-go",
    "openrouter",
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
    "radius",
    "snowflake-cortex",
    "together",
    "vercel-ai-gateway",
    "xai",
    "xiaomi",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-sgp",
    "zai",
    "zai-coding-cn",
];

#[derive(Debug, Deserialize)]
struct CatalogFile {
    providers: BTreeMap<String, CatalogProvider>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogProvider {
    base_url: String,
    api: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    auth_provider: Option<String>,
    #[serde(default)]
    compat: Option<OpenAICompat>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    models: Vec<CatalogModel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogModel {
    id: String,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    input: Option<Vec<String>>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    cost: Option<ModelCost>,
    #[serde(default)]
    prompt_cache: Option<PromptCache>,
    #[serde(default)]
    compat: Option<OpenAICompat>,
    #[serde(default)]
    thinking_level_map: BTreeMap<String, Option<String>>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RadiusGatewayConfig {
    base_url: String,
    models: Vec<RadiusGatewayModel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RadiusGatewayModel {
    id: String,
    name: String,
    reasoning: bool,
    input: Vec<String>,
    cost: ModelCost,
    context_window: u64,
    max_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct DatabricksModelServicesPage {
    #[serde(default)]
    model_services: Vec<DatabricksModelService>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DatabricksModelService {
    name: String,
}

#[derive(Debug, Clone)]
pub struct Registry {
    models: Vec<Model>,
    model_indices: HashMap<(String, String), usize>,
    manually_configured_providers: BTreeSet<String>,
    /// Placeholder API keys declared in models.json (e.g. "ollama").
    pub declared_keys: BTreeMap<String, String>,
    auth_providers: BTreeMap<String, String>,
}

impl Registry {
    /// Load built-ins plus the user overlay file if present.
    pub fn load(custom_path: Option<&Path>) -> Registry {
        let mut registry = Registry {
            models: Vec::new(),
            model_indices: HashMap::new(),
            manually_configured_providers: BTreeSet::new(),
            declared_keys: BTreeMap::new(),
            auth_providers: BTreeMap::new(),
        };
        registry.merge_builtins();
        let overlay = custom_path
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::var_os("KISS_MODELS_FILE").map(PathBuf::from))
            .or_else(|| dirs::home_dir().map(|h| h.join(".kiss/agent/models.json")));
        if let Some(path) = overlay
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Err(err) = registry.merge_catalog(&text)
        {
            eprintln!("warning: ignoring invalid {}: {err:#}", path.display());
        }
        if let Some(model_ids) = crate::auth::stored_oauth_model_ids("github-copilot") {
            registry.retain_provider_models("github-copilot", &model_ids);
        }
        registry
    }

    pub fn from_builtin() -> Registry {
        let mut r = Registry {
            models: Vec::new(),
            model_indices: HashMap::new(),
            manually_configured_providers: BTreeSet::new(),
            declared_keys: BTreeMap::new(),
            auth_providers: BTreeMap::new(),
        };
        r.merge_builtins();
        r
    }

    /// Load Radius's authenticated, dynamic model catalog when a Radius
    /// credential is available. Radius has no static models in Pi.
    pub async fn refresh_radius(&mut self) {
        let Ok(Some(api_key)) =
            crate::auth::resolve_api_key_async("radius", &self.declared_keys).await
        else {
            return;
        };
        let gateway =
            std::env::var("RADIUS_GATEWAY").unwrap_or_else(|_| "https://radius.pi.dev".into());
        if let Err(error) = self.refresh_radius_from(&gateway, &api_key).await {
            eprintln!("warning: could not refresh Radius models: {error:#}");
        }
    }

    /// Load Cursor's authenticated account model catalog when Cursor is the
    /// selected provider. The embedded fallback remains available if this
    /// request fails.
    pub async fn refresh_cursor(&mut self) {
        let Ok(Some(access_token)) =
            crate::auth::resolve_api_key_async("cursor", &self.declared_keys).await
        else {
            return;
        };
        match crate::api::cursor::discover_models(&access_token).await {
            Ok(models) => {
                for model in models {
                    self.upsert(model);
                }
            }
            Err(error) => eprintln!("warning: could not refresh Cursor models: {error:#}"),
        }
    }

    /// Load all built-in model services visible in a Databricks workspace.
    /// The embedded models remain available when discovery cannot run.
    pub async fn refresh_databricks_unity_gateway(&mut self) {
        let Some(workspace) =
            crate::auth::provider_env("databricks-unity-gateway", "DATABRICKS_HOST")
        else {
            return;
        };
        let Ok(Some(token)) =
            crate::auth::resolve_api_key_async("databricks-unity-gateway", &self.declared_keys)
                .await
        else {
            return;
        };
        if let Err(error) = self
            .refresh_databricks_unity_gateway_from(&workspace, &token)
            .await
        {
            eprintln!("warning: could not refresh Databricks Unity Gateway models: {error:#}");
        }
    }

    async fn refresh_databricks_unity_gateway_from(
        &mut self,
        workspace: &str,
        token: &str,
    ) -> Result<()> {
        let workspace = workspace.trim_end_matches('/');
        let mut page_token: Option<String> = None;
        let mut seen_page_tokens = BTreeSet::new();
        let mut model_ids = BTreeSet::new();
        for _ in 0..100 {
            let mut url = url::Url::parse(&format!(
                "{workspace}/api/2.1/unity-catalog/model-services"
            ))
            .with_context(|| {
                format!(
                    "the Databricks workspace URL '{workspace}' is invalid; use an HTTP or HTTPS workspace root and retry login"
                )
            })?;
            url.query_pairs_mut()
                .append_pair("parent", "schemas/system.ai")
                .append_pair("page_size", "100");
            if let Some(token) = page_token.as_deref() {
                url.query_pairs_mut().append_pair("page_token", token);
            }
            let response = crate::stream::http_client()
                .get(url.clone())
                .bearer_auth(token)
                .header("accept", "application/json")
                .send()
                .await
                .with_context(|| format!("request Databricks model services from {url}"))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!(
                    "Databricks model discovery returned HTTP {status}: {}; verify the token has USE CATALOG on system plus USE SCHEMA and EXECUTE on system.ai, then retry",
                    crate::truncate_err(&body)
                );
            }
            let page: DatabricksModelServicesPage = response
                .json()
                .await
                .context("parse the Databricks model-services response")?;
            for service in page.model_services {
                let id = service
                    .name
                    .trim()
                    .strip_prefix("model-services/")
                    .unwrap_or(service.name.trim());
                if id.starts_with("system.ai.") {
                    model_ids.insert(id.to_string());
                }
            }
            page_token = page.next_page_token.filter(|value| !value.is_empty());
            match page_token.as_ref() {
                Some(token) if !seen_page_tokens.insert(token.clone()) => anyhow::bail!(
                    "Databricks model discovery repeated page token '{token}'; retry after the workspace model catalog is stable"
                ),
                Some(_) => {}
                None => break,
            }
        }
        if page_token.is_some() {
            anyhow::bail!(
                "Databricks model discovery returned more than 100 pages; reduce the visible system.ai model services or use a custom model entry"
            );
        }
        if model_ids.is_empty() {
            anyhow::bail!(
                "Databricks model discovery returned no system.ai model services; grant USE CATALOG on system plus USE SCHEMA and EXECUTE on system.ai, then retry"
            );
        }

        let model_ids = model_ids.into_iter().collect::<Vec<_>>();
        self.retain_provider_models("databricks-unity-gateway", &model_ids);
        for id in model_ids {
            let lower = id.to_ascii_lowercase();
            let (api, base_url, input, context_window, headers) = if lower.contains("claude-") {
                (
                    "anthropic-messages",
                    format!("{workspace}/ai-gateway/anthropic"),
                    vec!["text".into(), "image".into()],
                    200_000,
                    BTreeMap::from([("x-databricks-use-coding-agent-mode".into(), "true".into())]),
                )
            } else if lower.contains("gemini-") {
                (
                    "google-generative-ai",
                    format!("{workspace}/ai-gateway/gemini/v1beta"),
                    vec!["text".into(), "image".into()],
                    1_000_000,
                    BTreeMap::new(),
                )
            } else {
                (
                    "openai-responses",
                    format!("{workspace}/ai-gateway/codex/v1"),
                    if lower.contains("gpt-") {
                        vec!["text".into(), "image".into()]
                    } else {
                        vec!["text".into()]
                    },
                    128_000,
                    BTreeMap::new(),
                )
            };
            self.upsert(Model {
                name: format!("Databricks {id}"),
                id,
                api: api.into(),
                provider: "databricks-unity-gateway".into(),
                base_url,
                reasoning: true,
                input,
                cost: ModelCost::default(),
                prompt_cache: None,
                context_window,
                max_tokens: 16_384,
                compat: None,
                thinking_level_map: BTreeMap::new(),
                headers,
            });
        }
        Ok(())
    }

    async fn refresh_radius_from(&mut self, gateway: &str, api_key: &str) -> Result<()> {
        let url = format!("{}/v1/config", gateway.trim_end_matches('/'));
        let response = crate::stream::http_client()
            .get(&url)
            .bearer_auth(api_key)
            .header("accept", "application/json")
            .send()
            .await
            .with_context(|| format!("request Radius config from {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Radius config returned HTTP {status}: {}",
                crate::truncate_err(&body)
            );
        }
        let config: RadiusGatewayConfig = response
            .json()
            .await
            .context("parse Radius gateway config")?;
        self.merge_radius_config(config);
        Ok(())
    }

    fn merge_radius_config(&mut self, config: RadiusGatewayConfig) {
        for model in config.models {
            self.upsert(Model {
                id: model.id,
                name: model.name,
                api: "pi-messages".into(),
                provider: "radius".into(),
                base_url: config.base_url.clone(),
                reasoning: model.reasoning,
                input: model.input,
                cost: model.cost,
                prompt_cache: None,
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                compat: None,
                thinking_level_map: BTreeMap::new(),
                headers: BTreeMap::new(),
            });
        }
    }

    fn merge_builtins(&mut self) {
        for catalog in BUILTIN_PROVIDER_CATALOGS {
            self.merge_generated_catalog(catalog)
                .expect("embedded Pi provider catalog must parse");
        }
    }

    fn merge_generated_catalog(&mut self, text: &str) -> Result<()> {
        let catalog: BTreeMap<String, BTreeMap<String, Model>> =
            serde_json::from_str(text).context("parse generated Pi model catalog")?;
        for models in catalog.into_values() {
            for (_, mut model) in models {
                // Mistral models also support its OpenAI-compatible chat API.
                // The native Conversations API is not needed by this harness.
                if model.api == "mistral-conversations" {
                    model.api = "openai-completions".into();
                    model.base_url = "https://api.mistral.ai/v1".into();
                    if model.id.starts_with("mistral-medium-") || model.id == "zai-glm-5-2" {
                        model
                            .compat
                            .get_or_insert_default()
                            .supports_reasoning_effort = Some(true);
                    }
                }
                expand_environment_placeholders(&model.provider, &mut model.base_url);
                self.upsert(model);
            }
        }
        Ok(())
    }

    fn upsert(&mut self, model: Model) {
        let key = (model.provider.clone(), model.id.clone());
        if let Some(&index) = self.model_indices.get(&key) {
            self.models[index] = model;
        } else {
            self.model_indices.insert(key, self.models.len());
            self.models.push(model);
        }
    }

    fn retain_provider_models(&mut self, provider: &str, model_ids: &[String]) {
        let model_ids = model_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        self.models
            .retain(|model| model.provider != provider || model_ids.contains(model.id.as_str()));
        self.model_indices = self
            .models
            .iter()
            .enumerate()
            .map(|(index, model)| ((model.provider.clone(), model.id.clone()), index))
            .collect();
    }

    fn merge_catalog(&mut self, text: &str) -> Result<()> {
        let catalog: CatalogFile = serde_json::from_str(text).context("parse model catalog")?;
        for (provider_id, provider) in catalog.providers {
            self.manually_configured_providers
                .insert(provider_id.clone());
            if let Some(key) = &provider.api_key {
                self.declared_keys.insert(provider_id.clone(), key.clone());
            }
            if let Some(auth_provider) = &provider.auth_provider {
                self.auth_providers
                    .insert(provider_id.clone(), auth_provider.clone());
            }
            for m in provider.models {
                let model = Model {
                    id: m.id.clone(),
                    name: m.name.or_else(|| provider.name.clone()).unwrap_or_default(),
                    api: m.api.unwrap_or_else(|| provider.api.clone()),
                    provider: provider_id.clone(),
                    base_url: m.base_url.unwrap_or_else(|| provider.base_url.clone()),
                    reasoning: m.reasoning.unwrap_or(false),
                    input: m.input.unwrap_or_else(|| vec!["text".into()]),
                    cost: m.cost.unwrap_or_default(),
                    prompt_cache: m.prompt_cache,
                    context_window: m.context_window.unwrap_or(128_000),
                    max_tokens: m.max_tokens.unwrap_or(16_384),
                    compat: match (provider.compat.clone(), m.compat) {
                        (Some(base), Some(overrides)) => Some(base.overlay(overrides)),
                        (base, overrides) => overrides.or(base),
                    },
                    thinking_level_map: m.thinking_level_map,
                    headers: if m.headers.is_empty() {
                        provider.headers.clone()
                    } else {
                        let mut headers = provider.headers.clone();
                        headers.extend(m.headers);
                        headers
                    },
                };
                // Overlay entries replace built-ins with the same provider/id.
                self.upsert(model);
            }
        }
        Ok(())
    }

    pub fn all(&self) -> &[Model] {
        &self.models
    }

    /// Models from providers that the user configured or can authenticate.
    /// The index is the model's position in `all()`.
    pub fn available_models(&self) -> Vec<(usize, &Model)> {
        let no_declared_keys = BTreeMap::new();
        let authenticated: BTreeSet<String> = self
            .models
            .iter()
            .map(|model| self.credential_provider(&model.provider))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|provider| {
                crate::auth::resolve_api_key_local(provider, &no_declared_keys).is_some()
            })
            .map(str::to_owned)
            .collect();
        self.available_models_for(&authenticated)
    }

    fn available_models_for(&self, authenticated: &BTreeSet<String>) -> Vec<(usize, &Model)> {
        self.models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                self.manually_configured_providers.contains(&model.provider)
                    || authenticated.contains(self.credential_provider(&model.provider))
            })
            .collect()
    }

    /// Provider whose credentials authenticate requests for `provider`.
    pub fn credential_provider<'a>(&'a self, provider: &'a str) -> &'a str {
        self.auth_providers
            .get(provider)
            .map(String::as_str)
            .unwrap_or(provider)
    }

    /// Resolve a model pattern: `provider/id`, exact id, then case-insensitive
    /// substring. An optional `:<thinking>` suffix selects a thinking level.
    pub fn resolve(
        &self,
        pattern: &str,
        provider: Option<&str>,
    ) -> Option<(Model, Option<ThinkingLevel>)> {
        let (pattern, thinking) = split_thinking_suffix(pattern);
        let (provider, pattern) = match pattern.split_once('/') {
            Some((p, rest)) if self.models.iter().any(|m| m.provider == p) => (Some(p), rest),
            _ => (provider, pattern),
        };
        let candidates: Vec<&Model> = self
            .models
            .iter()
            .filter(|m| provider.is_none_or(|p| m.provider == p))
            .collect();
        let found = candidates
            .iter()
            .find(|m| m.id == pattern)
            .or_else(|| {
                let lower = pattern.to_lowercase();
                candidates.iter().find(|m| {
                    m.id.to_lowercase().contains(&lower) || m.name.to_lowercase().contains(&lower)
                })
            })
            .map(|m| (*m).clone());
        found.map(|m| (m, thinking))
    }

    /// Match models against comma-separated glob-ish patterns (`claude-*`).
    pub fn match_patterns(&self, patterns: &[String]) -> Vec<Model> {
        let mut out = Vec::new();
        for m in &self.models {
            for pat in patterns {
                if pattern_matches(pat.trim(), &m.id)
                    || pattern_matches(pat.trim(), &format!("{}/{}", m.provider, m.id))
                {
                    out.push(m.clone());
                    break;
                }
            }
        }
        out
    }
}

fn expand_environment_placeholders(provider: &str, value: &mut String) {
    for (placeholder, variable) in [
        ("{CLOUDFLARE_ACCOUNT_ID}", "CLOUDFLARE_ACCOUNT_ID"),
        ("{CLOUDFLARE_GATEWAY_ID}", "CLOUDFLARE_GATEWAY_ID"),
        ("{DATABRICKS_HOST}", "DATABRICKS_HOST"),
        ("{SNOWFLAKE_CORTEX_BASE_URL}", "SNOWFLAKE_CORTEX_BASE_URL"),
        ("{location}", "GOOGLE_CLOUD_LOCATION"),
    ] {
        if value.contains(placeholder)
            && let Some(replacement) = crate::auth::provider_env(provider, variable)
        {
            *value = value.replace(placeholder, replacement.trim_end_matches('/'));
        }
    }
}

fn split_thinking_suffix(pattern: &str) -> (&str, Option<ThinkingLevel>) {
    if let Some((head, tail)) = pattern.rsplit_once(':')
        && let Some(level) = ThinkingLevel::parse(tail)
    {
        return (head, Some(level));
    }
    (pattern, None)
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    // Simple `*` glob, case-insensitive.
    let pattern = pattern.to_lowercase();
    let value = value.to_lowercase();
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return value.contains(&pattern);
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match value[pos..].find(part) {
            Some(found) => {
                if i == 0 && found != 0 {
                    return false;
                }
                pos += found + part.len();
            }
            None => return false,
        }
    }
    if let Some(last) = parts.last()
        && !last.is_empty()
        && !value.ends_with(last)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    #[test]
    fn builtin_catalog_loads() {
        let r = Registry::from_builtin();
        assert!(r.all().len() > 1_000);
        assert!(r.all().iter().any(|m| m.provider == "anthropic"));
        assert!(r.all().iter().any(|m| m.provider == "openai"));
        assert!(r.all().iter().any(|m| m.provider == "google"));
        assert!(r.all().iter().any(|m| m.provider == "openai-codex"));
        let providers: std::collections::BTreeSet<_> = r
            .all()
            .iter()
            .map(|model| model.provider.as_str())
            .collect();
        for provider in BUILTIN_PROVIDER_IDS {
            assert!(providers.contains(provider), "missing provider {provider}");
        }
        let unsupported: std::collections::BTreeSet<_> = r
            .all()
            .iter()
            .map(|model| model.api.as_str())
            .filter(|api| {
                !matches!(
                    *api,
                    "anthropic-messages"
                        | "bedrock-converse-stream"
                        | "cursor-agent"
                        | "google-generative-ai"
                        | "google-vertex"
                        | "openai-completions"
                        | "openai-responses"
                        | "openai-codex-responses"
                        | "azure-openai-responses"
                        | "pi-messages"
                )
            })
            .collect();
        assert!(unsupported.is_empty(), "unsupported APIs: {unsupported:?}");
    }

    #[test]
    fn pi_0860_catalog_changes_are_present() {
        let registry = Registry::from_builtin();
        for provider in ["openai", "openai-codex"] {
            let (model, _) = registry
                .resolve(&format!("{provider}/gpt-6-astra"), None)
                .expect("GPT-6 Astra");
            assert_eq!(model.id, "gpt-6-astra");
            assert!(!model.cost.tiers.is_empty());
        }
        for provider in [
            "qwen-token-plan",
            "qwen-token-plan-cn",
            "qwen-token-plan-individual",
        ] {
            assert!(
                registry
                    .resolve(&format!("{provider}/qwen3.8-flash"), None)
                    .is_some(),
                "missing Qwen3.8 Flash for {provider}"
            );
        }
        assert!(registry.resolve("xai/grok-build-0.1", None).is_none());

        let (baseten, _) = registry
            .resolve("baseten/zai-org/GLM-5.2", None)
            .expect("Baseten GLM-5.2");
        assert!(!baseten.supports_images());

        let (fable, _) = registry
            .resolve("github-copilot/claude-fable-5", None)
            .expect("Copilot Claude Fable 5");
        assert_eq!(fable.api, "anthropic-messages");
        assert_eq!(
            fable
                .compat
                .as_ref()
                .and_then(|compat| compat.force_adaptive_thinking),
            Some(true)
        );

        let (managed, _) = registry
            .resolve("anthropic/claude-fable-5-1", None)
            .expect("Anthropic Claude Fable 5.1");
        assert_eq!(
            managed.map_thinking_level(ThinkingLevel::Off),
            ThinkingLevel::Minimal
        );

        let (cached, _) = registry
            .resolve("anthropic/claude-opus-4-8", None)
            .expect("Anthropic Claude Opus 4.8");
        assert_eq!(cached.prompt_cache.and_then(|cache| cache.short), Some(300));
        assert!(registry.resolve("radius/balanced", None).is_some());
        assert!(registry.resolve("openai-codex/gpt-5.4", None).is_none());
    }

    #[test]
    fn deepseek_flash_is_in_the_catalog() {
        let registry = Registry::from_builtin();
        let (model, _) = registry
            .resolve("deepseek/deepseek-flash", None)
            .expect("DeepSeek Flash model");
        assert!(model.supports_images());
        assert_eq!(
            model.map_thinking_level(ThinkingLevel::Medium),
            ThinkingLevel::High
        );
        assert_eq!(
            model.map_thinking_level(ThinkingLevel::High),
            ThinkingLevel::High
        );
    }

    #[test]
    fn resolve_patterns() {
        let r = Registry::from_builtin();
        let (m, t) = r.resolve("sonnet", None).expect("sonnet resolves");
        assert!(m.id.contains("sonnet"));
        assert!(t.is_none());

        let (m, t) = r.resolve("sonnet:high", None).expect("thinking suffix");
        assert!(m.id.contains("sonnet"));
        assert_eq!(t, Some(ThinkingLevel::High));

        let (m, _) = r.resolve("openai/gpt", None).expect("provider prefix");
        assert_eq!(m.provider, "openai");
    }

    #[test]
    fn glob_patterns() {
        let r = Registry::from_builtin();
        let matched = r.match_patterns(&["claude-*".to_string()]);
        assert!(!matched.is_empty());
        assert!(matched.iter().all(|m| m.id.starts_with("claude-")));
    }

    #[test]
    fn account_model_filter_only_changes_selected_provider() {
        let mut registry = Registry::from_builtin();
        let keep = registry
            .all()
            .iter()
            .find(|model| model.provider == "github-copilot")
            .unwrap()
            .id
            .clone();
        registry.retain_provider_models("github-copilot", std::slice::from_ref(&keep));
        assert_eq!(
            registry
                .all()
                .iter()
                .filter(|model| model.provider == "github-copilot")
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            [keep.as_str()]
        );
        assert!(
            registry
                .all()
                .iter()
                .any(|model| model.provider == "openai")
        );
    }

    #[test]
    fn overlay_merges() {
        let mut r = Registry::from_builtin();
        r.merge_catalog(
            r#"{"providers": {"ollama": {"baseUrl": "http://localhost:11434/v1", "api": "openai-completions", "apiKey": "ollama", "models": [{"id": "llama3.1:8b"}]}}}"#,
        )
        .unwrap();
        let (m, _) = r.resolve("ollama/llama3.1:8b", None).expect("custom model");
        assert_eq!(m.api, "openai-completions");
        assert_eq!(
            r.declared_keys.get("ollama").map(String::as_str),
            Some("ollama")
        );
    }

    #[test]
    fn custom_model_can_override_api_and_headers() {
        let mut registry = Registry::from_builtin();
        registry
            .merge_catalog(
                r#"{"providers":{"custom":{"baseUrl":"http://localhost","api":"openai-completions","headers":{"x-provider":"one"},"models":[{"id":"mixed","api":"openai-responses","headers":{"x-model":"two"}}]}}}"#,
            )
            .unwrap();
        let (model, _) = registry.resolve("custom/mixed", None).unwrap();
        assert_eq!(model.api, "openai-responses");
        assert_eq!(model.headers["x-provider"], "one");
        assert_eq!(model.headers["x-model"], "two");
    }

    #[test]
    fn available_models_require_authentication_or_manual_configuration() {
        let mut registry = Registry::from_builtin();
        registry
            .merge_catalog(
                r#"{"providers":{"local":{"baseUrl":"http://localhost","api":"openai-completions","models":[{"id":"local-model"}]}}}"#,
            )
            .unwrap();
        let available = registry.available_models_for(&BTreeSet::from(["openai".into()]));
        assert!(
            available
                .iter()
                .any(|(_, model)| model.provider == "openai")
        );
        assert!(available.iter().any(|(_, model)| model.provider == "local"));
        assert!(
            !available
                .iter()
                .any(|(_, model)| model.provider == "anthropic")
        );
    }

    #[test]
    fn finish_reason_compat_deep_merges_at_provider_and_model_levels() {
        let mut registry = Registry::from_builtin();
        registry
            .merge_catalog(
                r#"{"providers":{"custom":{"baseUrl":"http://localhost","api":"openai-completions","compat":{"supportsFinishReason":true,"supportsUsageInStreaming":false},"models":[{"id":"strict"},{"id":"lenient","compat":{"supportsFinishReason":false}}]}}}"#,
            )
            .unwrap();
        let (strict, _) = registry.resolve("custom/strict", None).unwrap();
        let strict = strict.compat.unwrap();
        assert_eq!(strict.supports_finish_reason, Some(true));
        assert_eq!(strict.supports_usage_in_streaming, Some(false));

        let (lenient, _) = registry.resolve("custom/lenient", None).unwrap();
        let lenient = lenient.compat.unwrap();
        assert_eq!(lenient.supports_finish_reason, Some(false));
        assert_eq!(lenient.supports_usage_in_streaming, Some(false));
    }

    #[test]
    fn radius_dynamic_config_creates_pi_messages_models() {
        let mut registry = Registry::from_builtin();
        let config: RadiusGatewayConfig = serde_json::from_str(
            r#"{"baseUrl":"https://gateway.example/v1","models":[{"id":"gateway-model","name":"Gateway model","reasoning":true,"input":["text","image"],"cost":{"input":1.0,"output":2.0,"cacheRead":0.1,"cacheWrite":0.2},"contextWindow":200000,"maxTokens":32000}]}"#,
        )
        .unwrap();
        registry.merge_radius_config(config);
        let (model, _) = registry.resolve("radius/gateway-model", None).unwrap();
        assert_eq!(model.api, "pi-messages");
        assert_eq!(model.base_url, "https://gateway.example/v1");
        assert!(model.supports_images());
    }

    #[test]
    fn cursor_fallback_is_a_native_model() {
        let registry = Registry::from_builtin();
        let (model, _) = registry.resolve("cursor/auto", None).unwrap();
        assert_eq!(model.provider, "cursor");
        assert_eq!(model.api, "cursor-agent");
        assert!(model.supports_images());
        assert!(BUILTIN_PROVIDER_IDS.contains(&"cursor"));
    }

    #[test]
    fn databricks_gateway_uses_responses_and_anthropic_routes() {
        let registry = Registry::from_builtin();
        let (open, _) = registry
            .resolve("databricks-unity-gateway/system.ai.glm-5-2", None)
            .unwrap();
        assert_eq!(open.api, "openai-responses");
        assert!(open.base_url.ends_with("/ai-gateway/codex/v1"));

        let (claude, _) = registry
            .resolve("databricks-unity-gateway/system.ai.claude-sonnet-4-6", None)
            .unwrap();
        assert_eq!(claude.api, "anthropic-messages");
        assert!(claude.base_url.ends_with("/ai-gateway/anthropic"));
        assert_eq!(claude.headers["x-databricks-use-coding-agent-mode"], "true");
    }

    #[tokio::test]
    async fn databricks_discovery_loads_every_page_and_api_family() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (page, body) in [
                (
                    None,
                    r#"{"model_services":[{"name":"model-services/system.ai.claude-opus-5"},{"name":"model-services/system.ai.gpt-6-astra"}],"next_page_token":"next"}"#,
                ),
                (
                    Some("next"),
                    r#"{"model_services":[{"name":"model-services/system.ai.gemini-3-pro"},{"name":"model-services/main.private.model"}]}"#,
                ),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let count = socket.read(&mut chunk).await.unwrap();
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.contains("authorization: Bearer test-token"));
                assert!(request.contains("parent=schemas%2Fsystem.ai"));
                assert_eq!(request.contains("page_token=next"), page.is_some());
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });

        let mut registry = Registry::from_builtin();
        registry
            .refresh_databricks_unity_gateway_from(&format!("http://{address}"), "test-token")
            .await
            .unwrap();
        server.await.unwrap();

        let models = registry
            .all()
            .iter()
            .filter(|model| model.provider == "databricks-unity-gateway")
            .collect::<Vec<_>>();
        assert_eq!(models.len(), 3);
        assert_eq!(
            registry
                .resolve("databricks-unity-gateway/system.ai.claude-opus-5", None)
                .unwrap()
                .0
                .api,
            "anthropic-messages"
        );
        assert_eq!(
            registry
                .resolve("databricks-unity-gateway/system.ai.gemini-3-pro", None)
                .unwrap()
                .0
                .api,
            "google-generative-ai"
        );
        assert_eq!(
            registry
                .resolve("databricks-unity-gateway/system.ai.gpt-6-astra", None)
                .unwrap()
                .0
                .api,
            "openai-responses"
        );
    }

    #[test]
    fn snowflake_cortex_uses_chat_and_anthropic_routes() {
        let registry = Registry::from_builtin();
        assert_eq!(
            registry
                .all()
                .iter()
                .filter(|model| model.provider == "snowflake-cortex")
                .count(),
            34
        );
        let (open, _) = registry
            .resolve("snowflake-cortex/openai-gpt-5", None)
            .unwrap();
        assert_eq!(open.api, "openai-completions");
        assert!(open.base_url.ends_with("/v1"));

        let (claude, _) = registry
            .resolve("snowflake-cortex/claude-sonnet-4-5", None)
            .unwrap();
        assert_eq!(claude.api, "anthropic-messages");
        assert!(!claude.base_url.ends_with("/v1"));
        assert_eq!(claude.headers["snow-agent-name"], "kiss");
        for id in [
            "claude-opus-5",
            "claude-fable-5-1",
            "openai-gpt-6-astra",
            "deepseek-v4-flash",
            "snowflake-llama-3.3-70b",
        ] {
            assert!(
                registry
                    .resolve(&format!("snowflake-cortex/{id}"), None)
                    .is_some(),
                "missing Snowflake model {id}"
            );
        }
    }
}
