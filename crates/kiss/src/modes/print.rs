//! Print mode: run one prompt to completion, print the final assistant
//! text, exit non-zero on error.

use crate::args::Args;
use crate::setup::build_startup;
use anyhow::Result;
use kiss_agent::{AgentEvent, AgentMessage};
use kiss_coding::session_runner::SessionEvent;
use std::io::Read;
use std::sync::{Arc, Mutex};

pub async fn run(args: &Args) -> Result<i32> {
    let final_text: Arc<Mutex<Vec<String>>> = Default::default();
    let error: Arc<Mutex<Option<String>>> = Default::default();
    let sink = {
        let final_text = final_text.clone();
        let error = error.clone();
        Arc::new(move |event: SessionEvent| {
            if let SessionEvent::Agent(agent_event) = event
                && let AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(a),
                } = *agent_event
            {
                if a.stop_reason == kiss_ai::StopReason::Error {
                    *error.lock().unwrap() = Some(a.error_message.clone().unwrap_or_default());
                } else {
                    let text = a.text();
                    if !text.is_empty() {
                        final_text.lock().unwrap().push(text);
                    }
                }
            }
        })
    };

    let startup = build_startup(args, false, sink).await?;

    // Merge piped stdin into the prompt.
    let mut prompt = startup.initial_message.clone().unwrap_or_default();
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        let piped = piped.trim();
        if !piped.is_empty() {
            prompt = if prompt.is_empty() {
                piped.to_string()
            } else {
                format!("{piped}\n\n{prompt}")
            };
        }
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("no prompt provided (pass a message or pipe stdin)");
    }

    startup
        .session
        .prompt(vec![AgentMessage::user(prompt)])
        .await;

    if let Some(err) = error.lock().unwrap().as_ref() {
        eprintln!("error: {err}");
        return Ok(1);
    }
    let texts = final_text.lock().unwrap();
    if let Some(last) = texts.last() {
        println!("{last}");
    }
    Ok(0)
}
