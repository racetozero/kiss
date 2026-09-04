use kiss_agent::AgentMessage;
use kiss_ai::{
    ContentBlock, Context, Message, Model, ModelCost, OpenAICompat, StopReason, ThinkingLevel,
    ToolDef, Usage, UserContent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;
pub const MAX_PROMPT_BYTES: usize = 1024 * 1024;
pub const MAX_CHECKPOINT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TOOL_COUNT: usize = 128;
pub const MAX_TOOL_SCHEMA_BYTES: usize = 256 * 1024;
pub const MAX_TOOL_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_MODEL_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_HISTORY_BYTES: usize = MAX_CHECKPOINT_BYTES;
pub const MAX_METADATA_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_TURNS: usize = 64;
pub const DEFAULT_MAX_HISTORY: usize = 100;
pub const MAX_QUEUED_MESSAGES: usize = 128;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOptions {
    pub model: ModelInput,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub thinking_level: ThinkingLevel,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default = "default_max_history")]
    pub max_history_messages: usize,
    #[serde(default)]
    pub checkpoint: Option<Vec<u8>>,
}

fn default_max_turns() -> usize {
    DEFAULT_MAX_TURNS
}

fn default_max_history() -> usize {
    DEFAULT_MAX_HISTORY
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInput {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "host_api")]
    pub api: String,
    #[serde(default = "host_provider")]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default = "text_input")]
    pub input: Vec<String>,
    #[serde(default)]
    pub cost: ModelCost,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    #[serde(default = "default_model_tokens")]
    pub max_tokens: u64,
    pub compat: Option<OpenAICompat>,
    #[serde(default)]
    pub thinking_level_map: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn host_api() -> String {
    "host".into()
}
fn host_provider() -> String {
    "host".into()
}
fn text_input() -> Vec<String> {
    vec!["text".into()]
}
fn default_context_window() -> u64 {
    128_000
}
fn default_model_tokens() -> u64 {
    16_384
}

impl From<ModelInput> for Model {
    fn from(value: ModelInput) -> Self {
        Model {
            id: value.id,
            name: value.name,
            api: value.api,
            provider: value.provider,
            base_url: value.base_url,
            reasoning: value.reasoning,
            input: value.input,
            cost: value.cost,
            context_window: value.context_window,
            max_tokens: value.max_tokens,
            compat: value.compat,
            thinking_level_map: value.thinking_level_map,
            headers: value.headers,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PromptInput {
    Text(String),
    Object { content: UserContent },
}

impl PromptInput {
    pub fn into_content(self) -> UserContent {
        match self {
            Self::Text(text) => UserContent::Text(text),
            Self::Object { content } => content,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinitionInput {
    pub name: String,
    #[serde(default)]
    pub label: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub execution_mode: ToolExecutionMode,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRequest {
    pub model: Model,
    pub context: SerializableContext,
    pub reasoning: ThinkingLevel,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableContext {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}

impl From<&Context> for SerializableContext {
    fn from(value: &Context) -> Self {
        Self {
            system_prompt: value.system_prompt.clone(),
            messages: value.messages.clone(),
            tools: value.tools.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostModelResponse {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: Option<StopReason>,
    pub response_model: Option<String>,
    pub response_id: Option<String>,
    pub raw_stop_reason: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum HostModelEvent {
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    ToolcallStart {
        content_index: usize,
        tool_call: kiss_ai::ToolCall,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        content_index: usize,
        tool_call: kiss_ai::ToolCall,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolResultInput {
    Text(String),
    Object(ToolResultObject),
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultObject {
    #[serde(default)]
    pub content: ToolResultContent,
    #[serde(default)]
    pub details: Value,
    #[serde(default)]
    pub terminate: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
    #[default]
    Empty,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInvocationContext {
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStateView {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub is_streaming: bool,
    pub closed: bool,
    pub message_count: usize,
    pub steering_count: usize,
    pub follow_up_count: usize,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub text: String,
    pub stop_reason: StopReason,
    pub messages: Vec<AgentMessage>,
    pub usage: Usage,
    pub state: AgentStateView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub version: u32,
    pub messages: Vec<AgentMessage>,
}

pub fn validate_options(options: &AgentOptions) -> Result<(), String> {
    validate_model(&options.model)?;
    if options.system_prompt.len() > MAX_INSTRUCTIONS_BYTES {
        return Err(format!(
            "KISS_LIMIT: systemPrompt exceeds {MAX_INSTRUCTIONS_BYTES} bytes"
        ));
    }
    if options.max_turns == 0 || options.max_turns > DEFAULT_MAX_TURNS {
        return Err(format!(
            "KISS_INVALID_OPTIONS: maxTurns must be between 1 and {DEFAULT_MAX_TURNS}"
        ));
    }
    if options.max_history_messages == 0 || options.max_history_messages > 10_000 {
        return Err("KISS_INVALID_OPTIONS: maxHistoryMessages must be between 1 and 10000".into());
    }
    if let Some(checkpoint) = &options.checkpoint
        && checkpoint.len() > MAX_CHECKPOINT_BYTES
    {
        return Err(format!(
            "KISS_LIMIT: checkpoint exceeds {MAX_CHECKPOINT_BYTES} bytes"
        ));
    }
    Ok(())
}

pub fn validate_model(model: &ModelInput) -> Result<(), String> {
    if model.id.trim().is_empty() {
        return Err("KISS_INVALID_OPTIONS: model.id must not be empty".into());
    }
    let metadata = serde_json::to_vec(model)
        .map_err(|error| format!("KISS_INVALID_OPTIONS: invalid model: {error}"))?;
    if metadata.len() > MAX_METADATA_BYTES {
        return Err(format!(
            "KISS_LIMIT: model metadata exceeds {MAX_METADATA_BYTES} bytes"
        ));
    }
    Ok(())
}
