//! Print mode: run one prompt to completion, print the final assistant
//! text, exit non-zero on error.

use crate::args::Args;
use crate::setup::build_startup;
use anyhow::Result;
use kiss_agent::{AgentEvent, AgentMessage};
use kiss_coding::session_runner::{SessionEvent, WorkflowTurnStatus};
use std::io::Read;
use std::sync::{Arc, Mutex};

pub async fn run(args: &Args) -> Result<i32> {
    let final_text: Arc<Mutex<Vec<String>>> = Default::default();
    let error: Arc<Mutex<Option<String>>> = Default::default();
    let truncated: Arc<Mutex<bool>> = Default::default();
    let workflow_outcomes: Arc<Mutex<Vec<String>>> = Default::default();
    let sink = {
        let final_text = final_text.clone();
        let error = error.clone();
        let truncated = truncated.clone();
        let workflow_outcomes = workflow_outcomes.clone();
        Arc::new(move |event: SessionEvent| match event {
            SessionEvent::Agent(agent_event) => {
                if let AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(a),
                } = *agent_event
                {
                    *truncated.lock().unwrap() = a.stop_reason == kiss_ai::StopReason::Length;
                    // The last assistant message decides the outcome, so a
                    // failed attempt that a retry then answered is not an error.
                    if a.stop_reason == kiss_ai::StopReason::Error {
                        *error.lock().unwrap() = Some(a.error_message.clone().unwrap_or_default());
                    } else {
                        *error.lock().unwrap() = None;
                        let text = a.text();
                        if !text.is_empty() {
                            final_text.lock().unwrap().push(text);
                        }
                    }
                }
            }
            SessionEvent::WorkflowOutcome { run, name, status } => {
                let run = run.map(|id| format!(" run {id}")).unwrap_or_default();
                let result = match status {
                    WorkflowTurnStatus::Cancelled => "was cancelled. No agents ran",
                    WorkflowTurnStatus::Completed => "completed",
                    WorkflowTurnStatus::Failed => "failed",
                    WorkflowTurnStatus::Stopped => "was stopped",
                };
                workflow_outcomes
                    .lock()
                    .unwrap()
                    .push(format!("verified workflow{run} `{name}` {result}"));
            }
            _ => {}
        })
    };

    let startup = build_startup(args, false, sink).await?;
    for warning in &startup.warnings {
        eprintln!("warning: {warning}");
    }

    let prompt = prompt_with_stdin(startup.initial_message.clone())?;
    let prompt_mode = startup.session.prompt_mode_for(&prompt);
    startup
        .session
        .prompt_with_mode(vec![AgentMessage::user(prompt)], prompt_mode)
        .await;

    if let Some(err) = error.lock().unwrap().as_ref() {
        eprintln!("error: {err}");
        return Ok(1);
    }
    let texts = final_text.lock().unwrap();
    if let Some(last) = texts.last() {
        println!("{last}");
    }
    for outcome in workflow_outcomes.lock().unwrap().iter() {
        println!("\n{outcome}");
    }
    if *truncated.lock().unwrap() {
        eprintln!(
            "warning: the response stopped at the model's output token limit, so it is incomplete. Ask for a shorter answer, or choose a model with a larger output limit"
        );
        return Ok(1);
    }
    Ok(0)
}

/// The prompt of a one-shot run: piped stdin, then the message from the
/// command line, separated by a blank line.
pub(super) fn prompt_with_stdin(message: Option<String>) -> anyhow::Result<String> {
    let mut prompt = message.unwrap_or_default();
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
        anyhow::bail!(
            "no prompt provided. Pass a message as an argument, or pipe text to standard input"
        );
    }
    Ok(prompt)
}
