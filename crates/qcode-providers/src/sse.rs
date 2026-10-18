//! Incremental Server-Sent Events decoder.
//!
//! Works on raw bytes so a multi-byte UTF-8 character split across TCP chunks
//! is reassembled before decoding. Only complete lines are decoded, and an
//! incomplete trailing event is discarded — the same contract as the JSONL
//! store's torn-tail handling.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: String,
    id: Option<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
            line.pop(); // trailing '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }

            // A line is only decoded once complete, so this never splits a char.
            let Ok(line) = std::str::from_utf8(&line) else {
                continue;
            };
            self.process_line(line, &mut events);
        }

        events
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if self.data.is_empty() && self.event.is_none() && self.id.is_none() {
                return;
            }
            out.push(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data),
                id: self.id.take(),
            });
            return;
        }

        if line.starts_with(':') {
            return; // comment / keep-alive
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };

        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            "id" => self.id = Some(value.to_string()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_event() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn event_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(b"data: he").is_empty());
        let events = decoder.feed(b"llo\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"data: one\n\ndata: two\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
    }

    #[test]
    fn crlf_and_comments() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b": keep-alive\r\ndata: x\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"data: a\ndata: b\n\n");
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        let bytes = "你".as_bytes();
        let mut first = b"data: ".to_vec();
        first.extend_from_slice(&bytes[..2]);
        assert!(decoder.feed(&first).is_empty());

        let mut second = bytes[2..].to_vec();
        second.extend_from_slice(b"\n\n");
        let events = decoder.feed(&second);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "你");
    }

    #[test]
    fn incomplete_trailing_event_is_discarded() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(b"data: partial").is_empty());
        assert!(decoder.feed(b" more").is_empty());
    }
}
