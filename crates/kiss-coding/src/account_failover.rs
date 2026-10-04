//! Seamless account failover for providers with a multi-account pool.
//!
//! The wrapper sits between the agent loop and the provider stream. It holds
//! back the `Start` event until the provider sends real output. If the first
//! real event is a rate-limit error for a pooled account, it rotates to the
//! next ready account and sends the same request again. The agent loop, the
//! transcript, and the session file never see the failed attempt.

use kiss_agent::StreamFn;
use kiss_agent::config::BoxFuture;
use kiss_ai::auth::accounts::{self, AccountSwitch};
use kiss_ai::{AssistantEvent, EventStream, Registry, ResolvedCredential};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub(crate) type SwitchFn = Arc<dyn Fn(AccountSwitch) + Send + Sync>;

/// The pool operations failover needs. Production uses the saved pools;
/// tests substitute an in-memory pool.
pub(crate) trait AccountPool: Send + Sync + 'static {
    fn size(&self, provider: &str) -> usize;
    fn rotate(
        &self,
        provider: &str,
        failed_secret: &str,
        retry_after: Option<Duration>,
    ) -> Option<AccountSwitch>;
    fn credential(&self, provider: String) -> BoxFuture<Option<ResolvedCredential>>;
}

struct SavedPools {
    declared: BTreeMap<String, String>,
}

impl AccountPool for SavedPools {
    fn size(&self, provider: &str) -> usize {
        accounts::pool_size(provider)
    }

    fn rotate(
        &self,
        provider: &str,
        failed_secret: &str,
        retry_after: Option<Duration>,
    ) -> Option<AccountSwitch> {
        accounts::mark_limited_and_rotate(provider, failed_secret, retry_after)
            .ok()
            .flatten()
    }

    fn credential(&self, provider: String) -> BoxFuture<Option<ResolvedCredential>> {
        let declared = self.declared.clone();
        Box::pin(async move {
            kiss_ai::auth::resolve_credential_async(&provider, &declared)
                .await
                .ok()
                .flatten()
        })
    }
}

/// Wrap `inner` so pooled providers fail over on account rate limits.
pub(crate) fn wrap(inner: StreamFn, registry: Arc<Registry>, on_switch: SwitchFn) -> StreamFn {
    let pool = Arc::new(SavedPools {
        declared: registry.declared_keys.clone(),
    });
    wrap_with(
        inner,
        Arc::new(move |provider| registry.credential_provider(provider).to_string()),
        pool,
        on_switch,
    )
}

type ProviderMap = Arc<dyn Fn(&str) -> String + Send + Sync>;

