//! Platform-independent channel contract. Legacy Telegram presentation stays in its adapter.
pub mod dispatch;
pub mod state;
pub mod stream;
pub mod text;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, path::Path, pin::Pin, sync::Arc};

pub type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug)]
pub struct Uncertain;
impl std::fmt::Display for Uncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transport acceptance is uncertain; automatic resend refused")
    }
}
impl std::error::Error for Uncertain {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Address {
    pub channel: String,
    pub destination: String,
    pub person: String,
}

/// All IDs remain opaque: Discord snowflakes must never be narrowed to i64.
pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    fn redact(&self, text: &str) -> String {
        text.to_string()
    }
    fn send<'a>(&'a self, to: &'a Address, text: &'a str, nonce: &'a str) -> Call<'a, String>;
    fn edit<'a>(&'a self, to: &'a Address, id: &'a str, text: &'a str) -> Call<'a, ()>;
    fn file<'a>(
        &'a self,
        to: &'a Address,
        path: &'a Path,
        caption: &'a str,
        nonce: &'a str,
    ) -> Call<'a, Value>;
    fn download<'a>(&'a self, url: &'a str, max: usize) -> Call<'a, Vec<u8>>;
    /// Edit a persistent prompt and its buttons together. Legacy adapters need not implement it.
    fn controls<'a>(
        &'a self,
        _to: &'a Address,
        _message: &'a str,
        _text: &'a str,
        _buttons: Vec<(String, String)>,
    ) -> Call<'a, ()> {
        Box::pin(async { anyhow::bail!("This channel does not support prompt buttons") })
    }
    fn choices<'a>(
        &'a self,
        to: &'a Address,
        message: &'a str,
        options: Vec<(String, String)>,
    ) -> Call<'a, ()>;
}

/// The existing server dispatcher is the common backend for every chat adapter.
pub async fn op(server: &Arc<crate::server::Server>, request: Value) -> Result<Value> {
    server.handle_pub(request).await
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn command(text: &str) -> Option<(String, &str)> {
    let (name, args) = text.trim().split_once(' ').unwrap_or((text.trim(), ""));
    name.starts_with('/').then(|| {
        (
            name.split('@').next().unwrap_or("").to_lowercase(),
            args.trim(),
        )
    })
}

/// The `/list` rendering of a session's status note (`set_my_status`), or `None`
/// when the note is empty. Squashed to one line and truncated to 90 chars.
pub(crate) fn note_line(note: &str) -> Option<String> {
    let squashed = note.split_whitespace().collect::<Vec<_>>().join(" ");
    if squashed.is_empty() {
        return None;
    }
    let text = if squashed.chars().count() > 90 {
        let mut cut: String = squashed.chars().take(89).collect();
        cut.push('…');
        cut
    } else {
        squashed
    };
    Some(format!("📝 {text}"))
}

#[cfg(test)]
mod tests {
    use super::note_line;

    #[test]
    fn note_line_skips_empty_notes() {
        assert_eq!(note_line(""), None);
        assert_eq!(note_line("  \n\t "), None);
    }

    #[test]
    fn note_line_squashes_to_one_line() {
        assert_eq!(note_line("discord channel\n  enabling live   Discord"), Some("📝 discord channel enabling live Discord".into()));
    }

    #[test]
    fn note_line_truncates_long_notes() {
        let long = "x".repeat(200);
        let line = note_line(&long).unwrap();
        assert_eq!(line.chars().count(), 2 + 90);
        assert!(line.ends_with('…'), "{line}");
    }

    #[test]
    fn note_line_keeps_short_notes_verbatim() {
        assert_eq!(note_line(" reading argon-brief.md "), Some("📝 reading argon-brief.md".into()));
    }
}
