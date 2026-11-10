//! Telegram Bot API transport: retries with backoff, outage notices, text splitting.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::{clen, log, now_f, take, Bridge, Ctx};

pub const MAX_TG_CHARS: usize = 3900;
pub const MAX_TG_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024;
/// Telegram's own caps for files a bot uploads: 10 MB as a photo, 50 MB as a document.
pub const MAX_TG_PHOTO_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_TG_FILE_BYTES: u64 = 50 * 1024 * 1024;
const NET_RETRY_BUDGET_S: u64 = 1800;
const NET_BACKOFF_START_S: u64 = 2;
pub const NET_BACKOFF_CAP_S: u64 = 30;
const NET_NOTICE_MIN_S: f64 = 60.0;

/// A transient network failure (DNS, connection, timeout).
///
/// `1` is whether the request can be retried safely: true only when it
/// definitely never reached Telegram. A timeout or a body read error is
/// ambiguous - the message may already be delivered - and retrying one of
/// those is how the same bubble arrives twice.
#[derive(Debug)]
pub struct NetError(pub String, pub bool);

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NetError {}

pub fn is_network_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NetError>().is_some()
}

/// True when a failed call definitely did not deliver anything, so its relay
/// claim may be released for another path. Ambiguous failures keep the claim.
pub(crate) fn failed_before_delivery(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<NetError>() {
        Some(n) => n.1,
        None => true,
    }
}

fn safe_to_retry(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NetError>().is_some_and(|n| n.1)
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
    ("schedule", "List pending wakes, with Cancel buttons"),
    ("version", "Show running Kurisu server build"),
    ("usage", "Show installed CLI usage and reset times"),
    ("send_now", "Send queued messages to the active run"),
    ("compact", "Pick a session and compact its context"),
    ("stop", "Interrupt the current run"),
    ("undo", "Take back the last turn (files, history, messages)"),
    ("mode", "Permission mode of the current session"),
    ("terminal", "One-time link to a shell in your browser"),
    ("browser", "One-time link to the agents' live browser"),
    ("web", "One-time link to the chat UI: sessions, history and replies"),
    ("id", "Show the current session ID"),
    ("new", "Start a fresh Kurisu session"),
    ("use", "Switch session by ID or prefix"),
];

/// How Telegram will take one file: a photo (jpg/png) or a document, plus its size cap.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Upload {
    pub method: &'static str,
    pub field: &'static str,
    pub mime: &'static str,
    pub photo: bool,
    pub bytes: u64,
    pub limit: u64,
}

/// Classify a file for Telegram and enforce its own upload cap, with a readable error.
pub(crate) fn upload_kind(path: &Path) -> Result<Upload> {
    let meta = std::fs::metadata(path).map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let (method, field, mime, photo) = match ext.as_str() {
        "jpg" | "jpeg" => ("sendPhoto", "photo", "image/jpeg", true),
        "png" => ("sendPhoto", "photo", "image/png", true),
        _ => ("sendDocument", "document", "application/octet-stream", false),
    };
    let limit = if photo { MAX_TG_PHOTO_BYTES } else { MAX_TG_FILE_BYTES };
    let bytes = meta.len();
    if bytes > limit {
        let mb = |n: u64| format!("{:.1} MB", n as f64 / (1024.0 * 1024.0));
        bail!("{} is {}; Telegram accepts at most {} as a {} (jpg/png go as photos, everything else as documents); \
shrink it or split it first", path.display(), mb(bytes), mb(limit), if photo { "photo" } else { "document" });
    }
    Ok(Upload { method, field, mime, photo, bytes, limit })
}

