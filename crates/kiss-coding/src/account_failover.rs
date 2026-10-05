//! Seamless account failover for providers with a multi-account pool.
//!
//! The wrapper sits between the agent loop and the provider stream. It holds
//! back the `Start` event until the provider sends real output. If the first
//! real event is a rate-limit error for a pooled account, it rotates to the
//! next ready account and sends the same request again. The agent loop, the
//! transcript, and the session file never see the failed attempt.

use kiss_agent::StreamFn;
use kiss_ai::auth::accounts::{self, AccountSwitch};
use kiss_ai::{AssistantEvent, EventStream, Registry};
use std::sync::Arc;

pub(crate) type SwitchFn = Arc<dyn Fn(AccountSwitch) + Send + Sync>;

/// Wrap `inner` so pooled providers fail over on account rate limits.
pub(crate) fn wrap(inner: StreamFn, registry: Arc<Registry>, on_switch: SwitchFn) -> StreamFn {
    Arc::new(move |model, context, options| {
        let provider = registry.credential_provider(&model.provider).to_string();
        let attempts = accounts::pool(&provider).map_or(0, |pool| pool.accounts.len());
        if attempts < 2 || options.credential.is_none() {
            return inner(model, context, options);
        }
        let (sink, stream) = EventStream::channel();
        let (inner, on_switch) = (inner.clone(), on_switch.clone());
        let declared = registry.declared_keys.clone();
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
                                && let Ok(Some(switch)) =
                                    accounts::mark_limited_and_rotate(&provider, &failed, hint)
                                && let Ok(Some(credential)) =
                                    kiss_ai::auth::resolve_credential_async(&provider, &declared)
                                        .await
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
                    // A stream that ends without a terminal event still
                    // releases its held Start.
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
