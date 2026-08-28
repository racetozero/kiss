//! Settings: global `~/.kiss/agent/settings.json` deep-merged with project
//! `.kiss/settings.json` (project wins).

use kiss_ai::Transport;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        RetrySettings {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProjectTrustDefault {
    #[default]
    Ask,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MermaidRendering {
    Off,
    Final,
    #[default]
    Streaming,
}

impl MermaidRendering {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Final => "final",
            Self::Streaming => "streaming",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MarkdownSettings {
    pub code_block_indent: Option<String>,
    pub mermaid: MermaidRendering,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<String>,
    pub transport: Transport,
    pub hide_thinking_block: bool,
    pub theme: Option<String>,
    pub quiet_startup: bool,
    pub default_project_trust: ProjectTrustDefault,
    pub compaction: CompactionSettings,
    pub retry: RetrySettings,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    pub session_dir: Option<String>,
    pub enabled_models: Option<Vec<String>>,
    pub external_editor: Option<String>,
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub themes: Vec<String>,
    pub enable_skill_commands: Option<bool>,
    /// `None` preserves the default-on behavior for existing settings files.
    pub auto_recap: Option<bool>,
    pub markdown: MarkdownSettings,
    /// Unknown keys survive load/save.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

pub fn global_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".kiss/agent/settings.json"))
}

pub fn project_settings_path(cwd: &Path) -> PathBuf {
    cwd.join(".kiss/settings.json")
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Deep-merge `overlay` onto `base` (objects merge, everything else replaces).
fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (k, v) in overlay_map {
                match base_map.get_mut(&k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

impl Settings {
    pub fn auto_recap_enabled(&self) -> bool {
        self.auto_recap.unwrap_or(true)
    }

    /// Load global settings, overlaying project settings when trusted.
    pub fn load(cwd: &Path, project_trusted: bool) -> Settings {
        let mut merged = global_settings_path()
            .and_then(|p| read_json(&p))
            .unwrap_or_else(|| Value::Object(Default::default()));
        if project_trusted && let Some(project) = read_json(&project_settings_path(cwd)) {
            deep_merge(&mut merged, project);
        }
        serde_json::from_value(merged).unwrap_or_default()
    }

    pub fn save_global(&self) -> anyhow::Result<()> {
        let path = global_settings_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deep_merge_nested() {
        let mut base =
            json!({"theme": "dark", "compaction": {"enabled": true, "reserveTokens": 16384}});
        deep_merge(&mut base, json!({"compaction": {"reserveTokens": 8192}}));
        assert_eq!(base["theme"], "dark");
        assert_eq!(base["compaction"]["enabled"], true);
        assert_eq!(base["compaction"]["reserveTokens"], 8192);
    }

    #[test]
    fn defaults_match_pi() {
        let s = Settings::default();
        assert_eq!(s.compaction.reserve_tokens, 16_384);
        assert_eq!(s.compaction.keep_recent_tokens, 20_000);
        assert_eq!(s.retry.max_retries, 3);
        assert_eq!(s.steering_mode, QueueMode::OneAtATime);
        assert!(s.auto_recap_enabled());
        assert_eq!(s.markdown.mermaid, MermaidRendering::Streaming);
    }

    #[test]
    fn queue_mode_wire_format() {
        assert_eq!(
            serde_json::to_value(QueueMode::OneAtATime).unwrap(),
            "one-at-a-time"
        );
    }

    #[test]
    fn markdown_settings_use_the_pi_wire_shape() {
        let value = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(value["markdown"]["mermaid"], "streaming");
        assert!(value.get("mermaidRendering").is_none());
    }
}
