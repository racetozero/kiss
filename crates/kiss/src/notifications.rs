//! One best-effort notification route per interactive session. OSC delivery is
//! owned by the UI writer; only bounded external commands run in the background.

use anyhow::{Context, Result};
use kiss_tui::{InputEvent, ProgramStatus};
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

const GRACE: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Notice {
    Complete,
    Attention,
}

impl Notice {
    fn body(self) -> &'static str {
        match self {
            Self::Complete => "Your prompt has finished.",
            Self::Attention => "Your input is required.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Osc9,
    Osc99,
    Osc777,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Route {
    Terminal {
        protocol: Protocol,
        tmux: bool,
        screen: bool,
    },
    Cmux {
        workspace: String,
        surface: String,
    },
    Native,
    WindowsToast,
    #[default]
    Unavailable,
}

impl Route {
    fn detect(env: impl Fn(&str) -> Option<String>) -> Self {
        let present = |key| env(key).is_some_and(|value| !value.is_empty());
        let term = env("TERM").unwrap_or_default();
        if term == "dumb" {
            return Self::Unavailable;
        }
        if let (Some(workspace), Some(surface)) = (env("CMUX_WORKSPACE_ID"), env("CMUX_SURFACE_ID"))
            && !workspace.is_empty()
            && !surface.is_empty()
        {
            return Self::Cmux { workspace, surface };
        }
        let program = env("TERM_PROGRAM").unwrap_or_default();
        // Rex has no known notification sequence. Check it first so outer
        // terminal variables such as KITTY_WINDOW_ID, which leak into Rex
        // when it starts from another terminal, do not choose one.
        let protocol = if program == "rex" {
            None
        } else if present("KITTY_WINDOW_ID") || term == "xterm-kitty" || program == "kitty" {
            Some(Protocol::Osc99)
        } else if program == "iTerm.app" || present("ITERM_SESSION_ID") || program == "WarpTerminal"
        {
            Some(Protocol::Osc9)
        } else if matches!(program.as_str(), "ghostty" | "WezTerm") || term.starts_with("rxvt") {
            Some(Protocol::Osc777)
        } else {
            None
        };
        if let Some(protocol) = protocol {
            return Self::Terminal {
                protocol,
                tmux: present("TMUX"),
                screen: present("STY"),
            };
        }
        // Never launch a desktop utility on an SSH server. Unsupported remote
        // terminals stay silent rather than targeting somebody else's desktop.
        if present("SSH_CONNECTION") || present("SSH_CLIENT") || present("SSH_TTY") {
            Self::Unavailable
        } else if present("WT_SESSION") {
            // Windows Terminal can host WSL; notify-send would target Linux,
            // not the Windows desktop owning the terminal.
            Self::WindowsToast
        } else {
            Self::Native
        }
    }

    fn deliver(&self, notice: Notice, out: &mut impl Write) -> Result<Option<Command>> {
        match self {
            Self::Terminal {
                protocol,
                tmux,
                screen,
            } => {
                let body = notice.body();
                let sequences = match protocol {
                    Protocol::Osc9 => vec![format!("\x1b]9;KISS: {body}\x07")],
                    // Separate packets keep title/body metadata unambiguous.
                    // Fixed ID replaces the previous notice for this terminal.
                    Protocol::Osc99 => vec![
                        "\x1b]99;i=kiss:d=0:p=title;KISS\x1b\\".into(),
                        format!("\x1b]99;i=kiss:d=0:p=body;{body}\x1b\\"),
                        "\x1b]99;i=kiss:d=1:a=focus;\x1b\\".into(),
                    ],
                    Protocol::Osc777 => vec![format!("\x1b]777;notify;KISS;{body}\x07")],
                };
                for sequence in sequences {
                    if *tmux {
                        write!(
                            out,
                            "\x1bPtmux;{}\x1b\\",
                            sequence.replace('\x1b', "\x1b\x1b")
                        )?;
                    } else if *screen {
                        // An inner ST would end screen's DCS wrapper early.
                        // OSC also permits BEL termination, including Kitty.
                        let sequence = sequence.replace("\x1b\\", "\x07");
                        write!(out, "\x1bP{sequence}\x1b\\")?;
                    } else {
                        out.write_all(sequence.as_bytes())?;
                    }
                }
                out.flush()?;
                Ok(None)
            }
            Self::Cmux { workspace, surface } => {
                let mut command = Command::new("cmux");
                command.args([
                    "notify",
                    "--workspace",
                    workspace,
                    "--surface",
                    surface,
                    "--title",
                    "KISS",
                    "--body",
                    notice.body(),
                ]);
                Ok(Some(command))
            }
            Self::Native => notification_command(notice).map(Some),
            Self::WindowsToast => Ok(Some(windows_toast_command(notice))),
            Self::Unavailable => Ok(None),
        }
    }
}

#[derive(Default)]
pub(crate) struct Notifications {
    route: Route,
    focused: Option<bool>,
    pending: Option<(Notice, Instant)>,
    blocked: Option<ProgramStatus>,
    in_flight: bool,
}

impl Notifications {
    pub(crate) fn from_env() -> Self {
        Self {
            route: Route::detect(|key| std::env::var(key).ok()),
            ..Self::default()
        }
    }

    pub(crate) fn input(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Focus(focused) => {
                self.focused = Some(*focused);
                if *focused {
                    self.pending = None;
                }
            }
            InputEvent::Key(_) | InputEvent::Paste(_) => self.pending = None,
            InputEvent::KeyRelease(_) => {}
        }
    }

    fn queue(&mut self, notice: Notice, enabled: bool, now: Instant) {
        if !enabled || self.focused == Some(true) || self.route == Route::Unavailable {
            return;
        }
        if matches!(self.pending, Some((Notice::Attention, _))) && notice == Notice::Complete {
            return;
        }
        let deadline = if self.focused == Some(false) {
            now
        } else {
            now + GRACE
        };
        self.pending = Some((notice, deadline));
    }

    pub(crate) fn prompt_finished(&mut self, cancelled: bool, enabled: bool, now: Instant) {
        if cancelled {
            self.pending = None;
        } else {
            self.queue(Notice::Complete, enabled, now);
        }
    }

    pub(crate) fn status(&mut self, status: ProgramStatus, enabled: bool, now: Instant) {
        if !enabled {
            self.pending = None;
        }
        let blocked = matches!(status, ProgramStatus::Blocked(..)).then_some(status);
        if blocked != self.blocked {
            if matches!(self.pending, Some((Notice::Attention, _))) {
                self.pending = None;
            }
            if blocked.is_some() {
                self.queue(Notice::Attention, enabled, now);
            }
            self.blocked = blocked;
        }
    }

    pub(crate) fn started(&mut self) {
        if matches!(self.pending, Some((Notice::Complete, _))) {
            self.pending = None;
        }
    }

    pub(crate) fn description(&self) -> &'static str {
        match self.route {
            Route::Terminal { .. } => "terminal · when away",
            Route::Cmux { .. } => "cmux pane · when away",
            Route::Native => "local desktop · when away",
            Route::WindowsToast => "Windows desktop · when away",
            Route::Unavailable => "unavailable in this session",
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        if self.in_flight {
            None
        } else {
            self.pending.map(|(_, deadline)| deadline)
        }
    }

    pub(crate) fn poll(&mut self, now: Instant, out: &mut impl Write) -> Result<Option<Command>> {
        if self.deadline().is_none_or(|deadline| deadline > now) {
            return Ok(None);
        }
        let (notice, _) = self.pending.take().unwrap();
        let command = self.route.deliver(notice, out)?;
        self.in_flight = command.is_some();
        Ok(command)
    }

    pub(crate) fn delivery_finished(&mut self, result: &Result<(), String>) {
        self.in_flight = false;
        if result.is_err() {
            // A successful write/process does not acknowledge an OS banner.
            // Retrying via another route can duplicate an already delivered notice.
            self.route = Route::Unavailable;
            self.pending = None;
        }
    }
}

pub(crate) async fn send(mut command: Command) -> Result<()> {
    run_command(&mut command, Duration::from_secs(5)).await
}

fn notification_command(notice: Notice) -> Result<Command> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("/usr/bin/osascript");
        // Only fixed application strings are interpolated, never model/user text.
        command.args([
            "-e",
            &format!(
                "display notification \"{}\" with title \"KISS\"",
                notice.body()
            ),
        ]);
        Ok(command)
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("notify-send");
        command.args(["--app-name=kiss", "KISS", notice.body()]);
        Ok(command)
    }
    #[cfg(target_os = "windows")]
    {
        Ok(windows_toast_command(notice))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("desktop notifications are unsupported on this platform")
}

