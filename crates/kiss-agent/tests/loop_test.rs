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
use std::time::{Duration, Instant};
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

fn assistant_tool_batch(name: &str, count: usize) -> AssistantMessage {
    let mut message = AssistantMessage::empty("fake", "fake", "fake-model");
    message.content.extend((0..count).map(|index| {
        ContentBlock::ToolCall(ToolCall {
            id: format!("call_{index}"),
            name: name.to_string(),
            arguments: json!({"value": index.to_string()}),
            thought_signature: None,
        })
    }));
    message.stop_reason = StopReason::ToolUse;
    message
}

struct EchoTool {
    calls: Arc<AtomicUsize>,
}

struct TerminatingTool;

struct MeasuredTool {
    calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    delay: Duration,
}

impl MeasuredTool {
    fn new(delay: Duration) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            delay,
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl AgentTool for MeasuredTool {
    fn name(&self) -> &str {
        "measured"
    }

    fn description(&self) -> String {
        "measure parallel tool dispatch".into()
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(active, Ordering::Relaxed);
        let index = args["value"]
            .as_str()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_default();
        tokio::time::sleep(self.delay + Duration::from_millis(10 - index.min(10) as u64)).await;
        self.active.fetch_sub(1, Ordering::Relaxed);
        Ok(ToolResult::text(args["value"].as_str().unwrap_or_default()))
    }
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

#[async_trait::async_trait]
impl AgentTool for TerminatingTool {
    fn name(&self) -> &str {
        "finish"
    }

    fn description(&self) -> String {
        "finish the loop".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(
        &self,
        _id: &str,
        _args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            terminate: true,
            ..ToolResult::text("finished")
        })
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
async fn prepare_next_turn_replaces_context_before_the_next_request() {
    let seen_contexts: Arc<Mutex<Vec<Vec<String>>>> = Default::default();
    let seen_by_stream = seen_contexts.clone();
    let responses = Arc::new(Mutex::new(vec![
        assistant_tool_call("echo", json!({"value": "large"}), StopReason::ToolUse),
        assistant_text("done", StopReason::Stop),
    ]));
    let responses_by_stream = responses.clone();
    let mut config = AgentLoopConfig::new(fake_model());
    config.stream_fn = Arc::new(move |_, context, _| {
        seen_by_stream.lock().unwrap().push(
            context
                .messages
                .iter()
                .map(|message| match message {
                    kiss_ai::Message::User(user) => user.content.as_text(),
                    kiss_ai::Message::Assistant(assistant) => assistant.text(),
                    kiss_ai::Message::ToolResult(result) => result.tool_name.clone(),
                })
                .collect(),
        );
        let (sink, stream) = EventStream::channel();
        sink.done(responses_by_stream.lock().unwrap().remove(0));
        stream
    });
    config.prepare_next_turn = Some(Arc::new(|turn| {
        let after_tool = !turn.tool_results.is_empty();
        Box::pin(async move {
            after_tool.then(|| kiss_agent::TurnUpdate {
                context: Some(AgentContext {
                    system_prompt: String::new(),
                    openai_responses_input: None,
                    messages: vec![AgentMessage::user("compacted context")],
                    tools: Vec::new(),
                }),
                ..Default::default()
            })
        })
    }));
    let (sink, _) = collect_events();
    run_agent_loop(
        vec![AgentMessage::user("original context")],
        AgentContext {
            tools: vec![Arc::new(EchoTool {
                calls: Default::default(),
            })],
            ..Default::default()
        },
        config,
        CancellationToken::new(),
        sink,
    )
    .await;

    let seen = seen_contexts.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], ["original context"]);
    assert_eq!(seen[1], ["compacted context"]);
}

#[tokio::test]
async fn prepare_next_turn_knows_when_a_tool_terminates_the_loop() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_by_hook = observed.clone();
    let mut config = scripted_config(vec![assistant_tool_call(
        "finish",
        json!({}),
        StopReason::ToolUse,
    )]);
    config.prepare_next_turn = Some(Arc::new(move |turn| {
        observed_by_hook.lock().unwrap().push(turn.will_continue);
        Box::pin(async { None })
    }));

