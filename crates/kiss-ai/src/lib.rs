//! kiss-ai: unified multi-provider LLM streaming API.
//!
//! One message model, one event protocol, adapters per provider API dialect.
//! Failures are encoded in the stream (`StopReason::Error`/`Aborted`), never
//! thrown from the streaming entry point.

pub mod api;
pub mod auth;
pub mod event;
pub mod json_salvage;
pub mod model;
pub mod registry;
pub mod sse;
pub mod stream;
pub mod types;

pub use event::{AssistantEvent, EventSink, EventStream};
pub use model::{Model, ModelCost, OpenAICompat};
pub use registry::Registry;
pub use stream::{StreamOptions, Transport, stream_simple};
pub use types::{
    AssistantMessage, ContentBlock, Context, Cost, Message, StopReason, ThinkingLevel, TimestampMs,
    ToolCall, ToolDef, ToolResultMessage, Usage, UserContent, UserMessage, now_ms,
};

/// Trim a provider error body for display.
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
