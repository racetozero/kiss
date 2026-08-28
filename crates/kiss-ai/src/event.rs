//! Streaming event protocol and the channel-backed event stream.
//!
//! Contract (identical to pi's): a stream emits `Start` first, then partial
//! update events, and terminates with exactly one `Done` or `Error` event.
//! Failures are data — the streaming entry points never return `Err` for
//! request/model/runtime problems.

use crate::types::{AssistantMessage, StopReason, ToolCall};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum AssistantEvent {
    Start {
        partial: AssistantMessage,
    },
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
    ToolCallStart {
        content_index: usize,
        tool_call: ToolCall,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        message: AssistantMessage,
    },
}

impl AssistantEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AssistantEvent::Done { .. } | AssistantEvent::Error { .. }
        )
    }

    pub fn terminal_message(&self) -> Option<&AssistantMessage> {
        match self {
            AssistantEvent::Done { message, .. } | AssistantEvent::Error { message, .. } => {
                Some(message)
            }
            _ => None,
        }
    }
}

/// Receiving half of an assistant event stream. `next()` yields events until
/// the terminal one; `result()` drains the stream and returns the final
/// message.
pub struct EventStream {
    rx: mpsc::UnboundedReceiver<AssistantEvent>,
    finished: Option<AssistantMessage>,
}

impl EventStream {
    pub fn channel() -> (EventSink, EventStream) {
        let (tx, rx) = mpsc::unbounded_channel();
        (EventSink { tx }, EventStream { rx, finished: None })
    }

    /// Next event, or `None` after the terminal event has been delivered.
    pub async fn next(&mut self) -> Option<AssistantEvent> {
        if self.finished.is_some() {
            return None;
        }
        let event = self.rx.recv().await?;
        if let Some(message) = event.terminal_message() {
            self.finished = Some(message.clone());
        }
        Some(event)
    }

    /// Drain remaining events and return the final assistant message.
    pub async fn result(mut self) -> AssistantMessage {
        if let Some(message) = self.finished.take() {
            return message;
        }
        while let Some(event) = self.next().await {
            if event.is_terminal() {
                break;
            }
        }
        self.finished.take().unwrap_or_else(|| {
            // A sink dropped without a terminal event is a bug in an adapter;
            // synthesize an error message so callers keep the uniform contract.
            let mut m = AssistantMessage::empty("unknown", "unknown", "unknown");
            m.stop_reason = StopReason::Error;
            m.error_message = Some("stream ended without a terminal event".into());
            m
        })
    }
}

/// Sending half handed to provider adapters.
#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::UnboundedSender<AssistantEvent>,
}

impl EventSink {
    pub fn send(&self, event: AssistantEvent) {
        let _ = self.tx.send(event);
    }

    pub fn done(&self, message: AssistantMessage) {
        let reason = message.stop_reason;
        self.send(AssistantEvent::Done { reason, message });
    }

    pub fn error(&self, message: AssistantMessage) {
        let reason = message.stop_reason;
        self.send(AssistantEvent::Error { reason, message });
    }
}
