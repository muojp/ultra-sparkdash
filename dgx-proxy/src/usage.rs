//! Usage extraction from response bytes as they stream past (dgx-lb's UsageTap / normalise_usage).
//! The tap sees a copy of each frame; it never holds the response back.

use serde_json::Value;

const MAX_JSON: usize = 8 << 20;
const MAX_SSE_LINE: usize = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sse,
    Json,
    Other,
}

pub struct UsageTap {
    mode: Mode,
    buf: Vec<u8>,
    usage: Option<Value>,
    pub saw_done: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
}

impl UsageTap {
    pub fn new(content_type: &str) -> Self {
        let ct = content_type.to_ascii_lowercase();
        let mode = if ct.contains("text/event-stream") {
            Mode::Sse
        } else if ct.contains("json") {
            Mode::Json
        } else {
            Mode::Other
        };
        UsageTap { mode, buf: Vec::new(), usage: None, saw_done: false }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        match self.mode {
            Mode::Other => {}
            Mode::Json => {
                if self.buf.len() < MAX_JSON {
                    self.buf.extend_from_slice(chunk);
                }
            }
            Mode::Sse => {
                self.buf.extend_from_slice(chunk);
                let mut start = 0;
                while let Some(off) = self.buf[start..].iter().position(|&b| b == b'\n') {
                    let line = self.buf[start..start + off].to_vec();
                    start += off + 1;
                    self.line(&line);
                }
                self.buf.drain(..start);
                if self.buf.len() > MAX_SSE_LINE {
                    self.buf.clear(); // a runaway line without newline: give up on it, keep streaming
                }
            }
        }
    }

    fn line(&mut self, line: &[u8]) {
        let line = trim(line);
        let Some(data) = line.strip_prefix(b"data:") else { return };
        let data = trim(data);
        if data == b"[DONE]" {
            self.saw_done = true;
            return;
        }
        if let Ok(ev) = serde_json::from_slice::<Value>(data) {
            self.take(ev);
        }
    }

    fn take(&mut self, ev: Value) {
        let Value::Object(mut ev) = ev else { return };
        if let Some(u @ Value::Object(_)) = ev.get("usage") {
            self.usage = Some(u.clone());
        }
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(ty, "response.completed" | "response.incomplete" | "response.failed") {
            self.saw_done = true;
            if let Some(Value::Object(mut resp)) = ev.remove("response") {
                if let Some(u @ Value::Object(_)) = resp.remove("usage") {
                    self.usage = Some(u);
                }
            }
        }
    }

    pub fn finish(&mut self) -> Usage {
        if self.mode == Mode::Json && !self.buf.is_empty() {
            if let Ok(v) = serde_json::from_slice::<Value>(&self.buf) {
                self.take(v);
            }
            self.buf.clear();
        }
        normalise_usage(self.usage.as_ref())
    }
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// Chat (prompt/completion, completion_tokens_details) and Responses (input/output,
/// output_tokens_details) shapes. A value the server did not report stays None (null).
pub fn normalise_usage(u: Option<&Value>) -> Usage {
    let Some(Value::Object(u)) = u else { return Usage::default() };
    let num = |v: Option<&Value>| v.and_then(Value::as_i64);
    let first = |a: Option<i64>, b: Option<i64>| a.or(b);
    let ctd = u.get("completion_tokens_details");
    let otd = u.get("output_tokens_details");
    Usage {
        prompt_tokens: first(num(u.get("prompt_tokens")), num(u.get("input_tokens"))),
        completion_tokens: first(num(u.get("completion_tokens")), num(u.get("output_tokens"))),
        reasoning_tokens: num(ctd.and_then(|d| d.get("reasoning_tokens")))
            .or(num(otd.and_then(|d| d.get("reasoning_tokens"))))
            .or(num(u.get("reasoning_tokens"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_sse_split_across_frames() {
        let mut t = UsageTap::new("text/event-stream; charset=utf-8");
        let s = b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\ndata: [DONE]\n\n";
        for c in s.chunks(7) {
            t.feed(c);
        }
        assert!(t.saw_done);
        assert_eq!(t.finish(), Usage { prompt_tokens: Some(5), completion_tokens: Some(3), reasoning_tokens: Some(2) });
    }

    #[test]
    fn responses_sse_and_json_and_nulls() {
        let mut t = UsageTap::new("text/event-stream");
        t.feed(b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":9,\"output_tokens_details\":{\"reasoning_tokens\":4}}}}\n\n");
        assert_eq!(t.finish(), Usage { prompt_tokens: Some(20), completion_tokens: Some(9), reasoning_tokens: Some(4) });
        let mut t = UsageTap::new("application/json");
        t.feed(b"{\"usage\":{\"prompt_tokens\":11,");
        t.feed(b"\"completion_tokens\":7,\"completion_tokens_details\":null}}");
        assert_eq!(t.finish(), Usage { prompt_tokens: Some(11), completion_tokens: Some(7), reasoning_tokens: None });
        let mut t = UsageTap::new("text/plain");
        t.feed(b"{\"usage\":{\"prompt_tokens\":1}}");
        assert_eq!(t.finish(), Usage::default());
    }
}
