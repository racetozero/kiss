//! Terminal key decoding and key-spec parsing ("ctrl+x", "alt+enter").

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Escape,
    F(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct KeyEvent {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Default for Key {
    fn default() -> Self {
        Key::Char(' ')
    }
}

impl KeyEvent {
    pub fn char(c: char) -> Self {
        KeyEvent {
            key: Key::Char(c),
            ..Default::default()
        }
    }

    pub fn ctrl(c: char) -> Self {
        KeyEvent {
            key: Key::Char(c),
            ctrl: true,
            ..Default::default()
        }
    }

    /// Parse a spec like "ctrl+x", "alt+enter", "shift+tab", "escape".
    pub fn parse(spec: &str) -> Option<KeyEvent> {
        let mut event = KeyEvent::default();
        let parts: Vec<&str> = spec.split('+').collect();
        let (mods, key_part) = parts.split_at(parts.len().checked_sub(1)?);
        for m in mods {
            match m.to_lowercase().as_str() {
                "ctrl" | "control" => event.ctrl = true,
                "alt" | "meta" | "option" => event.alt = true,
                "shift" => event.shift = true,
                _ => return None,
            }
        }
        let key = key_part.first()?.to_lowercase();
        event.key = match key.as_str() {
            "enter" | "return" => Key::Enter,
            "tab" if event.shift => {
                event.shift = false;
                Key::BackTab
            }
            "tab" => Key::Tab,
            "backtab" => Key::BackTab,
            "backspace" => Key::Backspace,
            "delete" | "del" => Key::Delete,
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" => Key::PageUp,
            "pagedown" => Key::PageDown,
            "escape" | "esc" => Key::Escape,
            "space" => Key::Char(' '),
            k if k.len() == 1 => Key::Char(k.chars().next()?),
            k if k.starts_with('f') => Key::F(k[1..].parse().ok()?),
            _ => return None,
        };
        Some(event)
    }
}

impl fmt::Display for KeyEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.alt {
            f.write_str("alt+")?;
        }
        if self.shift {
            f.write_str("shift+")?;
        }
        match self.key {
            Key::Char(' ') => f.write_str("space"),
            Key::Char(c) => write!(f, "{c}"),
            Key::Enter => f.write_str("enter"),
            Key::Tab => f.write_str("tab"),
            Key::BackTab => f.write_str("backtab"),
            Key::Backspace => f.write_str("backspace"),
            Key::Delete => f.write_str("delete"),
            Key::Up => f.write_str("up"),
            Key::Down => f.write_str("down"),
            Key::Left => f.write_str("left"),
            Key::Right => f.write_str("right"),
            Key::Home => f.write_str("home"),
            Key::End => f.write_str("end"),
            Key::PageUp => f.write_str("pageup"),
            Key::PageDown => f.write_str("pagedown"),
            Key::Escape => f.write_str("escape"),
            Key::F(number) => write!(f, "f{number}"),
        }
    }
}

/// Decode raw terminal input bytes into key events (with leftover bytes kept
/// for the next read). Handles the common CSI/SS3 sequences, alt-prefix, and
/// bracketed paste.
#[derive(Debug, Default)]
pub struct InputDecoder {
    buffer: Vec<u8>,
    escape_since: Option<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key(KeyEvent),
    KeyRelease(KeyEvent),
    Paste(String),
    /// Terminal focus reporting (CSI I / CSI O).
    Focus(bool),
}

