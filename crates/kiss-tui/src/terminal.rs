//! Raw-mode terminal control. The only module that touches the real
//! terminal. Everything else renders to strings.

use base64::Engine as _;
use crossterm::terminal;
use std::io::Write;

const ENTER_SEQUENCE: &[u8] = b"\x1b[?2004h\x1b[>3u\x1b[1 q\x1b[?25l";
const RESTORE_SEQUENCE: &[u8] =
    b"\x1b]7501;state=clear\x1b\\\x1b]9;4;0;0\x1b\\\x1b]0;\x07\x1b[?2004l\x1b[<1u\x1b[0 q\x1b[?25h\x1b[0m\r\n";
const TITLE_PREFIX: &[u8] = b"\x1b]0;\xf0\x9f\x92\x8b ";
const TITLE_SUFFIX: &[u8] = b"\x07";
const WORKING_TITLE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PROGRESS_IDLE_SEQUENCE: &[u8] = b"\x1b]9;4;0;0\x1b\\";
const PROGRESS_WORKING_SEQUENCE: &[u8] = b"\x1b]9;4;3;0\x1b\\";
const PROGRAM_STATUS_PANIC_SEQUENCE: &[u8] = b"\x1b]7501;state=error:app=kiss\x1b\\";
const TITLE_TICKS_PER_FRAME: usize = 1;
const SESSION_TITLE_MAX_CHARS: usize = 48;

fn write_control_sequence(out: &mut impl Write, sequence: &[u8]) -> std::io::Result<()> {
    out.write_all(sequence)?;
    out.flush()
}

fn sanitize_session_title(title: &str) -> String {
    let mut sanitized = String::with_capacity(title.len().min(SESSION_TITLE_MAX_CHARS));
    let mut pending_space = false;
    let mut chars = 0;
    for character in title.trim().chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !sanitized.is_empty();
            continue;
        }
        if pending_space {
            if chars == SESSION_TITLE_MAX_CHARS {
                break;
            }
            sanitized.push(' ');
            chars += 1;
            pending_space = false;
        }
        if chars == SESSION_TITLE_MAX_CHARS {
            break;
        }
        sanitized.push(character);
        chars += 1;
    }
    sanitized
}

fn write_title(
    out: &mut impl Write,
    title_frame: usize,
    session_title: &str,
) -> std::io::Result<()> {
    out.write_all(TITLE_PREFIX)?;
    if title_frame != 0 {
        out.write_all(WORKING_TITLE_FRAMES[title_frame - 1].as_bytes())?;
        out.write_all(b" ")?;
    }
    out.write_all(if session_title.is_empty() {
        b"kiss"
    } else {
        session_title.as_bytes()
    })?;
    out.write_all(TITLE_SUFFIX)
}

/// Why the user must act, reported as the OSC 7501 `kind` key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Auth,
}

/// Program state reported with the OSC 7501 Program Status Protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStatus {
    Idle,
    Working,
    Done,
    Error,
    Blocked(BlockedKind, &'static str),
}

pub struct Terminal {
    raw: bool,
    #[cfg(windows)]
    windows_console: Option<crate::windows_console::WindowsConsole>,
    title_frame: u8,
    session_title: String,
    title_dirty: bool,
    progress_enabled: bool,
    program_status: Option<ProgramStatus>,
}

impl Terminal {
    pub fn new() -> anyhow::Result<Self> {
        #[cfg(windows)]
        let windows_console = Some(crate::windows_console::WindowsConsole::new()?);
        terminal::enable_raw_mode()?;
        let terminal = Terminal {
            raw: true,
            #[cfg(windows)]
            windows_console,
            title_frame: 0,
            session_title: String::new(),
            title_dirty: false,
            progress_enabled: std::env::var_os("TERM_PROGRAM")
                .is_none_or(|value| value != "ghostty"),
            program_status: None,
        };
        let mut out = std::io::stdout();
        // Bracketed paste and modified-key reporting on. The latter lets
        // supporting terminals report Shift+Enter separately from Enter.
        // Use a blinking block and hide it until the renderer places it.
        out.write_all(ENTER_SEQUENCE)?;
        write_title(&mut out, 0, "")?;
        out.write_all(PROGRESS_IDLE_SEQUENCE)?;
        out.flush()?;
        Ok(terminal)
    }

    pub fn size() -> (usize, usize) {
        terminal::size()
            .map(|(w, h)| (w as usize, h as usize))
            .unwrap_or((80, 24))
    }

    /// Cache a session description for the next activity update.
    pub fn set_session_title(&mut self, title: Option<&str>) {
        let title = title.map(sanitize_session_title).unwrap_or_default();
        if self.session_title != title {
            self.session_title = title;
            self.title_dirty = true;
        }
    }

