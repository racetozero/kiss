//! Terminal key decoding and key-spec parsing ("ctrl+x", "alt+enter").

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

/// Decode raw terminal input bytes into key events (with leftover bytes kept
/// for the next read). Handles the common CSI/SS3 sequences, alt-prefix, and
/// bracketed paste.
#[derive(Debug, Default)]
pub struct InputDecoder {
    buffer: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key(KeyEvent),
    Paste(String),
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
    }

    /// Try to decode one event from the buffer head. Returns (event, bytes
    /// consumed); None means incomplete input, wait for more bytes.
    fn try_decode(&self) -> Option<(Option<InputEvent>, usize)> {
        let buf = &self.buffer;
        if buf.is_empty() {
            return None;
        }
        if buf[0] != 0x1b {
            return self.decode_plain();
        }
        // Lone escape: emit only if nothing else is buffered (ambiguous).
        if buf.len() == 1 {
            return Some((
                Some(InputEvent::Key(KeyEvent {
                    key: Key::Escape,
                    ..Default::default()
                })),
                1,
            ));
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
        // Modifier encoding: CSI 1;<mods> X or CSI <num>;<mods> ~
        let parts: Vec<&str> = params.split(';').collect();
        let modifier_parts = parts.get(1).map(|part| part.split(':').collect::<Vec<_>>());
        if modifier_parts
            .as_ref()
            .and_then(|parts| parts.get(1))
            .and_then(|event_type| event_type.parse::<u8>().ok())
            == Some(3)
        {
            return Some((None, consumed));
        }
        if let Some(m) = modifier_parts
            .as_ref()
            .and_then(|parts| parts.first())
            .and_then(|modifier| modifier.parse::<u8>().ok())
        {
            let m = m.saturating_sub(1);
            event.shift = m & 1 != 0;
            event.alt = m & 2 != 0;
            event.ctrl = m & 4 != 0;
        }
        event.key = match final_byte {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            b'Z' => Key::BackTab,
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
                Some(11..=15) => Key::F(parts[0].parse::<u8>().unwrap() - 10),
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
                    Some(c) => Key::Char(c),
                    None => return Some((None, consumed)),
                }
            }
            _ => return Some((None, consumed)),
        };
        Some((Some(InputEvent::Key(event)), consumed))
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
    fn kitty_protocol_ignores_key_release_events() {
        assert!(decode(b"\x1b[13;1:3u").is_empty());
        assert_eq!(
            decode(b"\x1b[9;2:1u"),
            vec![InputEvent::Key(KeyEvent::parse("shift+tab").unwrap())]
        );
    }
}
