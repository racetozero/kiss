//! Incremental server-sent-events parser.
//!
//! Feeds arbitrary byte chunks, yields complete events. Handles multi-line
//! `data:` fields, comment lines, CRLF line endings, and UTF-8 sequences
//! split across chunk boundaries. One reusable buffer per stream.

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    /// Byte offset of unconsumed input in `buffer`.
    pos: usize,
    event_name: Option<String>,
    data: String,
    has_data: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk and collect any completed events.
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) {
        if self.pos > 0 && self.pos == self.buffer.len() {
            self.buffer.clear();
            self.pos = 0;
        }
        self.buffer.extend_from_slice(chunk);

        loop {
            let rest = &self.buffer[self.pos..];
            let Some(newline) = rest.iter().position(|&b| b == b'\n') else {
                break;
            };
            let mut line_end = newline;
            if line_end > 0 && rest[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            let line_start = self.pos;
            let absolute_line_end = line_start + line_end;
            self.pos += newline + 1;
            match std::str::from_utf8(&self.buffer[line_start..absolute_line_end]) {
                Ok(line) => Self::process_line(
                    &mut self.event_name,
                    &mut self.data,
                    &mut self.has_data,
                    line,
                    out,
                ),
                Err(_) => {
                    let line = String::from_utf8_lossy(&self.buffer[line_start..absolute_line_end])
                        .into_owned();
                    Self::process_line(
                        &mut self.event_name,
                        &mut self.data,
                        &mut self.has_data,
                        &line,
                        out,
                    );
                }
            }
        }

        // Compact consumed bytes when the buffer grows.
        if self.pos > 64 * 1024 {
            self.buffer.drain(..self.pos);
            self.pos = 0;
        }
    }

    fn process_line(
        event_name: &mut Option<String>,
        data: &mut String,
        has_data: &mut bool,
        line: &str,
        out: &mut Vec<SseEvent>,
    ) {
        if line.is_empty() {
            if *has_data || event_name.is_some() {
                let event = SseEvent {
                    event: event_name.take(),
                    data: std::mem::take(data),
                };
                *has_data = false;
                if !event.data.is_empty() || event.event.is_some() {
                    out.push(event);
                }
            }
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => *event_name = Some(value.to_string()),
            "data" => {
                if *has_data {
                    data.push('\n');
                }
                data.push_str(value);
                *has_data = true;
            }
            _ => {}
        }
    }

    /// Flush a trailing event at end of stream (no terminating blank line).
    pub fn finish(&mut self, out: &mut Vec<SseEvent>) {
        if self.pos < self.buffer.len() {
            let line = String::from_utf8_lossy(&self.buffer[self.pos..]).into_owned();
            self.pos = self.buffer.len();
            let line = line.strip_suffix('\r').unwrap_or(&line).to_string();
            Self::process_line(
                &mut self.event_name,
                &mut self.data,
                &mut self.has_data,
                &line,
                out,
            );
        }
        if self.has_data || self.event_name.is_some() {
            out.push(SseEvent {
                event: self.event_name.take(),
                data: std::mem::take(&mut self.data),
            });
            self.has_data = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&str]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        for c in chunks {
            parser.feed(c.as_bytes(), &mut out);
        }
        parser.finish(&mut out);
        out
    }

    #[test]
    fn basic_events() {
        let events = collect(&["event: ping\ndata: {}\n\ndata: hello\n\n"]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "{}");
        assert_eq!(events[1].event, None);
        assert_eq!(events[1].data, "hello");
    }

    #[test]
    fn split_across_chunks_and_crlf() {
        let events = collect(&["data: par", "tial\r\n\r\n", "data: a\ndata: b\n\n"]);
        assert_eq!(events[0].data, "partial");
        assert_eq!(events[1].data, "a\nb");
    }

    #[test]
    fn comments_ignored() {
        let events = collect(&[": keep-alive\n\ndata: x\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn trailing_event_flushed() {
        let events = collect(&["data: tail"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail");
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_sse_parser() {
        let payload = (0..10_000)
            .map(|index| format!("event: response.output_text.delta\ndata: {{\"index\":{index},\"delta\":\"token\"}}\n\n"))
            .collect::<String>();
        kiss_bench::measure(
            "sse_parse_10000",
            15,
            5,
            "10000_events_4096_byte_chunks",
            || {
                let mut parser = SseParser::new();
                let mut events = Vec::with_capacity(10_000);
                for chunk in payload.as_bytes().chunks(4_096) {
                    parser.feed(chunk, &mut events);
                }
                parser.finish(&mut events);
                events.len()
            },
        );
    }
}
