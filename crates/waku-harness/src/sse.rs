//! Minimal incremental SSE decoding (WHATWG frame semantics).
//!
//! `data:` lines accumulate until a blank line dispatches the frame;
//! multi-line data joins with `\n`. Comments and unknown fields are ignored.

use crate::error::HarnessError;
use futures::{Stream, StreamExt};
use std::collections::VecDeque;

/// One decoded SSE frame.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
struct FrameAcc {
    event: Option<String>,
    data: Vec<u8>,
    saw_data: bool,
}

/// Incremental SSE decoder fed byte chunks.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    pos: usize,
    /// Next unread byte to inspect for a line ending. Survives feeds so a
    /// large unterminated line is not rescanned from `pos` on every chunk.
    scan: usize,
    acc: FrameAcc,
    done: bool,
    #[cfg(test)]
    bytes_inspected: usize,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk; returns frames completed by this chunk.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        if self.done {
            return Vec::new();
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(line) = self.take_line(false) {
            self.process_line(&line, &mut out);
        }
        out
    }

    /// Flush trailing bytes and an unterminated frame at EOF.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = Vec::new();
        while let Some(line) = self.take_line(true) {
            self.process_line(&line, &mut out);
        }
        if let Some(evt) = self.take_frame() {
            out.push(evt);
        }
        out
    }

    fn take_line(&mut self, eof: bool) -> Option<Vec<u8>> {
        if self.scan < self.pos {
            self.scan = self.pos;
        }
        while self.scan < self.buf.len() {
            self.inspect(1);
            match self.buf[self.scan] {
                b'\n' => {
                    let mut line = self.buf[self.pos..self.scan].to_vec();
                    self.pos = self.scan + 1;
                    self.scan = self.pos;
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    self.compact_if_needed();
                    return Some(line);
                }
                b'\r' => {
                    if self.scan + 1 == self.buf.len() && !eof {
                        return None;
                    }
                    self.inspect(usize::from(self.scan + 1 < self.buf.len()));
                    let has_lf = self.buf.get(self.scan + 1) == Some(&b'\n');
                    let line = self.buf[self.pos..self.scan].to_vec();
                    self.pos = self.scan + 1 + usize::from(has_lf);
                    self.scan = self.pos;
                    self.compact_if_needed();
                    return Some(line);
                }
                _ => {
                    self.scan += 1;
                }
            }
        }
        if eof && self.pos < self.buf.len() {
            let line = self.buf[self.pos..].to_vec();
            self.buf.clear();
            self.pos = 0;
            self.scan = 0;
            return Some(line);
        }
        None
    }

    fn compact_if_needed(&mut self) {
        if self.pos >= 4096 || (self.pos > 0 && self.pos * 2 >= self.buf.len()) {
            self.buf.drain(..self.pos);
            self.scan = self.scan.saturating_sub(self.pos);
            self.pos = 0;
        }
    }

    fn inspect(&mut self, n: usize) {
        let _ = n;
        #[cfg(test)]
        {
            self.bytes_inspected = self.bytes_inspected.saturating_add(n);
        }
    }

    fn process_line(&mut self, line: &[u8], out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if let Some(evt) = self.take_frame() {
                out.push(evt);
            }
            return;
        }
        if line.first() == Some(&b':') {
            return;
        }
        let (field, value) = split_field(line);
        match field {
            b"data" => {
                if self.acc.saw_data {
                    self.acc.data.push(b'\n');
                }
                self.acc.saw_data = true;
                self.acc.data.extend_from_slice(value);
            }
            b"event" => {
                self.acc.event = Some(String::from_utf8_lossy(value).into_owned());
            }
            _ => {}
        }
    }

    fn take_frame(&mut self) -> Option<SseEvent> {
        // WHATWG dispatches only frames with data; an event-only frame is
        // metadata for a frame that never arrived and must be discarded.
        if !self.acc.saw_data {
            self.acc.event = None;
            return None;
        }
        let acc = std::mem::take(&mut self.acc);
        Some(SseEvent {
            event: acc.event,
            data: String::from_utf8_lossy(&acc.data).into_owned(),
        })
    }
}

fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|&b| b == b':') {
        Some(i) => {
            let v = &line[i + 1..];
            let v = if v.first() == Some(&b' ') { &v[1..] } else { v };
            (&line[..i], v)
        }
        None => (line, &[]),
    }
}

/// Drive a byte-chunk async stream to completion, yielding SSE frames.
pub fn sse_stream<S, E, B>(
    bytes: S,
    format: &'static str,
) -> impl Stream<Item = Result<SseEvent, HarnessError>>
where
    S: Stream<Item = Result<B, E>> + Send,
    B: AsRef<[u8]> + Send,
    E: std::fmt::Display + Send,
{
    let state = (Box::pin(bytes), SseDecoder::new(), VecDeque::new(), false);
    futures::stream::unfold(
        state,
        move |(mut bytes, mut decoder, mut pending, mut done)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((event, (bytes, decoder, pending, done)));
                }
                if done {
                    return None;
                }
                match bytes.as_mut().next().await {
                    Some(Ok(chunk)) => {
                        pending.extend(decoder.feed(chunk.as_ref()).into_iter().map(Ok));
                    }
                    Some(Err(error)) => {
                        pending.push_back(Err(HarnessError::Malformed {
                            format,
                            detail: format!("byte stream error: {error}"),
                        }));
                        done = true;
                    }
                    None => {
                        pending.extend(decoder.finish().into_iter().map(Ok));
                        done = true;
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_frames_with_blank_line_boundaries() {
        let mut d = SseDecoder::new();
        let evts = d.feed(b"event: message\ndata: {\"a\":1}\n\ndata: second\n\n");
        assert_eq!(evts.len(), 2);
        assert_eq!(evts[0].event.as_deref(), Some("message"));
        assert_eq!(evts[0].data, "{\"a\":1}");
        assert_eq!(evts[1].event, None);
        assert_eq!(evts[1].data, "second");
    }

    #[test]
    fn joins_multiline_data() {
        let mut d = SseDecoder::new();
        let evts = d.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "line1\nline2");
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut d = SseDecoder::new();
        let evts = d.feed(b": keepalive\r\ndata: x\r\n\r\n");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "x");
    }

    #[test]
    fn handles_lone_carriage_return_line_endings() {
        let mut d = SseDecoder::new();
        let mut evts = d.feed(b"data: x\r\rdata: y\r\r");
        evts.extend(d.finish());
        assert_eq!(evts.len(), 2);
        assert_eq!(evts[0].data, "x");
        assert_eq!(evts[1].data, "y");
    }

    #[test]
    fn chunks_split_across_lines() {
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: he").is_empty());
        assert!(d.feed(b"llo\n").is_empty());
        let evts = d.feed(b"\n");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "hello");
    }

    #[test]
    fn finish_flushes_unterminated_frame() {
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: tail").is_empty());
        let evts = d.finish();
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, "tail");
    }

    #[test]
    fn fragmented_large_frame_uses_cursor_without_losing_bytes() {
        let mut d = SseDecoder::new();
        let payload = "x".repeat(12_000);
        let frame = format!("data: {payload}\n\n");
        let bytes = frame.as_bytes();
        let mut evts = Vec::new();
        for chunk in bytes.chunks(700) {
            evts.extend(d.feed(chunk));
        }
        evts.extend(d.finish());
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, payload);
    }

    #[test]
    fn byte_fragmented_frame_scans_linearly() {
        let mut d = SseDecoder::new();
        let payload = "y".repeat(8_000);
        let frame = format!("data: {payload}\n\n");
        let mut evts = Vec::new();
        for byte in frame.as_bytes() {
            evts.extend(d.feed(std::slice::from_ref(byte)));
        }
        evts.extend(d.finish());
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].data, payload);
        // A restarting scan would inspect ~n²/2 bytes. The cursor inspects
        // each byte a small constant number of times (CR/LF lookahead).
        assert!(
            d.bytes_inspected <= frame.len().saturating_mul(3),
            "inspected {} bytes for {}-byte frame",
            d.bytes_inspected,
            frame.len()
        );
    }
}
