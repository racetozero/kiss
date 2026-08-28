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
    let mut wait = wait_before_first_poll;
    loop {
        if wait {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("device authorization expired");
            }
            tokio::select! {
                _ = tokio::time::sleep(delay.min(remaining)) => {},
                _ = cancel.cancelled() => anyhow::bail!("login cancelled"),
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("device authorization expired");
        }
        match request().await? {
            PollResult::Complete(value) => return Ok(value),
            PollResult::Pending => {}
            PollResult::SlowDown(next) => {
                delay = next.unwrap_or(delay + Duration::from_secs(5));
            }
            PollResult::Failed(message) => anyhow::bail!(message),
        }
        wait = true;
    }
}
