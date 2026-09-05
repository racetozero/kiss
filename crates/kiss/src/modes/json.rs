//! JSON event-stream mode: every session event as one JSON line on stdout.
//!
//! The encoding itself lives in `kiss_sdk::events` so this mode, RPC mode, and
//! the three language SDKs publish byte-identical objects for the same event.
//! Streaming `message_update` events omit the cumulative partial snapshot.

use crate::args::Args;
use crate::setup::build_startup;
use anyhow::Result;
use kiss_coding::session_runner::SessionEvent;
use std::io::Read;
use std::sync::Arc;

pub use kiss_sdk::events::session_event_json as event_json;

pub async fn run(args: &Args) -> Result<i32> {
    let sink = Arc::new(move |event: SessionEvent| {
        if let Some(value) = event_json(&event) {
            println!("{value}");
        }
    });
    let startup = build_startup(args, false, sink).await?;

    let mut prompt = startup.initial_message.clone().unwrap_or_default();
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        if !piped.trim().is_empty() {
            prompt = if prompt.is_empty() {
                piped.trim().to_string()
            } else {
                format!("{}\n\n{prompt}", piped.trim())
            };
        }
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("no prompt provided");
    }
    let prompt_mode = startup.session.prompt_mode_for(&prompt);
    startup
        .session
        .prompt_with_mode(vec![kiss_agent::AgentMessage::user(prompt)], prompt_mode)
        .await;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_coding::session_runner::WorkflowTurnStatus;
    use serde_json::json;

    #[test]
    fn this_mode_uses_the_shared_encoder() {
        // If the two encoders ever diverge, a client written against
        // `--mode json` would silently disagree with one written against RPC.
        let event = event_json(&SessionEvent::WorkflowOutcome {
            run: None,
            name: "audit".into(),
            status: WorkflowTurnStatus::Cancelled,
        })
        .expect("workflow outcome event");

        assert_eq!(
            event,
            json!({
                "type": "workflow_outcome",
                "run": null,
                "name": "audit",
                "status": "cancelled"
            })
        );
    }
}
