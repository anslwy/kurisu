//! Telegram Bot API transport: retries with backoff, outage notices, text splitting.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::{clen, log, now_f, take, Bridge, Ctx};

pub const MAX_TG_CHARS: usize = 3900;
pub const MAX_TG_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024;
const NET_RETRY_BUDGET_S: u64 = 1800;
const NET_BACKOFF_START_S: u64 = 2;
pub const NET_BACKOFF_CAP_S: u64 = 30;
const NET_NOTICE_MIN_S: f64 = 60.0;

/// A transient network failure (DNS, connection, timeout): retried, never fatal.
#[derive(Debug)]
pub struct NetError(pub String);

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NetError {}

pub fn is_network_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NetError>().is_some()
}

/// A reqwest error without its URL (the URL holds the bot token).
fn clean(e: reqwest::Error) -> String {
    format!("{}", e.without_url())
}

pub const BOT_COMMANDS: &[(&str, &str)] = &[
    ("list", "Show 5 recent sessions"),
    ("help", "Show bridge help and supported inputs"),
    ("status", "Show the current session status"),
    ("catch_up", "Summarize delegated sessions and blockers"),
    ("model", "Switch the current session model"),
    ("mcp", "List and toggle MCP servers"),
    ("usage", "Show Claude plan usage and reset times"),
    ("send_now", "Send queued messages to the active run"),
    ("compact", "Pick a session and compact its context"),
    ("stop", "Interrupt the current run"),
    ("terminal", "One-time link to a shell in your browser"),
    ("browser", "One-time link to the agents' live browser"),
    ("web", "One-time link to the chat UI: sessions, history and replies"),
    ("id", "Show the current session ID"),
    ("new", "Start a fresh Kurisu session"),
    ("use", "Switch session by ID or prefix"),
];

impl Bridge {
    fn net_failed(&self) {
        let mut n = self.net.lock().unwrap();
        if n.0.is_none() {
            n.0 = Some(now_f());
        }
    }

    fn net_ok(&self) {
        let mut n = self.net.lock().unwrap();
        let Some(start) = n.0.take() else { return };
        let end = now_f();
        if end - start >= NET_NOTICE_MIN_S {
            n.1.push((start, end));
        }
    }

    /// One (start, end) pair covering outages not yet announced.
    pub(crate) fn take_outage(&self) -> Option<(f64, f64)> {
        let mut n = self.net.lock().unwrap();
        if n.1.is_empty() {
            return None;
        }
        let o = (n.1[0].0, n.1[n.1.len() - 1].1);
        n.1.clear();
        Some(o)
    }

    async fn request_once(&self, method: &str, payload: Option<&Value>, timeout: u64) -> Result<Value> {
        let url = format!("{}/bot{}/{}", self.api, self.cfg.token, method);
        let req = match payload {
            Some(p) => self.http.post(&url).json(p),
            None => self.http.get(&url),
        }
        .timeout(Duration::from_secs(timeout));
        let resp = req.send().await.map_err(|e| anyhow!(NetError(clean(e))))?;
        let status = resp.status();
        let raw = resp.text().await.map_err(|e| anyhow!(NetError(clean(e))))?;
        if !status.is_success() {
            bail!("HTTP {}: {}", status.as_u16(), take(&raw, 1000));
        }
        Ok(if raw.is_empty() { json!({}) } else { serde_json::from_str(&raw)? })
    }

