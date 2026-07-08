//! Minimal Server-Sent Events parser. The Anthropic streaming API emits frames
//! as:
//!
//! ```text
//! event: message_start
//! data: {"message": {...}}
//!
//! ```
//!
//! A frame is terminated by a blank line (`\n\n`). Within a frame, `event:`
//! sets the event name and one or more `data:` lines carry the JSON payload
//! (concatenated with `\n`). This module is pure and synchronous so it can be
//! unit-tested without a network.

/// One decoded SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: String,
    pub data: String,
}

/// Incremental SSE parser. Feed it raw byte/string chunks via [`SseParser::feed`]
/// and pull complete frames via [`SseParser::next_frame`].
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
    /// Start index in `buf` of the next unscanned frame boundary search.
    cursor: usize,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk of bytes (interpreted as UTF-8, replacing invalid bytes).
    /// CR characters are stripped so CRLF and lone-CR line endings normalize to
    /// LF, per the SSE spec.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        let s = String::from_utf8_lossy(bytes);
        self.push_strip_cr(&s);
    }

    pub fn feed_str(&mut self, s: &str) {
        self.push_strip_cr(s);
    }

    fn push_strip_cr(&mut self, s: &str) {
        if s.contains('\r') {
            for c in s.chars() {
                if c != '\r' {
                    self.buf.push(c);
                }
            }
        } else {
            self.buf.push_str(s);
        }
    }

    /// Try to pull the next complete frame. Returns `None` if more data is
    /// needed. Frames are separated by a blank line.
    pub fn next_frame(&mut self) -> Option<SseFrame> {
        while let Some(rel) = self.buf[self.cursor..].find("\n\n") {
            let abs_start = self.cursor;
            let abs_end = self.cursor + rel;
            // Advance cursor past the separator (2 bytes for "\n\n").
            self.cursor = abs_end + 2;

            let frame_str = &self.buf[abs_start..abs_end];
            let frame = parse_frame(frame_str);
            // Only emit frames that actually carry an event or data; skip
            // comment/keep-alive lines.
            if frame.event.is_empty() && frame.data.is_empty() {
                continue;
            }
            return Some(frame);
        }
        // Drop already-consumed prefix to keep the buffer bounded.
        if self.cursor > 0 {
            self.buf.drain(..self.cursor);
            self.cursor = 0;
        }
        None
    }

    /// True if the parser holds no buffered data.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

fn parse_frame(raw: &str) -> SseFrame {
    let mut event = String::new();
    let mut data_parts: Vec<&str> = Vec::new();

    for line in raw.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            // Per spec, a single leading space after the colon is stripped.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            data_parts.push(rest);
        }
        // Ignore "id:", "retry:", ":" comments, and blank lines.
    }

    SseFrame {
        event,
        data: data_parts.join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_frame() {
        let mut p = SseParser::new();
        p.feed_str("event: message_start\ndata: {\"a\":1}\n\n");
        let f = p.next_frame().unwrap();
        assert_eq!(f.event, "message_start");
        assert_eq!(f.data, "{\"a\":1}");
        assert!(p.next_frame().is_none());
    }

    #[test]
    fn parses_two_frames_incrementally() {
        let mut p = SseParser::new();
        p.feed_str("event: ping\n\nevent: message_stop\ndata: {}\n\n");
        let f1 = p.next_frame().unwrap();
        assert_eq!(f1.event, "ping");
        let f2 = p.next_frame().unwrap();
        assert_eq!(f2.event, "message_stop");
        assert_eq!(f2.data, "{}");
        assert!(p.next_frame().is_none());
    }

    #[test]
    fn concatenates_multi_line_data() {
        let mut p = SseParser::new();
        p.feed_str("event: delta\ndata: line1\ndata: line2\n\n");
        let f = p.next_frame().unwrap();
        assert_eq!(f.data, "line1\nline2");
    }

    #[test]
    fn waits_for_more_data() {
        let mut p = SseParser::new();
        p.feed_str("event: partial\ndata: {");
        assert!(p.next_frame().is_none());
        p.feed_str("\"x\":2}\n\n");
        let f = p.next_frame().unwrap();
        assert_eq!(f.data, "{\"x\":2}");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut p = SseParser::new();
        p.feed_str("event: e\r\ndata: d\r\n\r\n");
        let f = p.next_frame().unwrap();
        assert_eq!(f.event, "e");
        assert_eq!(f.data, "d");
    }
}
