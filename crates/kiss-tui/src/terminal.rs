//! Raw-mode terminal control. The only module that touches the real
//! terminal. Everything else renders to strings.

use crossterm::terminal;
use std::io::Write;

const ENTER_SEQUENCE: &[u8] = b"\x1b[?2004h\x1b[>1u\x1b[1 q\x1b[?25l";
const RESTORE_SEQUENCE: &[u8] =
    b"\x1b]9;4;0;0\x1b\\\x1b]0;\x07\x1b[?2004l\x1b[<1u\x1b[0 q\x1b[?25h\x1b[0m\r\n";
const TITLE_PREFIX: &[u8] = b"\x1b]0;\xf0\x9f\x92\x8b ";
const TITLE_SUFFIX: &[u8] = b"\x07";
const WORKING_TITLE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PROGRESS_IDLE_SEQUENCE: &[u8] = b"\x1b]9;4;0;0\x1b\\";
const PROGRESS_WORKING_SEQUENCE: &[u8] = b"\x1b]9;4;3;0\x1b\\";
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

pub struct Terminal {
    raw: bool,
    title_frame: u8,
    session_title: String,
    title_dirty: bool,
}

impl Terminal {
    pub fn new() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        // Bracketed paste and modified-key reporting on. The latter lets
        // supporting terminals report Shift+Enter separately from Enter.
        // Use a blinking block and hide it until the renderer places it.
        out.write_all(ENTER_SEQUENCE)?;
        write_title(&mut out, 0, "")?;
        out.write_all(PROGRESS_IDLE_SEQUENCE)?;
        out.flush()?;
        Ok(Terminal {
            raw: true,
            title_frame: 0,
            session_title: String::new(),
            title_dirty: false,
        })
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
        let progress_keep_alive = working && title_frame == 1 && self.title_frame != 1;
        if !progress_changed && usize::from(self.title_frame) == title_frame && !self.title_dirty {
            return Ok(());
        }

        if progress_changed || progress_keep_alive {
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

    pub fn restore(&mut self) {
        if self.raw {
            self.raw = false;
            let mut out = std::io::stdout();
            let _ = write_control_sequence(&mut out, RESTORE_SEQUENCE);
            let _ = terminal::disable_raw_mode();
        }
    }

    /// Install a panic hook that restores the terminal before unwinding.
    pub fn install_panic_hook() {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut out = std::io::stdout();
            let _ = write_control_sequence(&mut out, RESTORE_SEQUENCE);
            let _ = terminal::disable_raw_mode();
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

    fn terminal() -> Terminal {
        Terminal {
            raw: true,
            title_frame: 0,
            session_title: String::new(),
            title_dirty: false,
        }
    }

    #[test]
    fn terminal_control_sequences_are_symmetric() {
        assert_eq!(ENTER_SEQUENCE, b"\x1b[?2004h\x1b[>1u\x1b[1 q\x1b[?25l");
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
            b"\x1b]9;4;0;0\x1b\\\x1b]0;\x07\x1b[?2004l\x1b[<1u\x1b[0 q\x1b[?25h\x1b[0m\r\n"
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
        assert_eq!(
            output,
            b"\x1b]9;4;3;0\x1b\\\x1b]0;\xf0\x9f\x92\x8b \xe2\xa0\x8b kiss\x07"
        );

        output.clear();
        terminal.write_activity(&mut output, false, 2).unwrap();
        assert_eq!(
            output,
            b"\x1b]9;4;0;0\x1b\\\x1b]0;\xf0\x9f\x92\x8b kiss\x07"
        );
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
}
