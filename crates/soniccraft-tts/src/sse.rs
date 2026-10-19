//! Incremental `text/event-stream` parser (WHATWG rules: LF, CR or CRLF line ends, `:` comments, multi-line
//! `data:` joined with `\n`, a blank line dispatches). It works on raw bytes, so UTF-8 sequences and lines split
//! across network chunks are safe: a line is only decoded once its terminator has arrived. Lines and events
//! are capped ([`MAX_LINE_BYTES`]), so a stream without line terminators cannot grow the buffer without bound.

/// Longest line, and longest event payload, the parser buffers.
pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// A line or event longer than the parser's limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineTooLong {
    /// The limit, in bytes.
    pub limit: usize,
}

/// One dispatched event.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct SseEvent {
    /// The `event:` field (`None` = "message").
    pub event: Option<String>,
    /// The `data:` lines joined with `\n`.
    pub data: String,
}

/// Parser state between chunks.
#[derive(Debug)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
    /// `buf[..scanned]` holds no line terminator (searched by an earlier call), so a long line arriving in
    /// many chunks is searched once, not once per chunk.
    scanned: usize,
    current: SseEvent,
    has_data: bool,
    limit: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::with_limit(MAX_LINE_BYTES)
    }
}

impl SseParser {
    /// A parser refusing lines and events longer than `limit` bytes.
    pub(crate) fn with_limit(limit: usize) -> Self {
        SseParser {
            buf: Vec::new(),
            scanned: 0,
            current: SseEvent::default(),
            has_data: false,
            limit,
        }
    }

    /// Feed one network chunk; returns every event completed by it, or an error once a line (or an event's
    /// data) passes the limit.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, LineTooLong> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        let mut from = self.scanned;
        while let Some(rel) = self.buf[from..]
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
        {
            let end = from + rel;
            let next = if self.buf[end] == b'\r' {
                match self.buf.get(end + 1) {
                    Some(b'\n') => end + 2,
                    Some(_) => end + 1,
                    None => break, // a CR at the end of the buffer may be the first half of CRLF
                }
            } else {
                end + 1
            };
            if end - start > self.limit {
                return Err(LineTooLong { limit: self.limit });
            }
            let line = String::from_utf8_lossy(&self.buf[start..end]).into_owned();
            start = next;
            from = next;
            self.line(&line, &mut out);
            if self.current.data.len() > self.limit {
                return Err(LineTooLong { limit: self.limit });
            }
        }
        self.buf.drain(..start);
        // Everything left was searched, except a final CR that may be the first half of a CRLF.
        self.scanned = self.buf.len() - usize::from(self.buf.last() == Some(&b'\r'));
        if self.buf.len() > self.limit {
            return Err(LineTooLong { limit: self.limit });
        }
        Ok(out)
    }

    /// End of stream: process a final unterminated line and dispatch a pending event. (Stricter parsers drop
    /// it; providers occasionally omit the final blank line, and losing the `done` event would turn a complete
    /// stream into a failure.)
    pub(crate) fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        self.scanned = 0;
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&rest);
            let line = line.trim_end_matches('\r').to_owned();
            self.line(&line, &mut out);
        }
        self.line("", &mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            let mut event = std::mem::take(&mut self.current);
            if std::mem::take(&mut self.has_data) {
                if event.data.ends_with('\n') {
                    event.data.pop();
                }
                out.push(event);
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
            "event" => self.current.event = Some(value.to_owned()),
            "data" => {
                self.current.data.push_str(value);
                self.current.data.push('\n');
                self.has_data = true;
            }
            _ => {} // `id`, `retry` and unknown fields are irrelevant here
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = ": keep-alive\n\nretry: 1000\n\nevent: chunk\ndata: {\"a\":1}\n\ndata: héllo ✓\r\ndata: second\r\n\r\ndata: last\rdata: cr\r\r";

    fn expected() -> Vec<SseEvent> {
        vec![
            SseEvent {
                event: Some("chunk".into()),
                data: "{\"a\":1}".into(),
            },
            SseEvent {
                event: None,
                data: "héllo ✓\nsecond".into(),
            },
            SseEvent {
                event: None,
                data: "last\ncr".into(),
            },
        ]
    }

    #[test]
    fn whole_stream() {
        let mut p = SseParser::default();
        let mut events = p.feed(STREAM.as_bytes()).unwrap();
        events.extend(p.finish());
        assert_eq!(events, expected());
    }

    #[test]
    fn byte_by_byte_and_every_split_point() {
        let bytes = STREAM.as_bytes();
        let mut p = SseParser::default();
        let mut events = Vec::new();
        for b in bytes {
            events.extend(p.feed(std::slice::from_ref(b)).unwrap());
        }
        events.extend(p.finish());
        assert_eq!(events, expected());
        for split in 0..=bytes.len() {
            let mut p = SseParser::default();
            let mut events = p.feed(&bytes[..split]).unwrap();
            events.extend(p.feed(&bytes[split..]).unwrap());
            events.extend(p.finish());
            assert_eq!(events, expected(), "split at {split}");
        }
    }

    #[test]
    fn lines_and_events_are_capped() {
        // An unterminated line fails as soon as the buffer passes the limit, whatever the chunking.
        let mut p = SseParser::with_limit(64);
        assert!(p.feed(&[b'x'; 40]).unwrap().is_empty());
        assert_eq!(p.feed(&[b'x'; 40]), Err(LineTooLong { limit: 64 }));
        // A terminated line over the limit, and an event made of many short data lines.
        let mut p = SseParser::with_limit(64);
        let long = format!("data: {}\n", "y".repeat(100));
        assert!(p.feed(long.as_bytes()).is_err());
        let mut p = SseParser::with_limit(64);
        assert!(p.feed("data: abcdefghij\n".repeat(10).as_bytes()).is_err());
        // Short lines and dispatched events never accumulate.
        let mut p = SseParser::with_limit(64);
        for _ in 0..1_000 {
            assert_eq!(p.feed(b": keep-alive\n\ndata: x\n\n").unwrap().len(), 1);
        }
    }

    #[test]
    fn long_lines_in_small_chunks_are_searched_once() {
        // 8 MiB of data in 1 KiB chunks: rescanning the line from its start on every chunk would touch
        // 32 GiB; searching each byte once keeps this instant.
        let payload = "z".repeat(8 << 20);
        let stream = format!("data: {payload}\r\n\r\ndata: b\n\n");
        let mut p = SseParser::default();
        let started = std::time::Instant::now();
        let mut events = Vec::new();
        for piece in stream.as_bytes().chunks(1024) {
            events.extend(p.feed(piece).unwrap());
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data.len(), payload.len());
        assert_eq!(events[1].data, "b");
        // A CRLF split across chunks is still one line end.
        let mut p = SseParser::default();
        let mut events = p.feed(b"data: x\r").unwrap();
        events.extend(p.feed(b"\n\r").unwrap());
        events.extend(p.feed(b"\ndata: y\n\n").unwrap());
        let data: Vec<&str> = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(data, ["x", "y"]);
    }

    #[test]
    fn finish_dispatches_unterminated_event() {
        let mut p = SseParser::default();
        assert!(p.feed(b"data: {\"type\":\"done\"}").unwrap().is_empty());
        assert_eq!(
            p.finish(),
            vec![SseEvent {
                event: None,
                data: "{\"type\":\"done\"}".into()
            }]
        );
        let mut empty = SseParser::default();
        assert!(empty.finish().is_empty());
    }
}