fn windows_toast_command(notice: Notice) -> Command {
    let mut command = Command::new("powershell.exe");
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    command.args([
        "-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
        &format!(r#"$ErrorActionPreference = 'Stop';
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null;
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null;
$xml = New-Object Windows.Data.Xml.Dom.XmlDocument;
$xml.LoadXml('<toast><visual><binding template="ToastGeneric"><text>KISS</text><text>{}</text></binding></visual></toast>');
$toast = [Windows.UI.Notifications.ToastNotification]::new($xml);
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Microsoft.PowerShell').Show($toast);"#, notice.body()),
    ]);
    command
}

async fn run_command(command: &mut Command, timeout: Duration) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let status = tokio::time::timeout(timeout, command.status())
        .await
        .with_context(|| format!("{program} timed out"))?
        .with_context(|| {
            format!("could not run {program}; check desktop notification tools and permissions")
        })?;
    anyhow::ensure!(
        status.success(),
        "{program} exited with {status}; check your desktop notification service and permissions"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_target_the_terminal_or_local_desktop_never_an_ssh_server() {
        for (env, expected) in [
            (vec![], Route::Native),
            (vec![("TERM_PROGRAM", "Apple_Terminal")], Route::Native),
            (
                vec![
                    ("TERM_PROGRAM", "rex"),
                    ("KITTY_WINDOW_ID", "1"),
                    ("ITERM_SESSION_ID", "session"),
                ],
                Route::Native,
            ),
            (vec![("TERM", "dumb")], Route::Unavailable),
            (vec![("WT_SESSION", "session")], Route::WindowsToast),
            (
                vec![("WT_SESSION", "session"), ("SSH_CONNECTION", "server")],
                Route::Unavailable,
            ),
            (
                vec![
                    ("TERM", "dumb"),
                    ("CMUX_WORKSPACE_ID", "w"),
                    ("CMUX_SURFACE_ID", "s"),
                ],
                Route::Unavailable,
            ),
            (vec![("SSH_CONNECTION", "server")], Route::Unavailable),
            (vec![("SSH_CLIENT", "client")], Route::Unavailable),
            (vec![("SSH_TTY", "/dev/pts/1")], Route::Unavailable),
            (
                vec![("TERM_PROGRAM", "iTerm.app"), ("SSH_CONNECTION", "server")],
                Route::Terminal {
                    protocol: Protocol::Osc9,
                    tmux: false,
                    screen: false,
                },
            ),
            (
                vec![("ITERM_SESSION_ID", "session"), ("TMUX", "mux")],
                Route::Terminal {
                    protocol: Protocol::Osc9,
                    tmux: true,
                    screen: false,
                },
            ),
            (
                vec![("TERM_PROGRAM", "WarpTerminal")],
                Route::Terminal {
                    protocol: Protocol::Osc9,
                    tmux: false,
                    screen: false,
                },
            ),
            (
                vec![("KITTY_WINDOW_ID", "1")],
                Route::Terminal {
                    protocol: Protocol::Osc99,
                    tmux: false,
                    screen: false,
                },
            ),
            (
                vec![("TERM", "xterm-kitty")],
                Route::Terminal {
                    protocol: Protocol::Osc99,
                    tmux: false,
                    screen: false,
                },
            ),
            (
                vec![("TERM_PROGRAM", "ghostty"), ("TMUX", "mux")],
                Route::Terminal {
                    protocol: Protocol::Osc777,
                    tmux: true,
                    screen: false,
                },
            ),
            (
                vec![("TERM_PROGRAM", "WezTerm"), ("STY", "screen")],
                Route::Terminal {
                    protocol: Protocol::Osc777,
                    tmux: false,
                    screen: true,
                },
            ),
            (
                vec![("TERM", "rxvt-unicode-256color")],
                Route::Terminal {
                    protocol: Protocol::Osc777,
                    tmux: false,
                    screen: false,
                },
            ),
            (
                vec![("CMUX_WORKSPACE_ID", "workspace"), ("CMUX_SURFACE_ID", "")],
                Route::Native,
            ),
            (
                vec![
                    ("CMUX_WORKSPACE_ID", "workspace"),
                    ("CMUX_SURFACE_ID", "surface"),
                    ("TERM_PROGRAM", "ghostty"),
                ],
                Route::Cmux {
                    workspace: "workspace".into(),
                    surface: "surface".into(),
                },
            ),
        ] {
            assert_eq!(
                Route::detect(|key| env
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.to_string())),
                expected,
                "{env:?}"
            );
        }
    }

    #[test]
    fn terminal_delivery_emits_protocol_and_multiplexer_contract_bytes() {
        for (protocol, tmux, screen, expected) in [
            (
                Protocol::Osc9,
                false,
                false,
                "\x1b]9;KISS: Your prompt has finished.\x07",
            ),
            (
                Protocol::Osc777,
                false,
                false,
                "\x1b]777;notify;KISS;Your prompt has finished.\x07",
            ),
            (
                Protocol::Osc99,
                false,
                false,
                "\x1b]99;i=kiss:d=0:p=title;KISS\x1b\\\x1b]99;i=kiss:d=0:p=body;Your prompt has finished.\x1b\\\x1b]99;i=kiss:d=1:a=focus;\x1b\\",
            ),
            (
                Protocol::Osc9,
                true,
                false,
                "\x1bPtmux;\x1b\x1b]9;KISS: Your prompt has finished.\x07\x1b\\",
            ),
            (
                Protocol::Osc777,
                false,
                true,
                "\x1bP\x1b]777;notify;KISS;Your prompt has finished.\x07\x1b\\",
            ),
            (
                Protocol::Osc99,
                true,
                false,
                "\x1bPtmux;\x1b\x1b]99;i=kiss:d=0:p=title;KISS\x1b\x1b\\\x1b\\\x1bPtmux;\x1b\x1b]99;i=kiss:d=0:p=body;Your prompt has finished.\x1b\x1b\\\x1b\\\x1bPtmux;\x1b\x1b]99;i=kiss:d=1:a=focus;\x1b\x1b\\\x1b\\",
            ),
            (
                Protocol::Osc99,
                false,
                true,
                "\x1bP\x1b]99;i=kiss:d=0:p=title;KISS\x07\x1b\\\x1bP\x1b]99;i=kiss:d=0:p=body;Your prompt has finished.\x07\x1b\\\x1bP\x1b]99;i=kiss:d=1:a=focus;\x07\x1b\\",
            ),
        ] {
            let mut out = Vec::new();
            let route = Route::Terminal {
                protocol,
                tmux,
                screen,
            };
            assert!(route.deliver(Notice::Complete, &mut out).unwrap().is_none());
            assert_eq!(out, expected.as_bytes(), "{route:?}");
        }
    }

    fn notifier() -> Notifications {
        Notifications {
            route: Route::Terminal {
                protocol: Protocol::Osc777,
                tmux: false,
                screen: false,
            },
            ..Default::default()
        }
    }

    #[test]
    fn notifications_are_once_only_private_and_attention_aware() {
        let now = Instant::now();
        let later = now + GRACE;
        let blocked = ProgramStatus::Blocked(
            kiss_tui::BlockedKind::Permission,
            "private decision details",
        );
        let mut out = Vec::new();
        // Disabled, cancelled and known-focused completions never emit bytes.
        for (enabled, cancelled, focus) in [
            (false, false, None),
            (true, true, None),
            (true, false, Some(true)),
        ] {
            let mut n = notifier();
            if let Some(focus) = focus {
                n.input(&InputEvent::Focus(focus));
            }
            n.prompt_finished(cancelled, enabled, now);
            n.poll(later, &mut out).unwrap();
            assert!(out.is_empty());
        }
        let mut n = notifier();
        n.prompt_finished(false, true, now);
        n.poll(now, &mut out).unwrap();
        assert!(out.is_empty(), "unknown focus needs a grace period");
        n.poll(later, &mut out).unwrap();
        assert_eq!(out, b"\x1b]777;notify;KISS;Your prompt has finished.\x07");
        out.clear();
        n.poll(later, &mut out).unwrap();
        assert!(out.is_empty(), "a completed delivery cannot repeat");

        // Input, returning focus, cancellation, disabling, or a new prompt
        // makes an undelivered completion obsolete.
        for action in 0..6 {
            let mut n = notifier();
            n.prompt_finished(false, true, now);
            match action {
                0 => n.input(&InputEvent::Key(kiss_tui::KeyEvent::char('x'))),
                1 => n.input(&InputEvent::Paste("private pasted input".into())),
                2 => n.input(&InputEvent::Focus(true)),
                3 => n.prompt_finished(true, true, now),
                4 => n.status(ProgramStatus::Done, false, now),
                _ => n.started(),
            }
            n.poll(later, &mut out).unwrap();
            assert!(out.is_empty(), "action {action}");
        }
        // Blocked state supersedes completion, ignores private details, and
        // does not repeat on every UI tick. Resolving a decision cancels it.
        let mut n = notifier();
        n.prompt_finished(false, true, now);
        n.status(blocked, true, now);
        n.prompt_finished(false, true, now);
        n.status(blocked, true, later);
        n.poll(later, &mut out).unwrap();
        assert_eq!(out, b"\x1b]777;notify;KISS;Your input is required.\x07");
        out.clear();
        n.status(blocked, true, later);
        n.poll(later, &mut out).unwrap();
        assert!(out.is_empty());
        n.status(ProgramStatus::Working, true, later);
        n.status(blocked, true, later);
        n.status(ProgramStatus::Working, true, later);
        n.poll(later + GRACE, &mut out).unwrap();
        assert!(out.is_empty());

        n.input(&InputEvent::Focus(false));
        n.prompt_finished(false, true, now);
        n.poll(now, &mut out).unwrap();
        assert!(!out.is_empty(), "known-unfocused delivery is immediate");
    }

    #[test]
    fn cmux_uses_one_addressed_command_and_failures_stop_future_delivery() {
        let mut n = Notifications {
            route: Route::Cmux {
                workspace: "workspace with spaces".into(),
                surface: "surface-3".into(),
            },
            ..Default::default()
        };
        let now = Instant::now();
        let mut out = Vec::new();
        n.input(&InputEvent::Focus(false));
        n.prompt_finished(false, true, now);
        let command = n.poll(now, &mut out).unwrap().unwrap();
        assert!(out.is_empty(), "cmux must not also emit OSC");
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(command.as_std().get_program(), "cmux");
        assert_eq!(
            args,
            [
                "notify",
                "--workspace",
                "workspace with spaces",
                "--surface",
                "surface-3",
                "--title",
                "KISS",
                "--body",
                "Your prompt has finished."
            ]
        );
        n.prompt_finished(false, true, now);
        assert!(
            n.poll(now, &mut out).unwrap().is_none(),
            "commands cannot overlap"
        );
        n.delivery_finished(&Err("cmux unavailable".into()));
        n.prompt_finished(false, true, now);
        assert!(n.poll(now + GRACE, &mut out).unwrap().is_none());
        assert!(
            out.is_empty(),
            "failure must not trigger a second delivery route"
        );
    }

    #[tokio::test]
    async fn notification_process_failures_are_bounded_and_reported() {
        fn shell(script: &str) -> Command {
            #[cfg(windows)]
            let mut command = Command::new("powershell.exe");
            #[cfg(windows)]
            command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
            #[cfg(not(windows))]
            let mut command = Command::new("/bin/sh");
            #[cfg(not(windows))]
            command.args(["-c", script]);
            command
        }
        let timeout = Duration::from_secs(5);
        run_command(&mut shell("exit 0"), timeout).await.unwrap();
        let error = run_command(&mut shell("exit 7"), timeout)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exited with"));
        let directory = tempfile::tempdir().unwrap();
        let error = run_command(&mut Command::new(directory.path().join("missing")), timeout)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("could not run"));
        #[cfg(windows)]
        let mut slow = shell("Start-Sleep -Seconds 30");
        #[cfg(not(windows))]
        let mut slow = shell("exec sleep 30");
        let error = run_command(&mut slow, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    #[ignore = "sends a real notification; requires a graphical desktop and notification permissions"]
    async fn native_notification() {
        send(notification_command(Notice::Complete).unwrap())
            .await
            .unwrap();
    }
}
