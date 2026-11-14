//! Anthropic message threads, as in Pi v1.5.1. The server keeps the
//! conversation of a thread, so a request can continue from the previous
//! reply and send only the new messages. KISS still builds the full request.
//! [`prepare`] compares it with the previous request of the session and
//! sends only the new part when nothing earlier changed.

use crate::model::Model;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

pub(crate) const BETA: &str = "message-threads-2026-08-12";

/// Sessions kept at most. A dropped session starts a new thread on its next
/// request; its old thread most likely expired on the server already.
const MAX_SESSIONS: usize = 10_000;

/// Rejections of a `continue` that a new thread fixes: expired, raced, or a
/// fixed setting changed.
const NEW_THREAD_ERRORS: [&str; 4] = [
    "thread_not_found",
    "thread_already_continued",
    "thread_continue_in_flight",
    "thread_fingerprint_mismatch",
];

/// Where a session's next request can continue.
struct Continuation {
    /// Messages in the last full request plus the converted reply.
    prefix_count: usize,
    /// [`prefix_hash`] of the request's settings and those messages.
    prefix_hash: String,
    /// ID of the reply to continue from.
    reply_id: String,
}

enum State {
    Ready(Continuation),
    /// The endpoint rejected threads. Requests still send the beta, which
    /// the full resend was accepted with, but no `thread` field.
    Off,
}

#[derive(Default)]
struct Sessions {
    /// Per session ID: the state and when it was last set, for the size cap.
    states: HashMap<String, (State, u64)>,
    clock: u64,
}

static SESSIONS: LazyLock<Mutex<Sessions>> = LazyLock::new(Mutex::default);

fn set(session_id: &str, state: State) {
    let mut sessions = SESSIONS.lock().unwrap();
    sessions.clock += 1;
    let touched = sessions.clock;
    sessions
        .states
        .insert(session_id.to_string(), (state, touched));
    if sessions.states.len() > MAX_SESSIONS
        && let Some(oldest) = sessions
            .states
            .iter()
            .min_by_key(|(_, (_, touched))| *touched)
            .map(|(id, _)| id.clone())
    {
        sessions.states.remove(&oldest);
    }
}

/// Whether requests for `model` may use threads. Gateways that drop unknown
/// fields would turn a `continue` into a request without the earlier
/// conversation, so the default is Anthropic's own endpoint only.
pub(crate) fn supported(model: &Model) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_message_threads)
        .unwrap_or(model.provider == "anthropic" && model.base_url.contains("api.anthropic.com"))
}

/// Hash of everything a `continue` builds on: the settings that must stay
/// the same within a thread, as sent, and the messages the server holds. A
/// `continue` omits `system` and `tools` and the server keeps the thread's
/// own, so only this check can notice a change to them. Messages are
/// normalized so that messages that mean the same to the API hash the same:
/// cache markers move between requests and are dropped, and string content
/// equals one text block.
fn prefix_hash(model: &Model, body: &Value, betas: &str, messages: &[Value]) -> String {
    fn normalize(value: &Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| *key != "cache_control")
                    .map(|(key, value)| match (key.as_str(), value) {
                        ("content", Value::String(text)) => {
                            (key.clone(), json!([{"type": "text", "text": text}]))
                        }
                        _ => (key.clone(), normalize(value)),
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
            other => other.clone(),
        }
    }
    let settings = json!([
        model.provider,
        model.base_url,
        body["model"],
        body["system"],
        body["tools"],
        body["thinking"],
        body["output_config"],
        body["tool_choice"],
        betas,
    ]);
    let conversation = Value::Array(messages.iter().map(normalize).collect());
    let digest = Sha256::digest(format!("{settings}{conversation}").as_bytes());
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The body to send for `body`, the full request of `session_id`: a
/// `continue` with only the new messages when the request starts with the
/// last request and its reply, otherwise a new thread. `None` after the
/// endpoint rejected threads for the session.
pub(crate) fn prepare(model: &Model, session_id: &str, body: &Value, betas: &str) -> Option<Value> {
    let mut guard = SESSIONS.lock().unwrap();
    let messages = body["messages"].as_array().map_or(&[][..], Vec::as_slice);
    let continued = match guard.states.get(session_id) {
        Some((State::Off, _)) => return None,
        Some((State::Ready(state), _))
            if messages.len() > state.prefix_count
                && prefix_hash(model, body, betas, &messages[..state.prefix_count])
                    == state.prefix_hash =>
        {
            let mut continued = body.clone();
            if let Some(object) = continued.as_object_mut() {
                object.remove("system");
                object.remove("tools");
            }
            continued["messages"] = Value::Array(messages[state.prefix_count..].to_vec());
            continued["thread"] =
                json!({"type": "continue", "previous_message_id": state.reply_id});
            Some(continued)
        }
        _ => None,
    };
    if continued.is_some() {
        // Each reply can be continued once. Removing also keeps a
        // concurrent request from trying.
        guard.states.remove(session_id);
    }
    drop(guard);
    Some(continued.unwrap_or_else(|| create(body)))
}

/// `body` as the first request of a new thread.
pub(crate) fn create(body: &Value) -> Value {
    let mut created = body.clone();
    created["thread"] = json!({"type": "create"});
    created
}

/// What to do after the endpoint rejected a thread request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// Resend in full as a new thread.
    NewThread,
    /// Resend in full without `thread`, and stop using threads for the session.
    WithoutThread,
}

