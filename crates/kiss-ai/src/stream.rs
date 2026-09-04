//! The streaming front door: pick the adapter for `model.api`, spawn it, and
//! return the unified event stream. Never returns `Err` — every failure is a
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
    pub api_key: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub reasoning: ThinkingLevel,
    pub tool_choice: Option<ToolChoice>,
    /// Session identifier for providers that support session routing/caching.
    pub session_id: Option<String>,
    /// Streaming transport. OpenAI Codex uses `Auto` by default.
    pub transport: Transport,
    pub cancel: CancellationToken,
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
async fn dispatch(model: Model, context: Context, mut options: StreamOptions, sink: EventSink) {
    options.reasoning = model.map_thinking_level(options.reasoning);
    match model.api.as_str() {
        "anthropic-messages" => api::anthropic::stream(&model, &context, &options, sink).await,
        "bedrock-converse-stream" => api::bedrock::stream(&model, &context, &options, sink).await,
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
fn ensure_tls_crypto_provider() {
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
}