/// A multipart/form-data body: text fields in order, then one file part.
pub(crate) fn multipart_body(boundary: &str, fields: &[(String, String)], file_field: &str, filename: &str,
                             mime: &str, bytes: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(bytes.len() + 1024);
    for (name, value) in fields {
        b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    // A filename goes in a quoted string: keep it to one simple token.
    let safe: String = filename.chars().map(|c| if c == '"' || c == '\\' || c == '\r' || c == '\n' { '_' } else { c }).collect();
    b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{safe}\"\r\nContent-Type: {mime}\r\n\r\n").as_bytes());
    b.extend_from_slice(bytes);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

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
        let resp = req.send().await.map_err(|e| { let safe = e.is_connect(); anyhow!(NetError(clean(e), safe)) })?;
        let status = resp.status();
        // The request is on the wire now: anything that fails from here on is
        // ambiguous, because Telegram may have accepted it already.
        let raw = resp.text().await.map_err(|e| anyhow!(NetError(clean(e), false)))?;
        if !status.is_success() {
            bail!("HTTP {}: {}", status.as_u16(), take(&raw, 1000));
        }
        Ok(if raw.is_empty() { json!({}) } else {
            serde_json::from_str(&raw).map_err(|e| anyhow!(NetError(format!("bad response body: {e}"), false)))?
        })
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
                    if !safe_to_retry(&e) {
                        log(format!("telegram {method} failed ({e}); not retrying: the send may have reached Telegram"));
                        return Err(e);
                    }
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

    /// Remember a bot message sent for an inbound update, so /undo can delete it.
    pub(crate) async fn note_sent(&self, ctx: &Ctx, chat_id: &Value, message_id: i64) {
        let Some(u) = ctx.update_id else { return };
        let chat = super::vstr(chat_id);
        if let Err(e) = self.server.with_store(|st| st.tg_record_sent(&chat, u, message_id, super::now_i())).await {
            log(format!("could not remember sent message {message_id}: {e:#}"));
        }
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
            if clen(&chunk) > MAX_TG_CHARS {
                log(format!("WARN telegram oversized_atomic_token chat_id={chat_id} chars={} limit={MAX_TG_CHARS}; sending alone unbroken", clen(&chunk)));
            }
            let r = self.deliver_text_payload(chat_id, &p, clen(&chunk), retry_budget).await?;
            if let Some(id) = r["message_id"].as_i64() {
                last = Some(id);
                self.note_sent(ctx, chat_id, id).await;
            }
            // What the owner's chat shows, kept for telegram_history.
            if self.is_owner_dm(chat_id) {
                super::history::agent(&self.server, &self.cfg.chat_id, ctx.update_id, &chunk, super::now_i()).await;
            }
        }
        Ok(last)
    }

    /// Retry a failed text chunk once, then send a bounded fallback directly
    /// through the transport (never recursively through send_text_with).
    async fn deliver_text_payload(&self, chat_id: &Value, payload: &Value, chars: usize, retry_budget: Option<u64>) -> Result<Value> {
        let first = match self.tg("sendMessage", Some(payload.clone()), 60, retry_budget).await {
            Ok(reply) => return Ok(reply),
            Err(error) => error,
        };
        log(format!("ERROR telegram_delivery_failure chat_id={chat_id} chars={chars} attempt=1 error={first}"));
        match self.tg("sendMessage", Some(payload.clone()), 60, Some(0)).await {
            Ok(reply) => {
                log(format!("telegram_delivery_recovered chat_id={chat_id} chars={chars} retries=1"));
                Ok(reply)
            }
            Err(error) => {
                log(format!("ERROR telegram_undelivered chat_id={chat_id} chars={chars} attempts=2 error={error} metric=telegram_undelivered_total+1"));
                let notice = format!("could not deliver reply, {chars} chars, error {}", take(&error.to_string(), 240));
                let mut fallback = json!({"chat_id": chat_id, "text": notice});
                if let Some(thread) = payload.get("message_thread_id") { fallback["message_thread_id"] = thread.clone(); }
                match self.tg("sendMessage", Some(fallback), 60, Some(0)).await {
                    Ok(_) => log(format!("telegram_delivery_fallback_sent chat_id={chat_id} chars={chars}")),
                    Err(e) => log(format!("ERROR telegram_delivery_fallback_failed chat_id={chat_id} chars={chars} error={e}")),
                }
                Err(error)
            }
        }
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

    /// Upload one local file to a chat: sendPhoto for jpg/png, sendDocument otherwise.
    /// The multipart body is built by hand so no extra reqwest feature is needed.
    pub(crate) async fn send_upload(&self, ctx: &Ctx, chat_id: &Value, path: &Path, caption: Option<&str>) -> Result<Value> {
        let up = upload_kind(path)?;
        let bytes = tokio::fs::read(path).await.map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let boundary = format!("kurisu{}", super::token(24));
        let mut fields: Vec<(String, String)> = vec![("chat_id".into(), match chat_id {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        })];
        if let Some(t) = Self::reply_thread_id(ctx) {
            fields.push(("message_thread_id".into(), t.to_string()));
        }
        // Telegram captions are one message: 1024 characters.
        if let Some(c) = caption.map(str::trim).filter(|c| !c.is_empty()) {
            fields.push(("caption".into(), take(c, 1024)));
        }
        let body = multipart_body(&boundary, &fields, up.field, &name, up.mime, &bytes);
        let url = format!("{}/bot{}/{}", self.api, self.cfg.token, up.method);
        let ctype = format!("multipart/form-data; boundary={boundary}");
        let result = self.upload_retry(&url, &ctype, &body).await?;
        Ok(json!({"kind": if up.photo { "photo" } else { "document" }, "bytes": up.bytes, "message_id": result["message_id"]}))
    }

    /// POST a prepared multipart body, retrying network errors with the same backoff as `tg`.
    async fn upload_retry(&self, url: &str, ctype: &str, body: &[u8]) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(NET_RETRY_BUDGET_S);
        let mut delay = NET_BACKOFF_START_S;
        let result = loop {
            let sent = self.http.post(url).header(reqwest::header::CONTENT_TYPE, ctype).body(body.to_vec())
                .timeout(Duration::from_secs(300)).send().await.map_err(|e| { let safe = e.is_connect(); anyhow!(NetError(clean(e), safe)) });
            let outcome = match sent {
                Ok(resp) => {
                    let status = resp.status();
                    match resp.text().await {
                        Ok(raw) if status.is_success() => Ok(if raw.is_empty() { json!({}) } else {
                            serde_json::from_str(&raw).map_err(|e| anyhow!(NetError(format!("bad response body: {e}"), false)))?
                        }),
                        Ok(raw) => Err(anyhow!("HTTP {}: {}", status.as_u16(), take(&raw, 1000))),
                        Err(e) => Err(anyhow!(NetError(clean(e), false))),
                    }
                }
                Err(e) => Err(e),
            };
            match outcome {
                Ok(v) => { self.net_ok(); break v; }
                Err(e) if is_network_error(&e) => {
                    self.net_failed();
                    if !safe_to_retry(&e) {
                        log(format!("telegram upload failed ({e}); not retrying: the send may have reached Telegram"));
                        return Err(e);
                    }
                    if tokio::time::Instant::now() + Duration::from_secs(delay) > deadline {
                        return Err(e);
                    }
                    log(format!("telegram upload failed ({e}); retrying in {delay}s"));
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    delay = (delay * 2).min(NET_BACKOFF_CAP_S);
                }
                Err(e) => return Err(e),
            }
        };
        if result["ok"].as_bool() != Some(true) {
            bail!("Telegram rejected the file: {}", result["description"].as_str().unwrap_or("None"));
        }
        Ok(result["result"].clone())
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
        let resp = self.http.get(&url).timeout(Duration::from_secs(120)).send().await.map_err(|e| { let safe = e.is_connect(); anyhow!(NetError(clean(e), safe)) })?;
        if !resp.status().is_success() {
            bail!("Telegram file download failed: HTTP {}", resp.status().as_u16());
        }
        let data = resp.bytes().await.map_err(|e| anyhow!(NetError(clean(e), false)))?;
        if data.len() > MAX_TG_DOWNLOAD_BYTES {
            bail!("file exceeds the 20 MB Telegram bot download limit");
        }
        Ok((data.to_vec(), file_path))
    }
}

