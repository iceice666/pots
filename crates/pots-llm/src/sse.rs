//! Server-sent events, read line by line from a blocking reader.

use std::io::{self, BufRead};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field; empty if the server sent none.
    pub event: String,
    /// `data:` lines, joined with `\n`.
    pub data: String,
}

pub struct Reader<R> {
    inner: R,
    line: String,
}

impl<R: BufRead> Reader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, line: String::new() }
    }

    /// The next event, or `None` at end of stream.
    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        let mut event = Event::default();
        let mut has_data = false;
        loop {
            self.line.clear();
            if self.inner.read_line(&mut self.line)? == 0 {
                // A final event without the blank line that should end it.
                return Ok(has_data.then_some(event));
            }
            let line = self.line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if has_data {
                    return Ok(Some(event));
                }
                event = Event::default();
                continue;
            }
            if line.starts_with(':') {
                continue; // comment / keep-alive
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "event" => event.event = value.to_string(),
                "data" => {
                    if has_data {
                        event.data.push('\n');
                    }
                    event.data.push_str(value);
                    has_data = true;
                }
                _ => {} // id, retry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(text: &str) -> Vec<Event> {
        let mut r = Reader::new(text.as_bytes());
        let mut out = Vec::new();
        while let Some(e) = r.next_event().unwrap() {
            out.push(e);
        }
        out
    }

    #[test]
    fn parses_events_comments_crlf_and_multiline_data() {
        let events = all(": keep-alive\r\nevent: ping\r\ndata: {}\r\n\r\ndata: a\ndata: b\n\nevent: x\n\ndata:tail");
        assert_eq!(
            events,
            vec![
                Event { event: "ping".into(), data: "{}".into() },
                Event { event: String::new(), data: "a\nb".into() },
                Event { event: String::new(), data: "tail".into() },
            ]
        );
    }
}