    let messages = run_agent_loop(
        vec![AgentMessage::user("finish")],
        AgentContext {
            tools: vec![Arc::new(TerminatingTool)],
            ..Default::default()
        },
        config,
        CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await;

    assert_eq!(*observed.lock().unwrap(), [false]);
    assert_eq!(messages.len(), 3);
}

#[tokio::test]
async fn prepare_generation_runs_before_first_and_following_model_calls() {
    let prepared = Arc::new(AtomicUsize::new(0));
    let prepared_by_hook = prepared.clone();
    let applied = Arc::new(AtomicUsize::new(0));
    let applied_by_stream = applied.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_by_stream = seen.clone();
    let mut config = scripted_config(vec![
        assistant_tool_call("echo", json!({"value": "x"}), StopReason::ToolUse),
        assistant_text("done", StopReason::Stop),
    ]);
    let stream = config.stream_fn.clone();
    config.stream_fn = Arc::new(move |model, context, options| {
        seen_by_stream.lock().unwrap().push((
            prepared_by_hook.load(Ordering::SeqCst),
            applied_by_stream.load(Ordering::SeqCst),
            options.reasoning,
        ));
        stream(model, context, options)
    });
    let prepared_by_hook = prepared.clone();
    let applied_by_hook = applied.clone();
    config.prepare_generation = Some(Arc::new(move |_| {
        let generation = prepared_by_hook.fetch_add(1, Ordering::SeqCst);
        let applied = applied_by_hook.clone();
        Box::pin(async move {
            Some(kiss_agent::TurnUpdate {
                thinking_level: Some(if generation == 0 {
                    kiss_ai::ThinkingLevel::Low
                } else {
                    kiss_ai::ThinkingLevel::High
                }),
                on_applied: Some(Box::new(move |_, _| {
                    applied.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            })
        })
    }));

    run_agent_loop(
        vec![AgentMessage::user("inspect")],
        AgentContext {
            tools: vec![Arc::new(EchoTool {
                calls: Default::default(),
            })],
            ..Default::default()
        },
        config,
        CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await;

    assert_eq!(
        *seen.lock().unwrap(),
        [
            (1, 1, kiss_ai::ThinkingLevel::Low),
            (2, 2, kiss_ai::ThinkingLevel::High)
        ]
    );
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

#[tokio::test]
async fn parallel_tool_limit_applies_backpressure_and_keeps_result_order() {
    let tool = Arc::new(MeasuredTool::new(Duration::from_millis(1)));
    let mut config = scripted_config(vec![
        assistant_tool_batch("measured", 12),
        assistant_text("done", StopReason::Stop),
    ]);
    config.max_concurrent_tools = 3;
    let messages = run_agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            tools: vec![tool.clone()],
            ..Default::default()
        },
        config,
        CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await;

    assert_eq!(tool.peak(), 3);
    let ids = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        (0..12)
            .map(|index| format!("call_{index}"))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn zero_parallel_tool_limit_runs_one_call_at_a_time() {
    let tool = Arc::new(MeasuredTool::new(Duration::ZERO));
    let mut config = scripted_config(vec![
        assistant_tool_batch("measured", 2),
        assistant_text("done", StopReason::Stop),
    ]);
    config.max_concurrent_tools = 0;
    run_agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            tools: vec![tool.clone()],
            ..Default::default()
        },
        config,
        CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await;

    assert_eq!(tool.peak(), 1);
    assert_eq!(tool.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn cancellation_does_not_start_queued_parallel_tools() {
    let tool = Arc::new(MeasuredTool::new(Duration::from_millis(20)));
    let mut config = scripted_config(vec![assistant_tool_batch("measured", 12)]);
    config.max_concurrent_tools = 3;
    let cancel = CancellationToken::new();
    let cancel_soon = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(2)).await;
        cancel_soon.cancel();
    });
    let messages = run_agent_loop(
        vec![AgentMessage::user("go")],
        AgentContext {
            tools: vec![tool.clone()],
            ..Default::default()
        },
        config,
        cancel,
        Arc::new(|_| {}),
    )
    .await;

    assert_eq!(tool.calls.load(Ordering::Relaxed), 3);
    assert_eq!(
        messages
            .iter()
            .filter(
                |message| matches!(message, AgentMessage::ToolResult(result) if result.is_error)
            )
            .count(),
        9
    );
}

#[tokio::test]
#[ignore = "release-mode performance benchmark"]
async fn benchmark_performance_200_tool_calls() {
    const CALLS: usize = 200;
    for (name, limit) in [("unlimited", Some(usize::MAX)), ("bounded", None)] {
        let tool = Arc::new(MeasuredTool::new(Duration::from_millis(10)));
        let mut config = scripted_config(vec![
            assistant_tool_batch("measured", CALLS),
            assistant_text("done", StopReason::Stop),
        ]);
        if let Some(limit) = limit {
            config.max_concurrent_tools = limit;
        }
        let context = AgentContext {
            tools: vec![tool.clone()],
            ..Default::default()
        };

        let started = Instant::now();
        let messages = run_agent_loop(
            vec![AgentMessage::user("go")],
            context,
            config,
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await;
        let elapsed = started.elapsed();
        let results = messages
            .iter()
            .filter(|message| matches!(message, AgentMessage::ToolResult(_)))
            .count();
        assert_eq!(results, CALLS);
        assert_eq!(tool.calls.load(Ordering::Relaxed), CALLS);

        let mut sample = [elapsed.as_nanos() / CALLS as u128];
        kiss_bench::report(
            &format!("agent_tool_batch_200_{name}"),
            &mut sample,
            CALLS,
            &format!("10-20ms_tool_peak_active={}", tool.peak()),
        );
    }
}
