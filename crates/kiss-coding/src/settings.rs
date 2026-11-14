//! Settings: global `~/.kiss/agent/settings.json` deep-merged with project
//! `.kiss/settings.json` (project wins).

use crate::decision::DecisionModel;
use kiss_ai::Transport;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompactionMode {
    #[default]
    Summary,
    Jev,
    #[serde(rename = "d1.1", alias = "d1")]
    D1,
    #[serde(rename = "d1-ultra")]
    D1Ultra,
    Clef,
    #[serde(rename = "clef-flash")]
    ClefFlash,
}

/// A setting that is either off or names a decision model, in a fixed
/// cycling order (the compaction and reasoning-effort modes).
pub trait DecisionMode: Copy + PartialEq + 'static {
    const ALL: &'static [Self];

    fn decision_model(self) -> Option<DecisionModel>;

    /// The next mode after this one whose decision model is available. The
    /// off mode is always available. `None` when no other mode is.
    fn next_available(self, available: impl Fn(DecisionModel) -> bool) -> Option<Self> {
        let index = Self::ALL.iter().position(|mode| *mode == self)?;
        Self::ALL
            .iter()
            .cycle()
            .skip(index + 1)
            .take(Self::ALL.len() - 1)
            .copied()
            .find(|mode| mode.decision_model().is_none_or(&available))
    }
}

