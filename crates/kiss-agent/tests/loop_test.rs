//! Agent-loop tests driven by a scripted fake provider.

use kiss_agent::config::{AgentContext, AgentLoopConfig};
use kiss_agent::message::AgentMessage;
use kiss_agent::tool::{AgentTool, ToolResult, ToolUpdateSink};
use kiss_agent::{AgentEvent, run_agent_loop};
use kiss_ai::{
    AssistantEvent, AssistantMessage, ContentBlock, EventStream, Model, StopReason, ToolCall,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

fn fake_model() -> Model {
    serde_json::from_value(json!({
        "id": "fake-model",
        "name": "Fake",
        "api": "fake",
        "provider": "fake",
        "baseUrl": "http://localhost:0",
    }))
    .unwrap()
}

/// Scripted provider: each call pops the next assistant message.
fn scripted_config(responses: Vec<AssistantMessage>) -> AgentLoopConfig {
    let queue = Arc::new(Mutex::new(responses));
    let mut config = AgentLoopConfig::new(fake_model());
    config.stream_fn = Arc::new(move |_, _, _| {
        let (sink, stream) = EventStream::channel();
        let mut queue = queue.lock().unwrap();
        let message = if queue.is_empty() {
            let mut m = AssistantMessage::empty("fake", "fake", "fake-model");
            m.stop_reason = StopReason::Error;
            m.error_message = Some("script exhausted".into());
            m
        } else {
            queue.remove(0)
        };
        sink.send(AssistantEvent::Start {
            partial: message.clone(),
        });
        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            sink.error(message);
        } else {
            sink.done(message);
        }
        stream
    });
    config
}

fn assistant_text(text: &str, stop: StopReason) -> AssistantMessage {
    let mut m = AssistantMessage::empty("fake", "fake", "fake-model");
    m.content.push(ContentBlock::text(text));
    m.stop_reason = stop;
    m
}

fn assistant_tool_call(name: &str, args: Value, stop: StopReason) -> AssistantMessage {
    let mut m = AssistantMessage::empty("fake", "fake", "fake-model");
    m.content.push(ContentBlock::ToolCall(ToolCall {
        id: format!("call_{name}"),
        name: name.to_string(),
        arguments: args,
        thought_signature: None,
    }));
    m.stop_reason = stop;
    m
}

struct EchoTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> String {
        "echo".into()
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]})
    }
    async fn execute(
        &self,
        _id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text(format!(
            "echo: {}",
            args["value"].as_str().unwrap_or("")
        )))
    }
}

fn collect_events() -> (kiss_agent::EventSink, Arc<Mutex<Vec<String>>>) {
    let log: Arc<Mutex<Vec<String>>> = Default::default();
    let log2 = log.clone();
    let sink: kiss_agent::EventSink = Arc::new(move |event: AgentEvent| {
        let tag = match &event {
            AgentEvent::AgentStart => "agent_start".to_string(),
            AgentEvent::AgentEnd { .. } => "agent_end".to_string(),
            AgentEvent::TurnStart => "turn_start".to_string(),
            AgentEvent::TurnEnd { .. } => "turn_end".to_string(),
            AgentEvent::MessageStart { message } => format!("message_start:{}", message.role()),
            AgentEvent::MessageUpdate { .. } => "message_update".to_string(),
            AgentEvent::MessageEnd { message } => format!("message_end:{}", message.role()),
            AgentEvent::ToolExecutionStart { tool_name, .. } => format!("tool_start:{tool_name}"),
            AgentEvent::ToolExecutionUpdate { .. } => "tool_update".to_string(),
            AgentEvent::ToolExecutionEnd { is_error, .. } => format!("tool_end:err={is_error}"),
        };
        log2.lock().unwrap().push(tag);
    });
    (sink, log)
}

