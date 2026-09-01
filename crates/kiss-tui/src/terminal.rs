//! Raw-mode terminal control. The only module that touches the real
//! terminal; everything else renders to strings.

use crossterm::terminal;
use std::io::Write;

const ENTER_SEQUENCE: &[u8] = b"\x1b[?2004h\x1b[>1u\x1b[1 q\x1b[?25l";
const RESTORE_SEQUENCE: &[u8] = b"\x1b[?2004l\x1b[<1u\x1b[0 q\x1b[?25h\x1b[0m\r\n";

fn write_control_sequence(out: &mut impl Write, sequence: &[u8]) -> std::io::Result<()> {
    out.write_all(sequence)?;
    out.flush()
}

pub struct Terminal {
    raw: bool,
}

impl Terminal {
    pub fn new() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        // Bracketed paste and modified-key reporting on. The latter lets
        // supporting terminals report Shift+Enter separately from Enter.
        // Use a blinking block and hide it until the renderer places it.
        write_control_sequence(&mut out, ENTER_SEQUENCE)?;
        Ok(Terminal { raw: true })
    }

    pub fn size() -> (usize, usize) {
        terminal::size()
            .map(|(w, h)| (w as usize, h as usize))
            .unwrap_or((80, 24))
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
        let mut entered = Vec::new();
        write_control_sequence(&mut entered, ENTER_SEQUENCE).unwrap();
        assert_eq!(entered, b"\x1b[?2004h\x1b[>1u\x1b[1 q\x1b[?25l");

        let mut restored = Vec::new();
        write_control_sequence(&mut restored, RESTORE_SEQUENCE).unwrap();
        assert_eq!(restored, b"\x1b[?2004l\x1b[<1u\x1b[0 q\x1b[?25h\x1b[0m\r\n");
    }
}
