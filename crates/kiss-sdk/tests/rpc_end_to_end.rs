//! Proof that RPC mode is a working, language-neutral interface.
//!
//! The test drives the same server loop `kiss --mode rpc` runs, over an
//! in-memory pipe, and checks the exact JSON lines a client in any language
//! would see.

#![cfg(all(feature = "mock", feature = "rpc"))]

use kiss_sdk::mock::{MockProvider, MockScript, MockTurn};
use kiss_sdk::protocol::{Incoming, Response, decode_line};
use kiss_sdk::{Session, SessionOptions};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

struct Harness {
    _provider: MockProvider,
    _directory: tempfile::TempDir,
    input: tokio::io::DuplexStream,
    output: BufReader<tokio::io::DuplexStream>,
}

impl Harness {
    async fn start(script: MockScript) -> Harness {
        let directory = tempfile::tempdir().expect("temp dir");
        let provider = MockProvider::start(directory.path(), script)
            .await
            .expect("mock provider starts");
        let session: Arc<Session> = Session::create(SessionOptions {
            cwd: directory.path().to_path_buf(),
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            no_context_files: true,
            ..Default::default()
        })
        .await
        .expect("session builds");

        // `client_write` -> server input, server output -> `client_read`.
        let (client_write, server_read) = tokio::io::duplex(64 * 1024);
        let (server_write, client_read) = tokio::io::duplex(1024 * 1024);
        tokio::spawn(async move {
            let _ = kiss_sdk::rpc::serve_streams(session, server_read, server_write).await;
        });

        Harness {
            _provider: provider,
            _directory: directory,
            input: client_write,
            output: BufReader::new(client_read),
        }
    }

    async fn send(&mut self, command: Value) {
        let line = format!("{command}\n");
        self.input
            .write_all(line.as_bytes())
            .await
            .expect("write command");
        self.input.flush().await.expect("flush");
    }

    async fn next_line(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(20), self.output.read_line(&mut line))
            .await
            .expect("the server answered in time")
            .expect("read a line");
        assert!(read > 0, "the server closed the connection");
        serde_json::from_str(line.trim_end()).unwrap_or_else(|error| {
            panic!("server sent a line that is not JSON: {line:?} ({error})")
        })
    }

    /// Read lines until one matches `predicate`, collecting the rest.
    async fn read_until(&mut self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let value = self.next_line().await;
            let matched = predicate(&value);
            seen.push(value);
            if matched {
                return seen;
            }
        }
    }
}

#[tokio::test]
async fn ping_answers_immediately_with_a_correlated_response() {
    let mut harness = Harness::start(MockScript::text("hi")).await;
    harness.send(json!({"id": "1", "type": "ping"})).await;
    let line = harness.next_line().await;
    assert_eq!(
        line,
        json!({
            "type": "response",
            "id": "1",
            "command": "ping",
            "success": true,
            "data": {"pong": true}
        })
    );
}

#[tokio::test]
async fn an_unknown_command_is_reported_without_closing_the_connection() {
    let mut harness = Harness::start(MockScript::text("hi")).await;
    harness.send(json!({"type": "teleport"})).await;
    let line = harness.next_line().await;
    assert_eq!(line["type"], "response");
    assert_eq!(line["command"], "parse");
    assert_eq!(line["success"], false);
    assert!(
        line["error"].as_str().unwrap().contains("teleport"),
        "{line}"
    );

    // The connection is still usable.
    harness.send(json!({"type": "ping"})).await;
    assert_eq!(harness.next_line().await["success"], true);
}

#[tokio::test]
async fn malformed_json_is_reported_without_closing_the_connection() {
    let mut harness = Harness::start(MockScript::text("hi")).await;
    harness
        .input
        .write_all(b"{not json at all}\n")
        .await
        .expect("write");
    let line = harness.next_line().await;
    assert_eq!(line["command"], "parse");
    assert_eq!(line["success"], false);

    harness.send(json!({"type": "ping"})).await;
    assert_eq!(harness.next_line().await["success"], true);
}