    /// Call the Bot API, retrying network errors with backoff. `retry_budget`: total
    /// seconds to keep retrying (default 30 min); 0 means one attempt.
    pub(crate) async fn tg(&self, method: &str, payload: Option<Value>, timeout: u64, retry_budget: Option<u64>) -> Result<Value> {
        let budget = retry_budget.unwrap_or(NET_RETRY_BUDGET_S);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(budget);
        let mut delay = NET_BACKOFF_START_S;
        let result = loop {
            match self.request_once(method, payload.as_ref(), timeout).await {
                Ok(v) => {
                    self.net_ok();
                    break v;
                }
                Err(e) if is_network_error(&e) => {
                    self.net_failed();
                    if tokio::time::Instant::now() + Duration::from_secs(delay) > deadline {
                        return Err(e);
                    }
                    log(format!("telegram {method} failed ({e}); retrying in {delay}s"));
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    delay = (delay * 2).min(NET_BACKOFF_CAP_S);
                }
                Err(e) => return Err(e),
            }
        };
        if result["ok"].as_bool() != Some(true) {
            bail!("Telegram {method} rejected: {}", result["description"].as_str().unwrap_or("None"));
        }
        Ok(result["result"].clone())
    }

    pub(crate) async fn tg1(&self, method: &str, payload: Value) -> Result<Value> {
        self.tg(method, Some(payload), 60, None).await
    }

    pub(crate) async fn answer_callback(&self, callback_id: &Value, text: Option<&str>) {
        let mut p = json!({"callback_query_id": callback_id});
        if let Some(t) = text {
            p["text"] = json!(take(t, 200));
        }
        if let Err(e) = self.tg1("answerCallbackQuery", p).await {
            log(format!("answerCallbackQuery failed: {e}"));
        }
    }

    pub(crate) async fn sync_bot_commands(&self) -> Result<()> {
        let cmds: Vec<Value> = BOT_COMMANDS.iter().map(|(c, d)| json!({"command": c, "description": d})).collect();
        self.tg("setMyCommands", Some(json!({"commands": cmds})), 20, Some(0)).await.map(|_| ())
    }

    fn reply_thread_id(ctx: &Ctx) -> Option<i64> {
        if ctx.scope == "dm" { None } else { ctx.thread_id }
    }

