use super::*;
use clap::Parser;
use kiss_tui::BlockedKind;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

struct HostLog {
    binary: PathBuf,
    log: PathBuf,
}

impl HostLog {
    fn new(directory: &std::path::Path, name: &str, behavior: &str) -> Self {
        let binary = directory.join(format!("{name}.sh"));
        let log = directory.join(format!("{name}.log"));
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" __END__ >> '{}'\n{behavior}\n",
                log.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { binary, log }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        let bytes = std::fs::read(&self.log).unwrap_or_default();
        let mut calls = Vec::new();
        let mut call = Vec::new();
        for argument in bytes.split(|byte| *byte == 0) {
            if argument == b"__END__" {
                calls.push(std::mem::take(&mut call));
            } else {
                call.push(String::from_utf8_lossy(argument).into_owned());
            }
        }
        calls
    }

    async fn wait(&self, predicate: impl Fn(&[Vec<String>]) -> bool) -> Vec<Vec<String>> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let calls = self.calls();
                if predicate(&calls) {
                    return calls;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Host did not receive the required command: {:?}",
                self.calls()
            )
        })
    }
}

fn session(directory: &std::path::Path) -> Arc<AgentSession> {
    let registry = kiss_ai::Registry::from_builtin();
    let model = registry.all()[0].clone();
    let mut manager =
        kiss_coding::SessionManager::create(directory, Some(directory.join("sessions"))).unwrap();
    manager
        .append_message(kiss_agent::AgentMessage::user("saved conversation"))
        .unwrap();
    AgentSession::new(
        manager,
        Vec::new(),
        registry,
        kiss_coding::Settings::default(),
        "test".into(),
        model,
        kiss_ai::ThinkingLevel::Off,
        None,
        Arc::new(|_| {}),
    )
}

#[tokio::test]
async fn native_hosts_deliver_state_resume_notifications_and_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let herdr = HostLog::new(directory.path(), "herdr", "");
    let cmux = HostLog::new(directory.path(), "cmux", "");
    let hosts = TerminalHosts::start(vec![
        Host::Herdr {
            binary: herdr.binary.clone().into(),
            pane: "pane-17".into(),
        },
        Host::Cmux {
            binary: cmux.binary.clone().into(),
            workspace: "workspace-9".into(),
            surface: "surface-3".into(),
        },
    ]);
    let session = session(directory.path());
    let args = Args::parse_from([
        "kiss",
        "--api-key",
        "private-key",
        "--tools",
        "read,bash",
        "--system-prompt",
        "literal $(echo unsafe) and `echo unsafe`",
        "private-prompt",
    ]);
    hosts.update(State::Idle, &session, &args);
    let first_herdr = herdr
        .wait(|calls| calls.iter().any(|call| call.contains(&"idle".into())))
        .await;
    let first_cmux = cmux
        .wait(|calls| {
            calls
                .iter()
                .any(|call| call.first().is_some_and(|arg| arg == "set-status"))
        })
        .await;
    let file = session
        .manager
        .lock()
        .unwrap()
        .session_file()
        .unwrap()
        .display()
        .to_string();
    let report = &first_herdr[0];
    assert_eq!(&report[..3], ["pane", "report-agent", "pane-17"]);
    assert!(report.windows(2).any(|pair| pair == ["--source", "kiss"]));
    assert!(report.windows(2).any(|pair| pair == ["--session", &file]));
    assert!(report.contains(&"literal $(echo unsafe) and `echo unsafe`".into()));
    let binding = first_cmux
        .iter()
        .find(|call| call.first().is_some_and(|arg| arg == "surface"))
        .unwrap();
    assert_eq!(&binding[..3], ["surface", "resume", "set"]);
    assert!(
        binding
            .windows(2)
            .any(|pair| pair == ["--surface", "surface-3"])
    );
    assert!(binding.windows(2).any(|pair| pair == ["--session", &file]));
    assert!(
        first_cmux
            .iter()
            .any(|call| call.contains(&"kiss-surface-3".into()))
    );
    assert!(
        !first_cmux
            .iter()
            .any(|call| call.first().is_some_and(|arg| arg == "notify"))
    );
    for _ in 0..1000 {
        hosts.update(State::Idle, &session, &args);
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(herdr.calls().len(), first_herdr.len());
    assert_eq!(cmux.calls().len(), first_cmux.len());

    hosts.update(State::Working, &session, &args);
    herdr
        .wait(|calls| calls.iter().any(|call| call.contains(&"working".into())))
        .await;
    cmux.wait(|calls| {
        calls
            .iter()
            .any(|call| call.contains(&"KISS: working".into()))
    })
    .await;
    hosts.update(
        State::Blocked(BlockedKind::Permission, "Workflow approval required"),
        &session,
        &args,
    );
    herdr
        .wait(|calls| calls.iter().any(|call| call.contains(&"blocked".into())))
        .await;
    cmux.wait(|calls| {
        calls
            .iter()
            .any(|call| call.contains(&"Workflow approval required".into()))
    })
    .await;
    hosts.update(State::Working, &session, &args);
    cmux.wait(|calls| {
        calls
            .iter()
            .filter(|call| call.contains(&"KISS: working".into()))
            .count()
            == 2
    })
    .await;
    hosts.update(State::Idle, &session, &args);
    cmux.wait(|calls| {
        calls
            .iter()
            .any(|call| call.contains(&"Task complete".into()))
    })
    .await;

    let mut replacement = session.manager.lock().unwrap().create_sibling().unwrap();
    replacement
        .append_message(kiss_agent::AgentMessage::user("second conversation"))
        .unwrap();
    let second_id = replacement.session_id().to_string();
    session.replace_manager(replacement);
    hosts.update(State::Idle, &session, &args);
    herdr
        .wait(|calls| calls.iter().any(|call| call.contains(&second_id)))
        .await;
    cmux.wait(|calls| calls.iter().any(|call| call.contains(&second_id)))
        .await;
    session.replace_manager(kiss_coding::SessionManager::in_memory(directory.path()));
    hosts.update(State::Idle, &session, &args);
    herdr
        .wait(|calls| {
            calls
                .last()
                .is_some_and(|call| call.contains(&"idle".into()) && !call.contains(&"--".into()))
        })
        .await;
    cmux.wait(|calls| {
        calls
            .iter()
            .any(|call| call.starts_with(&["surface".into(), "resume".into(), "clear".into()]))
    })
    .await;
    hosts.shutdown().await;
    let calls = herdr.calls();
    assert_eq!(
        &calls.last().unwrap()[..3],
        ["pane", "release-agent", "pane-17"]
    );
    let sequences: Vec<u64> = calls
        .iter()
        .map(|call| {
            let index = call.iter().position(|arg| arg == "--seq").unwrap();
            call[index + 1].parse().unwrap()
        })
        .collect();
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    let cmux_calls = cmux.calls();
    assert_eq!(
        cmux_calls.last().unwrap(),
        &[
            "clear-status",
            "kiss-surface-3",
            "--workspace",
            "workspace-9"
        ]
    );
    for call in calls.iter().chain(&cmux_calls) {
        assert!(
            !call
                .iter()
                .any(|arg| matches!(arg.as_str(), "private-key" | "private-prompt" | "--api-key"))
        );
    }
}

