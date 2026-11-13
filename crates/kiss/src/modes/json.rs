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

use kiss_sdk::events::session_event_json;

pub async fn run(args: &Args) -> Result<i32> {
    let sink = Arc::new(move |event: SessionEvent| {
        if let Some(value) = session_event_json(&event) {
            println!("{value}");
        }
    });
    let startup = build_startup(args, false, sink).await?;
    for warning in &startup.warnings {
        eprintln!("warning: {warning}");
    }

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
