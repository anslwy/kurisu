//! Token usage from a Claude Code transcript (`~/.claude/projects/.../<id>.jsonl`).
//! Assistant entries repeat the same `message.id` once per content block, so
//! usage is counted once per message id.

use std::collections::HashSet;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Usage {
    pub messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub models: Vec<String>,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.messages += other.messages;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        for m in &other.models {
            if !self.models.contains(m) {
                self.models.push(m.clone());
            }
        }
    }
}

pub fn from_transcript(text: &str) -> Usage {
    let mut u = Usage::default();
    let mut seen = HashSet::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let msg = &v["message"];
        let id = msg.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        let n = |k: &str| msg["usage"].get(k).and_then(Value::as_u64).unwrap_or(0);
        u.messages += 1;
        u.input_tokens += n("input_tokens");
        u.output_tokens += n("output_tokens");
        u.cache_creation_input_tokens += n("cache_creation_input_tokens");
        u.cache_read_input_tokens += n("cache_read_input_tokens");
        if let Some(m) = msg.get("model").and_then(Value::as_str) {
            if !u.models.iter().any(|x| x == m) {
                u.models.push(m.to_string());
            }
        }
    }
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_each_message_once() {
        let t = concat!(
            r#"{"type":"user","message":{"content":"hi"}}"#, "\n",
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-haiku-5-5","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100}}}"#, "\n",
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-haiku-5-5","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100}}}"#, "\n",
            r#"{"type":"assistant","message":{"id":"m2","model":"claude-haiku-5-5","usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":7}}}"#, "\n",
        );
        let u = from_transcript(t);
        assert_eq!((u.messages, u.input_tokens, u.output_tokens), (2, 11, 7));
        assert_eq!((u.cache_read_input_tokens, u.cache_creation_input_tokens), (100, 7));
        assert_eq!(u.models, ["claude-haiku-5-5"]);
    }
}
