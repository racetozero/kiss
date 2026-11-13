//! Native terminal-host reports. One latest-value queue per host keeps IPC out
//! of the input loop and prevents a slow host from accumulating old states.

use crate::args::Args;
use kiss_coding::AgentSession;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
const RETRY_DELAY: Duration = Duration::from_secs(2);

pub(crate) use kiss_tui::ProgramStatus as State;

fn label(state: State) -> &'static str {
    match state {
        State::Working => "working",
        State::Blocked(..) => "blocked",
        State::Idle | State::Done | State::Error => "idle",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Resume {
    id: String,
    file: PathBuf,
    cwd: PathBuf,
    argv: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Report {
    state: State,
    resume: Option<Resume>,
}

enum Host {
    Herdr {
        binary: OsString,
        pane: OsString,
        socket: PathBuf,
    },
    Cmux {
        binary: OsString,
        workspace: OsString,
        surface: OsString,
    },
}

pub(crate) struct TerminalHosts {
    sender: Option<watch::Sender<Option<Report>>>,
    workers: Vec<JoinHandle<()>>,
}

impl TerminalHosts {
    pub(crate) fn from_env() -> Self {
        let mut hosts = Vec::new();
        if std::env::var_os("HERDR_ENV").is_some_and(|value| value == "1")
            && let Some(pane) = std::env::var_os("HERDR_PANE_ID").filter(|v| !v.is_empty())
            && let Some(socket) = std::env::var_os("HERDR_SOCKET_PATH").filter(|v| !v.is_empty())
        {
            hosts.push(Host::Herdr {
                binary: std::env::var_os("HERDR_BIN_PATH")
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "herdr".into()),
                pane,
                socket: socket.into(),
            });
        }
        if let Some(workspace) = std::env::var_os("CMUX_WORKSPACE_ID").filter(|v| !v.is_empty())
            && let Some(surface) = std::env::var_os("CMUX_SURFACE_ID").filter(|v| !v.is_empty())
        {
            hosts.push(Host::Cmux {
                binary: "cmux".into(),
                workspace,
                surface,
            });
        }
        Self::start(hosts)
    }

    fn start(hosts: Vec<Host>) -> Self {
        if hosts.is_empty() {
            return Self {
                sender: None,
                workers: Vec::new(),
            };
        }
        let (sender, receiver) = watch::channel(None);
        let workers = hosts
            .into_iter()
            .map(|host| {
                let receiver = receiver.clone();
                tokio::spawn(host.run(receiver))
            })
            .collect();
        Self {
            sender: Some(sender),
            workers,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.sender.is_some()
    }

    pub(crate) fn update(&self, state: State, session: &AgentSession, args: &Args) {
        let Some(sender) = &self.sender else { return };
        let model = session.model();
        let thinking = session.thinking_level();
        let manager = session.manager.lock().unwrap();
        let resume = manager
            .session_file()
            .filter(|_| !manager.entries().is_empty())
            .and_then(|file| {
                let file = std::path::absolute(file)
                    .ok()?
                    .into_os_string()
                    .into_string()
                    .ok()?;
                let session_file = PathBuf::from(&file);
                let mut argv = vec![
                    "kiss".into(),
                    "--session".into(),
                    file,
                    "--model".into(),
                    format!("{}/{}", model.provider, model.id),
                    "--thinking".into(),
                    thinking.as_str().into(),
                ];
                // Preserve explicit resource choices, but never store a prompt or
                // an API-key override in another application's session file.
                for (flag, value) in [
                    ("--session-dir", &args.session_dir),
                    ("--models", &args.models),
                    ("--tools", &args.tools),
                    ("--exclude-tools", &args.exclude_tools),
                    ("--system-prompt", &args.system_prompt),
                    ("--append-system-prompt", &args.append_system_prompt),
                ] {
                    if let Some(value) = value {
                        argv.extend([flag.into(), value.clone()]);
                    }
                }
                for (flag, values) in [
                    ("--skill", &args.skills),
                    ("--prompt-template", &args.prompt_templates),
                    ("--theme", &args.themes),
                ] {
                    for value in values {
                        argv.extend([flag.into(), value.clone()]);
                    }
                }
                for (flag, enabled) in [
                    ("--no-tools", args.no_tools),
                    ("--no-mcp", args.no_mcp),
                    ("--no-skills", args.no_skills),
                    ("--no-prompt-templates", args.no_prompt_templates),
                    ("--no-themes", args.no_themes),
                    ("--no-context-files", args.no_context_files),
                    ("--approve", args.approve),
                    ("--no-approve", args.no_approve),
                    (
                        "--experimental-context-file",
                        args.experimental_context_file,
                    ),
                ] {
                    if enabled {
                        argv.push(flag.into());
                    }
                }
                Some(Resume {
                    id: manager.session_id().into(),
                    file: session_file,
                    cwd: manager.cwd().into(),
                    argv,
                })
            });
        let report = Report { state, resume };
        sender.send_if_modified(|current| {
            if current.as_ref() == Some(&report) {
                return false;
            }
            *current = Some(report);
            true
        });
    }

    pub(crate) async fn shutdown(mut self) {
        if let Some(sender) = self.sender.take() {
            sender.send_replace(None);
        }
        for worker in self.workers {
            let _ = worker.await;
        }
    }
}

impl Host {
    fn command(&self, args: &[&str]) -> Command {
        let binary = match self {
            Self::Herdr { binary, .. } | Self::Cmux { binary, .. } => binary,
        };
        let mut command = Command::new(binary);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn run(self, mut receiver: watch::Receiver<Option<Report>>) {
        let mut previous: Option<State> = None;
        let mut registered: Option<Resume> = None;
        let mut sequence = 0;
        if receiver.changed().await.is_err() {
            return;
        }
        loop {
            let current = receiver.borrow_and_update().clone();
            let Some(report) = current else { break };
            let file_pending = if let Some(resume) = &report.resume {
                !tokio::fs::metadata(&resume.file)
                    .await
                    .is_ok_and(|metadata| metadata.is_file())
            } else {
                false
            };
            let resume = report.resume.as_ref().filter(|resume| {
                !file_pending
                    && match self {
                        Self::Herdr { .. } => {
                            resume.argv.len() <= 64
                                && resume.argv.iter().map(|arg| arg.len() + 1).sum::<usize>()
                                    <= 8192
                                && resume.argv.iter().all(|arg| {
                                    !arg.contains('\'') && !arg.chars().any(char::is_control)
                                })
                        }
                        Self::Cmux { .. } => true,
                    }
            });
            let state_sent = self
                .report(
                    report.state,
                    resume,
                    previous,
                    &mut registered,
                    &mut sequence,
                )
                .await;
            if state_sent {
                previous = Some(report.state);
            }
            if state_sent && registered.as_ref() == resume && !file_pending {
                if receiver.changed().await.is_err() {
                    break;
                }
            } else {
                tokio::select! {
                    changed = receiver.changed() => if changed.is_err() { break; },
                    _ = tokio::time::sleep(RETRY_DELAY) => {},
                }
            }
        }
        self.release(registered.as_ref(), &mut sequence).await;
    }

    async fn report(
        &self,
        state: State,
        resume: Option<&Resume>,
        previous: Option<State>,
        registered: &mut Option<Resume>,
        sequence: &mut u64,
    ) -> bool {
        match self {
            Self::Herdr { pane, socket, .. } => {
                if registered.as_ref() != resume && registered.is_some() {
                    if !self.release(registered.as_ref(), sequence).await {
                        return false;
                    }
                    *registered = None;
                }
                let seq = next_sequence(sequence);
                let mut params = serde_json::json!({
                    "pane_id": pane.to_string_lossy(), "source": "kiss", "agent": "kiss",
                    "state": label(state), "seq": *sequence,
                });
                if let State::Blocked(_, message) = state {
                    params["message"] = message.into();
                }
                if let Some(resume) = resume {
                    params["agent_session_id"] = resume.id.clone().into();
                    params["resume_argv"] = serde_json::json!(resume.argv);
                }
                let mut command = self.command(&["pane", "report-agent"]);
                command
                    .arg(pane)
                    .args([
                        "--source",
                        "kiss",
                        "--agent",
                        "kiss",
                        "--state",
                        label(state),
                        "--seq",
                    ])
                    .arg(seq);
                if let State::Blocked(_, message) = state {
                    command.args(["--message", message]);
                }
                if let Some(resume) = resume {
                    command
                        .arg("--agent-session-id")
                        .arg(&resume.id)
                        .arg("--")
                        .args(&resume.argv);
                }
                let sent = match herdr_request(socket, "pane.report_agent", params).await {
                    Some(sent) => sent,
                    None => execute(&mut command).await,
                };
                if sent {
                    *registered = resume.cloned();
                }
                sent
            }
            Self::Cmux {
                workspace, surface, ..
            } => {
                if registered.as_ref() != resume
                    && let Some(binding) = resume.or(registered.as_ref())
                {
                    let mut command = self.command(&["surface", "resume"]);
                    command
                        .arg(if resume.is_some() { "set" } else { "clear" })
                        .arg("--workspace")
                        .arg(workspace)
                        .arg("--surface")
                        .arg(surface)
                        .args(["--source", "kiss", "--checkpoint"])
                        .arg(&binding.id);
                    if let Some(resume) = resume {
                        command
                            .args(["--kind", "kiss", "--cwd"])
                            .arg(&resume.cwd)
                            .arg("--")
                            .args(&resume.argv);
                    }
                    if execute(&mut command).await {
                        *registered = resume.cloned();
                    }
                }
                // Status remains available when the CLI cannot register resume commands.
                previous == Some(state)
                    || execute(
                        self.command(&["set-status"])
                            .arg(format!("kiss-{}", surface.to_string_lossy()))
                            .arg(format!("KISS: {}", label(state)))
                            .arg("--workspace")
                            .arg(workspace),
                    )
                    .await
            }
        }
    }

    async fn release(&self, registered: Option<&Resume>, sequence: &mut u64) -> bool {
        match self {
            Self::Herdr { pane, socket, .. } => {
                let seq = next_sequence(sequence);
                if let Some(sent) = herdr_request(socket, "pane.release_agent", serde_json::json!({
                    "pane_id": pane.to_string_lossy(), "source": "kiss", "agent": "kiss", "seq": *sequence,
                })).await {
                    return sent;
                }
                execute(
                    self.command(&["pane", "release-agent"])
                        .arg(pane)
                        .args(["--source", "kiss", "--agent", "kiss", "--seq"])
                        .arg(seq),
                )
                .await
            }
            Self::Cmux {
                workspace, surface, ..
            } => {
                let success = execute(
                    self.command(&["clear-status"])
                        .arg(format!("kiss-{}", surface.to_string_lossy()))
                        .arg("--workspace")
                        .arg(workspace),
                )
                .await;
                if let Some(resume) = registered {
                    execute(
                        self.command(&["surface", "resume", "clear", "--workspace"])
                            .arg(workspace)
                            .arg("--surface")
                            .arg(surface)
                            .args(["--source", "kiss", "--checkpoint"])
                            .arg(&resume.id),
                    )
                    .await;
                }
                success
            }
        }
    }
}

// Prefer the same socket API used by Herdr's pi integration. This avoids
// short-lived reporter processes in the pane's foreground job (notably on WSL).
// Only an unavailable Unix socket falls back to the CLI; rejection or timeout
// remains a failed report, retried by the latest-value worker.
#[cfg(unix)]
async fn herdr_request(
    socket: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> Option<bool> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    tokio::time::timeout(COMMAND_TIMEOUT, async {
        let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
        let request = serde_json::json!({"id": "kiss", "method": method, "params": params});
        if stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .is_err()
        {
            return Some(false);
        }
        let mut response = Vec::new();
        if BufReader::new(stream.take(64 * 1024))
            .read_until(b'\n', &mut response)
            .await
            .is_err()
            || response.last() != Some(&b'\n')
        {
            return Some(false);
        }
        Some(
            serde_json::from_slice::<serde_json::Value>(&response).is_ok_and(|value| {
                value["id"] == "kiss"
                    && value.get("result").is_some()
                    && value.get("error").is_none()
            }),
        )
    })
    .await
    .unwrap_or(Some(false))
}

#[cfg(not(unix))]
async fn herdr_request(_: &std::path::Path, _: &str, _: serde_json::Value) -> Option<bool> {
    None
}

async fn execute(command: &mut Command) -> bool {
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    match tokio::time::timeout(COMMAND_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        _ => {
            let _ = child.kill().await;
            false
        }
    }
}

fn next_sequence(sequence: &mut u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    *sequence = sequence
        .saturating_add(1)
        .max(u64::try_from(now).unwrap_or(u64::MAX));
    sequence.to_string()
}

#[cfg(all(test, unix))]
#[path = "terminal_hosts_tests.rs"]
mod tests;