/// The fallback for a rejected thread request, or `None` when the error is
/// not about the thread (prompt too long, unknown model, ...) and resending
/// would only fail again.
pub(crate) fn fallback(status: u16, body: &str) -> Option<Fallback> {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = parsed["error"]["details"]["error_code"].as_str();
    match code {
        Some(code) if NEW_THREAD_ERRORS.contains(&code) => Some(Fallback::NewThread),
        Some("thread_unsupported_request") => Some(Fallback::WithoutThread),
        // An endpoint that does not know the field rejects it without a
        // code ("thread: Extra inputs are not permitted").
        None if status == 400 && body.contains("thread") => Some(Fallback::WithoutThread),
        _ => None,
    }
}

pub(crate) fn turn_off(session_id: &str) {
    set(session_id, State::Off);
}

/// Record where the session's next request can continue: after `reply`,
/// the converted assistant message that answered `body`.
pub(crate) fn record(
    model: &Model,
    session_id: &str,
    body: &Value,
    betas: &str,
    reply: Option<Value>,
    reply_id: &str,
) {
    let mut prefix = body["messages"].as_array().cloned().unwrap_or_default();
    prefix.extend(reply);
    set(
        session_id,
        State::Ready(Continuation {
            prefix_count: prefix.len(),
            prefix_hash: prefix_hash(model, body, betas, &prefix),
            reply_id: reply_id.to_string(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model {
            id: "claude-test".into(),
            name: String::new(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 200_000,
            max_tokens: 1_000,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
            sampling_params_by_thinking_level: Default::default(),
        }
    }

    fn body(messages: Value) -> Value {
        json!({"model": "claude-test", "system": [{"type": "text", "text": "s"}], "tools": [], "messages": messages})
    }

    #[test]
    fn a_request_that_extends_the_last_one_continues_the_thread() {
        let model = model();
        let session = "threads-continue";
        let first = body(
            json!([{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]}]),
        );
        assert_eq!(
            prepare(&model, session, &first, "b").unwrap()["thread"],
            json!({"type": "create"})
        );
        let reply = json!({"role": "assistant", "content": [{"type": "text", "text": "hello"}]});
        record(&model, session, &first, "b", Some(reply.clone()), "msg_1");

        let second = body(json!([
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            reply,
            {"role": "user", "content": [{"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}]},
        ]));
        let sent = prepare(&model, session, &second, "b").unwrap();
        assert_eq!(
            sent["thread"],
            json!({"type": "continue", "previous_message_id": "msg_1"})
        );
        assert_eq!(sent["messages"].as_array().unwrap().len(), 1);
        assert!(sent.get("system").is_none() && sent.get("tools").is_none());
        // A reply can be continued once.
        assert_eq!(
            prepare(&model, session, &second, "b").unwrap()["thread"]["type"],
            "create"
        );
    }

    #[test]
    fn a_changed_prefix_or_setting_starts_a_new_thread() {
        let model = model();
        let session = "threads-changed";
        let first = body(json!([{"role": "user", "content": "hi"}]));
        record(&model, session, &first, "b", None, "msg_1");
        let edited =
            body(json!([{"role": "user", "content": "changed"}, {"role": "user", "content": "x"}]));
        assert_eq!(
            prepare(&model, session, &edited, "b").unwrap()["thread"]["type"],
            "create"
        );

        record(&model, session, &first, "b", None, "msg_1");
        let mut new_system =
            body(json!([{"role": "user", "content": "hi"}, {"role": "user", "content": "x"}]));
        new_system["system"] = json!("other");
        assert_eq!(
            prepare(&model, session, &new_system, "b").unwrap()["thread"]["type"],
            "create"
        );
    }

    #[test]
    fn thread_rejections_choose_a_fallback() {
        let error = |code: &str| {
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "x", "details": {"error_code": code}}})
                .to_string()
        };
        assert_eq!(
            fallback(400, &error("thread_not_found")),
            Some(Fallback::NewThread)
        );
        assert_eq!(
            fallback(400, &error("thread_unsupported_request")),
            Some(Fallback::WithoutThread)
        );
        assert_eq!(
            fallback(
                400,
                r#"{"error":{"message":"thread: Extra inputs are not permitted"}}"#
            ),
            Some(Fallback::WithoutThread)
        );
        assert_eq!(
            fallback(400, r#"{"error":{"message":"prompt is too long"}}"#),
            None
        );
        assert_eq!(fallback(500, &error("overloaded")), None);

        turn_off("threads-off");
        assert!(prepare(&model(), "threads-off", &body(json!([])), "b").is_none());
    }
}
