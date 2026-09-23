//! Agent loop configuration and hooks.

use crate::message::AgentMessage;
use crate::tool::{DynTool, ExecutionMode, ToolResult};
use kiss_ai::{
    ContentBlock, Message, Model, ResolvedCredential, StreamOptions, ThinkingLevel, ToolChoice,
    Transport, Usage,
};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub const DEFAULT_MAX_CONCURRENT_TOOLS: usize = 32;

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
    pub fast_mode: Option<bool>,
    /// Runs after the local settings are applied and before the model request.
    pub on_applied: Option<AppliedFn>,
}

pub struct TurnInfo<'a> {
    pub message: &'a AgentMessage,
    pub tool_results: &'a [kiss_ai::ToolResultMessage],
    pub messages: &'a [AgentMessage],
    /// The completed tool batch requires another model generation.
    pub will_continue: bool,
}

type SteeringFn = Arc<dyn Fn() -> BoxFuture<Vec<AgentMessage>> + Send + Sync>;
type BeforeToolFn =
    Arc<dyn Fn(&str, &Value) -> BoxFuture<Option<BeforeToolCallResult>> + Send + Sync>;
type AfterToolFn = Arc<
    dyn Fn(&str, &Value, &ToolResult, bool) -> BoxFuture<Option<AfterToolCallResult>> + Send + Sync,
>;
type TransformFn = Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<Vec<AgentMessage>> + Send + Sync>;
type ConvertFn = Arc<dyn Fn(&[AgentMessage]) -> Vec<Message> + Send + Sync>;
type AppliedFn = Box<dyn FnOnce(&Model, ThinkingLevel) + Send>;
type StopFn = Arc<dyn for<'a> Fn(&'a TurnInfo<'a>) -> BoxFuture<bool> + Send + Sync>;
type PrepareTurnFn =
    Arc<dyn for<'a> Fn(&'a TurnInfo<'a>) -> BoxFuture<Option<TurnUpdate>> + Send + Sync>;
type PrepareGenerationFn =
    Arc<dyn Fn(ThinkingLevel) -> BoxFuture<Option<TurnUpdate>> + Send + Sync>;
type CredentialFn = Arc<dyn Fn(String) -> BoxFuture<Option<ResolvedCredential>> + Send + Sync>;
/// The function the loop calls to reach a model provider.
///
/// It is public so embedders and tests can substitute their own transport (for
/// example a scripted fake provider) without going through the network.
pub type StreamFn =
    Arc<dyn Fn(&Model, &kiss_ai::Context, &StreamOptions) -> kiss_ai::EventStream + Send + Sync>;

/// Everything the loop needs besides the context.
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub fast_mode: bool,
    pub session_id: Option<String>,
    pub transport: Transport,
    pub tool_execution: ExecutionMode,
    /// Maximum active calls in one parallel tool batch.
    pub max_concurrent_tools: usize,
    /// Convert harness messages to provider messages at the call boundary.
    pub convert_to_llm: ConvertFn,
    pub transform_context: Option<TransformFn>,
    pub get_credential: Option<CredentialFn>,
    pub get_steering_messages: Option<SteeringFn>,
    pub get_follow_up_messages: Option<SteeringFn>,
    pub before_tool_call: Option<BeforeToolFn>,
    pub after_tool_call: Option<AfterToolFn>,
    pub should_stop_after_turn: Option<StopFn>,
    pub prepare_next_turn: Option<PrepareTurnFn>,
    /// Runs immediately before each model request, including the first one.
    pub prepare_generation: Option<PrepareGenerationFn>,
    /// Provider streaming function. Overridable for tests (faux provider).
    pub stream_fn: StreamFn,
}

impl AgentLoopConfig {
    /// Construct a loop around an explicit provider stream.
    ///
    /// Portable embedders such as browser WebAssembly use this constructor so
    /// the host owns network authority. Native callers normally use [`Self::new`].
    pub fn with_stream(model: Model, stream_fn: StreamFn) -> Self {
        AgentLoopConfig {
            model,
            thinking_level: ThinkingLevel::Off,
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            fast_mode: false,
            session_id: None,
            transport: Transport::Auto,
            tool_execution: ExecutionMode::Parallel,
            max_concurrent_tools: DEFAULT_MAX_CONCURRENT_TOOLS,
            convert_to_llm: Arc::new(crate::message::convert_to_llm),
            transform_context: None,
            get_credential: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            prepare_generation: None,
            stream_fn,
        }
    }

    /// Construct a loop using KISS's native provider adapters.
    #[cfg(feature = "native-tools")]
    pub fn new(model: Model) -> Self {
        Self::with_stream(model, Arc::new(kiss_ai::stream_simple))
    }
}