impl DecisionMode for CompactionMode {
    const ALL: &'static [Self] = &[
        Self::Summary,
        Self::Jev,
        Self::D1Ultra,
        Self::D1,
        Self::ClefFlash,
        Self::Clef,
    ];

    fn decision_model(self) -> Option<DecisionModel> {
        match self {
            Self::Summary => None,
            Self::Jev => Some(DecisionModel::Jev),
            Self::D1 => Some(DecisionModel::D1),
            Self::D1Ultra => Some(DecisionModel::D1Ultra),
            Self::Clef => Some(DecisionModel::Clef),
            Self::ClefFlash => Some(DecisionModel::ClefFlash),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub mode: CompactionMode,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub model_overrides: BTreeMap<String, CompactionModelOverride>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionModelOverride {
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
}

impl CompactionSettings {
    fn model_override(&self, model: &kiss_ai::Model) -> Option<&CompactionModelOverride> {
        self.model_overrides
            .get(&format!("{}/{}", model.provider, model.id))
    }

    /// Tokens to keep free for the reply, with the model's override.
    pub fn reserve_tokens_for(&self, model: &kiss_ai::Model) -> u64 {
        self.model_override(model)
            .and_then(|value| value.reserve_tokens)
            .unwrap_or(self.reserve_tokens)
    }

    /// Recent tokens a compaction keeps verbatim, with the model's override.
    pub fn keep_recent_tokens_for(&self, model: &kiss_ai::Model) -> u64 {
        self.model_override(model)
            .and_then(|value| value.keep_recent_tokens)
            .unwrap_or(self.keep_recent_tokens)
    }
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings {
            enabled: true,
            mode: CompactionMode::Summary,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            model_overrides: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffortMode {
    #[default]
    Fixed,
    Jev,
    #[serde(rename = "d1.1", alias = "d1")]
    D1,
    #[serde(rename = "d1-ultra")]
    D1Ultra,
    Clef,
    #[serde(rename = "clef-flash")]
    ClefFlash,
}

impl DecisionMode for ReasoningEffortMode {
    const ALL: &'static [Self] = &[
        Self::Fixed,
        Self::Jev,
        Self::D1Ultra,
        Self::D1,
        Self::ClefFlash,
        Self::Clef,
    ];

    fn decision_model(self) -> Option<DecisionModel> {
        match self {
            Self::Fixed => None,
            Self::Jev => Some(DecisionModel::Jev),
            Self::D1 => Some(DecisionModel::D1),
            Self::D1Ultra => Some(DecisionModel::D1Ultra),
            Self::Clef => Some(DecisionModel::Clef),
            Self::ClefFlash => Some(DecisionModel::ClefFlash),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ReasoningEffortSettings {
    pub mode: ReasoningEffortMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_agent_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        RetrySettings {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
            max_agent_delay_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentSettings {
    /// Subagent tools are hidden until the user enables them.
    pub enabled: bool,
}

/// How many agents to advise the model to aim for when it writes a workflow.
///
/// This is advice sent to the model, not a cap: a prompt that clearly calls for
/// a different scale still wins. The runtime limits are the real bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowSize {
    Small,
    #[default]
    Medium,
    Large,
    Unrestricted,
}

impl WorkflowSize {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Unrestricted => "unrestricted",
        }
    }

    /// The agent count the model is asked to stay under, if any.
    pub fn target_agents(self) -> Option<u32> {
        match self {
            Self::Small => Some(5),
            Self::Medium => Some(15),
            Self::Large => Some(50),
            Self::Unrestricted => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkflowSettings {
    /// Dynamic workflows are built on subagents, so they are available only
    /// when `subagents.enabled` is also on.
    pub enabled: bool,
    /// Ask before a run starts. This is a cost gate, not a permission gate: one
    /// run can start hundreds of child agents.
    pub confirm: bool,
    /// Let a typed prompt containing `ultracode`, or asking for a workflow in
    /// plain words, start one.
    pub keyword_trigger: bool,
    pub size: WorkflowSize,
}

impl Default for WorkflowSettings {
    fn default() -> Self {
        WorkflowSettings {
            enabled: true,
            confirm: true,
            keyword_trigger: true,
            size: WorkflowSize::default(),
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
pub enum CacheWarmingMode {
    Off,
    Streaming,
    #[default]
    Idle,
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
pub struct WebMcpSettings {
    pub allowed_origins: Option<Vec<String>>,
    pub disallowed_origins: Vec<String>,
    pub cdp: Option<Value>,
}

/// Where `/share` uploads a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionShareDestination {
    /// Radius when signed in, otherwise a secret GitHub gist.
    #[default]
    Auto,
    Gist,
    Radius,
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
    /// User-only opt-in for interactive prompt-completion desktop notifications.
    pub desktop_notifications: bool,
    /// User-only argv that `kiss update` runs instead of the KISS installer.
    pub self_update_command: Option<Vec<String>>,
    pub default_project_trust: ProjectTrustDefault,
    pub compaction: CompactionSettings,
    /// Let the model edit its live conversation through a private JSON file.
    pub experimental_context_file: bool,
    pub reasoning_effort: ReasoningEffortSettings,
    pub retry: RetrySettings,
    pub cache_warming: CacheWarmingMode,
    pub subagents: SubagentSettings,
    pub workflows: WorkflowSettings,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    pub session_dir: Option<String>,
    pub session_share_destination: SessionShareDestination,
    pub enabled_models: Option<Vec<String>>,
    pub default_tools: Option<Vec<String>>,
    pub external_editor: Option<String>,
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub themes: Vec<String>,
    pub enable_skill_commands: Option<bool>,
    /// `None` preserves the default-on behavior for existing settings files.
    pub auto_recap: Option<bool>,
    pub voice_language: Option<String>,
    pub voice_backend: Option<String>,
    pub markdown: MarkdownSettings,
    pub webmcp: WebMcpSettings,
    /// Unknown keys survive load/save.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
    /// These settings as loaded, so a save writes only what changed since.
    /// Project values and run-only overrides are part of it, so they never
    /// reach the user's file unless the user changes them.
    /// Public only so that struct literals with `..Default::default()` keep
    /// working; `load` and `save_global` own it.
    #[serde(skip)]
    #[doc(hidden)]
    pub baseline: Value,
    /// What `load` could not read and ignored, for the caller to show.
    /// Public for the same reason as `baseline`.
    #[serde(skip)]
    #[doc(hidden)]
    pub problems: Vec<String>,
}

pub fn global_settings_path() -> Option<PathBuf> {
    kiss_ai::paths::agent_dir().map(|dir| dir.join("settings.json"))
}

pub fn project_settings_path(cwd: &Path) -> PathBuf {
    cwd.join(".kiss/settings.json")
}

fn read_json(path: &Path, problems: &mut Vec<String>) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text)
        .map_err(|error| {
            problems.push(format!(
                "{} is not valid JSON ({error}), so kiss ignores all of it. Correct the file and restart kiss.",
                path.display()
            ));
        })
        .ok()
}

fn merged_settings(mut global: Value, project: Option<Value>) -> Settings {
    let desktop_notifications = global
        .get("desktopNotifications")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // A project must not choose a command that `kiss update` runs.
    let self_update_command = global
        .get("selfUpdateCommand")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let experimental_context_file = global
        .get("experimentalContextFile")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let subagents_enabled = global
        .pointer("/subagents/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let workflows_enabled = global
        .pointer("/workflows/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    // A project must not opt the user's microphone in to a cloud service.
    let voice_backend = global
        .get("voiceBackend")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cache_warming = global
        .get("cacheWarming")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    if let Some(mut project) = project {
        if let Some(object) = project.as_object_mut() {
            object.remove("deviceId");
        }
        if let Some(entries) = project.get("defaultTools").and_then(Value::as_array)
            && entries.iter().all(|entry| {
                entry
                    .as_str()
                    .is_some_and(|name| name.starts_with(['+', '-']))
            })
            && let Some(base) = global.get("defaultTools").and_then(Value::as_array)
        {
            let mut combined = base.clone();
            combined.extend(entries.iter().cloned());
            project["defaultTools"] = Value::Array(combined);
        }
        kiss_ai::config_file::deep_merge(&mut global, project);
    }
    let mut problems = Vec::new();
    // One value of the wrong type must not reset every other setting: drop
    // only the keys that cannot be read.
    if serde_json::from_value::<Settings>(global.clone()).is_err()
        && let Some(object) = global.as_object_mut()
    {
        object.retain(|key, value| {
            let readable = serde_json::from_value::<Settings>(Value::Object(Map::from_iter([(
                key.clone(),
                value.clone(),
            )])))
            .is_ok();
            if !readable {
                problems.push(format!(
                    "the setting `{key}` has a value kiss cannot read, so kiss uses its default. Correct `{key}` in {} or the project's .kiss/settings.json.",
                    global_settings_path().map_or_else(|| "settings.json".into(), |path| path.display().to_string())
                ));
            }
            readable
        });
    }
    let mut settings: Settings = serde_json::from_value(global).unwrap_or_default();
    settings.problems = problems;
    // Subagents are a user-level authority choice. A repository must not turn
    // them on through a trusted project settings file.
    settings.subagents.enabled = subagents_enabled;
    settings.experimental_context_file = experimental_context_file;
    // Workflows spend the user's tokens many agents at a time, so the same rule
    // applies: only the user's own settings file decides whether they are on.
    settings.workflows.enabled = workflows_enabled;
    settings.cache_warming = cache_warming;
    settings.voice_backend = voice_backend;
    settings.desktop_notifications = desktop_notifications;
    settings.self_update_command = self_update_command;
    settings
}

/// Write into `target` every leaf where `new` differs from `old`, and remove
/// the keys that `new` no longer has. Keys that did not change keep the
/// value `target` already holds.
fn apply_changes(
    target: &mut Map<String, Value>,
    old: &Map<String, Value>,
    new: &Map<String, Value>,
) {
    for (key, value) in new {
        match (old.get(key), value) {
            (Some(previous), _) if previous == value => {}
            (Some(Value::Object(previous)), Value::Object(value)) => {
                let slot = target
                    .entry(key.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !slot.is_object() {
                    *slot = Value::Object(Map::new());
                }
                apply_changes(slot.as_object_mut().unwrap(), previous, value);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    for key in old.keys() {
        if !new.contains_key(key) {
            target.remove(key);
        }
    }
}

impl Settings {
    /// Read only user settings when creating the installation ID.
    pub fn get_or_create_device_id() -> anyhow::Result<String> {
        let path = global_settings_path().ok_or_else(|| anyhow::anyhow!(
            "ChatGPT login has no user settings path. A home directory is required. Set HOME and run login again."
        ))?;
        kiss_ai::config_file::with_lock(&path, || {
            let mut settings: Value = kiss_ai::config_file::read_json(&path)?;
            if settings.is_null() {
                settings = Value::Object(Map::new());
            }
            if !settings.is_object() {
                anyhow::bail!(
                    "ChatGPT login cannot update {}. Settings must contain a JSON object. Correct the JSON and run login again.",
                    path.display()
                );
            }
            if let Some(id) = settings.get("deviceId").and_then(Value::as_str)
                && uuid::Uuid::parse_str(id).is_ok()
            {
                return Ok(id.to_string());
            }
            let id = uuid::Uuid::new_v4().to_string();
            settings["deviceId"] = Value::String(id.clone());
            kiss_ai::config_file::write_atomic(
                &path,
                serde_json::to_string_pretty(&settings)?.as_bytes(),
            )?;
            Ok(id)
        })
    }

    pub fn auto_recap_enabled(&self) -> bool {
        self.auto_recap.unwrap_or(true)
    }

    /// Load global settings, overlaying project settings when trusted.
    pub fn load(cwd: &Path, project_trusted: bool) -> Settings {
        let mut problems = Vec::new();
        let global = global_settings_path()
            .and_then(|p| read_json(&p, &mut problems))
            .unwrap_or_else(|| Value::Object(Default::default()));
        let project = project_trusted
            .then(|| read_json(&project_settings_path(cwd), &mut problems))
            .flatten();
        let mut settings = Self::from_layers(global, project);
        problems.append(&mut settings.problems);
        settings.problems = problems;
        settings
    }

    fn from_layers(global: Value, project: Option<Value>) -> Settings {
        let mut settings = merged_settings(global, project);
        settings.baseline = serde_json::to_value(&settings).unwrap_or_default();
        settings
    }

    /// Change a setting for this run only, for example from a CLI flag.
    /// `save_global` does not write it.
    pub fn override_for_run(&mut self, change: impl Fn(&mut Settings)) {
        change(self);
        if let Ok(mut baseline) = serde_json::from_value::<Settings>(self.baseline.clone()) {
            change(&mut baseline);
            self.baseline = serde_json::to_value(&baseline).unwrap_or_default();
        }
    }

    /// Write the settings changed since `load` (or the last save) into the
    /// user's settings file, leaving every other key in that file as it is.
    pub fn save_global(&mut self) -> anyhow::Result<()> {
        let path = global_settings_path().ok_or_else(|| anyhow::anyhow!(
            "Settings cannot be saved because the home directory is unknown. Set HOME, then try again."
        ))?;
        self.save_to(&path)
    }

    fn save_to(&mut self, path: &Path) -> anyhow::Result<()> {
        let current = serde_json::to_value(&*self)?;
        kiss_ai::config_file::with_lock(path, || {
            let mut disk: Value = kiss_ai::config_file::read_json(path)?;
            if disk.is_null() {
                disk = Value::Object(Map::new());
            }
            let Some(target) = disk.as_object_mut() else {
                anyhow::bail!(
                    "{} must contain a JSON object. Correct the file, then try again.",
                    path.display()
                );
            };
            let empty = Map::new();
            apply_changes(
                target,
                self.baseline.as_object().unwrap_or(&empty),
                current.as_object().unwrap_or(&empty),
            );
            kiss_ai::config_file::write_atomic(
                path,
                serde_json::to_string_pretty(&disk)?.as_bytes(),
            )
        })?;
        self.baseline = current;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_unreadable_value_keeps_the_other_settings_and_is_reported() {
        let settings = Settings::from_layers(
            json!({"defaultModel": 5, "defaultProvider": "openai"}),
            None,
        );
        assert_eq!(settings.default_provider.as_deref(), Some("openai"));
        assert_eq!(settings.default_model, None);
        assert_eq!(settings.problems.len(), 1);
        assert!(settings.problems[0].contains("defaultModel"));
    }

    #[test]
    fn a_save_writes_only_changed_settings_and_never_project_or_run_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let global = json!({"theme": "dark", "deviceId": "id", "futureKey": 1});
        std::fs::write(&path, global.to_string()).unwrap();
        let mut settings = Settings::from_layers(
            global,
            Some(json!({"shellCommandPrefix": "source .envrc", "quietStartup": true})),
        );
        settings.override_for_run(|settings| settings.experimental_context_file = true);

        settings.hide_thinking_block = true;
        settings.save_to(&path).unwrap();

        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            json!({"theme": "dark", "deviceId": "id", "futureKey": 1, "hideThinkingBlock": true})
        );
    }

    #[test]
    fn a_save_refuses_to_replace_a_settings_file_it_cannot_parse() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let mut settings = Settings::from_layers(json!({}), None);
        settings.quiet_startup = true;
        assert!(settings.save_to(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
    use serde_json::json;

    #[test]
    fn project_tool_modifiers_preserve_user_selection() {
        let user = json!({"defaultTools":["read", "+grep"]});
        let settings =
            merged_settings(user.clone(), Some(json!({"defaultTools":["-read", "+ls"]})));
        assert_eq!(
            settings.default_tools.unwrap(),
            ["read", "+grep", "-read", "+ls"]
        );
        let settings = merged_settings(user.clone(), Some(json!({"defaultTools":["bash"]})));
        assert_eq!(settings.default_tools.unwrap(), ["bash"]);
        let settings = merged_settings(user, Some(json!({"defaultTools":[]})));
        assert_eq!(settings.default_tools.unwrap(), ["read", "+grep"]);
        let settings = merged_settings(
            json!({"deviceId":"user-id"}),
            Some(json!({"deviceId":"project-id"})),
        );
        assert_eq!(settings.extra["deviceId"], "user-id");
    }

    #[test]
    fn only_user_settings_choose_the_update_command() {
        let project = Some(json!({"selfUpdateCommand": ["sh", "-c", "evil"]}));
        assert_eq!(
            merged_settings(json!({}), project.clone()).self_update_command,
            None
        );
        assert_eq!(
            merged_settings(
                json!({"selfUpdateCommand": ["brew", "upgrade", "kiss"]}),
                project
            )
            .self_update_command,
            Some(vec!["brew".into(), "upgrade".into(), "kiss".into()])
        );
    }

    #[test]
    fn desktop_notifications_require_user_opt_in() {
        assert!(!Settings::default().desktop_notifications);
        let old: Settings = serde_json::from_value(json!({})).unwrap();
        assert!(!old.desktop_notifications);
        assert!(
            !merged_settings(json!({}), Some(json!({"desktopNotifications": true})))
                .desktop_notifications
        );
        for enabled in [false, true] {
            let settings = merged_settings(
                json!({"desktopNotifications": enabled}),
                Some(json!({"desktopNotifications": !enabled})),
            );
            assert_eq!(settings.desktop_notifications, enabled);
            let saved = serde_json::to_value(settings).unwrap();
            assert_eq!(saved["desktopNotifications"], enabled);
            let restored: Settings = serde_json::from_value(saved).unwrap();
            assert_eq!(restored.desktop_notifications, enabled);
        }
    }

    #[test]
    fn project_cannot_enable_cloud_voice() {
        let settings = merged_settings(json!({}), Some(json!({"voiceBackend":"deepgram"})));
        assert_eq!(settings.voice_backend, None);
        let settings = merged_settings(
            json!({"voiceBackend":"elevenlabs"}),
            Some(json!({"voiceBackend":"local"})),
        );
        assert_eq!(settings.voice_backend.as_deref(), Some("elevenlabs"));
    }

    #[test]
    fn deep_merge_nested() {
        let mut base =
            json!({"theme": "dark", "compaction": {"enabled": true, "reserveTokens": 16384}});
        kiss_ai::config_file::deep_merge(&mut base, json!({"compaction": {"reserveTokens": 8192}}));
        assert_eq!(base["theme"], "dark");
        assert_eq!(base["compaction"]["enabled"], true);
        assert_eq!(base["compaction"]["reserveTokens"], 8192);
    }

    #[test]
    fn defaults_match_pi() {
        let s = Settings::default();
        assert_eq!(s.compaction.mode, CompactionMode::Summary);
        assert!(!s.experimental_context_file);
        assert_eq!(s.compaction.reserve_tokens, 16_384);
        assert_eq!(s.compaction.keep_recent_tokens, 20_000);
        assert_eq!(s.reasoning_effort.mode, ReasoningEffortMode::Fixed);
        assert_eq!(s.retry.max_retries, 3);
        assert_eq!(s.retry.max_agent_delay_ms, 60_000);
        assert_eq!(s.cache_warming, CacheWarmingMode::Idle);
        assert!(!s.subagents.enabled);
        assert_eq!(s.steering_mode, QueueMode::OneAtATime);
        assert!(s.auto_recap_enabled());
        assert_eq!(s.markdown.mermaid, MermaidRendering::Streaming);
    }

    #[test]
    fn compaction_model_overrides_use_exact_provider_model_keys() {
        let settings: Settings = serde_json::from_value(json!({
            "compaction": {
                "modelOverrides": {
                    "anthropic/claude-opus-4-8": {
                        "reserveTokens": 1000,
                        "keepRecentTokens": 2000
                    }
                }
            },
            "cacheWarming": "streaming"
        }))
        .unwrap();
        let override_settings = &settings.compaction.model_overrides["anthropic/claude-opus-4-8"];
        assert_eq!(override_settings.reserve_tokens, Some(1000));
        assert_eq!(override_settings.keep_recent_tokens, Some(2000));
        assert_eq!(settings.cache_warming, CacheWarmingMode::Streaming);
    }

    #[test]
    fn old_compaction_settings_default_to_summary_mode() {
        let settings: Settings = serde_json::from_value(json!({
            "compaction": {"enabled": true, "reserveTokens": 1, "keepRecentTokens": 2}
        }))
        .unwrap();
        assert_eq!(settings.compaction.mode, CompactionMode::Summary);
        assert_eq!(
            serde_json::to_value(settings).unwrap()["compaction"]["mode"],
            "summary"
        );
    }

    #[test]
    fn old_settings_default_to_fixed_reasoning_effort() {
        let settings: Settings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(settings.reasoning_effort.mode, ReasoningEffortMode::Fixed);
        assert_eq!(
            serde_json::to_value(settings).unwrap()["reasoningEffort"]["mode"],
            "fixed"
        );
    }

    #[test]
    fn decision_models_roundtrip_and_can_differ_between_features() {
        for (wire, provider, model) in [
            ("jev", "typesafe", "jev-latest"),
            ("d1.1", "liquid", "d1.1"),
            ("d1-ultra", "liquid", "d1-ultra"),
            ("clef", "cloudflare-workers-ai", "clef"),
            ("clef-flash", "cloudflare-workers-ai", "clef-flash"),
        ] {
            let settings: Settings = serde_json::from_value(json!({
                "compaction": {"mode": wire}, "reasoningEffort": {"mode": wire}
            }))
            .unwrap();
            for selected in [
                settings.compaction.mode.decision_model(),
                settings.reasoning_effort.mode.decision_model(),
            ] {
                let selected = selected.unwrap();
                assert_eq!((selected.provider(), selected.id()), (provider, model));
            }
            let value = serde_json::to_value(settings).unwrap();
            assert_eq!(value["compaction"]["mode"], wire);
            assert_eq!(value["reasoningEffort"]["mode"], wire);
        }
        let alias: Settings = serde_json::from_value(json!({"compaction":{"mode":"d1"}})).unwrap();
        assert_eq!(alias.compaction.mode.decision_model().unwrap().id(), "d1.1");
        assert_eq!(
            serde_json::to_value(alias).unwrap()["compaction"]["mode"],
            "d1.1"
        );
        let settings = merged_settings(
            json!({"compaction":{"mode":"jev"}, "reasoningEffort":{"mode":"d1-ultra"}}),
            Some(json!({"compaction":{"mode":"clef-flash"}})),
        );
        assert_eq!(
            settings.compaction.mode.decision_model(),
            Some(DecisionModel::ClefFlash)
        );
        assert_eq!(
            settings.reasoning_effort.mode.decision_model(),
            Some(DecisionModel::D1Ultra)
        );
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

    #[test]
    fn webmcp_settings_use_the_reference_wire_shape() {
        let loaded: Settings = serde_json::from_value(json!({
            "webmcp": {
                "allowedOrigins": ["example.com"],
                "disallowedOrigins": ["blocked.example"],
                "cdp": 9333
            }
        }))
        .unwrap();
        assert_eq!(
            loaded.webmcp.allowed_origins,
            Some(vec!["example.com".to_string()])
        );
        assert_eq!(
            loaded.webmcp.disallowed_origins,
            vec!["blocked.example".to_string()]
        );
        assert_eq!(loaded.webmcp.cdp, Some(json!(9333)));
    }

    #[test]
    fn subagents_are_opt_in_on_the_wire() {
        let value = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(value["subagents"]["enabled"], false);

        let loaded: Settings = serde_json::from_value(json!({})).unwrap();
        assert!(!loaded.subagents.enabled);
    }

    #[test]
    fn workflows_default_on_but_only_matter_with_subagents() {
        let settings = Settings::default();
        assert!(settings.workflows.enabled);
        assert!(settings.workflows.confirm);
        assert!(settings.workflows.keyword_trigger);
        assert_eq!(settings.workflows.size, WorkflowSize::Medium);
        // Subagents stay off, so workflows are unavailable until the user opts
        // in to child agents.
        assert!(!settings.subagents.enabled);
    }

    #[test]
    fn project_settings_cannot_turn_workflows_on_or_off() {
        let off = merged_settings(
            json!({"workflows": {"enabled": false}}),
            Some(json!({"workflows": {"enabled": true}})),
        );
        assert!(!off.workflows.enabled);

        let on = merged_settings(json!({}), Some(json!({"workflows": {"enabled": false}})));
        assert!(on.workflows.enabled);
    }

    #[test]
    fn project_settings_cannot_enable_paid_cache_warming() {
        let settings = merged_settings(
            json!({"cacheWarming": "off"}),
            Some(json!({"cacheWarming": "idle"})),
        );
        assert_eq!(settings.cache_warming, CacheWarmingMode::Off);
    }

    #[test]
    fn experimental_context_file_is_a_user_setting() {
        for enabled in [false, true] {
            let settings = merged_settings(
                json!({"experimentalContextFile": enabled}),
                Some(json!({"experimentalContextFile": !enabled})),
            );
            assert_eq!(settings.experimental_context_file, enabled);
            assert_eq!(
                serde_json::to_value(settings).unwrap()["experimentalContextFile"],
                enabled
            );
        }
    }

    #[test]
    fn the_size_guideline_maps_to_an_agent_count() {
        assert_eq!(WorkflowSize::Small.target_agents(), Some(5));
        assert_eq!(WorkflowSize::Medium.target_agents(), Some(15));
        assert_eq!(WorkflowSize::Large.target_agents(), Some(50));
        assert_eq!(WorkflowSize::Unrestricted.target_agents(), None);
    }

    #[test]
    fn project_settings_cannot_change_subagent_authority() {
        let enabled = merged_settings(
            json!({"subagents": {"enabled": true}}),
            Some(json!({"subagents": {"enabled": false}})),
        );
        assert!(enabled.subagents.enabled);

        let disabled = merged_settings(json!({}), Some(json!({"subagents": {"enabled": true}})));
        assert!(!disabled.subagents.enabled);
    }
}