pub(crate) use crate::channels::text::text_boundaries;
pub use crate::channels::text::split_text;

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
        assert_eq!(split_text(&"x".repeat(25), 10), ["x".repeat(25)]);
        assert_eq!(split_text("aaaa bbbbbbbbbbb", 10), ["aaaa", "bbbbbbbbbbb"]);
    }

    #[test]
    fn atomic_urls_code_fences_links_and_words() {
        let url = format!("https://accounts.google.com/o/oauth2/auth?scope={}", "x".repeat(850));
        let text = format!("{} {url} done", "word ".repeat(100));
        let chunks = split_text(&text, 1000);
        assert!(chunks.iter().any(|c| c.contains(&url)));
        assert!(chunks.iter().all(|c| clen(c) <= 1000));
        for token in [format!("`{} hash with spaces`", "a".repeat(80)),
                      format!("```rust\n{}\n```", "let x = 42;\n".repeat(5)),
                      "[link](https://example.com/a path (nested) more)".to_string()] {
            let chunks = split_text(&format!("{} {token} tail", "prose ".repeat(10)), 100);
            assert!(chunks.iter().any(|c| c.contains(&token)), "{chunks:?}");
        }
        let prose = "one two three\n\nfour five six\nseven eight nine ten";
        assert_eq!(split_text(prose, 30)[0], "one two three");
        assert!(split_text(prose, 30).iter().all(|c| clen(c) <= 30));
        for prefix in ["http://", "https://", "ftp://", "www.", ""] {
            let token = format!("{prefix}{}", "z".repeat(MAX_TG_CHARS + 1));
            assert_eq!(split_text(&format!("before {token} after"), MAX_TG_CHARS), ["before", &token, "after"]);
        }
        assert_eq!(split_text("ééé ééé", 5), ["ééé", "ééé"]);
    }

    #[test]
    fn only_failures_that_never_left_the_machine_may_be_retried() {
        // A connection error definitely did not reach Telegram: retry/release.
        let refused: anyhow::Error = anyhow!(NetError("connection refused".into(), true));
        // A timeout or body read error may have delivered the message: never
        // retry it, and keep the relay claim so no other path sends it either.
        let timed_out: anyhow::Error = anyhow!(NetError("timed out".into(), false));
        // A rejected call is a response, so it definitely sent nothing.
        let rejected: anyhow::Error = anyhow!("HTTP 400: bad request");
        assert!(safe_to_retry(&refused) && failed_before_delivery(&refused));
        assert!(!safe_to_retry(&timed_out) && !failed_before_delivery(&timed_out));
        assert!(failed_before_delivery(&rejected));
    }

    #[test]
    fn quotes_replies() {
        let m = json!({"reply_to_message": {"text": "line1\n\nline3"}});
        assert_eq!(quoted_message_text(&m, "ok"), "[Quoted Telegram text]\n> line1\n>\n> line3\n\nok");
        assert_eq!(quoted_message_text(&json!({}), "ok"), "ok");
    }

    #[test]
    fn uploads_are_photos_for_jpg_png_and_documents_for_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, bytes: u64| {
            let p = dir.path().join(name);
            std::fs::File::create(&p).unwrap().set_len(bytes).unwrap();
            p
        };
        let j = upload_kind(&mk("shot.JPG", 5)).unwrap();
        assert_eq!((j.method, j.field, j.mime, j.photo, j.bytes), ("sendPhoto", "photo", "image/jpeg", true, 5));
        let p = upload_kind(&mk("shot.png", 5)).unwrap();
        assert_eq!((p.method, p.mime), ("sendPhoto", "image/png"));
        let d = upload_kind(&mk("report.pdf", 5)).unwrap();
        assert_eq!((d.method, d.field, d.mime, d.photo), ("sendDocument", "document", "application/octet-stream", false));
        // Limits: 10 MB as a photo, 50 MB as a document, with the kind named in the message.
        let e = upload_kind(&mk("huge.jpg", MAX_TG_PHOTO_BYTES + 1)).unwrap_err().to_string();
        assert!(e.contains("10.0 MB") && e.contains("as a photo"), "{e}");
        let e = upload_kind(&mk("huge.bin", MAX_TG_FILE_BYTES + 1)).unwrap_err().to_string();
        assert!(e.contains("50.0 MB") && e.contains("as a document"), "{e}");
        assert!(upload_kind(&mk("ok.bin", MAX_TG_PHOTO_BYTES)).is_ok(), "10 MB is still a valid document");
        let e = upload_kind(dir.path()).unwrap_err().to_string();
        assert!(e.contains("not a regular file"), "{e}");
    }

    #[test]
    fn multipart_body_has_the_fields_then_the_file() {
        let fields = vec![("chat_id".to_string(), "42".to_string()), ("caption".to_string(), "hi".to_string())];
        let b = String::from_utf8_lossy(&multipart_body("B", &fields, "photo", "a\"b.jpg", "image/jpeg", b"JPEG")).into_owned();
        assert!(b.starts_with("--B\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n42\r\n"), "{b}");
        assert!(b.contains("name=\"caption\"\r\n\r\nhi\r\n"), "{b}");
        assert!(b.contains("name=\"photo\"; filename=\"a_b.jpg\"\r\nContent-Type: image/jpeg\r\n\r\nJPEG\r\n"), "{b}");
        assert!(b.ends_with("--B--\r\n"), "{b}");
    }
}