impl InputDecoder {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<InputEvent>) {
        self.buffer.extend_from_slice(bytes);
        while let Some((event, consumed)) = self.try_decode() {
            self.buffer.drain(..consumed);
            if let Some(event) = event {
                out.push(event);
            }
        }
        if self.buffer == [0x1b] {
            self.escape_since
                .get_or_insert_with(std::time::Instant::now);
        } else {
            self.escape_since = None;
        }
    }

    /// Give split escape sequences a short window before treating ESC as a key.
    pub fn escape_deadline(&self) -> Option<std::time::Instant> {
        self.escape_since
            .map(|since| since + std::time::Duration::from_millis(30))
    }

    /// Call when escape_deadline expires without receiving more bytes.
    pub fn flush_escape(&mut self, out: &mut Vec<InputEvent>) {
        if self.buffer == [0x1b] {
            self.buffer.clear();
            self.escape_since = None;
            out.push(InputEvent::Key(KeyEvent {
                key: Key::Escape,
                ..Default::default()
            }));
        }
    }

    /// Try to decode one event from the buffer head. Returns (event, bytes
    /// consumed). None means incomplete input, wait for more bytes.
    fn try_decode(&self) -> Option<(Option<InputEvent>, usize)> {
        let buf = &self.buffer;
        if buf.is_empty() {
            return None;
        }
        if buf[0] != 0x1b {
            return self.decode_plain();
        }
        // A lone ESC may be the first byte of a fragmented focus/key report.
        // The input owner calls flush_escape after the disambiguation deadline.
        if buf.len() == 1 {
            return None;
        }
        match buf[1] {
            b'[' => self.decode_csi(),
            b'O' => {
                if buf.len() < 3 {
                    return None;
                }
                let key = match buf[2] {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    b'P' => Key::F(1),
                    b'Q' => Key::F(2),
                    b'R' => Key::F(3),
                    b'S' => Key::F(4),
                    b'M' => Key::Enter,
                    _ => return Some((None, 3)),
                };
                Some((
                    Some(InputEvent::Key(KeyEvent {
                        key,
                        ..Default::default()
                    })),
                    3,
                ))
            }
            // Alt+arrow from terminals that prefix the whole sequence with ESC,
            // for example ESC ESC [D for Option+Left. Without this, `[D` would
            // reach the editor as text.
            0x1b if buf.len() > 2 && matches!(buf[2], b'[' | b'O') => {
                let inner = InputDecoder {
                    buffer: buf[1..].to_vec(),
                    escape_since: None,
                };
                let (event, consumed) = inner.try_decode()?;
                let event = event.map(|e| match e {
                    InputEvent::Key(mut k) => {
                        k.alt = true;
                        InputEvent::Key(k)
                    }
                    other => other,
                });
                Some((event, consumed + 1))
            }
            // Alt+key: ESC then a normal byte.
            _ => {
                let (event, consumed) = self.decode_plain_at(1)?;
                let event = event.map(|e| match e {
                    InputEvent::Key(mut k) => {
                        k.alt = true;
                        InputEvent::Key(k)
                    }
                    other => other,
                });
                Some((event, consumed + 1))
            }
        }
    }

    fn decode_plain(&self) -> Option<(Option<InputEvent>, usize)> {
        self.decode_plain_at(0)
    }

    fn decode_plain_at(&self, start: usize) -> Option<(Option<InputEvent>, usize)> {
        let buf = &self.buffer[start..];
        let byte = *buf.first()?;
        let event = match byte {
            b'\r' => Some(InputEvent::Key(KeyEvent {
                key: Key::Enter,
                ..Default::default()
            })),
            // Some terminals send Shift+Enter as line feed instead of a
            // modified Enter escape sequence. Keep carriage return as submit
            // and treat line feed as the newline shortcut.
            b'\n' => Some(InputEvent::Key(KeyEvent {
                key: Key::Enter,
                shift: true,
                ..Default::default()
            })),
            b'\t' => Some(InputEvent::Key(KeyEvent {
                key: Key::Tab,
                ..Default::default()
            })),
            0x7f | 0x08 => Some(InputEvent::Key(KeyEvent {
                key: Key::Backspace,
                ..Default::default()
            })),
            0x01..=0x1a => {
                let c = (byte - 1 + b'a') as char;
                Some(InputEvent::Key(KeyEvent {
                    key: Key::Char(c),
                    ctrl: true,
                    ..Default::default()
                }))
            }
            _ => {
                // UTF-8 char.
                let len = utf8_len(byte)?;
                if buf.len() < len {
                    return None;
                }
                let s = std::str::from_utf8(&buf[..len]).ok()?;
                let c = s.chars().next()?;
                return Some((Some(InputEvent::Key(KeyEvent::char(c))), len));
            }
        };
        Some((event, 1))
    }

    fn decode_csi(&self) -> Option<(Option<InputEvent>, usize)> {
        let buf = &self.buffer;
        // Find the final byte of the CSI sequence.
        let mut end = 2;
        while end < buf.len() {
            let b = buf[end];
            if (0x40..=0x7e).contains(&b) {
                break;
            }
            end += 1;
        }
        if end >= buf.len() {
            return None; // incomplete
        }
        let final_byte = buf[end];
        let params: String = buf[2..end].iter().map(|&b| b as char).collect();
        let consumed = end + 1;

        if params.is_empty() && matches!(final_byte, b'I' | b'O') {
            return Some((Some(InputEvent::Focus(final_byte == b'I')), consumed));
        }

        // Bracketed paste: ESC[200~ ... ESC[201~
        if final_byte == b'~' && params == "200" {
            let terminator = b"\x1b[201~";
            let rest = &buf[consumed..];
            let pos = rest
                .windows(terminator.len())
                .position(|w| w == terminator)?;
            let text = String::from_utf8_lossy(&rest[..pos]).into_owned();
            return Some((
                Some(InputEvent::Paste(text)),
                consumed + pos + terminator.len(),
            ));
        }

        let mut event = KeyEvent::default();
        let mut command = false;
        // Modifier encoding: CSI 1;<mods> X or CSI <num>;<mods> ~
        let parts: Vec<&str> = params.split(';').collect();
        let modifier_parts = parts.get(1).map(|part| part.split(':').collect::<Vec<_>>());
        let event_type = modifier_parts
            .as_ref()
            .and_then(|parts| parts.get(1))
            .and_then(|event_type| event_type.parse::<u8>().ok());
        if let Some(m) = modifier_parts
            .as_ref()
            .and_then(|parts| parts.first())
            .and_then(|modifier| modifier.parse::<u8>().ok())
        {
            let m = m.saturating_sub(1);
            event.shift = m & 1 != 0;
            event.alt = m & 2 != 0;
            event.ctrl = m & 4 != 0;
            // Kitty reports macOS Command as Super (8).
            command = m & 8 != 0;
        }
        event.key = match final_byte {
            b'A' => Key::Up,
            b'B' => Key::Down,
            // Command+Left/Right go to the line start or end, as in macOS
            // text fields.
            b'C' if command => Key::End,
            b'D' if command => Key::Home,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            b'Z' => Key::BackTab,
            // F1-F4 with modifiers: CSI 1;<mods> P/Q/R/S
            b'P' => Key::F(1),
            b'Q' => Key::F(2),
            b'R' => Key::F(3),
            b'S' => Key::F(4),
            b'~' if parts.first() == Some(&"27") => {
                // xterm modifyOtherKeys: CSI 27;<mods>;<codepoint>~
                match parts
                    .get(2)
                    .and_then(|p| p.parse::<u32>().ok())
                    .and_then(char::from_u32)
                {
                    Some('\r') | Some('\n') => Key::Enter,
                    Some(c) => Key::Char(c),
                    None => return Some((None, consumed)),
                }
            }
            b'~' => match parts.first().and_then(|p| p.parse::<u8>().ok()) {
                Some(1) | Some(7) => Key::Home,
                Some(3) => Key::Delete,
                Some(4) | Some(8) => Key::End,
                Some(5) => Key::PageUp,
                Some(6) => Key::PageDown,
                Some(number @ 11..=15) => Key::F(number - 10),
                Some(number @ 17..=21) => Key::F(number - 11),
                Some(number @ 23..=24) => Key::F(number - 12),
                _ => return Some((None, consumed)),
            },
            b'u' => {
                // Kitty keyboard protocol: codepoint;mods u
                match parts
                    .first()
                    .and_then(|p| p.split(':').next())
                    .and_then(|p| p.parse::<u32>().ok())
                    .and_then(char::from_u32)
                {
                    Some('\t') if event.shift => {
                        event.shift = false;
                        Key::BackTab
                    }
                    Some('\t') => Key::Tab,
                    Some('\r') | Some('\u{e046}') => Key::Enter,
                    Some('\x1b') => Key::Escape,
                    // Modified Backspace, such as Option+Backspace (CSI 127;3u).
                    Some('\x7f') => Key::Backspace,
                    // Kitty reports F13-F35 as private-use code points.
                    Some(c @ '\u{e020}'..='\u{e036}') => Key::F((c as u32 - 0xe020 + 13) as u8),
                    // Other functional keys, such as keypad and modifier
                    // keys, are private-use code points and not text.
                    Some('\u{e000}'..='\u{f8ff}') => return Some((None, consumed)),
                    Some(c) => Key::Char(c),
                    None => return Some((None, consumed)),
                }
            }
            _ => return Some((None, consumed)),
        };
        // Repeated Space must not toggle voice recording.
        if event_type == Some(2) && event.key == Key::Char(' ') {
            return Some((None, consumed));
        }
        let input = if event_type == Some(3) {
            InputEvent::KeyRelease(event)
        } else {
            InputEvent::Key(event)
        };
        Some((Some(input), consumed))
    }
}