    /// Update the terminal tab without allocating or starting another timer.
    pub fn set_activity(&mut self, working: bool, spinner_tick: usize) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        self.write_activity(&mut out, working, spinner_tick)
    }

    fn write_activity(
        &mut self,
        out: &mut impl Write,
        working: bool,
        spinner_tick: usize,
    ) -> std::io::Result<()> {
        let title_frame = if working {
            spinner_tick / TITLE_TICKS_PER_FRAME % WORKING_TITLE_FRAMES.len() + 1
        } else {
            0
        };
        let progress_changed = (self.title_frame != 0) != working;
        if !progress_changed && usize::from(self.title_frame) == title_frame && !self.title_dirty {
            return Ok(());
        }

        if self.progress_enabled && progress_changed {
            out.write_all(if working {
                PROGRESS_WORKING_SEQUENCE
            } else {
                PROGRESS_IDLE_SEQUENCE
            })?;
        }
        write_title(out, title_frame, &self.session_title)?;
        out.flush()?;
        self.title_frame = title_frame as u8;
        self.title_dirty = false;
        Ok(())
    }

    /// Report a changed status with OSC 7501. Unsupported terminals ignore it.
    pub fn set_program_status(&mut self, status: ProgramStatus) -> std::io::Result<()> {
        if self.program_status == Some(status) {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        self.write_program_status(&mut out, status)
    }

    fn write_program_status(
        &mut self,
        out: &mut impl Write,
        status: ProgramStatus,
    ) -> std::io::Result<()> {
        if self.program_status == Some(status) {
            return Ok(());
        }
        out.write_all(b"\x1b]7501;state=")?;
        match status {
            ProgramStatus::Idle => out.write_all(b"idle")?,
            ProgramStatus::Working => out.write_all(b"working")?,
            ProgramStatus::Done => out.write_all(b"done")?,
            ProgramStatus::Error => out.write_all(b"error")?,
            ProgramStatus::Blocked(kind, message) => {
                out.write_all(match kind {
                    BlockedKind::Permission => b"blocked:kind=permission:msg=",
                    BlockedKind::Auth => b"blocked:kind=auth:msg=",
                })?;
                let message = base64::engine::general_purpose::STANDARD.encode(message);
                out.write_all(message.as_bytes())?;
            }
        }
        out.write_all(b":app=kiss")?;
        out.write_all(b"\x1b\\")?;
        out.flush()?;
        self.program_status = Some(status);
        Ok(())
    }

    pub fn restore(&mut self) {
        if self.raw {
            self.raw = false;
            let mut out = std::io::stdout();
            let _ = write_control_sequence(&mut out, RESTORE_SEQUENCE);
            let _ = terminal::disable_raw_mode();
            #[cfg(windows)]
            self.windows_console.take();
        }
    }

    /// Install before terminal setup. Restore modes even when a panic aborts.
    pub fn install_panic_hook() {
        #[cfg(windows)]
        let windows_console = crate::windows_console::WindowsConsole::snapshot().ok();
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut out = std::io::stdout();
            let _ = write_control_sequence(&mut out, RESTORE_SEQUENCE);
            let _ = write_control_sequence(&mut out, PROGRAM_STATUS_PANIC_SEQUENCE);
            let _ = terminal::disable_raw_mode();
            #[cfg(windows)]
            if let Some(modes) = &windows_console {
                crate::windows_console::WindowsConsole::restore(modes);
            }
            default(info);
        }));
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_panic_restores_console_modes_before_abort() {
        use crate::windows_console::WindowsConsole;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn AllocConsole() -> i32;
            fn FreeConsole() -> i32;
            fn SetErrorMode(mode: u32) -> u32;
        }

        let role = std::env::var("KISS_TEST_PANIC_CONSOLE").unwrap_or_default();
        if role == "abort" {
            // Suppress Windows crash dialogs in the child process.
            unsafe {
                SetErrorMode(0x0001 | 0x0002);
            }
            std::panic::set_hook(Box::new(|_| std::process::abort()));
            Terminal::install_panic_hook();
            let _terminal = Terminal::new().unwrap();
            println!("terminal ready to panic");
            std::io::stdout().flush().unwrap();
            panic!("intentional console cleanup check");
        }

        if role == "observer" {
            // Give the observer and aborting child a console separate from the test runner.
            unsafe {
                FreeConsole();
                assert_ne!(AllocConsole(), 0);
            }
            let mut original = WindowsConsole::snapshot().unwrap();
            for ((mode, saved), flag) in original.iter_mut().zip([0x0200, 0x0004]) {
                *saved &= !flag;
                mode.set_mode(*saved).unwrap();
            }
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "terminal::tests::windows_panic_restores_console_modes_before_abort",
                    "--nocapture",
                ])
                .env("KISS_TEST_PANIC_CONSOLE", "abort")
                .output()
                .unwrap();
            assert!(!child.status.success());
            assert!(
                String::from_utf8_lossy(&child.stdout).contains("terminal ready to panic"),
                "{child:?}"
            );
            for (mode, saved) in original {
                assert_eq!(mode.mode().unwrap(), saved);
            }
            return;
        }

        let observer = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal::tests::windows_panic_restores_console_modes_before_abort",
                "--nocapture",
            ])
            .env("KISS_TEST_PANIC_CONSOLE", "observer")
            .output()
            .unwrap();
        assert!(observer.status.success(), "{observer:?}");
    }

    fn terminal() -> Terminal {
        Terminal {
            raw: true,
            #[cfg(windows)]
            windows_console: None,
            title_frame: 0,
            session_title: String::new(),
            title_dirty: false,
            progress_enabled: true,
            program_status: None,
        }
    }

    #[test]
    fn terminal_control_sequences_are_symmetric() {
        assert_eq!(ENTER_SEQUENCE, b"\x1b[?2004h\x1b[>3u\x1b[1 q\x1b[?25l");
        let mut title = Vec::new();
        write_title(&mut title, 0, "").unwrap();
        assert_eq!(title, b"\x1b]0;\xf0\x9f\x92\x8b kiss\x07");
        title.clear();
        write_title(&mut title, 1, "Fix login flow").unwrap();
        assert_eq!(
            title,
            b"\x1b]0;\xf0\x9f\x92\x8b \xe2\xa0\x8b Fix login flow\x07"
        );
        assert_eq!(PROGRESS_IDLE_SEQUENCE, b"\x1b]9;4;0;0\x1b\\");

        let mut restored = Vec::new();
        write_control_sequence(&mut restored, RESTORE_SEQUENCE).unwrap();
        assert_eq!(
            restored,
            b"\x1b]7501;state=clear\x1b\\\x1b]9;4;0;0\x1b\\\x1b]0;\x07\x1b[?2004l\x1b[<1u\x1b[0 q\x1b[?25h\x1b[0m\r\n"
        );
    }

    #[test]
    fn terminal_activity_writes_only_changed_states_and_frames() {
        let mut terminal = terminal();
        let mut output = Vec::new();

        terminal.write_activity(&mut output, false, 0).unwrap();
        assert!(output.is_empty());

        terminal.write_activity(&mut output, true, 0).unwrap();
        assert_eq!(
            output,
            b"\x1b]9;4;3;0\x1b\\\x1b]0;\xf0\x9f\x92\x8b \xe2\xa0\x8b kiss\x07"
        );

        output.clear();
        terminal.write_activity(&mut output, true, 1).unwrap();
        assert_eq!(output, b"\x1b]0;\xf0\x9f\x92\x8b \xe2\xa0\x99 kiss\x07");

        output.clear();
        terminal.write_activity(&mut output, true, 10).unwrap();
        assert_eq!(output, b"\x1b]0;\xf0\x9f\x92\x8b \xe2\xa0\x8b kiss\x07");

        output.clear();
        terminal.write_activity(&mut output, false, 2).unwrap();
        assert_eq!(
            output,
            b"\x1b]9;4;0;0\x1b\\\x1b]0;\xf0\x9f\x92\x8b kiss\x07"
        );

        output.clear();
        terminal.progress_enabled = false;
        terminal.write_activity(&mut output, true, 0).unwrap();
        assert_eq!(output, b"\x1b]0;\xf0\x9f\x92\x8b \xe2\xa0\x8b kiss\x07");
    }

    #[test]
    fn terminal_activity_caches_and_sanitizes_session_titles() {
        let mut terminal = terminal();
        let mut output = Vec::new();

        terminal.set_session_title(Some("  Fix\nlogin\x1b]0;bad\x07  "));
        terminal.write_activity(&mut output, false, 0).unwrap();
        assert_eq!(output, b"\x1b]0;\xf0\x9f\x92\x8b Fix login ]0;bad\x07");

        output.clear();
        terminal.write_activity(&mut output, false, 0).unwrap();
        assert!(output.is_empty());

        terminal.set_session_title(Some(&"🚀".repeat(60)));
        terminal.write_activity(&mut output, true, 0).unwrap();
        assert_eq!(
            terminal.session_title.chars().count(),
            SESSION_TITLE_MAX_CHARS
        );
        assert!(!terminal.session_title.chars().any(char::is_control));
    }

    #[test]
    fn program_status_writes_osc_7501_only_when_status_changes() {
        let mut terminal = terminal();
        let mut output = Vec::new();

        terminal
            .write_program_status(&mut output, ProgramStatus::Idle)
            .unwrap();
        assert_eq!(output, b"\x1b]7501;state=idle:app=kiss\x1b\\");

        output.clear();
        terminal
            .write_program_status(&mut output, ProgramStatus::Idle)
            .unwrap();
        assert!(output.is_empty());

        terminal
            .write_program_status(
                &mut output,
                ProgramStatus::Blocked(BlockedKind::Permission, "Workflow approval required"),
            )
            .unwrap();
        assert_eq!(
            output,
            b"\x1b]7501;state=blocked:kind=permission:msg=V29ya2Zsb3cgYXBwcm92YWwgcmVxdWlyZWQ=:app=kiss\x1b\\"
        );

        output.clear();
        terminal
            .write_program_status(
                &mut output,
                ProgramStatus::Blocked(BlockedKind::Auth, "Login"),
            )
            .unwrap();
        assert_eq!(
            output,
            b"\x1b]7501;state=blocked:kind=auth:msg=TG9naW4=:app=kiss\x1b\\"
        );

        output.clear();
        terminal
            .write_program_status(&mut output, ProgramStatus::Done)
            .unwrap();
        assert_eq!(output, b"\x1b]7501;state=done:app=kiss\x1b\\");
    }
}