fn wrap_with(
    inner: StreamFn,
    credential_provider: ProviderMap,
    pool: Arc<dyn AccountPool>,
    on_switch: SwitchFn,
) -> StreamFn {
    Arc::new(move |model, context, options| {
        let provider = credential_provider(&model.provider);
        let attempts = pool.size(&provider);
        if attempts < 2 || options.credential.is_none() {
            return inner(model, context, options);
        }
        let (sink, stream) = EventStream::channel();
        let (inner, pool, on_switch) = (inner.clone(), pool.clone(), on_switch.clone());
        let (model, context, mut options) = (model.clone(), context.clone(), options.clone());
        tokio::spawn(async move {
            for attempt in 0..attempts {
                let mut upstream = inner(&model, &context, &options);
                let mut held = Vec::new();
                let mut output_started = false;
                let mut next_account = None;
                while let Some(event) = upstream.next().await {
                    if output_started {
                        sink.send(event);
                        continue;
                    }
                    match &event {
                        AssistantEvent::Start { .. } => {
                            held.push(event);
                            continue;
                        }
                        AssistantEvent::Error { message, .. }
                            if attempt + 1 < attempts && !options.cancel.is_cancelled() =>
                        {
                            let failed = options
                                .credential
                                .as_ref()
                                .map(|credential| credential.value().to_string())
                                .unwrap_or_default();
                            if let Some(hint) = message
                                .error_message
                                .as_deref()
                                .and_then(accounts::rate_limit_retry_after)
                                && let Some(switch) = pool.rotate(&provider, &failed, hint)
                                && let Some(credential) = pool.credential(provider.clone()).await
                                && credential.value() != failed
                            {
                                next_account = Some((switch, credential));
                                break;
                            }
                        }
                        _ => {}
                    }
                    output_started = true;
                    for held in held.drain(..) {
                        sink.send(held);
                    }
                    sink.send(event);
                }
                let Some((switch, credential)) = next_account else {
                    // The upstream ended without output; let the consumer
                    // synthesize its usual "ended unexpectedly" error.
                    for held in held {
                        sink.send(held);
                    }
                    return;
                };
                on_switch(switch);
                options.credential = Some(credential);
            }
        });
        stream
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::{AssistantMessage, StopReason, StreamOptions};
    use std::sync::Mutex;

    /// In-memory pool of secrets. Rotation moves to the next secret.
    struct FakePool {
        secrets: Vec<&'static str>,
        active: Mutex<usize>,
        rotations: Mutex<Vec<(String, Option<Duration>)>>,
    }

    impl FakePool {
        fn new(secrets: Vec<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                secrets,
                active: Mutex::new(0),
                rotations: Mutex::new(Vec::new()),
            })
        }
    }

    impl AccountPool for FakePool {
        fn size(&self, provider: &str) -> usize {
            if provider == "openai-codex" {
                self.secrets.len()
            } else {
                0
            }
        }

        fn rotate(
            &self,
            _: &str,
            failed: &str,
            retry_after: Option<Duration>,
        ) -> Option<AccountSwitch> {
            self.rotations
                .lock()
                .unwrap()
                .push((failed.to_string(), retry_after));
            let mut active = self.active.lock().unwrap();
            let failed_index = self.secrets.iter().position(|secret| *secret == failed)?;
            let next = (failed_index + 1) % self.secrets.len();
            *active = next;
            Some(AccountSwitch {
                provider: "openai-codex".into(),
                from: failed.into(),
                to: self.secrets[next].into(),
                index: next,
                total: self.secrets.len(),
                retry_after_secs: retry_after.map_or(0, |d| d.as_secs()),
            })
        }

        fn credential(&self, _: String) -> BoxFuture<Option<ResolvedCredential>> {
            let secret = self.secrets[*self.active.lock().unwrap()];
            Box::pin(async move { Some(ResolvedCredential::bearer(secret)) })
        }
    }

    fn model(provider: &str) -> kiss_ai::Model {
        kiss_ai::Model {
            id: "gpt-test".into(),
            name: "test".into(),
            api: "openai-codex-responses".into(),
            provider: provider.into(),
            base_url: String::new(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 1000,
            max_tokens: 100,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
            sampling_params: Default::default(),
        }
    }

    /// A provider whose behavior depends on the credential it receives.
    fn scripted(
        script: fn(&str) -> Vec<AssistantEvent>,
        calls: Arc<Mutex<Vec<String>>>,
    ) -> StreamFn {
        Arc::new(move |_, _, options: &StreamOptions| {
            let secret = options.credential.as_ref().unwrap().value().to_string();
            calls.lock().unwrap().push(secret.clone());
            let (sink, stream) = EventStream::channel();
            for event in script(&secret) {
                sink.send(event);
            }
            stream
        })
    }

    fn message(error: Option<&str>) -> AssistantMessage {
        let mut message = AssistantMessage::empty("openai-codex-responses", "openai-codex", "gpt");
        if let Some(error) = error {
            message.stop_reason = StopReason::Error;
            message.error_message = Some(error.into());
        }
        message
    }

    fn start() -> AssistantEvent {
        AssistantEvent::Start {
            partial: message(None),
        }
    }

    fn error(text: &str) -> AssistantEvent {
        AssistantEvent::Error {
            reason: StopReason::Error,
            message: message(Some(text)),
        }
    }

    fn text(delta: &str) -> AssistantEvent {
        AssistantEvent::TextDelta {
            content_index: 0,
            delta: delta.into(),
        }
    }

    fn done() -> AssistantEvent {
        AssistantEvent::Done {
            reason: StopReason::Stop,
            message: message(None),
        }
    }

    const LIMITED: &str = r#"HTTP 429 Too Many Requests: {"error":{"type":"usage_limit_reached","resets_in_seconds":3600}}"#;

    async fn run(
        pool: Arc<FakePool>,
        provider: &str,
        script: fn(&str) -> Vec<AssistantEvent>,
    ) -> (Vec<AssistantEvent>, Vec<String>, Vec<AccountSwitch>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let switches = Arc::new(Mutex::new(Vec::new()));
        let recorded = switches.clone();
        let stream_fn = wrap_with(
            scripted(script, calls.clone()),
            Arc::new(|provider| provider.to_string()),
            pool,
            Arc::new(move |switch| recorded.lock().unwrap().push(switch)),
        );
        let options = StreamOptions {
            credential: Some(ResolvedCredential::bearer("one")),
            ..Default::default()
        };
        let mut stream = stream_fn(&model(provider), &Default::default(), &options);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        let calls = calls.lock().unwrap().clone();
        let switches = switches.lock().unwrap().clone();
        (events, calls, switches)
    }

    fn has_error(events: &[AssistantEvent]) -> bool {
        events
            .iter()
            .any(|event| matches!(event, AssistantEvent::Error { .. }))
    }

    #[tokio::test]
    async fn rate_limited_account_fails_over_before_output() {
        let pool = FakePool::new(vec!["one", "two"]);
        let (events, calls, switches) = run(pool.clone(), "openai-codex", |secret| match secret {
            "one" => vec![start(), error(LIMITED)],
            _ => vec![start(), text("hello"), done()],
        })
        .await;
        assert_eq!(calls, ["one", "two"]);
        assert!(!has_error(&events));
        assert!(matches!(events[0], AssistantEvent::Start { .. }));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantEvent::Start { .. }))
                .count(),
            1
        );
        assert!(matches!(&events[1], AssistantEvent::TextDelta { delta, .. } if delta == "hello"));
        assert_eq!(switches.len(), 1);
        assert_eq!(
            (switches[0].from.as_str(), switches[0].to.as_str()),
            ("one", "two")
        );
        assert_eq!(
            pool.rotations.lock().unwrap()[0],
            ("one".to_string(), Some(Duration::from_secs(3600)))
        );
    }

    #[tokio::test]
    async fn every_account_limited_returns_the_last_error() {
        let pool = FakePool::new(vec!["one", "two"]);
        let (events, calls, switches) =
            run(pool, "openai-codex", |_| vec![start(), error(LIMITED)]).await;
        // One attempt per account, then the error reaches the agent loop.
        assert_eq!(calls, ["one", "two"]);
        assert_eq!(switches.len(), 1);
        assert!(has_error(&events));
    }

    #[tokio::test]
    async fn errors_after_output_or_unrelated_errors_pass_through() {
        let pool = FakePool::new(vec!["one", "two"]);
        let (events, calls, _) = run(pool.clone(), "openai-codex", |_| {
            vec![start(), text("partial"), error(LIMITED)]
        })
        .await;
        assert_eq!(calls, ["one"]);
        assert!(has_error(&events));

        let (events, calls, _) = run(pool.clone(), "openai-codex", |_| {
            vec![start(), error("HTTP 401 Unauthorized: bad token")]
        })
        .await;
        assert_eq!(calls, ["one"]);
        assert!(has_error(&events));

        let (events, calls, _) = run(pool.clone(), "openai-codex", |_| {
            vec![start(), error("HTTP 529: overloaded_error")]
        })
        .await;
        assert_eq!(calls, ["one"]);
        assert!(has_error(&events));
        assert!(pool.rotations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn providers_without_a_pool_are_not_wrapped() {
        let pool = FakePool::new(vec!["one", "two"]);
        let (events, calls, switches) =
            run(pool, "anthropic", |_| vec![start(), error(LIMITED)]).await;
        assert_eq!(calls, ["one"]);
        assert!(switches.is_empty());
        assert!(has_error(&events));
    }
}