#[tokio::test]
async fn a_prompt_streams_events_and_ends_with_agent_settled() {
    let mut harness = Harness::start(MockScript::text("Hello from the mock provider.")).await;
    harness
        .send(json!({"id": "1", "type": "prompt", "message": "say hi"}))
        .await;

    let lines = harness
        .read_until(|value| value["type"] == "agent_settled")
        .await;

    // The command was accepted before the run finished.
    let acceptance = lines
        .iter()
        .find(|value| value["type"] == "response")
        .expect("an acceptance response");
    assert_eq!(acceptance["command"], "prompt");
    assert_eq!(acceptance["success"], true);
    assert_eq!(acceptance["id"], "1");

    let types: Vec<&str> = lines
        .iter()
        .filter_map(|value| value["type"].as_str())
        .collect();
    for expected in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "agent_end",
        "agent_settled",
    ] {
        assert!(types.contains(&expected), "missing {expected}: {types:?}");
    }

    let streamed: String = lines
        .iter()
        .filter(|value| value["type"] == "message_update")
        .filter_map(|value| {
            let inner = &value["assistantMessageEvent"];
            (inner["type"] == "text_delta")
                .then(|| inner["delta"].as_str().unwrap_or_default().to_string())
        })
        .collect();
    assert_eq!(streamed, "Hello from the mock provider.");
}

#[tokio::test]
async fn a_tool_call_reaches_the_client_and_the_filesystem() {
    let script = MockScript {
        turns: vec![
            vec![MockTurn::ToolCall {
                id: "call_1".into(),
                name: "write".into(),
                arguments: json!({"path": "note.txt", "content": "written over rpc\n"}),
            }],
            vec![MockTurn::Text("Wrote it.".into())],
        ],
    };
    let mut harness = Harness::start(script).await;
    let path = harness._directory.path().join("note.txt");

    harness
        .send(json!({"type": "prompt", "message": "write a note"}))
        .await;
    let lines = harness
        .read_until(|value| value["type"] == "agent_settled")
        .await;

    let start = lines
        .iter()
        .find(|value| value["type"] == "tool_execution_start")
        .expect("tool_execution_start");
    assert_eq!(start["toolName"], "write");
    let end = lines
        .iter()
        .find(|value| value["type"] == "tool_execution_end")
        .expect("tool_execution_end");
    assert_eq!(end["isError"], false);
    assert_eq!(start["toolCallId"], end["toolCallId"]);

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "written over rpc\n"
    );

    harness
        .send(json!({"type": "get_last_assistant_text"}))
        .await;
    let response = harness
        .read_until(|value| value["type"] == "response")
        .await
        .pop()
        .unwrap();
    assert_eq!(response["data"]["text"], "Wrote it.");
}

#[tokio::test]
async fn direct_bash_updates_carry_the_request_id() {
    let mut harness = Harness::start(MockScript::text("hi")).await;
    harness
        .send(json!({"id": "shell-7", "type": "bash", "command": "printf streamed"}))
        .await;
    let lines = harness
        .read_until(|value| value["type"] == "response" && value["command"] == "bash")
        .await;
    let update = lines
        .iter()
        .find(|value| value["type"] == "bash_execution_update")
        .expect("a streaming shell update");
    assert_eq!(update["id"], "shell-7");
    assert_eq!(update["delta"], "streamed");
    let response = lines.last().unwrap();
    assert_eq!(response["id"], "shell-7");
    assert_eq!(response["data"]["exitCode"], 0);
}

#[tokio::test]
async fn responses_are_decodable_with_the_shipped_client_helper() {
    let mut harness = Harness::start(MockScript::text("hi")).await;
    harness.send(json!({"id": "9", "type": "get_state"})).await;
    let line = harness.next_line().await.to_string();
    match decode_line(&line).expect("decodes") {
        Incoming::Response(Response {
            id,
            command,
            success,
            data,
            ..
        }) => {
            assert_eq!(id.as_deref(), Some("9"));
            assert_eq!(command, "get_state");
            assert!(success);
            assert_eq!(data.unwrap()["model"]["provider"], "mock");
        }
        other => panic!("expected a response, got {other:?}"),
    }
}
