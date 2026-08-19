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
    /// Leftover bytes from a previous chunk that end in an incomplete UTF-8
    /// sequence; prepended to the next chunk before decoding.
    pending_bytes: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk of bytes. A chunk may end in the middle of a multi-byte
    /// UTF-8 character (network reads have no respect for char boundaries);
    /// the incomplete tail is buffered and completed by the next chunk instead
    /// of being corrupted into U+FFFD replacement chars. CR characters are
    /// stripped so CRLF and lone-CR line endings normalize to LF, per the SSE
    /// spec.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        // Prepend any incomplete UTF-8 tail held back from the previous chunk.
        let owned;
        let bytes: &[u8] = if self.pending_bytes.is_empty() {
            bytes
        } else {
            let mut joined = std::mem::take(&mut self.pending_bytes);
            joined.extend_from_slice(bytes);
            owned = joined;
            &owned
        };
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                self.push_strip_cr(s);
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: valid_up_to() guarantees this prefix is valid UTF-8.
                let s = unsafe { std::str::from_utf8_unchecked(&bytes[..valid]) };
                self.push_strip_cr(s);
                self.pending_bytes = bytes[valid..].to_vec();
            }
        }
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
    fn multibyte_char_split_across_chunks_survives() {
        // Regression: feed_bytes used from_utf8_lossy per chunk, so a UTF-8
        // char (here '═' U+2550, 3 bytes) split across chunk boundaries was
        // corrupted into two U+FFFD replacement chars.
        let frame = "event: content_block\ndata: ════════════════\n\n";
        let bytes = frame.as_bytes();
        // Split at every possible position to exercise all boundaries.
        for split in 1..bytes.len() {
            let mut p = SseParser::new();
            p.feed_bytes(&bytes[..split]);
            p.feed_bytes(&bytes[split..]);
            let f = p.next_frame().unwrap();
            assert_eq!(f.data, "════════════════", "corrupted at split {split}");
        }
    }

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
