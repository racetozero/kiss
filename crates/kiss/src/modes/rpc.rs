//! RPC mode: expose the agent over newline-delimited JSON so any language can
//! drive it.
//!
//! Two transports are available. Without `--rpc-listen` the process reads
//! commands from its standard input and writes responses and events to its
//! standard output, which is what a parent process spawning `kiss` wants.
//! With `--rpc-listen <ADDR>` it instead accepts WebSocket connections on that
//! address, which is what a browser page wants.
//!
//! The protocol itself is documented in `docs/rpc.md` and defined once in
//! `crates/kiss-sdk/src/protocol.rs`.

use crate::args::Args;
use anyhow::Result;
use kiss_sdk::options::{SessionOptions, SessionSource};
use std::path::PathBuf;

/// Translate command-line arguments into SDK session options.
pub fn options_from_args(args: &Args) -> Result<SessionOptions> {
    let cwd = std::env::current_dir()?;
    let session = if args.no_session {
        SessionSource::InMemory
    } else if let Some(reference) = &args.session {
        SessionSource::Open(PathBuf::from(reference))
    } else if let Some(reference) = &args.fork {
        SessionSource::Fork(PathBuf::from(reference))
    } else if args.continue_recent {
        SessionSource::ContinueRecent
    } else {
        SessionSource::Create
    };

    Ok(SessionOptions {
        cwd,
        model: args.model.clone(),
        provider: args.provider.clone(),
        thinking_level: args
            .thinking
            .as_deref()
            .and_then(kiss_ai::ThinkingLevel::parse),
        api_key: args.api_key.clone(),
        models_file: std::env::var_os("KISS_MODELS_FILE").map(PathBuf::from),
        tools: args.tools.as_ref().map(|_| Args::split_csv(&args.tools)),
        exclude_tools: Args::split_csv(&args.exclude_tools),
        no_tools: args.no_tools,
        system_prompt: args.system_prompt.clone(),
        append_system_prompt: args.append_system_prompt.clone(),
        session,
        session_dir: args.session_dir.clone().map(PathBuf::from),
        session_name: args.name.clone(),
        trust_project_files: args.approve && !args.no_approve,
        no_context_files: args.no_context_files,
        ..Default::default()
    })
}

pub async fn run(args: &Args) -> Result<i32> {
    let session = kiss_sdk::Session::create(options_from_args(args)?).await?;
    match &args.rpc_listen {
        Some(address) => {
            let address: std::net::SocketAddr = address.parse().map_err(|error| {
                anyhow::anyhow!("invalid --rpc-listen address '{address}': {error}")
            })?;
            kiss_sdk::rpc::serve_websocket(session, address).await?;
        }
        None => kiss_sdk::rpc::serve_stdio(session).await?,
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn no_session_means_nothing_is_written_to_disk() {
        let args = Args::parse_from(["kiss", "--mode", "rpc", "--no-session"]);
        let options = options_from_args(&args).expect("options");
        assert_eq!(options.session, SessionSource::InMemory);
    }

    #[test]
    fn a_session_is_persisted_by_default() {
        let args = Args::parse_from(["kiss", "--mode", "rpc"]);
        let options = options_from_args(&args).expect("options");
        assert_eq!(options.session, SessionSource::Create);
    }

    #[test]
    fn tool_flags_are_carried_across() {
        let args = Args::parse_from([
            "kiss",
            "--mode",
            "rpc",
            "--tools",
            "read,bash",
            "--exclude-tools",
            "bash",
        ]);
        let options = options_from_args(&args).expect("options");
        assert_eq!(
            options.tools.as_deref(),
            Some(&["read".to_string(), "bash".to_string()][..])
        );
        assert_eq!(options.exclude_tools, ["bash"]);
    }
}