    /// Send (split into Telegram-sized chunks); returns the last message id.
    pub(crate) async fn send_text_with(&self, ctx: &Ctx, chat_id: &Value, text: &str, markup: Option<Value>, retry_budget: Option<u64>) -> Result<Option<i64>> {
        let chunks = split_text(text, MAX_TG_CHARS);
        let thread = Self::reply_thread_id(ctx);
        let mut last = None;
        let n = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut p = json!({"chat_id": chat_id, "text": chunk, "disable_web_page_preview": true});
            if let Some(t) = thread {
                p["message_thread_id"] = json!(t);
            }
            if let (Some(m), true) = (&markup, i == n - 1) {
                p["reply_markup"] = m.clone();
            }
            let r = self.tg("sendMessage", Some(p), 60, retry_budget).await?;
            if let Some(id) = r["message_id"].as_i64() {
                last = Some(id);
            }
        }
        Ok(last)
    }

    pub(crate) async fn send_text(&self, ctx: &Ctx, chat_id: &Value, text: &str) -> Result<Option<i64>> {
        self.send_text_with(ctx, chat_id, text, None, None).await
    }

    pub(crate) async fn send_markup(&self, ctx: &Ctx, chat_id: &Value, text: &str, markup: Option<Value>) -> Result<Option<i64>> {
        self.send_text_with(ctx, chat_id, text, markup, None).await
    }

    /// Best-effort cosmetic notice: one attempt, never fails.
    pub(crate) async fn send_notice(&self, ctx: &Ctx, chat_id: &Value, text: &str) {
        if let Err(e) = self.send_text_with(ctx, chat_id, text, None, Some(0)).await {
            log(format!("notice not sent ({e}): {}", take(text, 40)));
        }
    }

    pub(crate) async fn send_chat_action(&self, ctx: &Ctx, chat_id: &Value, action: &str) {
        let mut p = json!({"chat_id": chat_id, "action": action});
        if let Some(t) = Self::reply_thread_id(ctx) {
            p["message_thread_id"] = json!(t);
        }
        let _ = self.tg("sendChatAction", Some(p), 15, Some(0)).await;
    }

    pub(crate) async fn send_typing(&self, ctx: &Ctx, chat_id: &Value) {
        self.send_chat_action(ctx, chat_id, "typing").await
    }

    /// Edit a placeholder into its final text; send a new message if editing is impossible.
    pub(crate) async fn update_or_send(&self, ctx: &Ctx, chat_id: &Value, message_id: Option<i64>, text: &str) -> Result<Option<i64>> {
        if let (Some(id), true) = (message_id, clen(text) <= MAX_TG_CHARS) {
            match self.tg1("editMessageText", json!({"chat_id": chat_id, "message_id": id, "text": text, "disable_web_page_preview": true})).await {
                Ok(_) => return Ok(Some(id)),
                Err(e) => log(format!("placeholder edit failed: {e}")),
            }
        }
        self.send_text(ctx, chat_id, text).await
    }

    pub(crate) async fn bot_identity(&self) -> Value {
        let have = self.bot_info.lock().unwrap().clone();
        if have["username"].as_str().is_some_and(|u| !u.is_empty()) {
            return have;
        }
        let info = self.tg("getMe", None, 20, None).await.unwrap_or(json!({}));
        *self.bot_info.lock().unwrap() = info.clone();
        info
    }

    pub(crate) async fn bot_mentioned(&self, message: &Value) -> bool {
        let info = self.bot_identity().await;
        let username = info["username"].as_str().unwrap_or("").trim_start_matches('@').to_lowercase();
        let bot_id = info["id"].clone();
        let text = message["text"].as_str().or(message["caption"].as_str()).unwrap_or("");
        let chars: Vec<char> = text.encode_utf16().map(|_| ' ').collect(); // offsets are UTF-16
        let _ = chars;
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let entities = message["entities"].as_array().into_iter().flatten().chain(message["caption_entities"].as_array().into_iter().flatten());
        for e in entities {
            let kind = e["type"].as_str().unwrap_or("");
            if kind == "text_mention" && e["user"]["id"] == bot_id && !bot_id.is_null() {
                return true;
            }
            if kind == "mention" && !username.is_empty() {
                let start = e["offset"].as_u64().unwrap_or(0) as usize;
                let end = start + e["length"].as_u64().unwrap_or(0) as usize;
                if end <= utf16.len() {
                    let piece = String::from_utf16_lossy(&utf16[start..end]);
                    if piece.trim_start_matches('@').to_lowercase() == username {
                        return true;
                    }
                }
            }
        }
        if !username.is_empty() && text.to_lowercase().contains(&format!("@{username}")) {
            return true;
        }
        !bot_id.is_null() && message["reply_to_message"]["from"]["id"] == bot_id
    }

    pub(crate) async fn strip_bot_mention(&self, text: Option<&str>) -> Option<String> {
        let text = text?;
        if text.is_empty() {
            return Some(text.to_string());
        }
        let username = self.bot_identity().await["username"].as_str().unwrap_or("").trim_start_matches('@').to_string();
        if username.is_empty() {
            return Some(text.trim().to_string());
        }
        // re.sub(r"@user\b", "", flags=I), then collapse runs of 2+ whitespace.
        let lower = text.to_lowercase();
        let pat = format!("@{}", username.to_lowercase());
        let mut out = String::new();
        let mut i = 0;
        let bytes = text.as_bytes();
        while i < text.len() {
            if lower[i..].starts_with(&pat) {
                let end = i + pat.len();
                let boundary = end >= text.len() || !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_');
                if boundary {
                    i = end;
                    continue;
                }
            }
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        let mut collapsed = String::new();
        let mut run = String::new();
        for ch in out.chars() {
            if ch.is_whitespace() {
                run.push(ch);
            } else {
                if run.chars().count() >= 2 { collapsed.push(' ') } else { collapsed.push_str(&run) }
                run.clear();
                collapsed.push(ch);
            }
        }
        if run.chars().count() >= 2 { collapsed.push(' ') } else { collapsed.push_str(&run) }
        Some(collapsed.trim().to_string())
    }

    /// Download a Telegram file (bots may download at most 20 MB).
    pub(crate) async fn download_file(&self, file_id: &str) -> Result<(Vec<u8>, String)> {
        let info = self.tg("getFile", Some(json!({"file_id": file_id})), 30, None).await?;
        let file_path = info["file_path"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("Telegram getFile returned no file_path"))?.to_string();
        if let Some(n) = info["file_size"].as_u64() {
            if n as usize > MAX_TG_DOWNLOAD_BYTES {
                bail!("file is {:.1} MB; Telegram bots can download at most 20 MB", n as f64 / 1048576.0);
            }
        }
        let url = format!("{}/file/bot{}/{}", self.api, self.cfg.token, file_path);
        let resp = self.http.get(&url).timeout(Duration::from_secs(120)).send().await.map_err(|e| anyhow!(NetError(clean(e))))?;
        if !resp.status().is_success() {
            bail!("Telegram file download failed: HTTP {}", resp.status().as_u16());
        }
        let data = resp.bytes().await.map_err(|e| anyhow!(NetError(clean(e))))?;
        if data.len() > MAX_TG_DOWNLOAD_BYTES {
            bail!("file exceeds the 20 MB Telegram bot download limit");
        }
        Ok((data.to_vec(), file_path))
    }
}

