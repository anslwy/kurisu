//! Persistent assistant-message identity, text delta and finished-row reconciliation.
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Relay {
    pub segments: Vec<Segment>,
    pub cursor: i64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Segment {
    pub message: Option<String>,
    pub text: String,
    pub row: Option<i64>,
    pub closed: bool,
}

impl Relay {
    pub fn delta(&mut self, text: &str, message: Option<&str>) {
        if text.is_empty() {
            return;
        }
        let new = self.segments.last().is_none_or(|s| {
            s.closed
                || message
                    .zip(s.message.as_deref())
                    .is_some_and(|(a, b)| a != b)
        });
        if new {
            self.segments.push(Segment {
                message: message.map(str::to_string),
                text: String::new(),
                row: None,
                closed: false,
            });
        }
        self.segments.last_mut().unwrap().text.push_str(text);
    }
    pub fn boundary(&mut self) {
        if let Some(s) = self.segments.last_mut() {
            s.closed = true;
        }
    }
    pub fn row(&mut self, seq: i64, text: &str) {
        if self.segments.iter().any(|s| s.row == Some(seq)) {
            return;
        }
        let text = text.trim();
        if let Some(s) = self.segments.iter_mut().find(|s| {
            s.row.is_none()
                && (s.text.trim() == text
                    || text.starts_with(s.text.trim_end())
                    || s.text.trim().starts_with(text))
        }) {
            s.text = text.to_string();
            s.row = Some(seq);
            s.closed = true;
        } else {
            self.segments.push(Segment {
                message: None,
                text: text.into(),
                row: Some(seq),
                closed: true,
            });
        }
    }
    /// Hold an unfinished URL/inline-code/fence until its boundary or final row.
    pub fn visible(segment: &Segment) -> &str {
        if segment.closed {
            return segment.text.trim();
        }
        let end = super::text::discord_boundaries(&segment.text)
            .last()
            .map(|(i, _)| *i)
            .unwrap_or(0);
        segment.text[..end].trim()
    }
}
