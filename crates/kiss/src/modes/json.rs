//! JSON event-stream mode: every session event as one JSON line on stdout.
//!
//! The encoding itself lives in `kiss_sdk::events` so this mode, RPC mode, and
//! the three language SDKs publish byte-identical objects for the same event.
//! Streaming `message_update` events omit the cumulative partial snapshot.

use crate::args::Args;
use crate::setup::build_startup;
use anyhow::Result;
use kiss_coding::session_runner::SessionEvent;
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

    let prompt = super::print::prompt_with_stdin(startup.initial_message.clone())?;
    let prompt_mode = startup.session.prompt_mode_for(&prompt);
    startup
        .session
        .prompt_with_mode(vec![kiss_agent::AgentMessage::user(prompt)], prompt_mode)
        .await;
    Ok(0)
}
