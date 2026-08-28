//! Model catalog: built-in entries plus a `~/.kiss/agent/models.json`
//! overlay for custom providers (Ollama, vLLM, proxies, ...).

use crate::model::{Model, ModelCost, OpenAICompat};
use crate::types::ThinkingLevel;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Generated model data verified against `@earendil-works/pi-ai` 0.84.3.
const BUILTIN_PROVIDER_CATALOGS: &[&str] = &[
    include_str!("../data/providers/amazon-bedrock.json"),
    include_str!("../data/providers/ant-ling.json"),
    include_str!("../data/providers/anthropic.json"),
    include_str!("../data/providers/azure-openai-responses.json"),
    include_str!("../data/providers/baseten.json"),
    include_str!("../data/providers/cerebras.json"),
    include_str!("../data/providers/cloudflare-ai-gateway.json"),
    include_str!("../data/providers/cloudflare-workers-ai.json"),
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
    compat: Option<OpenAICompat>,
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

#[derive(Debug, Clone)]
pub struct Registry {
    models: Vec<Model>,
    /// Placeholder API keys declared in models.json (e.g. "ollama").
    pub declared_keys: BTreeMap<String, String>,
}

impl Registry {
    /// Load built-ins plus the user overlay file if present.
    pub fn load(custom_path: Option<&Path>) -> Registry {
        let mut registry = Registry {
            models: Vec::new(),
            declared_keys: BTreeMap::new(),
        };
        registry.merge_builtins();
        let overlay = custom_path
            .map(|p| p.to_path_buf())
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
            declared_keys: BTreeMap::new(),
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
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                compat: None,
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
                }
                expand_environment_placeholders(&model.provider, &mut model.base_url);
                self.upsert(model);
            }
        }
        Ok(())
    }

    fn upsert(&mut self, model: Model) {
        if let Some(existing) = self
            .models
            .iter_mut()
            .find(|entry| entry.provider == model.provider && entry.id == model.id)
        {
            *existing = model;
        } else {
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
    }

    fn merge_catalog(&mut self, text: &str) -> Result<()> {
        let catalog: CatalogFile = serde_json::from_str(text).context("parse model catalog")?;
        for (provider_id, provider) in catalog.providers {
            if let Some(key) = &provider.api_key {
                self.declared_keys.insert(provider_id.clone(), key.clone());
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
                    context_window: m.context_window.unwrap_or(128_000),
                    max_tokens: m.max_tokens.unwrap_or(16_384),
                    compat: match (provider.compat.clone(), m.compat) {
                        (Some(base), Some(overrides)) => Some(base.overlay(overrides)),
                        (base, overrides) => overrides.or(base),
                    },
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
        ("{location}", "GOOGLE_CLOUD_LOCATION"),
    ] {
        if value.contains(placeholder)
            && let Some(replacement) = crate::auth::provider_env(provider, variable)
        {
            *value = value.replace(placeholder, &replacement);
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
            if *provider != "radius" {
                assert!(providers.contains(provider), "missing provider {provider}");
            }
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
}
