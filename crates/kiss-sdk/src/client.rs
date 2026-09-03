//! A transport-free ("sans-io") client for the KISS RPC protocol.
//!
//! This type does no I/O at all. You give it a [`Command`] and it hands back
//! the correlation id plus the exact line to send; you give it a line you read
//! and it hands back a decoded [`Incoming`]. That makes it usable from a tokio
//! task, from a browser `WebSocket` compiled to WebAssembly, or from a test
//! that never opens a socket, without any of those concerns leaking in here.
//!
//! It compiles with `--no-default-features`, which is what the WebAssembly
//! binding uses.

use crate::protocol::{Command, Incoming, ProtocolError, Request, decode_line};

/// Correlates outgoing commands with incoming responses.
#[derive(Debug, Default)]
pub struct Client {
    next_id: u64,
}

impl Client {
    pub fn new() -> Self {
        Client { next_id: 1 }
    }

    /// Turn a command into `(id, line)`.
    ///
    /// The line has no trailing newline; the transport adds one for stream
    /// protocols and sends it as a whole message for WebSocket.
    pub fn encode(&mut self, command: Command) -> (String, String) {
        let id = format!("c{}", self.next_id);
        self.next_id += 1;
        let request = Request::with_id(id.clone(), command);
        // Serialization of a closed enum of plain data cannot fail.
        let line = serde_json::to_string(&request).unwrap_or_else(|error| {
            format!(r#"{{"type":"ping","__encodeError":"{error}"}}"#)
        });
        (id, line)
    }

    /// Decode one line received from the agent.
    pub fn decode(&self, line: &str) -> Result<Incoming, ProtocolError> {
        decode_line(line)
    }
}

/// Split a chunk of received bytes into complete lines.
///
/// The RPC protocol uses strict JSON Lines: `\n` is the only record separator,
/// and one optional `\r` immediately before it is stripped. Nothing else counts
/// as a separator — in particular `U+2028` and `U+2029` do not, because they are
/// legal inside JSON strings and generic "line readers" in several languages
/// wrongly split on them.
#[derive(Debug, Default)]
pub struct LineBuffer {
    buffer: String,
}

impl LineBuffer {
    pub fn new() -> Self {
        LineBuffer::default()
    }

    /// Append text and return every complete line it produced.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        let mut lines = Vec::new();
        while let Some(index) = self.buffer.find('\n') {
            let mut line = self.buffer[..index].to_string();
            self.buffer.drain(..index + 1);
            if line.ends_with('\r') {
                line.pop();
            }
            if !line.trim().is_empty() {
                lines.push(line);
            }
        }
        lines
    }

    /// Return any trailing partial line, for use when the stream ends.
    pub fn finish(&mut self) -> Option<String> {
        let mut line = std::mem::take(&mut self.buffer);
        if line.ends_with('\r') {
            line.pop();
        }
        (!line.trim().is_empty()).then_some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Incoming;

    #[test]
    fn encoded_commands_carry_a_fresh_id() {
        let mut client = Client::new();
        let (first, line) = client.encode(Command::Ping {});
        let (second, _) = client.encode(Command::Ping {});
        assert_ne!(first, second);
        assert!(line.contains("\"type\":\"ping\""), "{line}");
        assert!(line.contains(&format!("\"id\":\"{first}\"")), "{line}");
    }

    #[test]
    fn responses_and_events_are_distinguished() {
        let client = Client::new();
        let response = client
            .decode(r#"{"type":"response","command":"ping","success":true}"#)
            .unwrap();
        assert!(matches!(response, Incoming::Response(_)));
        let event = client.decode(r#"{"type":"agent_start"}"#).unwrap();
        match event {
            Incoming::Event(event) => assert_eq!(event.event_type(), "agent_start"),
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[test]
    fn line_buffer_splits_only_on_line_feed() {
        let mut buffer = LineBuffer::new();
        assert!(buffer.push("{\"a\":\"x\u{2028}y\"}").is_empty());
        let lines = buffer.push("\r\n{\"b\":1}\n");
        assert_eq!(lines, ["{\"a\":\"x\u{2028}y\"}", "{\"b\":1}"]);
        assert_eq!(buffer.finish(), None);
    }
}
