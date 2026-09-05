//! Proof that the SDK really runs an agent.
//!
//! These tests start a scripted mock model server on `127.0.0.1`, point a real
//! `Session` at it through a real `models.json`, and then check observable
//! effects: a file appearing on disk, events arriving in order, and the final
//! assistant text. No API key and no internet are involved.

#![cfg(all(feature = "mock", feature = "native"))]

use kiss_sdk::mock::{MockProvider, MockScript, MockTurn};
use kiss_sdk::protocol::{Command, StreamingBehavior};
use kiss_sdk::{PromptArgs, Session, SessionOptions};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Build a session whose only reachable model is the mock provider.
async fn session_with(
    directory: &std::path::Path,
    script: MockScript,
) -> (MockProvider, Arc<Session>) {
    let provider = MockProvider::start(directory, script)
        .await
        .expect("mock provider starts");
    let options = SessionOptions {
        cwd: directory.to_path_buf(),
        model: Some("mock/mock-1".into()),
        models_file: Some(provider.catalog_path()),
        // Project files in a temporary directory are ours, but leaving trust
        // off keeps the test independent of the developer's global settings.
        trust_project_files: false,
        no_context_files: true,
        ..Default::default()
    };
    let session = Session::create(options).await.expect("session builds");
    (provider, session)
}

/// Collect events into a shared vector until the session settles.
fn collect_events(session: &Arc<Session>) -> Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
    let collected: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Default::default();
    let sink = collected.clone();
    let mut events = session.events();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            sink.lock().unwrap().push(event.0);
        }
    });
    collected
}