fn utf8_len(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<InputEvent> {
        let mut d = InputDecoder::default();
        let mut out = Vec::new();
        d.feed(bytes, &mut out);
        out
    }

    #[test]
    fn focus_reports_survive_fragmentation_without_becoming_cancel_or_editor_input() {
        let wire = b"\x1b[I\x1b[O";
        for split in 0..=wire.len() {
            let mut decoder = InputDecoder::default();
            let mut events = Vec::new();
            decoder.feed(&wire[..split], &mut events);
            decoder.feed(&wire[split..], &mut events);
            assert_eq!(
                events,
                [InputEvent::Focus(true), InputEvent::Focus(false)],
                "split {split}"
            );
            assert!(decoder.escape_deadline().is_none());
        }
        let mut decoder = InputDecoder::default();
        let mut events = Vec::new();
        for byte in wire {
            decoder.feed(&[*byte], &mut events);
        }
        assert_eq!(events, [InputEvent::Focus(true), InputEvent::Focus(false)]);
        events.clear();
        decoder.feed(b"\x1b", &mut events);
        assert!(events.is_empty());
        assert!(decoder.escape_deadline().is_some());
        decoder.flush_escape(&mut events);
        decoder.flush_escape(&mut events);
        assert_eq!(
            events,
            [InputEvent::Key(KeyEvent {
                key: Key::Escape,
                ..Default::default()
            })]
        );
        assert!(decoder.escape_deadline().is_none());
        assert_eq!(
            decode(b"\x1b[200~\x1b[I\x1b[201~"),
            [InputEvent::Paste("\x1b[I".into())]
        );
    }

    #[test]
    fn function_keys_decode_and_private_use_keys_are_not_text() {
        let f = |number: u8, shift: bool| {
            InputEvent::Key(KeyEvent {
                key: Key::F(number),
                shift,
                ..Default::default()
            })
        };
        assert_eq!(decode(b"\x1bOP"), [f(1, false)]);
        assert_eq!(decode(b"\x1b[1;2P"), [f(1, true)]);
        assert_eq!(decode(b"\x1b[1;2S"), [f(4, true)]);
        assert_eq!(decode(b"\x1b[13~"), [f(3, false)]);
        assert_eq!(decode(b"\x1b[15~"), [f(5, false)]);
        assert_eq!(decode(b"\x1b[17~"), [f(6, false)]);
        assert_eq!(decode(b"\x1b[21;2~"), [f(10, true)]);
        assert_eq!(decode(b"\x1b[23~"), [f(11, false)]);
        assert_eq!(decode(b"\x1b[24~"), [f(12, false)]);
        assert_eq!(decode(b"\x1b[57376u"), [f(13, false)]);
        assert_eq!(decode(b"\x1b[57398;2u"), [f(35, true)]);
        // Keypad 0 and Left Shift are private-use code points.
        assert!(decode(b"\x1b[57399u").is_empty());
        assert!(decode(b"\x1b[57441u").is_empty());
        assert_eq!(
            decode(b"\x1b[57414u"),
            [InputEvent::Key(KeyEvent {
                key: Key::Enter,
                ..Default::default()
            })]
        );
    }

    #[test]
    fn plain_chars_and_ctrl() {
        assert_eq!(decode(b"a"), vec![InputEvent::Key(KeyEvent::char('a'))]);
        assert_eq!(decode(&[0x03]), vec![InputEvent::Key(KeyEvent::ctrl('c'))]);
        assert_eq!(
            decode(b"\r"),
            vec![InputEvent::Key(KeyEvent {
                key: Key::Enter,
                ..Default::default()
            })]
        );
        assert_eq!(
            decode(b"\n"),
            vec![InputEvent::Key(KeyEvent {
                key: Key::Enter,
                shift: true,
                ..Default::default()
            })]
        );
    }

    #[test]
    fn arrows_and_modified() {
        assert_eq!(
            decode(b"\x1b[A"),
            vec![InputEvent::Key(KeyEvent {
                key: Key::Up,
                ..Default::default()
            })]
        );
        // Alt+Enter via ESC prefix
        assert_eq!(
            decode(b"\x1b\r"),
            vec![InputEvent::Key(KeyEvent {
                key: Key::Enter,
                alt: true,
                ..Default::default()
            })]
        );
        // Ctrl+Right: CSI 1;5C
        assert_eq!(
            decode(b"\x1b[1;5C"),
            vec![InputEvent::Key(KeyEvent {
                key: Key::Right,
                ctrl: true,
                ..Default::default()
            })]
        );
        // Enhanced keyboard protocol: Shift+Enter and Alt+Enter.
        assert_eq!(
            decode(b"\x1b[13;2u"),
            vec![InputEvent::Key(KeyEvent {
                key: Key::Enter,
                shift: true,
                ..Default::default()
            })]
        );
        assert_eq!(
            decode(b"\x1b[13;3u"),
            vec![InputEvent::Key(KeyEvent {
                key: Key::Enter,
                alt: true,
                ..Default::default()
            })]
        );
        // xterm modifyOtherKeys encoding.
        assert_eq!(
            decode(b"\x1b[27;2;13~"),
            vec![InputEvent::Key(KeyEvent {
                key: Key::Enter,
                shift: true,
                ..Default::default()
            })]
        );
    }

    #[test]
    fn bracketed_paste() {
        let events = decode(b"\x1b[200~hello\nworld\x1b[201~");
        assert_eq!(events, vec![InputEvent::Paste("hello\nworld".into())]);
    }

    #[test]
    fn utf8_multibyte() {
        assert_eq!(
            decode("é".as_bytes()),
            vec![InputEvent::Key(KeyEvent::char('é'))]
        );
    }

    #[test]
    fn spec_parsing() {
        assert_eq!(KeyEvent::parse("ctrl+x"), Some(KeyEvent::ctrl('x')));
        assert_eq!(
            KeyEvent::parse("alt+enter"),
            Some(KeyEvent {
                key: Key::Enter,
                alt: true,
                ..Default::default()
            })
        );
        assert_eq!(
            KeyEvent::parse("shift+tab"),
            Some(KeyEvent {
                key: Key::BackTab,
                ..Default::default()
            })
        );
    }

    #[test]
    fn terminal_backtab_matches_shift_tab_spec() {
        assert_eq!(
            decode(b"\x1b[Z"),
            vec![InputEvent::Key(KeyEvent::parse("shift+tab").unwrap())]
        );
    }

    #[test]
    fn kitty_protocol_normalizes_tab_and_enter_keys() {
        assert_eq!(
            decode(b"\x1b[9;2u"),
            vec![InputEvent::Key(KeyEvent::parse("shift+tab").unwrap())]
        );
        assert_eq!(
            decode(b"\x1b[13u"),
            vec![InputEvent::Key(KeyEvent::parse("enter").unwrap())]
        );
        assert_eq!(
            decode(b"\x1b[57414u"),
            vec![InputEvent::Key(KeyEvent::parse("enter").unwrap())]
        );
        assert_eq!(
            decode(b"\x1bOM"),
            vec![InputEvent::Key(KeyEvent::parse("enter").unwrap())]
        );
    }

    #[test]
    fn kitty_protocol_reports_release_and_repeats_except_voice_space() {
        assert_eq!(
            decode(b"\x1b[1;1:1B\x1b[1;1:2B\x1b[1;1:2B\x1b[1;1:3B"),
            vec![
                InputEvent::Key(KeyEvent::parse("down").unwrap()),
                InputEvent::Key(KeyEvent::parse("down").unwrap()),
                InputEvent::Key(KeyEvent::parse("down").unwrap()),
                InputEvent::KeyRelease(KeyEvent::parse("down").unwrap()),
            ]
        );
        for (bytes, spec) in [
            (b"\x1b[1;1:2A".as_slice(), "up"),
            (b"\x1b[1;1:2C".as_slice(), "right"),
            (b"\x1b[1;1:2D".as_slice(), "left"),
            (b"\x1b[1;5:2A".as_slice(), "ctrl+up"),
            (b"\x1b[97;1:2u".as_slice(), "a"),
        ] {
            assert_eq!(
                decode(bytes),
                vec![InputEvent::Key(KeyEvent::parse(spec).unwrap())]
            );
        }
        assert_eq!(
            decode(b"\x1b[32;1:3u"),
            vec![InputEvent::KeyRelease(KeyEvent::char(' '))]
        );
        assert!(decode(b"\x1b[32;1:2u").is_empty());
        assert_eq!(
            decode(b"\x1b[9;2:1u"),
            vec![InputEvent::Key(KeyEvent::parse("shift+tab").unwrap())]
        );
    }
}
