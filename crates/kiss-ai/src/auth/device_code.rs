//! Shared RFC 8628 device-code polling.

use anyhow::Result;
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub enum PollResult<T> {
    Complete(T),
    Pending,
    SlowDown(Option<Duration>),
    Failed(String),
}

enum PollPhase {
    Waiting,
    Requesting,
}

pub async fn poll<T, F, Fut>(
    interval: Duration,
    expires_in: Duration,
    wait_before_first_poll: bool,
    cancel: &CancellationToken,
    mut request: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<PollResult<T>>>,
{
    let deadline = tokio::time::Instant::now() + expires_in;
    let mut delay = interval.max(Duration::from_secs(1));
    let mut phase = if wait_before_first_poll {
        PollPhase::Waiting
    } else {
        PollPhase::Requesting
    };
    loop {
        // The owner bounds both the delay and the complete token request,
        // including response-body reads performed by provider callbacks.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Device login was cancelled. Start login again to use a new authorization code."),
            _ = tokio::time::sleep_until(deadline) => anyhow::bail!("The device authorization code expired during login. Start login again and use the new code before it expires."),
            result = async {
                match phase {
                    PollPhase::Waiting => {
                        tokio::time::sleep(delay).await;
                        None
                    }
                    PollPhase::Requesting => Some(request().await),
                }
            } => match result {
                None => phase = PollPhase::Requesting,
                Some(result) => {
                    match result? {
                        PollResult::Complete(value) => return Ok(value),
                        PollResult::Pending => {}
                        PollResult::SlowDown(next) => {
                            delay = next.unwrap_or(delay + Duration::from_secs(5));
                        }
                        PollResult::Failed(message) => anyhow::bail!(message),
                    }
                    phase = PollPhase::Waiting;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn pending_and_slow_down_preserve_poll_delays_until_completion() {
        let cancel = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let mut results = [
            PollResult::Pending,
            PollResult::SlowDown(None),
            PollResult::SlowDown(Some(Duration::from_secs(3))),
            PollResult::Complete("token"),
        ]
        .into_iter();
        let mut requests = Vec::new();
        let token = poll(
            Duration::from_secs(2),
            Duration::from_secs(30),
            true,
            &cancel,
            || {
                requests.push(started.elapsed().as_secs());
                std::future::ready(Ok(results.next().unwrap()))
            },
        )
        .await
        .unwrap();
        assert_eq!(token, "token");
        assert_eq!(requests, [2, 4, 11, 14]);
        let error = poll(
            Duration::from_secs(1),
            Duration::from_secs(30),
            false,
            &cancel,
            || async { Ok(PollResult::<()>::Failed("authorization denied".into())) },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "authorization denied");
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_and_expiry_end_a_pending_token_request() {
        for case in [
            "cancel before request",
            "cancel during request",
            "expire during request",
        ] {
            let cancel = CancellationToken::new();
            if case == "cancel before request" {
                cancel.cancel();
            }
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                poll(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    false,
                    &cancel,
                    || async {
                        if case == "cancel during request" {
                            cancel.cancel();
                        }
                        std::future::pending::<Result<PollResult<()>>>().await
                    },
                ),
            )
            .await
            .expect(case);
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains(if case.starts_with("cancel") {
                    "cancelled"
                } else {
                    "expired"
                }),
                "{case}: {error}"
            );
        }
    }
}
