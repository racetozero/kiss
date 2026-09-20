//! Raw-mode terminal control. The only module that touches the real
//! terminal; everything else renders to strings.

use crossterm::terminal;
use std::io::Write;

const ENTER_SEQUENCE: &[u8] = b"\x1b[?2004h\x1b[>1u\x1b[1 q\x1b[?25l";
const RESTORE_SEQUENCE: &[u8] =
    b"\x1b]9;4;0;0\x1b\\\x1b]0;\x07\x1b[?2004l\x1b[<1u\x1b[0 q\x1b[?25h\x1b[0m\r\n";
const IDLE_TITLE_SEQUENCE: &str = "\x1b]0;💋 kiss\x07";
const WORKING_TITLE_SEQUENCES: [&str; 10] = [
    "\x1b]0;💋 ⠋ kiss\x07",
    "\x1b]0;💋 ⠙ kiss\x07",
    "\x1b]0;💋 ⠹ kiss\x07",
    "\x1b]0;💋 ⠸ kiss\x07",
    "\x1b]0;💋 ⠼ kiss\x07",
    "\x1b]0;💋 ⠴ kiss\x07",
    "\x1b]0;💋 ⠦ kiss\x07",
    "\x1b]0;💋 ⠧ kiss\x07",
    "\x1b]0;💋 ⠇ kiss\x07",
    "\x1b]0;💋 ⠏ kiss\x07",
];
const PROGRESS_IDLE_SEQUENCE: &[u8] = b"\x1b]9;4;0;0\x1b\\";
const PROGRESS_WORKING_SEQUENCE: &[u8] = b"\x1b]9;4;3;0\x1b\\";
const TITLE_TICKS_PER_FRAME: usize = 1;

fn write_control_sequence(out: &mut impl Write, sequence: &[u8]) -> std::io::Result<()> {
    out.write_all(sequence)?;
    out.flush()
}

pub struct Terminal {
    raw: bool,
    title_frame: u8,
}

impl Terminal {
    pub fn new() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        // Bracketed paste and modified-key reporting on. The latter lets
        // supporting terminals report Shift+Enter separately from Enter.
        // Use a blinking block and hide it until the renderer places it.
        out.write_all(ENTER_SEQUENCE)?;
        out.write_all(IDLE_TITLE_SEQUENCE.as_bytes())?;
        out.write_all(PROGRESS_IDLE_SEQUENCE)?;
        out.flush()?;
        Ok(Terminal {
            raw: true,
            title_frame: 0,
        })
    }

    pub fn size() -> (usize, usize) {
        terminal::size()
            .map(|(w, h)| (w as usize, h as usize))
            .unwrap_or((80, 24))
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
            spinner_tick / TITLE_TICKS_PER_FRAME % WORKING_TITLE_SEQUENCES.len() + 1
        } else {
            0
        };
        let progress_changed = (self.title_frame != 0) != working;
        let progress_keep_alive = working && title_frame == 1 && self.title_frame != 1;
        if !progress_changed && usize::from(self.title_frame) == title_frame {
            return Ok(());
        }

        if progress_changed || progress_keep_alive {
            out.write_all(if working {
                PROGRESS_WORKING_SEQUENCE
            } else {
                PROGRESS_IDLE_SEQUENCE
            })?;
        }
        if title_frame == 0 {
            out.write_all(IDLE_TITLE_SEQUENCE.as_bytes())?;
        } else {
            out.write_all(WORKING_TITLE_SEQUENCES[title_frame - 1].as_bytes())?;
        }
        out.flush()?;
        self.title_frame = title_frame as u8;
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

    #[test]
    fn terminal_control_sequences_are_symmetric() {
        assert_eq!(ENTER_SEQUENCE, b"\x1b[?2004h\x1b[>1u\x1b[1 q\x1b[?25l");
        assert_eq!(
            IDLE_TITLE_SEQUENCE.as_bytes(),
            b"\x1b]0;\xf0\x9f\x92\x8b kiss\x07"
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
        let mut terminal = Terminal {
            raw: true,
            title_frame: 0,
        };
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
        assert_eq!(output, WORKING_TITLE_SEQUENCES[1].as_bytes());

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
}
