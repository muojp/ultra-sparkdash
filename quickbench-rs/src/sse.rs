//! Incremental SSE parsing for OpenAI-compatible chat streams.
//!
//! Split out because it is the part that decides whether a row has numbers at all: vLLM emits
//! `reasoning` deltas, SGLang emits `reasoning_content`, and a model that thinks before it speaks
//! sends nothing else until it does. Reading only one key loses time-to-first-token entirely.

use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct Usage {
    pub completion: u64,
    pub prompt: u64,
    pub reasoning: u64,
    pub cached: u64,
}

#[derive(Debug, Default)]
pub struct StreamState {
    pub saw_first_token: bool,
    pub text: String,
    pub reasoning_text: String,
    pub finish: Option<String>,
    pub usage: Option<Usage>,
}

pub enum Event {
    /// A delta that produced visible progress; true when it is the first one.
    Token { first: bool },
    Other,
    Done,
}

impl StreamState {
    /// Feed one `data:` line. Returns what it meant, so the caller can stamp TTFT itself.
    pub fn feed(&mut self, line: &str, keep_text: bool) -> Event {
        let Some(body) = line.strip_prefix("data: ") else {
            return Event::Other;
        };
        let body = body.trim();
        if body == "[DONE]" {
            return Event::Done;
        }
        let Ok(v) = serde_json::from_str::<Value>(body) else {
            return Event::Other;
        };
        let mut produced = false;
        if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
            for ch in choices {
                let delta = ch.get("delta").cloned().unwrap_or(Value::Null);
                let content = delta.get("content").and_then(|x| x.as_str()).unwrap_or("");
                let think = delta
                    .get("reasoning")
                    .and_then(|x| x.as_str())
                    .or_else(|| delta.get("reasoning_content").and_then(|x| x.as_str()))
                    .unwrap_or("");
                if !content.is_empty() || !think.is_empty() {
                    produced = true;
                    if keep_text {
                        if !content.is_empty() {
                            self.text.push_str(content);
                        } else {
                            self.reasoning_text.push_str(think);
                        }
                    }
                }
                if let Some(f) = ch.get("finish_reason").and_then(|x| x.as_str()) {
                    self.finish = Some(f.to_string());
                }
            }
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            let det = u.get("completion_tokens_details");
            let pdet = u.get("prompt_tokens_details");
            self.usage = Some(Usage {
                completion: u.get("completion_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                prompt: u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                reasoning: det
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                cached: pdet
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
            });
        }
        if produced {
            let first = !self.saw_first_token;
            self.saw_first_token = true;
            Event::Token { first }
        } else {
            Event::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_token(line: &str) -> bool {
        let mut s = StreamState::default();
        matches!(s.feed(line, false), Event::Token { first: true })
    }

    #[test]
    fn content_delta_is_a_token() {
        assert!(first_token(r#"data: {"choices":[{"delta":{"content":"x"}}]}"#));
    }

    #[test]
    fn both_reasoning_key_spellings_count() {
        assert!(first_token(r#"data: {"choices":[{"delta":{"reasoning":"x"}}]}"#));
        assert!(first_token(r#"data: {"choices":[{"delta":{"reasoning_content":"x"}}]}"#));
    }

    #[test]
    fn empty_content_with_role_is_not_a_token() {
        assert!(!first_token(r#"data: {"choices":[{"delta":{"role":"assistant","content":""}}]}"#));
    }

    #[test]
    fn usage_and_finish_reason_are_captured() {
        let mut s = StreamState::default();
        s.feed(r#"data: {"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"completion_tokens":7,"prompt_tokens":11,"completion_tokens_details":{"reasoning_tokens":3},"prompt_tokens_details":{"cached_tokens":5}}}"#, false);
        let u = s.usage.expect("usage");
        assert_eq!((u.completion, u.prompt, u.reasoning, u.cached), (7, 11, 3, 5));
        assert_eq!(s.finish.as_deref(), Some("length"));
    }

    #[test]
    fn text_and_reasoning_are_kept_apart() {
        let mut s = StreamState::default();
        s.feed(r#"data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#, true);
        s.feed(r#"data: {"choices":[{"delta":{"content":"answer"}}]}"#, true);
        assert_eq!(s.text, "answer");
        assert_eq!(s.reasoning_text, "thinking");
    }

    #[test]
    fn done_and_garbage_are_survivable() {
        let mut s = StreamState::default();
        assert!(matches!(s.feed("data: [DONE]", false), Event::Done));
        assert!(matches!(s.feed("data: {not json", false), Event::Other));
        assert!(matches!(s.feed(": keep-alive", false), Event::Other));
    }
}
