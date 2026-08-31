//! Model descriptors.

use crate::ThinkingLevel;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    pub thinking_format: Option<String>,
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
            requires_reasoning_content_on_assistant_messages: override_values
                .requires_reasoning_content_on_assistant_messages
                .or(self.requires_reasoning_content_on_assistant_messages),
            thinking_format: override_values.thinking_format.or(self.thinking_format),
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
    /// Per-model translation from harness thinking levels to provider levels.
    /// A null value marks that input level as unsupported.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub thinking_level_map: BTreeMap<String, Option<String>>,
    /// Extra headers sent with every request for this model.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
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

    pub fn supported_thinking_levels(&self) -> Vec<ThinkingLevel> {
        const LEVELS: [ThinkingLevel; 7] = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ];
        if !self.reasoning {
            return vec![ThinkingLevel::Off];
        }
        LEVELS
            .into_iter()
            .filter(|level| match self.thinking_level_map.get(level.as_str()) {
                Some(None) => false,
                None if matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max) => false,
                _ => true,
            })
            .collect()
    }

    pub fn clamp_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        let supported = self.supported_thinking_levels();
        if supported.contains(&level) {
            return level;
        }
        supported
            .iter()
            .copied()
            .find(|candidate| *candidate > level)
            .or_else(|| {
                supported
                    .iter()
                    .rev()
                    .copied()
                    .find(|candidate| *candidate < level)
            })
            .unwrap_or(ThinkingLevel::Off)
    }

    pub fn map_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        if level == ThinkingLevel::Off {
            return level;
        }
        let level = self.clamp_thinking_level(level);
        match self.thinking_level_map.get(level.as_str()) {
            Some(None) => ThinkingLevel::Off,
            Some(Some(mapped)) if mapped.eq_ignore_ascii_case("none") => ThinkingLevel::Off,
            Some(Some(mapped)) => {
                ThinkingLevel::parse(&mapped.to_ascii_lowercase()).unwrap_or(level)
            }
            None => level,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model {
            id: "test".into(),
            name: "Test".into(),
            api: "openai-completions".into(),
            provider: "test".into(),
            base_url: "https://example.invalid".into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: ModelCost::default(),
            context_window: 1_000,
            max_tokens: 100,
            compat: None,
            thinking_level_map: BTreeMap::new(),
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn thinking_level_map_preserves_maps_and_disables_levels() {
        let mut model = model();
        assert_eq!(
            model.map_thinking_level(ThinkingLevel::High),
            ThinkingLevel::High
        );
        model
            .thinking_level_map
            .insert("high".into(), Some("medium".into()));
        model.thinking_level_map.insert("minimal".into(), None);
        model
            .thinking_level_map
            .insert("max".into(), Some("unknown".into()));
        assert_eq!(
            model.map_thinking_level(ThinkingLevel::High),
            ThinkingLevel::Medium
        );
        assert_eq!(
            model.map_thinking_level(ThinkingLevel::Minimal),
            ThinkingLevel::Low
        );
        assert_eq!(
            model.map_thinking_level(ThinkingLevel::Max),
            ThinkingLevel::Max
        );
    }
}
