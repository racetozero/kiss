//! kiss-ai: unified multi-provider LLM streaming API.
//!
//! One message model, one event protocol, adapters per provider API dialect.
//! Failures are encoded in the stream (`StopReason::Error`/`Aborted`), never
//! thrown from the streaming entry point.

#[cfg(feature = "native")]
pub mod api;
#[cfg(feature = "native")]
pub mod auth;
pub mod event;
#[cfg(feature = "native")]
pub mod json_salvage;
pub mod model;
#[cfg(feature = "native")]
pub mod registry;
#[cfg(feature = "native")]
pub mod sse;
pub mod stream;
pub mod types;

pub use event::{AssistantEvent, EventSink, EventStream};
pub use model::{Model, ModelCost, OpenAICompat};
#[cfg(feature = "native")]
pub use registry::Registry;
#[cfg(feature = "native")]
pub use stream::stream_simple;
pub use stream::{StreamOptions, ToolChoice, Transport};
pub use types::{
    AssistantMessage, ContentBlock, Context, Cost, Message, StopReason, ThinkingLevel, TimestampMs,
    ToolCall, ToolDef, ToolResultMessage, Usage, UserContent, UserMessage, now_ms,
};

/// Trim a provider error body for display.
#[cfg(feature = "native")]
pub(crate) fn truncate_err(text: &str) -> String {
    let text = text.trim();
    if text.len() > 600 {
        format!(
            "{}…",
            &text[..text
                .char_indices()
                .take_while(|(i, _)| *i < 600)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0)]
        )
    } else {
        text.to_string()
    }
}