#[tokio::test]
async fn simple_turn_no_tools() {
    let config = scripted_config(vec![assistant_text("hello!", StopReason::Stop)]);
    let (sink, log) = collect_events();
    let messages = run_agent_loop(
        vec![AgentMessage::user("hi")],
        AgentContext {
            system_prompt: "sys".into(),
            openai_responses_input: None,
            messages: vec![],
            tools: vec![],
        },
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    assert_eq!(messages.len(), 2);
    let log = log.lock().unwrap();
    assert_eq!(log.first().unwrap(), "agent_start");
    assert_eq!(log.last().unwrap(), "agent_end");
    assert!(log.contains(&"message_end:assistant".to_string()));
}

#[tokio::test]
async fn tool_call_roundtrip() {
    let calls = Arc::new(AtomicUsize::new(0));
    let config = scripted_config(vec![
        assistant_tool_call("echo", json!({"value": "x"}), StopReason::ToolUse),
        assistant_text("done", StopReason::Stop),
    ]);
    let (sink, log) = collect_events();
    let context = AgentContext {
        system_prompt: String::new(),
        openai_responses_input: None,
        messages: vec![],
        tools: vec![Arc::new(EchoTool {
            calls: calls.clone(),
        })],
    };
    let messages = run_agent_loop(
        vec![AgentMessage::user("go")],
        context,
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // user, assistant(tool call), toolResult, assistant(done)
    assert_eq!(messages.len(), 4);
    let AgentMessage::ToolResult(tr) = &messages[2] else {
        panic!("expected tool result")
    };
    assert!(
        tr.content
            .iter()
            .any(|c| matches!(c, ContentBlock::Text { text, .. } if text == "echo: x"))
    );
    let log = log.lock().unwrap();
    assert!(log.contains(&"tool_start:echo".to_string()));
    assert!(log.contains(&"tool_end:err=false".to_string()));
}

#[tokio::test]
async fn unknown_tool_yields_error_result() {
    let config = scripted_config(vec![
        assistant_tool_call("missing", json!({}), StopReason::ToolUse),
        assistant_text("recovered", StopReason::Stop),
    ]);
    let (sink, _log) = collect_events();
    let messages = run_agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext::default(),
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    let AgentMessage::ToolResult(tr) = &messages[2] else {
        panic!("expected tool result")
    };
    assert!(tr.is_error);
    assert!(
        tr.content
            .iter()
            .any(|c| matches!(c, ContentBlock::Text { text, .. } if text.contains("not found")))
    );
}

#[tokio::test]
async fn length_stop_fails_tool_calls_without_executing() {
    let calls = Arc::new(AtomicUsize::new(0));
    let config = scripted_config(vec![
        assistant_tool_call("echo", json!({"value": "trunc"}), StopReason::Length),
        assistant_text("retry ok", StopReason::Stop),
    ]);
    let (sink, _log) = collect_events();
    let context = AgentContext {
        system_prompt: String::new(),
        openai_responses_input: None,
        messages: vec![],
        tools: vec![Arc::new(EchoTool {
            calls: calls.clone(),
        })],
    };
    let messages = run_agent_loop(
        vec![AgentMessage::user("go")],
        context,
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "tool must not run on length stop"
    );
    let AgentMessage::ToolResult(tr) = &messages[2] else {
        panic!("expected tool result")
    };
    assert!(tr.is_error);
    assert!(tr.content.iter().any(
        |c| matches!(c, ContentBlock::Text { text, .. } if text.contains("output token limit"))
    ));
}

#[tokio::test]
async fn error_stop_ends_run() {
    let mut error = assistant_text("", StopReason::Error);
    error.error_message = Some("boom".into());
    let config = scripted_config(vec![error]);
    let (sink, log) = collect_events();
    let messages = run_agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext::default(),
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    assert_eq!(messages.len(), 2);
    let AgentMessage::Assistant(a) = &messages[1] else {
        panic!()
    };
    assert_eq!(a.stop_reason, StopReason::Error);
    let log = log.lock().unwrap();
    assert_eq!(log.last().unwrap(), "agent_end");
}

#[tokio::test]
async fn steering_injected_between_turns() {
    let steering: Arc<Mutex<Vec<AgentMessage>>> =
        Arc::new(Mutex::new(vec![AgentMessage::user("also do Y")]));
    let mut config = scripted_config(vec![
        assistant_tool_call("echo", json!({"value": "1"}), StopReason::ToolUse),
        assistant_text("done with both", StopReason::Stop),
    ]);
    let steering2 = steering.clone();
    config.get_steering_messages = Some(Arc::new(move || {
        let drained: Vec<AgentMessage> = steering2.lock().unwrap().drain(..).collect();
        Box::pin(async move { drained })
    }));
    let (sink, _log) = collect_events();
    let context = AgentContext {
        system_prompt: String::new(),
        openai_responses_input: None,
        messages: vec![],
        tools: vec![Arc::new(EchoTool {
            calls: Default::default(),
        })],
    };
    let messages = run_agent_loop(
        vec![AgentMessage::user("do X")],
        context,
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    // do X, assistant tool, result, "also do Y" injected, assistant done.
    let roles: Vec<&str> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "toolResult", "user", "assistant"]
    );
}

#[tokio::test]
async fn follow_ups_drained_at_stop() {
    let follow_ups: Arc<Mutex<Vec<AgentMessage>>> =
        Arc::new(Mutex::new(vec![AgentMessage::user("follow up")]));
    let mut config = scripted_config(vec![
        assistant_text("first answer", StopReason::Stop),
        assistant_text("second answer", StopReason::Stop),
    ]);
    let f2 = follow_ups.clone();
    config.get_follow_up_messages = Some(Arc::new(move || {
        let drained: Vec<AgentMessage> = f2.lock().unwrap().drain(..).collect();
        Box::pin(async move { drained })
    }));
    let (sink, _log) = collect_events();
    let messages = run_agent_loop(
        vec![AgentMessage::user("q")],
        AgentContext::default(),
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    let roles: Vec<&str> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
}

#[tokio::test]
async fn before_hook_blocks_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut config = scripted_config(vec![
        assistant_tool_call("echo", json!({"value": "z"}), StopReason::ToolUse),
        assistant_text("ok", StopReason::Stop),
    ]);
    config.before_tool_call = Some(Arc::new(|_name, _args| {
        Box::pin(async {
            Some(kiss_agent::BeforeToolCallResult {
                block: true,
                reason: Some("policy says no".into()),
                terminate: false,
            })
        })
    }));
    let (sink, _log) = collect_events();
    let context = AgentContext {
        system_prompt: String::new(),
        openai_responses_input: None,
        messages: vec![],
        tools: vec![Arc::new(EchoTool {
            calls: calls.clone(),
        })],
    };
    let messages = run_agent_loop(
        vec![AgentMessage::user("go")],
        context,
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let AgentMessage::ToolResult(tr) = &messages[2] else {
        panic!()
    };
    assert!(tr.is_error);
    assert!(
        tr.content.iter().any(
            |c| matches!(c, ContentBlock::Text { text, .. } if text.contains("policy says no"))
        )
    );
}

#[tokio::test]
async fn invalid_arguments_rejected_by_schema() {
    let calls = Arc::new(AtomicUsize::new(0));
    let config = scripted_config(vec![
        assistant_tool_call("echo", json!({"value": 42}), StopReason::ToolUse),
        assistant_text("ok", StopReason::Stop),
    ]);
    let (sink, _log) = collect_events();
    let context = AgentContext {
        system_prompt: String::new(),
        openai_responses_input: None,
        messages: vec![],
        tools: vec![Arc::new(EchoTool {
            calls: calls.clone(),
        })],
    };
    let messages = run_agent_loop(
        vec![AgentMessage::user("go")],
        context,
        config,
        CancellationToken::new(),
        sink,
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let AgentMessage::ToolResult(tr) = &messages[2] else {
        panic!()
    };
    assert!(tr.is_error);
}
