//! Model descriptors.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    /// USD per million input tokens.
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

/// Compatibility overrides for OpenAI-compatible chat-completions servers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenAICompat {
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub supports_usage_in_streaming: Option<bool>,
    pub supports_finish_reason: Option<bool>,
    pub max_tokens_field: Option<String>,
}

impl OpenAICompat {
    pub fn overlay(self, override_values: Self) -> Self {
        Self {
            supports_developer_role: override_values
                .supports_developer_role
                .or(self.supports_developer_role),
            supports_reasoning_effort: override_values
                .supports_reasoning_effort
                .or(self.supports_reasoning_effort),
            supports_usage_in_streaming: override_values
                .supports_usage_in_streaming
                .or(self.supports_usage_in_streaming),
            supports_finish_reason: override_values
                .supports_finish_reason
                .or(self.supports_finish_reason),
            max_tokens_field: override_values.max_tokens_field.or(self.max_tokens_field),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// API dialect: "anthropic-messages", "openai-completions",
    /// "openai-responses", "openai-codex-responses",
    /// "azure-openai-responses", "google-generative-ai", or
    /// "google-vertex".
    pub api: String,
    pub provider: String,
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default = "default_input")]
    pub input: Vec<String>,
    #[serde(default)]
    pub cost: ModelCost,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<OpenAICompat>,
    /// Extra headers sent with every request for this model.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
}

fn default_input() -> Vec<String> {
    vec!["text".to_string()]
}
fn default_context_window() -> u64 {
    128_000
}
fn default_max_tokens() -> u64 {
    16_384
}

impl Model {
    pub fn supports_images(&self) -> bool {
        self.input.iter().any(|i| i == "image")
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}