#[tokio::test]
async fn empty_session_registers_resume_only_after_its_first_saved_entry() {
    let directory = tempfile::tempdir().unwrap();
    let log = HostLog::new(directory.path(), "herdr", "");
    let hosts = TerminalHosts::start(vec![Host::Herdr {
        binary: log.binary.clone().into(),
        pane: "pane".into(),
    }]);
    let session = session(directory.path());
    session.replace_manager(
        kiss_coding::SessionManager::create(
            directory.path(),
            Some(directory.path().join("sessions")),
        )
        .unwrap(),
    );
    let args = Args::parse_from(["kiss"]);
    hosts.update(State::Idle, &session, &args);
    let calls = log.wait(|calls| !calls.is_empty()).await;
    assert!(!calls[0].contains(&"--agent-session-id".into()));
    session
        .manager
        .lock()
        .unwrap()
        .append_message(kiss_agent::AgentMessage::user("first saved entry"))
        .unwrap();
    hosts.update(State::Idle, &session, &args);
    log.wait(|calls| {
        calls
            .iter()
            .any(|call| call.contains(&"--agent-session-id".into()))
    })
    .await;
    hosts.shutdown().await;
}

#[tokio::test]
async fn herdr_keeps_state_when_resume_arguments_are_not_accepted() {
    let directory = tempfile::tempdir().unwrap();
    let log = HostLog::new(directory.path(), "herdr", "");
    let hosts = TerminalHosts::start(vec![Host::Herdr {
        binary: log.binary.clone().into(),
        pane: "pane".into(),
    }]);
    let session = session(directory.path());
    hosts.update(State::Idle, &session, &Args::parse_from(["kiss"]));
    log.wait(|calls| {
        calls
            .iter()
            .any(|call| call.contains(&"--agent-session-id".into()))
    })
    .await;
    for input in ["can't resume", "line\nbreak", &"x".repeat(8192)] {
        let args = Args::parse_from(["kiss", "--system-prompt", input]);
        let count = log.calls().len();
        hosts.update(State::Working, &session, &args);
        let calls = log
            .wait(|calls| {
                calls.len() > count
                    && calls
                        .last()
                        .is_some_and(|call| call.contains(&"working".into()))
            })
            .await;
        assert!(!calls.last().unwrap().contains(&"--agent-session-id".into()));
    }
    hosts.shutdown().await;
    assert!(
        log.calls()
            .iter()
            .any(|call| call.get(1).is_some_and(|arg| arg == "release-agent"))
    );
}