/// Wait until an event of the given type has been collected.
///
/// The collector runs in its own task, so a test that inspects the vector the
/// instant `prompt()` returns can race with the last few forwarded events.
async fn wait_for(
    collected: &Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    event_type: &str,
) -> Vec<serde_json::Value> {
    for _ in 0..400 {
        {
            let events = collected.lock().unwrap();
            if events.iter().any(|event| event["type"] == event_type) {
                return events.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "timed out waiting for a `{event_type}` event; saw {:?}",
        collected
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| event["type"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_scripted_model_writes_a_real_file_through_the_write_tool() {
    let directory = tempfile::tempdir().expect("temp dir");
    let script = MockScript {
        turns: vec![
            vec![MockTurn::ToolCall {
                id: "call_1".into(),
                name: "write".into(),
                arguments: json!({"path": "hello.txt", "content": "hello from kiss\n"}),
            }],
            vec![MockTurn::Text("Done.".into())],
        ],
    };
    let (_provider, session) = session_with(directory.path(), script).await;
    let events = collect_events(&session);

    session
        .prompt("create hello.txt")
        .await
        .expect("prompt succeeds");

    // The tool really ran: the file exists with the requested contents.
    let written = std::fs::read_to_string(directory.path().join("hello.txt"))
        .expect("the write tool created hello.txt");
    assert_eq!(written, "hello from kiss\n");

    let events = wait_for(&events, "agent_settled").await;
    let types: Vec<&str> = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert!(types.contains(&"agent_start"), "{types:?}");
    assert!(types.contains(&"tool_execution_start"), "{types:?}");
    assert!(types.contains(&"tool_execution_end"), "{types:?}");
    assert!(types.contains(&"agent_end"), "{types:?}");

    let tool_start = events
        .iter()
        .find(|event| event["type"] == "tool_execution_start")
        .expect("a tool started");
    assert_eq!(tool_start["toolName"], "write");
    let tool_end = events
        .iter()
        .find(|event| event["type"] == "tool_execution_end")
        .expect("a tool finished");
    assert_eq!(tool_end["isError"], false);

    // Streaming really happened: text arrived as deltas.
    let streamed: String = events
        .iter()
        .filter(|event| event["type"] == "message_update")
        .filter_map(|event| {
            let inner = &event["assistantMessageEvent"];
            (inner["type"] == "text_delta")
                .then(|| inner["delta"].as_str().unwrap_or_default().to_string())
        })
        .collect();
    assert_eq!(streamed, "Done.");

    // And the session agrees about the final answer.
    let response = session.execute(Command::GetLastAssistantText {}).await;
    assert!(response.success);
    assert_eq!(response.data.unwrap()["text"], "Done.");
}

#[tokio::test]
async fn prompting_while_streaming_requires_an_explicit_behavior() {
    let directory = tempfile::tempdir().expect("temp dir");
    // A script that calls a slow shell command keeps the agent busy.
    let script = MockScript {
        turns: vec![
            vec![MockTurn::ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: json!({"command": "sleep 1"}),
            }],
            vec![MockTurn::Text("Finished.".into())],
        ],
    };
    let (_provider, session) = session_with(directory.path(), script).await;

    session
        .prompt_detached(PromptArgs::new("run something slow"))
        .expect("first prompt is accepted");
    // Give the run a moment to mark itself busy.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let rejected = session
        .execute(Command::Prompt {
            message: "and this too".into(),
            images: Vec::new(),
            streaming_behavior: None,
        })
        .await;
    assert!(!rejected.success, "{rejected:?}");
    assert!(
        rejected
            .error
            .as_deref()
            .unwrap()
            .contains("streamingBehavior"),
        "{rejected:?}"
    );

    let queued = session
        .execute(Command::Prompt {
            message: "and this too".into(),
            images: Vec::new(),
            streaming_behavior: Some(StreamingBehavior::Steer),
        })
        .await;
    assert!(queued.success, "{queued:?}");

    session.abort();
    session.wait_idle().await;
}

#[tokio::test]
async fn a_direct_bash_command_returns_its_exit_code_and_joins_the_history() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_provider, session) = session_with(directory.path(), MockScript::text("ok")).await;
    let events = collect_events(&session);

    let response = session
        .execute(Command::Bash {
            command: "printf 'from-bash'; exit 3".into(),
        })
        .await;
    assert!(response.success, "{response:?}");
    let data = response.data.expect("bash returns data");
    assert_eq!(data["output"], "from-bash");
    assert_eq!(data["exitCode"], 3);
    assert_eq!(data["cancelled"], false);

    // The command is now part of the conversation the next prompt will send.
    let messages = session.execute(Command::GetMessages {}).await;
    let messages = messages.data.unwrap();
    let roles: Vec<&str> = messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["role"].as_str())
        .collect();
    assert!(roles.contains(&"bashExecution"), "{roles:?}");

    // Output streamed as it was produced.
    let events = wait_for(&events, "bash_execution_update").await;
    let streamed: String = events
        .iter()
        .filter(|event| event["type"] == "bash_execution_update")
        .filter_map(|event| event["delta"].as_str().map(str::to_string))
        .collect();
    assert_eq!(streamed, "from-bash");
}

#[tokio::test]
async fn the_dispatcher_answers_state_and_model_questions() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_provider, session) = session_with(directory.path(), MockScript::text("hi")).await;

    let ping = session.execute(Command::Ping {}).await;
    assert!(ping.success);
    assert_eq!(ping.data.unwrap()["pong"], true);

    let state = session.execute(Command::GetState {}).await;
    let state = state.data.expect("state data");
    assert_eq!(state["model"]["provider"], "mock");
    assert_eq!(state["model"]["id"], "mock-1");
    assert_eq!(state["isStreaming"], false);
    assert_eq!(state["thinkingLevel"], "off");
    let tools = state["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4, "the four default tools: {tools:?}");

    let models = session
        .execute(Command::GetAvailableModels {
            search: Some("mock".into()),
        })
        .await;
    let models = models.data.unwrap();
    assert_eq!(models["models"].as_array().unwrap().len(), 1);

    // A model that does not exist is a clean failure, not a panic.
    let missing = session
        .execute(Command::SetModel {
            provider: "nope".into(),
            model_id: "nope".into(),
        })
        .await;
    assert!(!missing.success);
    assert!(missing.error.unwrap().contains("nope/nope"));

    // The mock model declares no reasoning support, so only "off" is offered.
    let levels = session
        .execute(Command::GetAvailableThinkingLevels {})
        .await;
    assert_eq!(levels.data.unwrap()["levels"], json!(["off"]));
    let rejected = session
        .execute(Command::SetThinkingLevel {
            level: "high".into(),
        })
        .await;
    assert!(!rejected.success, "{rejected:?}");
}

#[tokio::test]
async fn session_statistics_count_the_conversation() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_provider, session) = session_with(directory.path(), MockScript::text("Hello!")).await;
    session.prompt("hi").await.expect("prompt succeeds");

    let stats = session.execute(Command::GetSessionStats {}).await;
    let stats = stats.data.expect("stats data");
    assert_eq!(stats["userMessages"], 1);
    assert_eq!(stats["assistantMessages"], 1);
    assert_eq!(stats["totalMessages"], 2);
    assert!(stats["contextUsage"]["contextWindow"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn export_html_writes_an_escaped_transcript() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_provider, session) = session_with(directory.path(), MockScript::text("A < B & C")).await;
    session.prompt("compare").await.expect("prompt succeeds");
    let path = directory.path().join("transcript.html");
    let response = session
        .execute(Command::ExportHtml {
            output_path: Some(path.display().to_string()),
        })
        .await;
    assert!(response.success, "{response:?}");
    let html = std::fs::read_to_string(path).expect("HTML exists");
    assert!(html.contains("A &lt; B &amp; C"), "{html}");
    assert!(!html.contains("A < B & C"), "content must be escaped");
}

#[tokio::test]
async fn an_unknown_entry_cursor_is_reported_rather_than_ignored() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_provider, session) = session_with(directory.path(), MockScript::text("hi")).await;
    let response = session
        .execute(Command::GetEntries {
            since: Some("does-not-exist".into()),
        })
        .await;
    assert!(!response.success);
    assert!(response.error.unwrap().contains("does-not-exist"));
}
