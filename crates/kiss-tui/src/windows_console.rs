//! Enable the escape sequences used by the byte reader and renderer.

use crossterm_winapi::{ConsoleMode, Handle};

pub(crate) struct WindowsConsole([(ConsoleMode, u32); 2]);

impl WindowsConsole {
    pub(crate) fn snapshot() -> std::io::Result<[(ConsoleMode, u32); 2]> {
        let input = ConsoleMode::from(Handle::current_in_handle()?);
        let output = ConsoleMode::from(Handle::current_out_handle()?);
        let input_mode = input.mode()?;
        let output_mode = output.mode()?;
        Ok([(input, input_mode), (output, output_mode)])
    }

    pub(crate) fn new() -> std::io::Result<Self> {
        let console = Self(Self::snapshot()?);
        // ENABLE_VIRTUAL_TERMINAL_INPUT and ENABLE_VIRTUAL_TERMINAL_PROCESSING.
        for ((mode, original), flag) in console.0.iter().zip([0x0200, 0x0004]) {
            mode.set_mode(original | flag)?;
        }
        Ok(console)
    }

    pub(crate) fn restore(modes: &[(ConsoleMode, u32); 2]) {
        for (mode, original) in modes {
            let _ = mode.set_mode(*original);
        }
    }
}

impl Drop for WindowsConsole {
    fn drop(&mut self) {
        Self::restore(&self.0);
    }
}