/// Split at a newline (or space) in the second half of each chunk, as the bridge does.
pub fn split_text(text: &str, limit: usize) -> Vec<String> {
    let mut text: Vec<char> = text.trim().chars().collect();
    let mut chunks = vec![];
    while text.len() > limit {
        let rfind = |c: char| text[..limit].iter().rposition(|&x| x == c).map(|i| i as isize).unwrap_or(-1);
        let mut at = rfind('\n');
        if at < (limit / 2) as isize {
            at = rfind(' ');
        }
        if at < (limit / 2) as isize {
            at = limit as isize;
        }
        let at = at as usize;
        chunks.push(text[..at].iter().collect::<String>().trim_end().to_string());
        let rest: String = text[at..].iter().collect();
        text = rest.trim_start().chars().collect();
    }
    if !text.is_empty() {
        chunks.push(text.into_iter().collect());
    }
    chunks
}

pub fn outage_notice_text(outage: (f64, f64)) -> String {
    let hm = |t: f64| chrono::DateTime::from_timestamp(t as i64, 0).map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default();
    format!("⚠️ Connection was down {}–{}; resend anything unanswered.", hm(outage.0), hm(outage.1))
}

/// Prefix a reply/quote excerpt, as the bridge's quoted_message_text.
pub fn quoted_message_text(message: &Value, text: &str) -> String {
    let mut excerpt = message["quote"]["text"].as_str().map(String::from);
    if excerpt.as_deref().map(str::trim).unwrap_or("").is_empty() {
        excerpt = message["reply_to_message"]["text"].as_str().or(message["reply_to_message"]["caption"].as_str()).map(String::from);
    }
    let Some(ex) = excerpt.filter(|e| !e.trim().is_empty()) else { return text.to_string() };
    let mut ex = ex.trim().to_string();
    if clen(&ex) > 2000 {
        ex = format!("{}…", take(&ex, 1999).trim_end());
    }
    let quoted = ex.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n");
    format!("[Quoted Telegram text]\n{quoted}\n\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_the_bridge() {
        assert_eq!(split_text("  hi  ", 10), ["hi"]);
        let t = format!("{}\n{}", "a".repeat(8), "b".repeat(8));
        assert_eq!(split_text(&t, 10), ["aaaaaaaa", "bbbbbbbb"]);
        assert_eq!(split_text(&"x".repeat(25), 10), ["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx"]);
        assert_eq!(split_text("aaaa bbbbbbbbbbb", 10), ["aaaa bbbbb", "bbbbbb"]);
    }

    #[test]
    fn quotes_replies() {
        let m = json!({"reply_to_message": {"text": "line1\n\nline3"}});
        assert_eq!(quoted_message_text(&m, "ok"), "[Quoted Telegram text]\n> line1\n>\n> line3\n\nok");
        assert_eq!(quoted_message_text(&json!({}), "ok"), "ok");
    }
}