#[tokio::test]
async fn slow_host_drops_old_states_and_does_not_delay_other_hosts() {
    let directory = tempfile::tempdir().unwrap();
    // exec makes the timeout kill the actual sleeping process, not an orphan.
    let slow = HostLog::new(
        directory.path(),
        "slow",
        "if [ \"$2\" = report-agent ]; then exec sleep 30; fi",
    );
    let fast = HostLog::new(directory.path(), "fast", "");
    let hosts = TerminalHosts::start(vec![
        Host::Herdr {
            binary: slow.binary.clone().into(),
            pane: "pane".into(),
        },
        Host::Cmux {
            binary: fast.binary.clone().into(),
            workspace: "workspace".into(),
            surface: "surface".into(),
        },
    ]);
    let session = session(directory.path());
    let args = Args::parse_from(["kiss"]);
    hosts.update(State::Working, &session, &args);
    slow.wait(|calls| !calls.is_empty()).await;
    let started = std::time::Instant::now();
    for _ in 0..1000 {
        hosts.update(
            State::Blocked(BlockedKind::Permission, "Old decision"),
            &session,
            &args,
        );
    }
    hosts.update(State::Idle, &session, &args);
    fast.wait(|calls| calls.iter().any(|call| call.contains(&"KISS: idle".into())))
        .await;
    assert!(
        started.elapsed() < COMMAND_TIMEOUT,
        "Fast host waited for the slow host"
    );
    let calls = slow.wait(|calls| calls.len() == 2).await;
    assert!(calls[1].contains(&"idle".into()));
    assert!(
        !calls
            .iter()
            .any(|call| call.contains(&"Old decision".into()))
    );
    let started = std::time::Instant::now();
    hosts.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(slow.calls().last().unwrap()[1], "release-agent");
}

#[tokio::test]
async fn failed_status_still_cleans_up_an_accepted_cmux_resume_binding() {
    let directory = tempfile::tempdir().unwrap();
    let log = HostLog::new(
        directory.path(),
        "cmux",
        "if [ \"$1\" = set-status ]; then exit 1; fi",
    );
    let hosts = TerminalHosts::start(vec![Host::Cmux {
        binary: log.binary.clone().into(),
        workspace: "workspace".into(),
        surface: "surface".into(),
    }]);
    let session = session(directory.path());
    hosts.update(State::Idle, &session, &Args::parse_from(["kiss"]));
    log.wait(|calls| {
        calls
            .iter()
            .any(|call| call.first().is_some_and(|arg| arg == "set-status"))
    })
    .await;
    hosts.shutdown().await;
    assert_eq!(
        &log.calls().last().unwrap()[..3],
        ["surface", "resume", "clear"]
    );
}

#[tokio::test]
async fn cmux_delivers_status_when_resume_is_unavailable_and_retries_resume() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("resume-ready");
    let log = HostLog::new(
        directory.path(),
        "cmux",
        &format!(
            "if [ \"$1\" = surface ] && [ \"$3\" = set ] && [ ! -f '{}' ]; then exit 1; fi",
            ready.display(),
        ),
    );
    let hosts = TerminalHosts::start(vec![Host::Cmux {
        binary: log.binary.clone().into(),
        workspace: "workspace".into(),
        surface: "surface".into(),
    }]);
    let session = session(directory.path());
    hosts.update(State::Idle, &session, &Args::parse_from(["kiss"]));
    log.wait(|calls| calls.iter().any(|call| call.contains(&"KISS: idle".into())))
        .await;
    std::fs::write(ready, "ready").unwrap();
    log.wait(|calls| {
        calls
            .iter()
            .filter(|call| call.starts_with(&["surface".into(), "resume".into(), "set".into()]))
            .count()
            == 2
    })
    .await;
    hosts.shutdown().await;
    let calls = log.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.first().is_some_and(|arg| arg == "set-status"))
            .count(),
        1
    );
    assert_eq!(&calls.last().unwrap()[..3], ["surface", "resume", "clear"]);
}

#[tokio::test]
async fn host_detection_requires_complete_inherited_context() {
    const NAME: &str = "terminal_hosts::tests::host_detection_requires_complete_inherited_context";
    if let Ok(expected) = std::env::var("KISS_TEST_HOST_DETECTION") {
        let hosts = TerminalHosts::from_env();
        assert_eq!(hosts.enabled(), expected == "true");
        hosts.shutdown().await;
        return;
    }
    for (values, enabled) in [
        (vec![], false),
        (vec![("HERDR_ENV", "1"), ("HERDR_PANE_ID", "pane")], false),
        (
            vec![("CMUX_WORKSPACE_ID", "workspace"), ("CMUX_SURFACE_ID", "")],
            false,
        ),
        (
            vec![
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "pane"),
                ("HERDR_BIN_PATH", "/missing/kiss-host"),
                ("HERDR_SOCKET_PATH", "/missing/socket"),
            ],
            true,
        ),
        (
            vec![
                ("CMUX_WORKSPACE_ID", "workspace"),
                ("CMUX_SURFACE_ID", "surface"),
            ],
            true,
        ),
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", NAME, "--nocapture"]);
        for variable in [
            "HERDR_ENV",
            "HERDR_PANE_ID",
            "HERDR_BIN_PATH",
            "HERDR_SOCKET_PATH",
            "CMUX_WORKSPACE_ID",
            "CMUX_SURFACE_ID",
        ] {
            child.env_remove(variable);
        }
        child
            .env("KISS_TEST_HOST_DETECTION", enabled.to_string())
            .envs(values);
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
