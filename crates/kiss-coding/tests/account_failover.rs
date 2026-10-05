//! Account failover through a real `AgentSession` and real pool files.
//! Own test binary: it points HOME at a temporary directory.

use kiss_agent::AgentMessage;
use kiss_ai::{AssistantEvent, AssistantMessage, ContentBlock, EventStream, StopReason};
use kiss_coding::{AgentSession, SessionEvent, SessionManager, Settings};
use std::sync::{Arc, Mutex};

const LIMITED: &str = r#"HTTP 429 Too Many Requests: {"error":{"type":"usage_limit_reached","resets_in_seconds":3600}}"#;

#[tokio::test]
async fn rate_limited_account_fails_over_before_output() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads HOME.
    unsafe { std::env::set_var("HOME", home.path()) };
    use kiss_ai::auth::{accounts, store_api_key};
    store_api_key("xai", "key-one").unwrap();
    accounts::begin_add("xai").unwrap();
    store_api_key("xai", "key-two").unwrap();
    accounts::finish_add("xai", Some("backup")).unwrap();

    let registry = kiss_ai::Registry::load(None);
    let model = registry
        .all()
        .iter()
        .find(|m| m.provider == "xai")
        .unwrap()
        .clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let saved = events.clone();
    let session = AgentSession::new(
        SessionManager::in_memory(home.path()),
        Vec::new(),
        registry,
        Settings::default(),
        "test".into(),
        model,
        kiss_ai::ThinkingLevel::Off,
        None,
        Arc::new(move |event| saved.lock().unwrap().push(event)),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    session.set_stream_fn(Some(Arc::new(move |_, _, options| {
        let secret = options.credential.as_ref().unwrap().value().to_string();
        seen.lock().unwrap().push(secret.clone());
        let (sink, stream) = EventStream::channel();
        let mut message = AssistantMessage::empty("openai-completions", "xai", "grok");
        sink.send(AssistantEvent::Start {
            partial: message.clone(),
        });
        if secret == "key-one" {
            message.stop_reason = StopReason::Error;
            message.error_message = Some(LIMITED.into());
            sink.error(message);
        } else {
            message.content.push(ContentBlock::text("hello"));
            sink.done(message);
        }
        stream
    })));

    session.prompt(vec![AgentMessage::user("hi")]).await;

    assert_eq!(*calls.lock().unwrap(), ["key-one", "key-two"]);
    let events = events.lock().unwrap();
    assert!(events.iter().any(|e| matches!(e, SessionEvent::AccountSwitched(s) if s.to == "backup" && s.retry_after_secs == 3600)));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::Retry { .. }))
    );
    let pool = accounts::pool("xai").unwrap();
    assert_eq!(pool.active().unwrap().label, "backup");
    assert!(pool.accounts[0].is_limited());
    let text = serde_json::to_string(
        &session
            .manager
            .lock()
            .unwrap()
            .build_session_context()
            .messages,
    )
    .unwrap();
    assert!(text.contains("hello") && !text.contains("usage_limit_reached"));
}
