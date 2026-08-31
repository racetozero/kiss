//! Agent loop configuration and hooks.

use crate::message::AgentMessage;
use crate::tool::{DynTool, ExecutionMode, ToolResult};
use kiss_ai::{
    ContentBlock, Message, Model, StreamOptions, ThinkingLevel, ToolChoice, Transport, Usage,
};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Context snapshot passed into the loop.
#[derive(Clone, Default)]
pub struct AgentContext {
    pub system_prompt: String,
    /// Provider-native OpenAI Responses input restored from compaction.
    pub openai_responses_input: Option<Vec<Value>>,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<DynTool>,
}

impl AgentContext {
    pub fn find_tool(&self, name: &str) -> Option<&DynTool> {
        self.tools.iter().find(|t| t.name() == name)
    }
}

/// Outcome of the before-tool-call hook.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

/// Field-level overrides from the after-tool-call hook.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ContentBlock>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

/// State swap applied between turns.
#[derive(Default)]
pub struct TurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

pub struct TurnInfo<'a> {
    pub message: &'a AgentMessage,
    pub tool_results: &'a [kiss_ai::ToolResultMessage],
    pub messages: &'a [AgentMessage],
}

type SteeringFn = Arc<dyn Fn() -> BoxFuture<Vec<AgentMessage>> + Send + Sync>;
type BeforeToolFn =
    Arc<dyn Fn(&str, &Value) -> BoxFuture<Option<BeforeToolCallResult>> + Send + Sync>;
type AfterToolFn = Arc<
    dyn Fn(&str, &Value, &ToolResult, bool) -> BoxFuture<Option<AfterToolCallResult>> + Send + Sync,
>;
type TransformFn = Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<Vec<AgentMessage>> + Send + Sync>;
type ConvertFn = Arc<dyn Fn(&[AgentMessage]) -> Vec<Message> + Send + Sync>;
type StopFn = Arc<dyn for<'a> Fn(&'a TurnInfo<'a>) -> BoxFuture<bool> + Send + Sync>;
type PrepareTurnFn =
    Arc<dyn for<'a> Fn(&'a TurnInfo<'a>) -> BoxFuture<Option<TurnUpdate>> + Send + Sync>;
type ApiKeyFn = Arc<dyn Fn(String) -> BoxFuture<Option<String>> + Send + Sync>;
type StreamFn =
    Arc<dyn Fn(&Model, &kiss_ai::Context, &StreamOptions) -> kiss_ai::EventStream + Send + Sync>;

/// Everything the loop needs besides the context.
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub session_id: Option<String>,
    pub transport: Transport,
    pub tool_execution: ExecutionMode,
    /// Convert harness messages to provider messages at the call boundary.
    pub convert_to_llm: ConvertFn,
    pub transform_context: Option<TransformFn>,
    pub get_api_key: Option<ApiKeyFn>,
    pub get_steering_messages: Option<SteeringFn>,
    pub get_follow_up_messages: Option<SteeringFn>,
    pub before_tool_call: Option<BeforeToolFn>,
    pub after_tool_call: Option<AfterToolFn>,
    pub should_stop_after_turn: Option<StopFn>,
    pub prepare_next_turn: Option<PrepareTurnFn>,
    /// Provider streaming function; overridable for tests (faux provider).
    pub stream_fn: StreamFn,
}

impl AgentLoopConfig {
    pub fn new(model: Model) -> Self {
        AgentLoopConfig {
            model,
            thinking_level: ThinkingLevel::Off,
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            session_id: None,
            transport: Transport::Auto,
            tool_execution: ExecutionMode::Parallel,
            convert_to_llm: Arc::new(crate::message::convert_to_llm),
            transform_context: None,
            get_api_key: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            stream_fn: Arc::new(|model, context, options| {
                kiss_ai::stream_simple(model, context, options)
            }),
        }
    }
}
