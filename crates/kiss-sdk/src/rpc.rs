//! RPC mode: drive the agent from any language over newline-delimited JSON.
//!
//! # The framing rule, which clients get wrong
//!
//! Records are separated by a line feed (`\n`) and nothing else. One optional
//! carriage return immediately before it is stripped. In particular `U+2028`
//! (line separator) and `U+2029` (paragraph separator) are **not** separators,
//! because both are legal inside a JSON string; several languages' convenience
//! line readers — notably Node's `readline` — split on them and will corrupt a
//! reply that contains one. Split on `\n` yourself.
//!
//! # Concurrency
//!
//! Each command runs in its own task. That is deliberate: `abort` must be
//! processable while a long `prompt` is still streaming, so commands must not
//! be serialized behind one another. The underlying session guards its own
//! state, and the `prompt` command returns as soon as the prompt is accepted or
//! queued rather than when the run finishes. Wait for the `agent_settled` event
//! to learn that the agent is idle again.
//!
//! # Ordering
//!
//! One writer task owns the output side, so responses and events never
//! interleave halfway through a line.

use crate::protocol::{ProtocolError, Response, decode_request};
use crate::session::Session;
use futures::{SinkExt as _, StreamExt as _};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;

/// Serve one session on this process's standard input and output.
pub async fn serve_stdio(session: Arc<Session>) -> anyhow::Result<()> {
    serve_streams(session, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve one session over any pair of byte streams.
pub async fn serve_streams<R, W>(session: Arc<Session>, input: R, output: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (lines_tx, mut lines_rx) = mpsc::unbounded_channel::<String>();

    // One writer owns the sink so lines are never interleaved.
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(line) = lines_rx.recv().await {
            if output.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if output.write_all(b"\n").await.is_err() {
                break;
            }
            if output.flush().await.is_err() {
                break;
            }
        }
    });

    let events_tx = lines_tx.clone();
    let mut events = session.events();
    let event_pump = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if events_tx.send(event.to_line()).is_err() {
                break;
            }
        }
    });

    // `read_line` splits on `\n` only, which is exactly the framing rule.
    let mut reader = BufReader::new(input);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                let _ = lines_tx.send(error_line("parse", error));
                break;
            }
        }
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed.trim().is_empty() {
            continue;
        }
        dispatch(&session, trimmed, &lines_tx);
    }

    drop(lines_tx);
    event_pump.abort();
    let _ = writer.await;
    Ok(())
}

/// Serve one session to every client that connects over WebSocket.
///
/// This is what a browser (including the WebAssembly client in
/// `crates/kiss-wasm`) connects to. All clients share the one session, which is
/// what a local web interface for a single working directory wants.
#[cfg(feature = "rpc")]
pub async fn serve_websocket(session: Arc<Session>, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("kiss rpc listening on ws://{bound}");
    loop {
        let (stream, _peer) = listener.accept().await?;
        let session = session.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_websocket_connection(session, stream).await {
                eprintln!("kiss rpc connection ended: {error:#}");
            }
        });
    }
}

#[cfg(feature = "rpc")]
async fn serve_websocket_connection(
    session: Arc<Session>,
    stream: tokio::net::TcpStream,
) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::Message;

    let websocket = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = websocket.split();
    let (lines_tx, mut lines_rx) = mpsc::unbounded_channel::<String>();

    let writer = tokio::spawn(async move {
        while let Some(line) = lines_rx.recv().await {
            if sink.send(Message::Text(line.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let events_tx = lines_tx.clone();
    let mut events = session.events();
    let event_pump = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if events_tx.send(event.to_line()).is_err() {
                break;
            }
        }
    });

    while let Some(message) = source.next().await {
        match message? {
            Message::Text(text) => {
                // A client may pack several commands into one message.
                for line in text.split('\n') {
                    let line = line.trim_end_matches('\r');
                    if line.trim().is_empty() {
                        continue;
                    }
                    dispatch(&session, line, &lines_tx);
                }
            }
            Message::Binary(bytes) => match std::str::from_utf8(&bytes) {
                Ok(text) => dispatch(&session, text.trim(), &lines_tx),
                Err(error) => {
                    let _ = lines_tx.send(error_line("parse", error));
                }
            },
            Message::Close(_) => break,
            _ => {}
        }
    }

    drop(lines_tx);
    event_pump.abort();
    let _ = writer.await;
    Ok(())
}

/// Decode one line and run it, replying on `out`.
fn dispatch(session: &Arc<Session>, line: &str, out: &mpsc::UnboundedSender<String>) {
    match decode_request(line) {
        Ok(request) => {
            let session = session.clone();
            let out = out.clone();
            tokio::spawn(async move {
                let id = request.id.clone();
                let response = session.execute(request.command).await.with_id(id);
                let _ = out.send(encode(&response));
            });
        }
        Err(error) => {
            let _ = out.send(error_line("parse", error));
        }
    }
}

fn encode(response: &Response) -> String {
    serde_json::to_string(response).unwrap_or_else(|error| error_line("parse", error))
}

fn error_line(command: &str, error: impl std::fmt::Display) -> String {
    // Hand-built so an encoding failure cannot recurse.
    let message = serde_json::to_string(&error.to_string())
        .unwrap_or_else(|_| "\"internal error\"".to_string());
    format!(r#"{{"type":"response","command":"{command}","success":false,"error":{message}}}"#)
}

/// A parse failure that should be reported rather than closing the connection.
#[allow(dead_code)]
fn is_recoverable(error: &ProtocolError) -> bool {
    matches!(
        error,
        ProtocolError::Json(_) | ProtocolError::MissingType | ProtocolError::UnknownCommand(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parse_failure_is_reported_as_a_response_not_a_crash() {
        let line = error_line("parse", "unexpected token");
        assert!(line.contains(r#""command":"parse""#), "{line}");
        assert!(line.contains(r#""success":false"#), "{line}");
        assert!(serde_json::from_str::<serde_json::Value>(&line).is_ok());
    }

    #[test]
    fn error_text_containing_quotes_stays_valid_json() {
        let line = error_line("parse", r#"bad "quote" here"#);
        let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(value["error"], r#"bad "quote" here"#);
    }
}
