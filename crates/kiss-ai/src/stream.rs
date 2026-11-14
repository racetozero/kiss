//! The streaming front door: pick the adapter for `model.api`, spawn it, and
//! return the unified event stream. Never returns `Err`: every failure is a
//! terminal `Error` event.

#[cfg(feature = "native")]
use crate::api;
#[cfg(feature = "native")]
use crate::event::{EventSink, EventStream};
#[cfg(feature = "native")]
use crate::model::Model;
#[cfg(feature = "native")]
use crate::types::Context;
use crate::types::ThinkingLevel;
use tokio_util::sync::CancellationToken;

/// Network transport for providers that support more than one streaming path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// Prefer a cached WebSocket and fall back to server-sent events before
    /// any assistant output starts.
    #[default]
    Auto,
    /// Always use HTTP server-sent events.
    Sse,
    /// Use a reusable WebSocket, but send the full context on each request.
    WebSocket,
    /// Reuse the WebSocket and continue from the prior response ID.
    WebSocketCached,
}

/// A provider credential plus the HTTP authentication scheme it requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedCredential {
    ApiKey(String),
    Bearer(String),
}

impl ResolvedCredential {
    pub fn api_key(value: impl Into<String>) -> Self {
        Self::ApiKey(value.into())
    }

    pub fn bearer(value: impl Into<String>) -> Self {
        Self::Bearer(value.into())
    }

    pub fn value(&self) -> &str {
        match self {
            Self::ApiKey(value) | Self::Bearer(value) => value,
        }
    }

    pub fn into_value(self) -> String {
        match self {
            Self::ApiKey(value) | Self::Bearer(value) => value,
        }
    }

    pub fn is_bearer(&self) -> bool {
        matches!(self, Self::Bearer(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Function(String),
}

#[cfg(feature = "native")]
impl ToolChoice {
    pub(crate) fn openai_chat_value(&self) -> serde_json::Value {
        match self {
            Self::Auto => serde_json::json!("auto"),
            Self::None => serde_json::json!("none"),
            Self::Required => serde_json::json!("required"),
            Self::Function(name) => serde_json::json!({
                "type": "function",
                "function": {"name": name},
            }),
        }
    }

    pub(crate) fn openai_responses_value(&self) -> serde_json::Value {
        match self {
            Self::Function(name) => serde_json::json!({"type": "function", "name": name}),
            _ => self.openai_chat_value(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub credential: Option<ResolvedCredential>,
    pub temperature: Option<f64>,
    pub sampling_params: std::collections::BTreeMap<String, serde_json::Value>,
    pub max_tokens: Option<u64>,
    pub reasoning: ThinkingLevel,
    pub tool_choice: Option<ToolChoice>,
    /// Ask a supported provider to use its higher-cost low-latency tier.
    pub fast_mode: bool,
    /// Session identifier for providers that support session routing/caching.
    pub session_id: Option<String>,
    /// Streaming transport. OpenAI, OpenAI Codex, and Azure OpenAI use `Auto`
    /// by default.
    pub transport: Transport,
    pub cancel: CancellationToken,
    /// Do not use or record server-side continuation state, such as
    /// Anthropic message threads. Replays such as cache warming set it, so
    /// they do not replace the state of the real conversation.
    pub disable_continuation: bool,
}

#[cfg(feature = "native")]
pub fn stream_simple(model: &Model, context: &Context, options: &StreamOptions) -> EventStream {
    ensure_tls_crypto_provider();
    let (sink, stream) = EventStream::channel();
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    tokio::spawn(async move {
        dispatch(model, context, options, sink).await;
    });
    stream
}

#[cfg(feature = "native")]
async fn dispatch(model: Model, mut context: Context, mut options: StreamOptions, sink: EventSink) {
    options.reasoning = model.map_thinking_level(options.reasoning);
    drop_foreign_thinking(&model, &mut context);
    if model.base_url.contains("{DATABRICKS_HOST}") {
        let builder = api::PartialBuilder::new(&model, sink);
        builder.fail(
            "the selected Databricks model has no workspace URL. Set DATABRICKS_HOST to an HTTP or HTTPS workspace root, or run `kiss login databricks-unity-gateway --base-url URL`",
            false,
            &model,
        );
        return;
    }
    if model.base_url.contains("{SNOWFLAKE_CORTEX_BASE_URL}") {
        let builder = api::PartialBuilder::new(&model, sink);
        builder.fail(
            "the selected Snowflake model has no account URL. Set SNOWFLAKE_CORTEX_BASE_URL to an HTTP or HTTPS Cortex or AI Gateway root, or run `kiss login snowflake-cortex --base-url URL`",
            false,
            &model,
        );
        return;
    }
    match model.api.as_str() {
        "anthropic-messages" => api::anthropic::stream(&model, &context, &options, sink).await,
        "bedrock-converse-stream" => api::bedrock::stream(&model, &context, &options, sink).await,
        "cursor-agent" => api::cursor::stream(&model, &context, &options, sink).await,
        "openai-completions" => {
            api::openai_completions::stream(&model, &context, &options, sink).await
        }
        "openai-responses" | "openai-codex-responses" | "azure-openai-responses" => {
            api::openai_responses::stream(&model, &context, &options, sink).await
        }
        "google-generative-ai" | "google-vertex" => {
            api::google::stream(&model, &context, &options, sink).await
        }
        "pi-messages" => api::pi_messages::stream(&model, &context, &options, sink).await,
        other => {
            let builder = api::PartialBuilder::new(&model, sink);
            builder.fail(format!("unsupported api: {other}"), false, &model);
        }
    }
}

/// Thinking is valid only for the model that produced it: signatures and
/// encrypted reasoning fail elsewhere, and another model reads plain thinking
/// as part of its own visible reply.
#[cfg(feature = "native")]
fn drop_foreign_thinking(model: &Model, context: &mut Context) {
    for message in &mut context.messages {
        if let crate::Message::Assistant(assistant) = message
            && (assistant.provider != model.provider
                || assistant.api != model.api
                || assistant.model != model.id)
        {
            assistant
                .content
                .retain(|block| !matches!(block, crate::ContentBlock::Thinking { .. }));
        }
    }
}

/// Shared HTTP client with connection pooling across requests.
#[cfg(feature = "native")]
pub fn http_client() -> &'static reqwest::Client {
    ensure_tls_crypto_provider();
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("reqwest client")
    })
}

#[cfg(feature = "native")]
pub(crate) fn ensure_tls_crypto_provider() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "failed to install the Rustls crypto provider"
        );
    });
}

#[cfg(all(test, feature = "native"))]
mod tests {
    #[test]
    fn tls_crypto_provider_is_installed_for_clients() {
        super::ensure_tls_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        reqwest::Client::builder().build().unwrap();
    }

    #[test]
    fn thinking_from_another_model_is_dropped() {
        use crate::{AssistantMessage, ContentBlock, Context, Message};
        let model: crate::Model = serde_json::from_value(serde_json::json!({
            "id": "new", "api": "openai-responses", "provider": "openai", "baseUrl": "",
        }))
        .unwrap();
        let reply = |model_id: &str| {
            let mut message = AssistantMessage::empty("openai-responses", "openai", model_id);
            message.content = vec![
                ContentBlock::Thinking {
                    thinking: "plan".into(),
                    thinking_signature: Some("sig".into()),
                    redacted: false,
                },
                ContentBlock::Text {
                    text: "answer".into(),
                    text_signature: None,
                },
            ];
            Message::Assistant(message)
        };
        let mut context = Context {
            messages: vec![reply("old"), reply("new")],
            ..Default::default()
        };
        super::drop_foreign_thinking(&model, &mut context);
        let lengths: Vec<usize> = context
            .messages
            .iter()
            .map(|message| match message {
                Message::Assistant(message) => message.content.len(),
                _ => 0,
            })
            .collect();
        assert_eq!(lengths, [1, 2]);
    }
}
