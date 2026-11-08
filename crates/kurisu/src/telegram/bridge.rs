//! Turn relay, queue, commands, attachments and the update loop (Python bridge,
//! Kurisu backend, function for function).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::time::Instant;

use super::backend::{disabled_model, item_text};
use super::tg::{is_network_error, outage_notice_text, quoted_message_text, NET_BACKOFF_CAP_S};
use super::{clen, log, now_f, now_i, squash, take, token, vstr, Bridge, Ctx, Job, JobKind};

pub const TURN_TIMEOUT_S: u64 = 15 * 60;
pub const LIST_PAGE_SIZE: usize = 5;
pub const MODEL_CARD_TTL_S: f64 = 24.0 * 60.0 * 60.0;
pub const MODEL_CARD_LIMIT: usize = 20;
pub const MCP_CARD_LIMIT: usize = 10;
pub const USAGE_STALE_AFTER_S: i64 = 900;
const JOB_RETRY_BUDGET_S: u64 = 7200;
/// Kurisu's own per-attachment limit (the bridge's 7 MB was Omnigent's event limit).
const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;

pub const CATCH_UP_PROMPT: &str = "The user sent /catch_up. Generate a concise, self-contained operational overview of the Kurisu sessions you created or delegated on the user's behalf.

Use the existing conversation context and available read-only session tools (the kurisu-mcp tools peers_list, peer_get, session_status, session_history and board_read) to identify and inspect those sessions. Do not assume that an idle session is unfinished or that a completed session is still running.

Report:
1. Anything requiring the user's attention first: blockers, questions, pending approvals, failed work, and the exact decision or action needed.
2. Completed work and its outcome, including relevant PR/issue links or commits when available.
3. Work still in progress, its current state, and the next expected milestone.

Exclude this catch-up request itself and omit irrelevant old idle sessions. If session attribution or status is uncertain, say so rather than guessing. If nothing requires attention, state that explicitly.

This is a read-only review: do not create, message, resume, stop, or otherwise modify sessions, and do not change code, services, credentials, or settings. Return the overview directly for Telegram.";

const HELP: &str = "Kurisu Telegram bridge

/list — five most recent sessions, with paging
/status — current session status and context usage
/catch-up — summarize delegated sessions and blockers
/model — choose or switch the current session model
/mcp — list MCP servers and switch them on or off
/schedule — pending wakes (scheduled messages), with Cancel buttons
/usage — Claude plan usage (5-hour and 7-day) and reset times
/send-now — send queued messages to the active run
/compact — pick a session to compact (/compact this = current)
/stop — interrupt the run in the current session
/mode — permission mode of the current session (auto, default, …)
/terminal — one-time link to a shell in your browser (/terminal close, /terminal disable)
/browser — one-time link to the agents' live browser (/browser close, /browser disable)
/web — one-time link to the chat UI: sessions, history and replies (/web close, /web disable)
/id — current session ID
/new — start a fresh session
/use <id|prefix> — switch to an existing session

Final replies from every Kurisu session are broadcast here.
Approval prompts appear with Approve / Reject buttons.
Questions appear with one button per option; while one is
pending, your next text message answers it instead.
Send text, photos, or files (PDF, text, code, zip, and
most other types). A caption is sent with the attachment.";

#[derive(Clone, Copy, PartialEq)]
pub enum Announce {
    No,
    Yes,
    Busy,
}

fn busy(status: &str) -> bool {
    matches!(status, "running" | "starting" | "waiting")
}

fn fmt_int(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn fmt_money(x: f64) -> String {
    let cents = (x * 100.0).round() as i64;
    format!("{}.{:02}", fmt_int(cents / 100), (cents % 100).abs())
}

/// Python "{:g}".format(round(x, 1)).
fn fmt_g(x: f64) -> String {
    let r = format!("{x:.1}");
    r.strip_suffix(".0").map(String::from).unwrap_or(r)
}

fn local_fmt(ts: i64, f: &str) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.with_timezone(&chrono::Local).format(f).to_string()).unwrap_or_default()
}

impl Bridge {
    fn reply(&self, ctx: &Ctx) -> Value {
        ctx.chat_id.clone()
    }

    pub(crate) async fn say(&self, ctx: &Ctx, text: &str) -> Result<Option<i64>> {
        self.send_text(ctx, &ctx.chat_id, text).await
    }

    // ------------------------------------------------------------ active sets

    fn act<T>(&self, f: impl FnOnce(&mut super::Active) -> T) -> T {
        f(&mut self.act.lock().unwrap())
    }

    pub(crate) fn is_active(&self, sid: &str) -> bool {
        self.act(|a| a.active.contains(sid))
    }

    fn stop_requested(&self, sid: &str) -> bool {
        self.act(|a| a.stop.contains(sid))
    }

    fn end_active(&self, sid: &str) {
        self.act(|a| {
            a.active.remove(sid);
            a.relaying.remove(sid);
            a.stop.remove(sid);
        })
    }

    fn end_relay(&self, sid: &str) {
        self.act(|a| {
            a.relaying.remove(sid);
            a.stop.remove(sid);
        })
    }

    pub(crate) fn work_pending(&self) -> bool {
        self.queued.load(Ordering::SeqCst) > 0 || self.act(|a| !a.active.is_empty())
    }

    // ------------------------------------------------------------ pending updates

    fn record_pending(&self, update_id: i64, entry: Value) {
        self.with_state(|st| {
            let p = st.entry("pending_updates").or_insert(json!({}));
            if !p.is_object() {
                *p = json!({});
            }
            p[update_id.to_string()] = entry;
        });
        self.save_state();
    }

    pub(crate) fn clear_pending(&self, update_id: Option<i64>) {
        let Some(id) = update_id else { return };
        let removed = self.with_state(|st| st.get_mut("pending_updates").and_then(|p| p.as_object_mut()).and_then(|p| p.remove(&id.to_string())));
        if removed.is_some() {
            self.save_state();
        }
    }

    fn is_pending(&self, update_id: i64) -> bool {
        self.with_state(|st| st.get("pending_updates").and_then(|p| p.get(update_id.to_string())).is_some())
    }

    fn job_unposted(&self, ctx: &Ctx) -> bool {
        ctx.update_id.is_some_and(|id| self.is_pending(id))
    }

    // ------------------------------------------------------------ worker queue

    pub(crate) fn enqueue(&self, job: Job) {
        self.queued.fetch_add(1, Ordering::SeqCst);
        self.jobs.lock().unwrap().push_back(job);
        self.job_notify.notify_one();
    }

    fn drain_queued(&self, pred: impl Fn(&Job) -> bool) -> Vec<Job> {
        let mut q = self.jobs.lock().unwrap();
        let (sel, kept): (Vec<Job>, Vec<Job>) = q.drain(..).partition(|j| pred(j));
        q.extend(kept);
        self.queued.fetch_sub(sel.len(), Ordering::SeqCst);
        sel
    }

    pub(crate) async fn worker_loop(self: Arc<Self>) {
        loop {
            let job = self.jobs.lock().unwrap().pop_front();
            let Some(job) = job else {
                self.job_notify.notified().await;
                continue;
            };
            self.run_job(&job).await;
            self.queued.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn run_job_once(self: &Arc<Self>, job: &Job, immediate: bool) -> Result<()> {
        let ov = job.override_session.clone();
        match &job.kind {
            JobKind::Text { text, ack } => Box::pin(self.handle_text(&job.ctx, text, ov, true, *ack && !immediate, immediate)).await,
            JobKind::Attachment { attachment, caption } => Box::pin(self.handle_attachment(&job.ctx, attachment, caption, ov, immediate)).await,
            JobKind::CatchUp => {
                let content = vec![json!({"type": "input_text", "text": CATCH_UP_PROMPT})];
                Box::pin(self.dispatch_content(&job.ctx, content, ov, true, immediate, Announce::No)).await
            }
        }
    }

    async fn on_job_error(&self, job: &Job, e: &anyhow::Error) {
        let text = match job.kind {
            JobKind::Text { .. } => format!("Bridge error: {e}"),
            JobKind::Attachment { .. } => format!("Attachment failed: {e}"),
            JobKind::CatchUp => format!("Catch-up failed: {e}"),
        };
        if self.say(&job.ctx, &text).await.is_err() {
            log(format!("worker error reply failed: {}", job.label));
        }
    }

    async fn run_job(self: &Arc<Self>, job: &Job) {
        log(format!("worker start: {}", job.label));
        let until = Instant::now() + Duration::from_secs(JOB_RETRY_BUDGET_S);
        let mut delay = 5;
        let err = loop {
            match self.run_job_once(job, false).await {
                Ok(()) => return,
                Err(e) => {
                    // A message that never reached its session is retried, not dropped.
                    if is_network_error(&e) && self.job_unposted(&job.ctx) && Instant::now() < until {
                        log(format!("retrying {} after network error {e} in {delay}s", job.label));
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                        delay = (delay * 2).min(NET_BACKOFF_CAP_S);
                        continue;
                    }
                    break e;
                }
            }
        };
        log(format!("worker failed ({}): {err}", job.label));
        if !is_network_error(&err) {
            self.clear_pending(job.ctx.update_id);
        } else if !self.job_unposted(&job.ctx) {
            log("reply relay interrupted by network; leaving it to the broadcast watcher");
            return;
        }
        self.on_job_error(job, &err).await;
    }

    // ------------------------------------------------------------ relay

    async fn relay_turn_inner(&self, ctx: &Ctx, sid: &str, baseline: Option<String>) -> Result<()> {
        let chat = self.reply(ctx);
        let mut cursor = baseline;
        let (mut sent_any, mut saw_work) = (false, false);
        let mut activity = "working".to_string();
        let started = Instant::now();
        let mut last_item = started;
        let mut last_typing: Option<Instant> = None;
        let mut last_progress = started;
        loop {
            if self.stop_requested(sid) {
                log("turn relay stopped on request");
                return Ok(());
            }
            if last_typing.is_none_or(|t| t.elapsed() >= Duration::from_secs(4)) {
                self.send_typing(ctx, &chat).await;
                last_typing = Some(Instant::now());
            }
            let poll: Result<()> = async {
                loop {
                    let page = self.list_items(sid, cursor.as_deref(), 100, false).await?;
                    let items = page["data"].as_array().cloned().unwrap_or_default();
                    for item in &items {
                        last_item = Instant::now();
                        if item["type"] != "message" || item["role"] != "assistant" {
                            continue;
                        }
                        let text = item_text(item);
                        if !text.is_empty() {
                            self.send_text(ctx, &chat, &text).await?;
                            let id = vstr(&item["id"]);
                            self.mark_relayed(&id);
                            log(format!("relayed assistant item {id} ({} chars)", clen(&text)));
                            sent_any = true;
                            saw_work = true;
                            last_progress = Instant::now();
                        }
                    }
                    if let Some(last) = items.last() {
                        cursor = Some(vstr(&last["id"])).filter(|c| !c.is_empty()).or(cursor.clone());
                    }
                    if page["has_more"] != json!(true) || items.is_empty() {
                        break;
                    }
                }
                Ok(())
            }.await;
            if let Err(e) = poll {
                if is_network_error(&e) {
                    return Err(e);
                }
                log(format!("item poll failed: {e}"));
            }
            let mut status: Option<String> = None;
            match self.session_snapshot(sid).await {
                Ok(snap) => {
                    status = snap["status"].as_str().map(String::from);
                    if snap["activity"]["state"] == "running" {
                        // Kurisu reports the running tool on the session instead of as items.
                        let what = snap["activity"]["title"].as_str().filter(|s| !s.is_empty())
                            .or(snap["activity"]["tool"].as_str().filter(|s| !s.is_empty())).unwrap_or("a tool");
                        activity = format!("running {}", take(what, 110));
                        saw_work = true;
                    }
                }
                Err(e) => log(format!("session status failed: {e}")),
            }
            if status.as_deref().is_some_and(busy) {
                saw_work = true;
            }
            let elapsed = started.elapsed().as_secs();
            if last_progress.elapsed() >= Duration::from_secs(25) && elapsed >= 20 {
                self.send_text(ctx, &chat, &format!("⏳ Still working — {activity} ({elapsed}s)")).await?;
                last_progress = Instant::now();
            }
            let quiet = last_item.elapsed().as_secs_f64();
            let idle = status.as_deref() == Some("idle");
            if sent_any && quiet >= 2.5 && idle {
                log(format!("turn relay ended: status=idle quiet={quiet:.1}s sent=true"));
                return Ok(());
            }
            if sent_any && quiet >= 15.0 && status.is_none() {
                log(format!("turn relay ended without status: quiet={quiet:.1}s sent=true"));
                return Ok(());
            }
            if saw_work && idle && quiet >= 15.0 {
                if !sent_any {
                    self.send_text(ctx, &chat, "The agent finished without a text response.").await?;
                }
                return Ok(());
            }
            if !saw_work && elapsed >= 45 {
                self.send_text(ctx, &chat, "Message was accepted, but the session did not start a turn. Check Kurisu.").await?;
                return Ok(());
            }
            if elapsed >= TURN_TIMEOUT_S {
                self.send_text(ctx, &chat, "Timed out waiting for the agent response (15 minutes).").await?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    }

    async fn relay_turn(&self, ctx: &Ctx, sid: &str, baseline: Option<String>) -> Result<()> {
        self.act(|a| a.relaying.insert(sid.to_string()));
        let r = self.relay_turn_inner(ctx, sid, baseline).await;
        self.end_relay(sid);
        r
    }

    /// Release a message that is waiting for another turn; true if one was waiting.
    fn request_send_now(&self, sid: &str) -> bool {
        self.act(|a| {
            if !a.waiting.contains(sid) {
                return false;
            }
            a.send_now.insert(sid.to_string());
            true
        })
    }

    fn take_send_now(&self, sid: &str) -> bool {
        self.act(|a| a.send_now.remove(sid))
    }

    fn end_waiting(&self, sid: &str) {
        self.act(|a| {
            a.waiting.remove(sid);
            a.send_now.remove(sid);
        })
    }

    /// Wait for a turn the bridge did not start; one acknowledgement ("On it" or "Queued").
    async fn wait_until_idle(&self, ctx: &Ctx, sid: &str, announce: Announce) -> Result<bool> {
        let chat = self.reply(ctx);
        let deadline = Instant::now() + Duration::from_secs(TURN_TIMEOUT_S);
        let mut announced = false;
        while Instant::now() < deadline {
            if self.stop_requested(sid) {
                return Ok(false);
            }
            if self.take_send_now(sid) {
                return Ok(true);
            }
            let status = match self.session_snapshot(sid).await {
                Ok(s) => s["status"].as_str().unwrap_or("").to_string(),
                Err(e) => {
                    log(format!("session status failed while queueing: {e}"));
                    if announce == Announce::Yes && !announced {
                        self.send_text(ctx, &chat, "⏳ On it.").await?;
                    }
                    return Ok(true);
                }
            };
            if status != "running" && status != "starting" {
                if announce == Announce::Yes && !announced {
                    self.send_text(ctx, &chat, "⏳ On it.").await?;
                }
                return Ok(true);
            }
            if !announced {
                if announce != Announce::No {
                    self.send_text(ctx, &chat, "⏳ Queued — the session is finishing another turn first. Tap /send_now to send it now.").await?;
                }
                announced = true;
            }
            for _ in 0..10 {
                if self.stop_requested(sid) {
                    return Ok(false);
                }
                if self.act(|a| a.send_now.contains(sid)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        Ok(false)
    }

    /// Deliver final replies of turns that ended while a message was waiting.
    async fn flush_turn_backlog(&self, ctx: &Ctx, sid: &str, after: Option<String>) -> Result<()> {
        let items = match self.list_items(sid, after.as_deref(), 100, false).await {
            Ok(p) => p["data"].as_array().cloned().unwrap_or_default(),
            Err(e) => {
                log(format!("backlog poll failed: {e}"));
                return Ok(());
            }
        };
        let finals: Vec<(Value, String)> = super::cards::broadcast_final_messages(&items).into_iter().filter(|(i, _)| !self.was_relayed(&vstr(&i["id"]))).collect();
        if finals.is_empty() {
            return Ok(());
        }
        let session = self.session_snapshot(sid).await.unwrap_or(json!({"id": sid}));
        for (item, text) in finals {
            self.send_text(ctx, &self.chat_id(), &format!("{}\n\n{text}", super::cards::broadcast_prefix(&session))).await?;
            self.mark_relayed(&vstr(&item["id"]));
            log(format!("flushed backlog item {} from {}", vstr(&item["id"]), take(sid, 8)));
        }
        Ok(())
    }

    pub(crate) async fn dispatch_content(&self, ctx: &Ctx, content: Vec<Value>, override_session: Option<String>, persist: bool,
                                         send_immediately: bool, announce: Announce) -> Result<()> {
        let _ = persist;
        let sid = match override_session {
            Some(s) => s,
            None => self.ensure_session(ctx).await?,
        };
        if send_immediately {
            // Kurisu types it into the running turn (or runs it right after).
            self.post_message(&sid, &content, true).await?;
            self.clear_pending(ctx.update_id);
            return Ok(());
        }
        self.act(|a| {
            a.active.insert(sid.clone());
            a.waiting.insert(sid.clone());
        });
        let r = async {
            let backlog_after = match self.latest_item_id(&sid).await {
                Ok(x) => Some(x),
                Err(e) => {
                    log(format!("backlog baseline failed: {e}"));
                    None
                }
            };
            if !self.wait_until_idle(ctx, &sid, announce).await? {
                self.clear_pending(ctx.update_id);
                self.say(ctx, "🛑 Stopped before the queued message was sent.").await?;
                return Ok(());
            }
            if self.stop_requested(&sid) {
                self.clear_pending(ctx.update_id);
                return Ok(());
            }
            if let Some(after) = backlog_after {
                self.flush_turn_backlog(ctx, &sid, after).await?;
            }
            let baseline = self.latest_item_id(&sid).await?;
            self.post_message(&sid, &content, false).await?;
            self.clear_pending(ctx.update_id);
            self.end_waiting(&sid);
            if !self.stop_requested(&sid) {
                self.relay_turn(ctx, &sid, baseline).await?;
            }
            Ok(())
        }.await;
        self.end_waiting(&sid);
        self.end_active(&sid);
        r
    }

    // ------------------------------------------------------------ attachments

    fn save_upload(&self, filename: &str, data: &[u8]) -> Result<std::path::PathBuf> {
        let dir = self.upload_dir();
        std::fs::create_dir_all(&dir)?;
        let base = std::path::Path::new(filename).file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let mut safe: String = base.chars().map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
        safe = take(&safe, 120);
        if safe.is_empty() {
            safe = "file".into();
        }
        let path = dir.join(format!("{}-{}-{safe}", chrono::Local::now().format("%Y%m%d-%H%M%S"), token(6)));
        super::write_private(&path, data)?;
        Ok(path)
    }

    async fn handle_attachment(self: &Arc<Self>, ctx: &Ctx, attachment: &Value, caption: &str, override_session: Option<String>, send_immediately: bool) -> Result<()> {
        let fid = attachment["file_id"].as_str().unwrap_or("");
        let (data, file_path) = self.download_file(fid).await?;
        let name = attachment["name"].as_str().filter(|n| !n.is_empty()).map(String::from)
            .unwrap_or_else(|| std::path::Path::new(&file_path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string());
        let mime = attachment["mime"].as_str().filter(|m| !m.is_empty()).map(String::from).unwrap_or_else(|| mime_for_path(&file_path));
        self.deliver_attachment(ctx, &name, &mime, data, caption, override_session, send_immediately).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver_attachment(self: &Arc<Self>, ctx: &Ctx, name: &str, mime: &str, data: Vec<u8>, caption: &str,
                                override_session: Option<String>, send_immediately: bool) -> Result<()> {
        use base64::Engine;
        let caption = caption.trim();
        if mime.starts_with("image/") {
            if data.len() > MAX_IMAGE_BYTES {
                bail!("image is too large ({:.1} MB; Kurisu accepts at most 25 MB)", data.len() as f64 / 1048576.0);
            }
            let mut content = vec![json!({"type": "input_image",
                "image_url": format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&data))})];
            if !caption.is_empty() {
                content.push(json!({"type": "input_text", "text": caption}));
            }
            self.say(ctx, "🖼 Image received.").await?;
            return self.dispatch_content(ctx, content, override_session, true, send_immediately, Announce::Busy).await;
        }
        if is_audio_attachment(name, mime) {
            if let Some(stt) = self.stt_config().await {
                let path = self.save_upload(&audio_name(name, mime), &data)?;
                let started = Instant::now();
                let placeholder = match self.say(ctx, "🎙 Transcribing…").await {
                    Ok(id) => id,
                    Err(e) => {
                        log(format!("transcribing placeholder failed: {e}"));
                        None
                    }
                };
                let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
                let keeper = {
                    let (b, c, chat) = (self.clone(), ctx.clone(), self.reply(ctx));
                    tokio::spawn(async move {
                        loop {
                            b.send_chat_action(&c, &chat, "record_voice").await;
                            if tokio::time::timeout(Duration::from_secs(4), &mut rx).await.is_ok() {
                                break;
                            }
                        }
                    })
                };
                let transcript = crate::stt::transcribe(&stt, &path).await;
                let _ = tx.send(());
                let _ = keeper.await;
                match transcript {
                    Err(e) => {
                        // Quietly pass the audio through as a file (spec 8.1): drop the placeholder.
                        log(format!("stt failed: {e:#}"));
                        if let Some(mid) = placeholder {
                            let _ = self.tg1("deleteMessage", json!({"chat_id": self.reply(ctx), "message_id": mid})).await;
                        }
                    }
                    Ok(t) => {
                        log(format!("voice transcribed in {:.1}s ({} chars)", started.elapsed().as_secs_f64(), clen(&t)));
                        let heard = if t.is_empty() { "(no speech detected)" } else { t.as_str() };
                        self.update_or_send(ctx, &self.reply(ctx), placeholder, &format!("🎙 Heard: {heard}")).await?;
                        let text = voice_transcript_text(name, &path.to_string_lossy(), &t, caption);
                        return self.dispatch_content(ctx, vec![json!({"type": "input_text", "text": text})], override_session, true, send_immediately, Announce::Busy).await;
                    }
                }
            }
        }
        let sid = match &override_session {
            Some(s) => s.clone(),
            None => self.ensure_session(ctx).await?,
        };
        let content = match self.upload_file(&sid, name, &data).await? {
            Some(path) => {
                let mut c = vec![json!({"type": "input_file", "file_id": path, "filename": name})];
                if !caption.is_empty() {
                    c.push(json!({"type": "input_text", "text": caption}));
                }
                self.say(ctx, "📎 File received.").await?;
                c
            }
            None => {
                let path = self.save_upload(name, &data)?;
                let mut text = format!("The user uploaded a file named \"{name}\" ({} bytes, {mime}). It is saved at {}.\nUse your tools to inspect it.",
                                       data.len(), path.display());
                if !caption.is_empty() {
                    text.push_str(&format!("\n\nUser note: {caption}"));
                }
                self.say(ctx, "📎 File received and saved to the workspace.").await?;
                vec![json!({"type": "input_text", "text": text})]
            }
        };
        self.dispatch_content(ctx, content, override_session, true, send_immediately, Announce::Busy).await
    }

    /// The speech-to-text to use, as the bridge's stt_command: `ku stt use/setup` wins,
    /// `ku stt off` disables it, otherwise a working `stt` CLI (checked every 60 s).
    async fn stt_config(&self) -> Option<crate::stt::Config> {
        match crate::stt::load(&self.home) {
            Some(crate::stt::Config::Off) => return None,
            Some(c) => return Some(c),
            None => {}
        }
        {
            let p = self.stt_probe.lock().unwrap();
            if now_f() - p.0 < 60.0 {
                return p.1.clone();
            }
        }
        let found = crate::stt::detect().await;
        *self.stt_probe.lock().unwrap() = (now_f(), found.clone());
        found
    }

    /// /terminal and /browser: a one-time link when on; otherwise an approval card that
    /// turns it on (and then sends the link). `close` and `disable` too. Owner's DM only.
    async fn remote_command(&self, ctx: &Ctx, kind: &str, arg: &str) -> Result<()> {
        let what = kind;
        if !self.is_owner_dm(&ctx.chat_id) {
            self.say(ctx, &format!("/{what} works only in the owner's private chat.")).await?;
            return Ok(());
        }
        match arg.trim().to_lowercase().as_str() {
            "close" => {
                let n = self.k(json!({"op": "remote_close", "kind": kind, "why": format!("/{what} close")})).await.map(|v| v["closed"].as_u64().unwrap_or(0)).unwrap_or(0);
                let noun = match kind { "browser" => "browser view", "web" => "web page", _ => "terminal session" };
                self.say(ctx, &format!("⏹ Closed {n} {noun}{}; open links are dead.", if n == 1 { "" } else { "s" })).await?;
            }
            "disable" | "off" => {
                let r = self.k(json!({"op": "remote_disable", "kind": kind})).await?;
                self.say(ctx, &if r["was_on"] == json!(true) { format!("⏹ /{what} is off; links and sessions are closed. Send /{what} to turn it on again.") }
                              else { format!("/{what} was already off.") }).await?;
            }
            _ => {
                let offer = self.k(json!({"op": "remote_offer", "kind": kind})).await?;
                if offer["enabled"] == json!(true) {
                    return self.send_remote_link(ctx, kind).await;
                }
                match offer["where"].as_str() {
                    None => {
                        self.say(ctx, offer["error"].as_str().unwrap_or(crate::remote::NO_ADDRESS)).await?;
                    }
                    Some(place) => self.post_enable_card(kind, place, offer["lb_installed"] == json!(true)).await?,
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn send_remote_link(&self, ctx: &Ctx, kind: &str) -> Result<()> {
        match self.k(json!({"op": "remote_link", "kind": kind})).await {
            // The link is sent, never logged.
            Ok(v) => {
                let head = match kind { "browser" => "🌐 One-time link to the live browser", "web" => "💬 One-time link to the web chat", _ => "🖥 One-time terminal link" };
                self.say(ctx, &format!("{head} (expires in 5 min). Opening it shows a code and asks for your approval here:\n{}", v["url"].as_str().unwrap_or(""))).await?;
            }
            Err(e) => {
                let what = match kind { "browser" => "Browser", "web" => "Web chat", _ => "Terminal" };
                self.say(ctx, &format!("{what} unavailable: {}", e.to_string().trim_start_matches("HTTP 400: "))).await?;
            }
        }
        Ok(())
    }

    /// /mode [auto|default|acceptEdits|plan]: the current session's permission mode.
    async fn handle_mode_command(&self, ctx: &Ctx, arg: &str) -> Result<()> {
        let sid = self.ensure_session(ctx).await?;
        let want = arg.trim();
        if !want.is_empty() {
            return self.set_mode(ctx, &sid, want, None).await;
        }
        let snap = self.session_snapshot(&sid).await?;
        let cur = snap["permission_mode"].as_str().unwrap_or("default").to_string();
        let title = squash(snap["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)"));
        let text = format!("Permission mode for {title} ({}): {cur}\n\n\
auto — routine actions run; risky ones ask you here\ndefault — asks for most tools\nacceptEdits — file edits run, commands ask\n\
plan — read and plan only\n\nApplies from the session's next start.", take(&sid, 8));
        let buttons: Vec<Value> = ["auto", "default", "acceptEdits", "plan"].iter()
            .map(|m| json!([{"text": format!("{}{m}", if *m == cur { "✓ " } else { "" }), "callback_data": format!("mode:{}:{m}", take(&sid, 36))}])).collect();
        self.send_markup(ctx, &ctx.chat_id, &text, Some(json!({"inline_keyboard": buttons}))).await?;
        Ok(())
    }

    async fn set_mode(&self, ctx: &Ctx, sid: &str, mode: &str, message: Option<&Value>) -> Result<()> {
        if mode == "bypassPermissions" {
            self.say(ctx, "bypassPermissions can only be set on the computer (ku update <name> --mode bypassPermissions).").await?;
            return Ok(());
        }
        let text = match self.k(json!({"op": "update", "name": sid, "fields": {"permission_mode": mode}})).await {
            Ok(_) => format!("✅ Permission mode for {} is now {mode} (from its next start).", take(sid, 8)),
            Err(e) => format!("Mode change failed: {}", e.to_string().trim_start_matches("HTTP 400: ")),
        };
        match message.and_then(|m| m["message_id"].as_i64()) {
            Some(mid) => {
                let _ = self.tg1("editMessageText", json!({"chat_id": ctx.chat_id, "message_id": mid, "text": text, "reply_markup": {"inline_keyboard": []}})).await;
            }
            None => {
                self.say(ctx, &text).await?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ /usage

    async fn handle_usage_command(&self, ctx: &Ctx) -> Result<()> {
        // The same report the kurisu-mcp `usage` tool returns (server op usage_report).
        let text = match self.k(json!({"op": "usage_report"})).await {
            Ok(r) => r["text"].as_str().unwrap_or("").to_string(),
            Err(e) => {
                log(format!("kurisu usage failed: {e}"));
                format_usage(&json!({"confidence": "unavailable", "five_hour": null, "seven_day": null}), None, now_i())
            }
        };
        self.say(ctx, &text).await?;
        Ok(())
    }

    /// "CLIs: claude 2.1.319 ✅ · codex 0.166.0 ✅ · stt ✅ (idle)" (Kurisu's own detection, cached 60 s).
    async fn cli_status_line(&self) -> String {
        let r = self.k(json!({"op": "clis"})).await;
        let mut entries = vec![];
        match &r {
            Ok(v) => {
                for name in ["claude", "codex"] {
                    let c = v["clis"].as_array().into_iter().flatten().find(|c| c["name"] == name);
                    entries.push(match c {
                        Some(c) if c["installed"] == json!(true) => {
                            let ver = short_version(c["version"].as_str().unwrap_or(""));
                            let mut t = format!("{name} {ver}").trim().to_string() + " ✅";
                            if c["logged_in"] == json!(false) {
                                t.push_str(" (not logged in)");
                            }
                            t
                        }
                        _ => format!("{name} ❌"),
                    });
                }
            }
            Err(e) => {
                log(format!("kurisu status failed: {e}"));
                entries.push("claude/codex: Kurisu status unavailable".into());
            }
        }
        let stt = r.as_ref().ok().and_then(|v| v["clis"].as_array().and_then(|a| a.iter().find(|c| c["name"] == "stt").cloned()));
        let off = crate::stt::load(&self.home) == Some(crate::stt::Config::Off);
        entries.push(match stt {
            Some(s) if s["set_up"] == json!(true) && !off => format!("stt ✅ ({})", if s["daemon"] == "running" { "warm" } else { "idle" }),
            _ => "stt ❌".into(),
        });
        format!("CLIs: {}", entries.join(" · "))
    }

    // ------------------------------------------------------------ /list

    async fn sorted_session_rows(&self) -> Result<(Vec<(f64, Option<f64>, Value)>, HashMap<String, Value>)> {
        let sessions = self.fetch_sessions().await?;
        let by_id: HashMap<String, Value> = sessions.iter().filter_map(|s| s["id"].as_str().map(|i| (i.to_string(), s.clone()))).collect();
        let mut rows: Vec<(f64, Option<f64>, Value)> = sessions.into_iter().map(|s| {
            let last_user = s["last_user_at"].as_f64().filter(|x| *x != 0.0);
            let sort = last_user.or(s["updated_at"].as_f64().filter(|x| *x != 0.0)).or(s["created_at"].as_f64().filter(|x| *x != 0.0)).unwrap_or(0.0);
            (sort, last_user, s)
        }).collect();
        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok((rows, by_id))
    }

    async fn list_snapshot(&self, session: &Value) -> Value {
        let id = session["id"].as_str().unwrap_or("");
        match self.session_snapshot(id).await {
            Ok(s) => s,
            Err(e) => {
                log(format!("list snapshot failed for {}: {e}", take(id, 8)));
                session.clone()
            }
        }
    }

    async fn configured_codex_model(&self) -> Option<String> {
        {
            let c = self.codex_model.lock().unwrap();
            if now_f() - c.0 < 60.0 {
                return c.1.clone();
            }
        }
        let mut model = self.default_model("codex").await.filter(|m| m != "default");
        if model.is_none() {
            let path = std::env::var("CODEX_HOME").map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex")).join("config.toml");
            model = std::fs::read_to_string(path).ok().and_then(|t| toml_top_level_string(&t, "model"));
        }
        let model = model.map(|m| m.strip_suffix(":cloud").map(String::from).unwrap_or(m)).filter(|m| !m.is_empty());
        *self.codex_model.lock().unwrap() = (now_f(), model.clone());
        model
    }

    async fn resolved_model(&self, snap: &Value, harness: Option<&str>) -> (Option<String>, String) {
        let mut model = snap["model_override"].as_str().filter(|m| !m.is_empty()).or(snap["llm_model"].as_str().filter(|m| !m.is_empty())).map(String::from);
        if model.as_deref().is_some_and(|m| m.trim().to_lowercase() == "default") {
            model = None;
        }
        if let Some(m) = model {
            let short = short_model(&m);
            return (Some(m), short);
        }
        if harness.is_some_and(|h| h.starts_with("codex")) {
            let real = self.configured_codex_model().await;
            let label = real.as_deref().map(short_model).unwrap_or_else(|| "codex default".into());
            return (real, label);
        }
        (None, "default".into())
    }

    async fn model_effort_label(&self, session: &Value, snap: &Value) -> String {
        let harness = harness_label(snap, Some(session));
        let (_, model) = self.resolved_model(snap, harness.as_deref()).await;
        let effort = snap["reasoning_effort"].as_str().filter(|e| !e.is_empty()).or(session["reasoning_effort"].as_str().filter(|e| !e.is_empty())).unwrap_or("default");
        let label = format!("{model} / {effort}");
        match harness {
            Some(h) => format!("{h} · {label}"),
            None => label,
        }
    }

    fn session_skill_names(&self, workspace: Option<&str>) -> Vec<String> {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut roots = vec![std::path::PathBuf::from(&home).join(".claude/skills")];
        if let Some(w) = workspace.filter(|w| !w.is_empty()) {
            roots.push(std::path::PathBuf::from(w).join(".claude/skills"));
        }
        let stamp: Vec<Option<std::time::SystemTime>> = roots.iter().map(|r| std::fs::metadata(r).and_then(|m| m.modified()).ok()).collect();
        let key = roots.iter().map(|r| r.to_string_lossy().into_owned()).collect::<Vec<_>>().join("|");
        if let Some((s, v)) = self.skills_cache.lock().unwrap().get(&key) {
            if *s == stamp {
                return v.clone();
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for r in &roots {
            for e in std::fs::read_dir(r).into_iter().flatten().flatten() {
                if e.path().join("SKILL.md").is_file() {
                    names.insert(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        let v: Vec<String> = names.into_iter().collect();
        self.skills_cache.lock().unwrap().insert(key, (stamp, v.clone()));
        v
    }

    async fn session_extras_line(&self, snap: &Value) -> String {
        let mut parts = vec![];
        if let Some(c) = estimated_cost(snap) {
            parts.push(format!("💰 ~${} est", fmt_money(c)));
        }
        if let Some(c) = context_label(snap) {
            parts.push(format!("🧠 {c}"));
        }
        if let Some(id) = snap["id"].as_str() {
            parts.push(format!("🔌 {}", short_list(&self.kurisu_mcp_names(id).await, 3)));
        }
        if snap["harness"].as_str().unwrap_or("").starts_with("claude") {
            let skills = self.session_skill_names(snap["workspace"].as_str());
            if !skills.is_empty() {
                parts.push(format!("🛠 {}", short_list(&skills, 3)));
            }
        }
        parts.join(" · ")
    }

    /// /schedule: pending wakes, one Cancel button each.
    async fn schedule_card(&self) -> (String, Option<Value>) {
        match self.k(json!({"op": "wakes"})).await {
            Ok(w) => schedule_text(w.as_array().map(Vec::as_slice).unwrap_or(&[]), now_i()),
            Err(e) => (format!("Schedule failed: {e}"), None),
        }
    }

    pub(crate) async fn session_overview(&self, ctx: &Ctx, page: i64) -> Result<(String, Option<Value>)> {
        let (rows, by_id) = self.sorted_session_rows().await?;
        let current = self.current_session(ctx);
        let total = rows.len();
        let (page, start) = page_bounds(total, page);
        let visible: Vec<&(f64, Option<f64>, Value)> = rows.iter().skip(start).take(LIST_PAGE_SIZE).collect();
        let end = start + visible.len();
        let mut lines = if total > 0 { vec![format!("Recent sessions ({}-{end} of {total})", start + 1), String::new()] } else { vec!["Recent sessions (0)".to_string()] };
        let mut buttons: Vec<Value> = vec![];
        for (i, (_, last_user, session)) in visible.iter().enumerate() {
            let index = start + i + 1;
            let sid = session["id"].as_str().unwrap_or("?").to_string();
            let status = session["status"].as_str().filter(|s| !s.is_empty()).unwrap_or("?").to_string();
            let icon = match status.as_str() { "running" | "starting" => "🟢", "waiting" => "🟡", "failed" => "🔴", _ => "⚪" };
            let mut title = squash(session["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)"));
            if clen(&title) > 50 {
                title = format!("{}…", take(&title, 49));
            }
            let is_current = current.as_deref() == Some(sid.as_str());
            let parent = session["parent_session_id"].as_str().filter(|p| !p.is_empty());
            lines.push(format!("{index}. {icon} {}{title}{}", if parent.is_some() { "↳ " } else { "" }, if is_current { " ✓ current" } else { "" }));
            let age = last_user.map(duration_ago).unwrap_or_else(|| "no user message".into());
            let snap = self.list_snapshot(session).await;
            let mut detail = format!("last user: {age} · {status} · {}", self.model_effort_label(session, &snap).await);
            if let Some(p) = parent {
                let mut pt = squash(by_id.get(p).and_then(|x| x["title"].as_str()).unwrap_or(""));
                if clen(&pt) > 28 {
                    pt = format!("{}…", take(&pt, 27));
                }
                detail.push_str(&format!(" · child of {}", if pt.is_empty() { take(p, 8) } else { pt }));
            }
            if status == "running" || status == "starting" {
                let a = &snap["activity"];
                let what = if a.is_object() {
                    a["title"].as_str().filter(|s| !s.is_empty()).or(a["tool"].as_str()).unwrap_or("").to_string()
                } else {
                    "working".into()
                };
                detail.push_str(&format!(" · {what}"));
            }
            lines.push(format!("   {detail} · {}", take(&sid, 8)));
            if let Some(dir) = session["workspace"].as_str().filter(|d| !d.is_empty()) {
                lines.push(format!("   📁 {}", short_path(dir, std::env::var("HOME").ok().as_deref())));
            }
            let extras = self.session_extras_line(&snap).await;
            if !extras.is_empty() {
                lines.push(format!("   {extras}"));
            }
            buttons.push(json!([{"text": format!("{} {}", if is_current { "✓" } else { "Use" }, take(&title, 26)), "callback_data": format!("use:{sid}")}]));
        }
        let mut nav = vec![];
        if page > 0 {
            nav.push(json!({"text": "⬅️ Newer", "callback_data": format!("list:{}", page - 1)}));
        }
        if end < total {
            nav.push(json!({"text": format!("More ({} left) ➜", total - end), "callback_data": format!("list:{}", page + 1)}));
        }
        if !nav.is_empty() {
            buttons.push(json!(nav));
        }
        if total > 0 {
            buttons.push(json!([{"text": "🔄 Refresh", "callback_data": format!("list:{page}")}]));
        }
        Ok((lines.join("\n"), if buttons.is_empty() { None } else { Some(json!({"inline_keyboard": buttons})) }))
    }

    // ------------------------------------------------------------ /compact

    async fn compact_picker(&self, ctx: &Ctx, page: i64) -> Result<(String, Value)> {
        let (rows, _) = self.sorted_session_rows().await?;
        let current = self.current_session(ctx);
        let total = rows.len();
        let (page, start) = page_bounds(total, page);
        let visible: Vec<&(f64, Option<f64>, Value)> = rows.iter().skip(start).take(LIST_PAGE_SIZE).collect();
        let mut buttons = vec![];
        if let Some(c) = &current {
            buttons.push(json!([{"text": format!("🗜 This session ({})", take(c, 8)), "callback_data": format!("cmp:y:{c}")}]));
        }
        for (_, _, s) in &visible {
            let sid = s["id"].as_str().unwrap_or("");
            if sid.is_empty() {
                continue;
            }
            let title = take(&squash(s["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)")), 28);
            let mark = if current.as_deref() == Some(sid) { "✓ " } else { "" };
            buttons.push(json!([{"text": format!("{mark}{title} · {}", s["status"].as_str().filter(|x| !x.is_empty()).unwrap_or("?")), "callback_data": format!("cmp:a:{sid}")}]));
        }
        let mut nav = vec![];
        if page > 0 {
            nav.push(json!({"text": "⬅️ Newer", "callback_data": format!("cmp:p:{}", page - 1)}));
        }
        if start + visible.len() < total {
            nav.push(json!({"text": "More ➜", "callback_data": format!("cmp:p:{}", page + 1)}));
        }
        if !nav.is_empty() {
            buttons.push(json!(nav));
        }
        buttons.push(json!([{"text": "✖ Cancel", "callback_data": "cmp:n"}]));
        let text = format!("🗜 Compact which session? ({}-{} of {total})", if total > 0 { start + 1 } else { 0 }, start + visible.len());
        Ok((text, json!({"inline_keyboard": buttons})))
    }

    async fn compact_confirm_view(&self, sid: &str) -> (String, Value) {
        let snap = self.session_snapshot(sid).await.ok();
        let title = take(&squash(snap.as_ref().and_then(|s| s["title"].as_str()).filter(|t| !t.is_empty()).unwrap_or("(untitled)")), 40);
        let text = format!("🗜 Compact {title} ({})?\nstatus: {} · context: {}", take(sid, 8),
                           snap.as_ref().and_then(|s| s["status"].as_str()).filter(|s| !s.is_empty()).unwrap_or("?"),
                           snap.as_ref().map(format_context_usage).unwrap_or_else(|| "?".into()));
        (text, json!({"inline_keyboard": [[{"text": "✅ Compact", "callback_data": format!("cmp:y:{sid}")}, {"text": "⬅️ Back", "callback_data": "cmp:p:0"}]]}))
    }

    async fn edit_or_send(&self, ctx: &Ctx, message: &Value, text: &str, markup: Option<Value>) -> Result<()> {
        if let Some(mid) = message["message_id"].as_i64() {
            let mut p = json!({"chat_id": ctx.chat_id, "message_id": mid, "text": text});
            if let Some(m) = &markup {
                p["reply_markup"] = m.clone();
            }
            match self.tg1("editMessageText", p).await {
                Ok(_) => return Ok(()),
                Err(e) => log(format!("edit compact picker failed: {e}")),
            }
        }
        self.send_markup(ctx, &ctx.chat_id, text, markup).await?;
        Ok(())
    }

    async fn handle_compact_callback(self: &Arc<Self>, ctx: &Ctx, callback_id: &Value, message: &Value, data: &str) -> Result<()> {
        let parts: Vec<&str> = data.splitn(3, ':').collect();
        let action = parts.get(1).copied().unwrap_or("");
        let arg = parts.get(2).copied().unwrap_or("");
        self.answer_callback(callback_id, None).await;
        match (action, arg.is_empty()) {
            ("p", _) => {
                let (t, m) = self.compact_picker(ctx, arg.parse().unwrap_or(0)).await?;
                self.edit_or_send(ctx, message, &t, Some(m)).await
            }
            ("a", false) => {
                let (t, m) = self.compact_confirm_view(arg).await;
                self.edit_or_send(ctx, message, &t, Some(m)).await
            }
            ("y", false) => {
                self.edit_or_send(ctx, message, &format!("🗜 Compacting {}…", take(arg, 8)), Some(json!({"inline_keyboard": []}))).await?;
                self.compact_current_session(ctx, Some(arg.to_string())).await
            }
            _ => self.edit_or_send(ctx, message, "Compaction cancelled.", Some(json!({"inline_keyboard": []}))).await,
        }
    }

    async fn compact_current_session(self: &Arc<Self>, ctx: &Ctx, sid: Option<String>) -> Result<()> {
        let Some(sid) = sid.or_else(|| self.current_session(ctx)) else {
            self.say(ctx, "No session selected.").await?;
            return Ok(());
        };
        if self.compactions.lock().unwrap().contains(&sid) {
            self.say(ctx, &format!("🗜 Compaction is already in progress for {}. I'll report the result here.", take(&sid, 8))).await?;
            return Ok(());
        }
        let snapshot = match self.session_snapshot(&sid).await {
            Ok(s) => s,
            Err(e) => {
                self.say(ctx, &format!("Compact failed for {}: {e}", take(&sid, 8))).await?;
                return Ok(());
            }
        };
        let status = snapshot["status"].as_str().unwrap_or("");
        if busy(status) || self.is_active(&sid) {
            self.say(ctx, &format!("⏳ {} is busy. Compaction can only start while the session is idle; wait for the current response or use /stop, then retry.", take(&sid, 8))).await?;
            return Ok(());
        }
        if snapshot["harness"] == "codex" {
            self.say(ctx, "Manual compaction is unavailable for this Codex app-server session; /compact requires a codex-native session.").await?;
            return Ok(());
        }
        // Start the watcher (it runs Kurisu's blocking compact).
        let started = {
            let mut w = self.compactions.lock().unwrap();
            w.insert(sid.clone())
        };
        if started {
            let (b, c, s) = (self.clone(), ctx.clone(), sid.clone());
            tokio::spawn(async move { b.kurisu_watch_compaction(&c, &s).await });
        }
        self.say(ctx, &format!("🗜 Compaction requested for {}.\nKurisu will summarize older conversation context. The session may show 'running' until it finishes; I'll send the result here.\nContext before: {}",
                               take(&sid, 8), format_context_usage(&snapshot))).await?;
        Ok(())
    }

    async fn kurisu_watch_compaction(&self, ctx: &Ctx, sid: &str) {
        let chat = self.reply(ctx);
        let r = self.k(json!({"op": "compact", "name": sid})).await;
        let result = match r {
            Ok(v) => v,
            Err(e) => {
                let _ = self.send_text(ctx, &chat, &format!("❌ Compaction failed for {}: {}\nYou can retry with /compact.", take(sid, 8), take(&e.to_string(), 500))).await;
                self.compactions.lock().unwrap().remove(sid);
                return;
            }
        };
        let snapshot = self.session_snapshot(sid).await.ok();
        let mut metrics = vec![];
        if let Some(n) = result["pre_tokens"].as_i64() {
            metrics.push(format!("before: {} tokens", fmt_int(n)));
        }
        if let Some(n) = result["post_tokens_estimate"].as_i64() {
            metrics.push(format!("after: ~{} tokens (estimate)", fmt_int(n)));
        }
        let summary = result["summary"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
            .unwrap_or_else(|| "Kurisu did not return summary text for this compaction.".into());
        let text = format!("✅ Compaction complete for {}.\nContext now: {}{}\n\n📝 Compaction summary\n{summary}", take(sid, 8),
                           snapshot.as_ref().map(format_context_usage).unwrap_or_else(|| "unknown".into()),
                           if metrics.is_empty() { String::new() } else { format!("\nCompaction tokens — {}", metrics.join(", ")) });
        let _ = self.send_text(ctx, &chat, &text).await;
        self.compactions.lock().unwrap().remove(sid);
    }

    // ------------------------------------------------------------ /stop

    async fn stop_current_session(&self, ctx: &Ctx) -> Result<()> {
        let Some(sid) = self.current_session(ctx) else {
            self.say(ctx, "Nothing is running.").await?;
            return Ok(());
        };
        let status = match self.session_snapshot(&sid).await {
            Ok(s) => s["status"].as_str().unwrap_or("").to_string(),
            Err(e) => {
                self.say(ctx, &format!("Stop failed: {e}")).await?;
                return Ok(());
            }
        };
        if !busy(&status) && !self.is_active(&sid) {
            self.say(ctx, &format!("Nothing is running in {}.", take(&sid, 8))).await?;
            return Ok(());
        }
        if self.is_active(&sid) {
            self.act(|a| a.stop.insert(sid.clone()));
        }
        let mut idle = false;
        for _ in 0..2 {
            if let Err(e) = self.k(json!({"op": "stop", "name": sid})).await {
                if self.is_active(&sid) {
                    self.end_relay(&sid);
                }
                self.say(ctx, &format!("Stop failed for {}: {e}", take(&sid, 8))).await?;
                return Ok(());
            }
            let deadline = Instant::now() + Duration::from_millis(2500);
            while Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(350)).await;
                match self.status_of(&sid).await {
                    None => break,
                    Some(s) if s == "idle" => {
                        idle = true;
                        break;
                    }
                    _ => {}
                }
            }
            if idle {
                break;
            }
        }
        if idle {
            self.say(ctx, &format!("🛑 Stopped the current run in {}. The session is still active.", take(&sid, 8))).await?;
        } else {
            self.say(ctx, &format!("🛑 Stop requested for {}. The agent is still winding down; try /stop again.", take(&sid, 8))).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ /model

    /// Store a /model card (token -> card) with the bridge's TTL and limit.
    fn model_card(&self, mut card: Value) -> String {
        let tok = token(16);
        let now = now_f();
        card["created_at"] = json!(now as i64);
        self.with_state(|st| {
            let cards = st.entry("model_cards").or_insert(json!({}));
            if !cards.is_object() {
                *cards = json!({});
            }
            let m = cards.as_object_mut().unwrap();
            m.retain(|_, c| c["created_at"].as_f64().is_some_and(|t| now - t <= MODEL_CARD_TTL_S));
            m.insert(tok.clone(), card);
            while m.len() > MODEL_CARD_LIMIT {
                let first = m.keys().next().cloned().unwrap();
                m.remove(&first);
            }
        });
        self.save_state();
        tok
    }

    fn session_harness(snapshot: &Value) -> String {
        snapshot["kurisu_harness"].as_str().filter(|h| !h.is_empty()).unwrap_or("claude").to_string()
    }

    /// Harnesses /model offers in step 1: the installed CLIs (the session's own always).
    async fn available_harnesses(&self, snapshot: &Value) -> Vec<String> {
        let current = Self::session_harness(snapshot);
        let mut found: Vec<String> = match self.k(json!({"op": "clis"})).await {
            Ok(v) => v["clis"].as_array().into_iter().flatten()
                .filter(|c| c["installed"] == json!(true) && (c["name"] == "claude" || c["name"] == "codex"))
                .filter_map(|c| c["name"].as_str().map(String::from)).collect(),
            Err(e) => {
                log(format!("cli list failed: {e}"));
                vec![]
            }
        };
        if (current == "claude" || current == "codex") && !found.contains(&current) {
            found.insert(0, current);
        }
        // One button per harness (bridge 982f783).
        let mut seen: Vec<String> = vec![];
        for h in found {
            if !seen.contains(&h) {
                seen.push(h);
            }
        }
        seen
    }

    /// Step 1 (harness) of /model, or step 2 straight away when only one harness is available.
    async fn model_picker(&self, ctx: &Ctx) -> Result<(String, Option<Value>)> {
        let sid = self.ensure_session(ctx).await?;
        let snapshot = self.session_snapshot(&sid).await?;
        let harnesses = self.available_harnesses(&snapshot).await;
        let current = Self::session_harness(&snapshot);
        if harnesses.len() <= 1 {
            return self.model_overview(&snapshot, None).await;
        }
        let title = squash(snapshot["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)"));
        let model = snapshot["model_override"].as_str().filter(|m| !m.is_empty()).or(snapshot["llm_model"].as_str().filter(|m| !m.is_empty())).unwrap_or("agent default");
        let tok = self.model_card(json!({"session_id": sid, "current_harness": current, "harnesses": harnesses, "step": "harness"}));
        let label = |h: &str| match h { "claude" => "Claude Code", "codex" => "Codex", other => other }.to_string();
        let lines = [format!("Session: {title} ({})", take(&sid, 8)), format!("Current: {current} · {model}"), String::new(),
                     "Step 1 of 3 — pick a harness:".into(), String::new(), "You can also send /model <model-id> (same harness).".into()];
        let buttons: Vec<Value> = harnesses.iter().enumerate().map(|(i, h)| json!([{
            "text": format!("{}{} ({h})", if *h == current { "✓ " } else { "" }, label(h)), "callback_data": format!("mh:{tok}:{i}")}])).collect();
        Ok((lines.join("\n"), Some(json!({"inline_keyboard": buttons}))))
    }

    /// Step 2: a harness's models (the session's own catalogue for its current harness).
    async fn model_overview(&self, snapshot: &Value, harness: Option<String>) -> Result<(String, Option<Value>)> {
        let sid = snapshot["id"].as_str().unwrap_or("").to_string();
        let current_harness = Self::session_harness(snapshot);
        let harness = harness.unwrap_or_else(|| current_harness.clone());
        let switching = harness != current_harness;
        let mut for_harness = snapshot.clone();
        for_harness["harness"] = json!(if harness == "codex" { "codex" } else { "claude-native" });
        let (mut options, err) = self.model_options(&for_harness).await?;
        let model_now = snapshot["model_override"].as_str().filter(|m| !m.is_empty()).or(snapshot["llm_model"].as_str().filter(|m| !m.is_empty())).unwrap_or("agent default").to_string();
        let current = if switching { String::new() } else { model_now.clone() };
        let title = squash(snapshot["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)"));
        let mut lines = vec![format!("Current model: {model_now}"), format!("Session: {title} ({})", take(&sid, 8))];
        if switching {
            lines[0] = format!("Step 2 of 3 — {harness} models (session is on {current_harness} now)");
        }
        if switching && harness == "codex" {
            // Codex's catalogue is embedded in Kurisu; surface the configured default's real id.
            let real = self.configured_codex_model().await;
            if let Some(default) = options.iter_mut().find(|o| o["id"] == "default") {
                default["label"] = json!(format!("Codex default{}", real.map(|r| format!(" ({r})")).unwrap_or_default()));
            }
        }
        let mut buttons = vec![];
        if !options.is_empty() {
            let tok = self.model_card(json!({"session_id": sid, "models": options.iter().map(|o| o["id"].clone()).collect::<Vec<_>>(),
                                             "harness": harness, "current_harness": current_harness}));
            lines.extend(["".into(), "Tap a model to switch this session:".into()]);
            for (i, o) in options.iter().enumerate() {
                let id = o["id"].as_str().unwrap_or("");
                let selected = !current.is_empty() && (id == current || id.rsplit('/').next() == Some(current.as_str()));
                let label = format!("{}{}", if selected { "✓ " } else { "" }, o["label"].as_str().unwrap_or(id));
                buttons.push(json!([{"text": take(&label, 64), "callback_data": format!("model:{tok}:{i}")}]));
            }
        } else {
            lines.extend(["".into(), "No model choices are available from the host.".into()]);
            if let Some(e) = err {
                lines.push(format!("Reason: {e}"));
            }
        }
        if switching {
            lines.extend(["".into(), "/model <model-id> changes the model on the current harness only.".into()]);
        } else {
            lines.extend(["".into(), "You can also send /model <model-id>.".into(), "Send /model default to restore the agent default.".into()]);
        }
        Ok((lines.join("\n"), if buttons.is_empty() { None } else { Some(json!({"inline_keyboard": buttons})) }))
    }

    async fn edit_model_card(&self, ctx: &Ctx, message: &Value, text: &str, markup: Option<Value>) -> Result<()> {
        if let Some(mid) = message["message_id"].as_i64() {
            match self.tg1("editMessageText", json!({"chat_id": ctx.chat_id, "message_id": mid, "text": text, "disable_web_page_preview": true,
                                                       "reply_markup": markup.clone().unwrap_or(json!({"inline_keyboard": []}))})).await {
                Ok(_) => return Ok(()),
                Err(e) => log(format!("edit model card failed: {e}")),
            }
        }
        self.send_markup(ctx, &ctx.chat_id, text, markup).await?;
        Ok(())
    }

    /// mh:<token>:<index> (step 1 pick) and mx:<token>:y|n (harness switch confirm).
    async fn handle_model_harness_callback(&self, ctx: &Ctx, cid: &Value, message: &Value, data: &str) -> Result<()> {
        let parts: Vec<&str> = data.splitn(3, ':').collect();
        let (kind, tok, arg) = (parts[0], parts.get(1).copied().unwrap_or(""), parts.get(2).copied().unwrap_or(""));
        let Some(card) = self.with_state(|st| st.get("model_cards").and_then(|c| c.get(tok)).cloned()) else {
            self.answer_callback(cid, Some("Model picker expired; run /model again")).await;
            return Ok(());
        };
        let remove = |b: &Self| b.with_state(|st| st.get_mut("model_cards").and_then(|c| c.as_object_mut()).map(|c| c.remove(tok)));
        let sid = card["session_id"].as_str().unwrap_or("").to_string();
        if kind == "mh" {
            let Some(harness) = arg.parse::<usize>().ok().and_then(|i| card["harnesses"].get(i)).and_then(|h| h.as_str()).map(String::from) else {
                self.answer_callback(cid, Some("Model picker expired; run /model again")).await;
                return Ok(());
            };
            let snapshot = self.session_snapshot(&sid).await?;
            remove(self);
            let (text, markup) = self.model_overview(&snapshot, Some(harness.clone())).await?;
            self.answer_callback(cid, Some(&harness)).await;
            return self.edit_model_card(ctx, message, &text, markup).await;
        }
        // mx: confirm a harness switch
        remove(self);
        self.save_state();
        if arg != "y" {
            self.answer_callback(cid, Some("Cancelled")).await;
            return self.edit_model_card(ctx, message, "Harness switch cancelled; the session is unchanged.", None).await;
        }
        let snapshot = self.session_snapshot(&sid).await?;
        if busy(snapshot["status"].as_str().unwrap_or("")) || self.is_active(&sid) {
            self.alert(cid, "A run is active; use /stop or wait for it to finish").await;
            return Ok(());
        }
        let harness = card["harness"].as_str().unwrap_or("").to_string();
        let model = card["model"].as_str().filter(|m| !m.is_empty()).map(String::from);
        let mut fields = json!({"harness": harness});
        if let Some(m) = &model {
            fields["model"] = json!(m);
        }
        let r = match self.k(json!({"op": "update", "name": sid, "fields": fields})).await {
            Ok(r) if r["new"]["id"].is_string() => r,
            Ok(_) => {
                self.alert(cid, "Harness switch failed: Kurisu returned no replacement session").await;
                return Ok(());
            }
            Err(e) => {
                log(format!("harness switch failed: {e}"));
                self.alert(cid, &format!("Harness switch failed: {e}")).await;
                return Ok(());
            }
        };
        // The server already pointed every chat that used the old session at the new one.
        let new_id = r["new"]["id"].as_str().unwrap_or("").to_string();
        // Effort: the pick, or ("default") the old setting when the new harness supports it.
        let effort = card["effort"].as_str().or(card["old_effort"].as_str())
            .filter(|e| crate::store::effort_levels(&harness).contains(e)).map(String::from);
        if let Some(e) = &effort {
            if let Err(err) = self.k(json!({"op": "update", "name": new_id, "fields": {"effort": e}})).await {
                log(format!("effort after harness switch failed: {err}"));
            }
        }
        self.answer_callback(cid, Some(&format!("Switched to {harness}"))).await;
        let title = squash(snapshot["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)"));
        let note = r["note"].as_str().filter(|n| !n.is_empty()).map(|n| format!("\n{n}")).unwrap_or_default();
        self.edit_model_card(ctx, message, &format!("✅ Switched {title} to {harness}{}{}.\nNew session: {new_id}{note}",
                                                      model.map(|m| format!(" ({m})")).unwrap_or_default(),
                                                      effort.map(|e| format!(", effort {e}")).unwrap_or_default()), None).await
    }

    async fn alert(&self, cid: &Value, text: &str) {
        if let Err(e) = self.tg1("answerCallbackQuery", json!({"callback_query_id": cid, "text": take(text, 180), "show_alert": true})).await {
            log(format!("callback alert failed: {e}"));
        }
    }

    async fn switch_session_model(&self, sid: &str, model_id: &str) -> Result<Value> {
        let model_id = model_id.trim();
        if model_id.is_empty() {
            bail!("model id cannot be empty");
        }
        if disabled_model(model_id) {
            bail!("Kimi models are disabled");
        }
        let snapshot = self.session_snapshot(sid).await?;
        if busy(snapshot["status"].as_str().unwrap_or("")) || self.is_active(sid) {
            bail!("a run is active; use /stop or wait for it to finish");
        }
        self.kurisu_switch_model(&snapshot, model_id).await
    }

    async fn handle_model_command(&self, ctx: &Ctx, arg: &str) -> Result<()> {
        let target = arg.trim();
        if !target.is_empty() {
            let sid = self.ensure_session(ctx).await?;
            match self.switch_session_model(&sid, target).await {
                Ok(snap) => self.say(ctx, &model_update_text(&snap, target)).await?,
                Err(e) => self.say(ctx, &format!("Model switch failed for {}: {e}", take(&sid, 8))).await?,
            };
            return Ok(());
        }
        match self.model_picker(ctx).await {
            Ok((t, m)) => self.send_markup(ctx, &ctx.chat_id, &t, m).await?,
            Err(e) => self.say(ctx, &format!("Model list failed: {e}")).await?,
        };
        Ok(())
    }

    async fn handle_model_callback(&self, ctx: &Ctx, callback_id: &Value, message: &Value, data: &str) -> Result<()> {
        let parts: Vec<&str> = data.splitn(3, ':').collect();
        let tok = parts.get(1).copied().unwrap_or("");
        let index: i64 = parts.get(2).and_then(|i| i.parse().ok()).unwrap_or(-1);
        let card = self.with_state(|st| st.get("model_cards").and_then(|c| c.get(tok)).cloned());
        let picked = card.as_ref().and_then(|c| {
            if index < 0 {
                return None;
            }
            let model = if data.starts_with("me:") { c["model"].as_str()? } else { c["models"].get(index as usize)?.as_str()? };
            Some((model.to_string(), c["session_id"].as_str()?.to_string()))
        });
        let Some((model_id, sid)) = picked else {
            self.answer_callback(callback_id, Some("Model picker expired; run /model again")).await;
            return Ok(());
        };
        let card = card.unwrap_or_default();
        // Step 3: effort, unless this is already the effort pick (index came via me:).
        if !data.starts_with("me:") {
            let h = card["harness"].as_str().filter(|h| !h.is_empty()).unwrap_or(card["current_harness"].as_str().unwrap_or("claude")).to_string();
            let snapshot = self.session_snapshot(&sid).await?;
            self.with_state(|st| st.get_mut("model_cards").and_then(|c| c.as_object_mut()).map(|c| c.remove(tok)));
            let (text, markup) = effort_step(self.model_card(json!({"session_id": sid, "harness": card["harness"], "current_harness": card["current_harness"],
                                                                     "model": model_id, "effort_step": true})),
                                             &h, &model_id, snapshot["reasoning_effort"].as_str());
            self.answer_callback(callback_id, Some("Pick an effort")).await;
            return self.edit_model_card(ctx, message, &text, Some(markup)).await;
        }
        let effort = picked_effort(&card, data);
        // A model on another harness: confirm the harness switch first.
        let (h, cur) = (card["harness"].as_str().unwrap_or(""), card["current_harness"].as_str().unwrap_or(""));
        if !h.is_empty() && !cur.is_empty() && h != cur {
            let snapshot = self.session_snapshot(&sid).await?;
            self.with_state(|st| st.get_mut("model_cards").and_then(|c| c.as_object_mut()).map(|c| c.remove(tok)));
            let confirm = self.model_card(json!({"session_id": sid, "harness": h, "current_harness": cur, "model": model_id, "effort": effort,
                                                 "old_effort": snapshot["reasoning_effort"]}));
            self.answer_callback(callback_id, Some("Confirm the switch")).await;
            let title = squash(snapshot["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)"));
            let text = format!("🔁 Switch {title} from {cur} to {h}{}{}?\nIt restarts as a new session that reads the old history.",
                               if model_id.is_empty() { String::new() } else { format!(" ({model_id})") },
                               effort.as_deref().map(|e| format!(", effort {e}")).unwrap_or_default());
            return self.edit_model_card(ctx, message, &text, Some(json!({"inline_keyboard": [[
                {"text": "🔁 Switch", "callback_data": format!("mx:{confirm}:y")}, {"text": "✖ Cancel", "callback_data": format!("mx:{confirm}:n")}]]}))).await;
        }
        let snap = match self.switch_session_model(&sid, &model_id).await {
            Ok(s) => match &effort {
                Some(e) => match self.k(json!({"op": "update", "name": sid, "fields": {"effort": e}})).await {
                    Ok(_) => self.session_snapshot(&sid).await.unwrap_or(s),
                    Err(e) => {
                        self.answer_callback(callback_id, Some(&take(&format!("Effort change failed: {e}"), 180))).await;
                        return Ok(());
                    }
                },
                None => s,
            },
            Err(e) => {
                self.answer_callback(callback_id, Some(&take(&format!("Model switch failed: {e}"), 180))).await;
                return Ok(());
            }
        };
        self.with_state(|st| st.get_mut("model_cards").and_then(|c| c.as_object_mut()).map(|c| c.remove(tok)));
        self.save_state();
        self.answer_callback(callback_id, Some("Model switched")).await;
        let mut text = model_update_text(&snap, &model_id);
        text.push_str(&format!("\nEffort: {}", snap["reasoning_effort"].as_str().unwrap_or("default")));
        if let Some(mid) = message["message_id"].as_i64() {
            match self.tg1("editMessageText", json!({"chat_id": ctx.chat_id, "message_id": mid, "text": text, "disable_web_page_preview": true,
                                                       "reply_markup": {"inline_keyboard": []}})).await {
                Ok(_) => return Ok(()),
                Err(e) => log(format!("edit model picker failed: {e}")),
            }
        }
        self.say(ctx, &text).await?;
        Ok(())
    }

    // ------------------------------------------------------------ /mcp

    async fn mcp_overview(&self, ctx: &Ctx, notice: Option<&str>, interactive: bool) -> Result<(String, Option<Value>)> {
        let session = match self.current_session(ctx) {
            Some(sid) => match self.session_snapshot(&sid).await {
                Ok(s) => s,
                Err(e) => {
                    log(format!("mcp snapshot failed: {e}"));
                    json!({"id": sid})
                }
            },
            None => json!({}),
        };
        self.kurisu_mcp_overview(&session, notice, interactive).await
    }

    async fn handle_mcp_callback(&self, ctx: &Ctx, callback_id: &Value, message: &Value, data: &str) -> Result<()> {
        if !self.is_owner_dm(&ctx.chat_id) {
            self.answer_callback(callback_id, Some("MCP changes are only allowed from the owner's DM")).await;
            return Ok(());
        }
        let mut notice = None;
        if data == "mcp:r" {
            self.answer_callback(callback_id, Some("Updated")).await;
        } else {
            let parts: Vec<&str> = data.splitn(3, ':').collect();
            let tok = parts.get(1).copied().unwrap_or("").to_string();
            let action = parts.get(2).and_then(|i| i.parse::<i64>().ok()).filter(|i| *i >= 0).and_then(|i| {
                self.with_state(|st| st.get("mcp_cards").and_then(|c| c.get(&tok)).and_then(|c| c["actions"].get(i as usize)).cloned())
            });
            let Some(action) = action else {
                self.answer_callback(callback_id, Some("MCP card expired; run /mcp again")).await;
                return Ok(());
            };
            match self.kurisu_mcp_apply(&action).await {
                Ok(n) => notice = Some(n),
                Err(e) => {
                    log(format!("mcp action failed: {e}"));
                    self.answer_callback(callback_id, Some(&format!("MCP change failed: {e}"))).await;
                    return Ok(());
                }
            }
            self.with_state(|st| st.get_mut("mcp_cards").and_then(|c| c.as_object_mut()).map(|c| c.remove(&tok)));
            log(format!("mcp action {} kurisu {}", vstr(&action["op"]), action["name"].as_str().unwrap_or("lb")));
            let first = notice.as_deref().unwrap_or("").split('\n').next().unwrap_or("").to_string();
            self.answer_callback(callback_id, Some(&first)).await;
        }
        let (text, markup) = self.mcp_overview(ctx, notice.as_deref(), true).await?;
        if let Some(mid) = message["message_id"].as_i64() {
            let mut p = json!({"chat_id": ctx.chat_id, "message_id": mid, "text": text, "disable_web_page_preview": true});
            if let Some(m) = &markup {
                p["reply_markup"] = m.clone();
            }
            match self.tg1("editMessageText", p).await {
                Ok(_) => return Ok(()),
                Err(e) => log(format!("edit mcp card failed: {e}")),
            }
        }
        self.send_markup(ctx, &ctx.chat_id, &text, markup).await?;
        Ok(())
    }

    // ------------------------------------------------------------ /catch_up and /send_now

    async fn handle_catch_up(&self, ctx: &Ctx, override_session: Option<String>) -> Result<()> {
        let queued = self.work_pending();
        self.say(ctx, if queued { "⏳ Catch-up queued behind the current work." } else { "⏳ Catching you up..." }).await?;
        self.enqueue(Job { ctx: ctx.clone(), label: "catch-up overview".into(), kind: JobKind::CatchUp, override_session });
        Ok(())
    }

    async fn handle_send_now(self: &Arc<Self>, ctx: &Ctx) -> Result<()> {
        let sid = self.current_session(ctx);
        let Some(sid) = sid.filter(|s| self.is_active(s)) else {
            self.say(ctx, "No active run in the current session; queued messages were left unchanged.").await?;
            return Ok(());
        };
        let released = self.request_send_now(&sid);
        if released {
            // Let the waiting (oldest) message go first so the order is kept.
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && self.act(|a| a.waiting.contains(&sid)) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let (chat, scope) = (vstr(&ctx.chat_id), ctx.scope.clone());
        let jobs = self.drain_queued(|j| matches!(j.kind, JobKind::Text { .. } | JobKind::Attachment { .. }) && vstr(&j.ctx.chat_id) == chat && j.ctx.scope == scope);
        if jobs.is_empty() {
            self.say(ctx, if released { "🚀 Sent your waiting message to the active run." } else { "No queued messages to send." }).await?;
            return Ok(());
        }
        let mut sent = 0;
        for mut job in jobs.clone() {
            job.override_session = Some(sid.clone());
            match self.run_job_once(&job, true).await {
                Ok(()) => {
                    sent += 1;
                    log(format!("sent queued job now: {}", job.label));
                }
                Err(e) => {
                    log(format!("send-now failed ({}): {e}", job.label));
                    self.clear_pending(job.ctx.update_id);
                    self.on_job_error(&job, &e).await;
                }
            }
        }
        if sent == jobs.len() {
            self.say(ctx, &format!("🚀 Sent {sent} queued message{} to the active run.", if sent == 1 { "" } else { "s" })).await?;
        } else {
            self.say(ctx, &format!("🚀 Sent {sent} of {} queued messages to the active run; errors were reported above.", jobs.len())).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ text and commands

    pub(crate) async fn handle_text(self: &Arc<Self>, ctx: &Ctx, text: &str, override_session: Option<String>, persist: bool,
                                    ack: bool, send_immediately: bool) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        if text.starts_with('/') {
            let (command, arg) = text.split_once(' ').unwrap_or((text, ""));
            let command = command.split('@').next().unwrap_or("").to_lowercase();
            match command.as_str() {
                "/start" | "/help" => {
                    self.say(ctx, HELP).await?;
                }
                "/list" => match self.session_overview(ctx, 0).await {
                    Ok((t, m)) => {
                        self.send_markup(ctx, &ctx.chat_id, &t, m).await?;
                    }
                    Err(e) => {
                        self.say(ctx, &format!("List failed: {e}")).await?;
                    }
                },
                "/id" => {
                    self.say(ctx, &self.current_session(ctx).unwrap_or_else(|| "No session selected.".into())).await?;
                }
                "/catch_up" | "/catch-up" => self.handle_catch_up(ctx, override_session).await?,
                "/stop" => self.stop_current_session(ctx).await?,
                "/model" => self.handle_model_command(ctx, arg).await?,
                "/mode" => self.handle_mode_command(ctx, arg).await?,
                "/mcp" => match self.mcp_overview(ctx, None, self.is_owner_dm(&ctx.chat_id)).await {
                    Ok((t, m)) => {
                        self.send_markup(ctx, &ctx.chat_id, &t, m).await?;
                    }
                    Err(e) => {
                        self.say(ctx, &format!("MCP list failed: {e}")).await?;
                    }
                },
                "/usage" => self.handle_usage_command(ctx).await?,
                "/schedule" | "/wakes" => {
                    let (t, m) = self.schedule_card().await;
                    self.send_markup(ctx, &ctx.chat_id, &t, m).await?;
                }
                "/browser" => self.remote_command(ctx, "browser", arg).await?,
                "/web" => self.remote_command(ctx, "web", arg).await?,
                "/terminal" => self.remote_command(ctx, "terminal", arg).await?,
                "/send-now" | "/send_now" => self.handle_send_now(ctx).await?,
                "/compact" => {
                    if ["this", "current", "here", "."].contains(&arg.trim().to_lowercase().as_str()) {
                        self.compact_current_session(ctx, None).await?;
                    } else {
                        match self.compact_picker(ctx, 0).await {
                            Ok((t, m)) => {
                                self.send_markup(ctx, &ctx.chat_id, &t, Some(m)).await?;
                            }
                            Err(e) => {
                                self.say(ctx, &format!("Compact picker failed: {e}")).await?;
                            }
                        }
                    }
                }
                "/status" => {
                    let sid = self.ensure_session(ctx).await?;
                    match self.session_snapshot(&sid).await {
                        Ok(snap) => {
                            let harness = harness_label(&snap, None);
                            let (model_id, short) = self.resolved_model(&snap, harness.as_deref()).await;
                            let mut model = model_id.clone().unwrap_or(short);
                            let raw = snap["model_override"].as_str().filter(|m| !m.is_empty()).or(snap["llm_model"].as_str().filter(|m| !m.is_empty())).unwrap_or("default");
                            if model_id.is_some() && raw.to_lowercase() == "default" {
                                model.push_str(" (codex default)");
                            }
                            let mut clis = self.cli_status_line().await;
                            if let Ok(b) = self.k(json!({"op": "browser_status"})).await {
                                if let Some(l) = b["line"].as_str() {
                                    let mut l = l.to_string();
                                    if let Some(r) = l.get_mut(0..1) {
                                        r.make_ascii_uppercase();
                                    }
                                    clis.push_str(&format!("\n{l}"));
                                }
                            }
                            self.say(ctx, &format!("status: {}\ntitle: {}\nharness: {}\nmodel: {model}\neffort: {}\npermissions: {} (/mode)\ncontext: {}\nsession: {sid}\n{clis}",
                                                   vstr(&snap["status"]), snap["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)"),
                                                   harness.unwrap_or_else(|| "unknown".into()),
                                                   snap["reasoning_effort"].as_str().filter(|e| !e.is_empty()).unwrap_or("default"),
                                                   snap["permission_mode"].as_str().unwrap_or("default"),
                                                   format_context_usage(&snap))).await?;
                        }
                        Err(e) => {
                            self.say(ctx, &format!("Status failed: {e}")).await?;
                        }
                    }
                }
                "/new" | "/reset" => {
                    let sid = self.create_session(ctx).await?;
                    self.select_session(ctx, &sid);
                    if persist {
                        self.save_state();
                    }
                    self.say(ctx, &format!("Started a new session: {sid}")).await?;
                }
                "/use" => {
                    let mut target = arg.trim().to_string();
                    if target.is_empty() {
                        self.say(ctx, "Usage: /use <session-id-or-prefix>").await?;
                        return Ok(());
                    }
                    let ids: Vec<String> = match self.fetch_sessions().await {
                        Ok(s) => s.iter().filter_map(|x| x["id"].as_str().map(String::from)).collect(),
                        Err(e) => {
                            self.say(ctx, &format!("Cannot list sessions: {e}")).await?;
                            return Ok(());
                        }
                    };
                    if !ids.contains(&target) {
                        let matches: Vec<&String> = ids.iter().filter(|i| i.starts_with(&target)).collect();
                        match matches.len() {
                            1 => target = matches[0].clone(),
                            0 => {
                                self.say(ctx, &format!("No session matches {target}")).await?;
                                return Ok(());
                            }
                            _ => {
                                self.say(ctx, &format!("Ambiguous prefix. Matches: {}", matches.iter().take(8).map(|m| take(m, 8)).collect::<Vec<_>>().join(", "))).await?;
                                return Ok(());
                            }
                        }
                    }
                    let snap = match self.session_snapshot(&target).await {
                        Ok(s) => s,
                        Err(e) => {
                            self.say(ctx, &format!("Cannot use {target}: {e}")).await?;
                            return Ok(());
                        }
                    };
                    self.select_session(ctx, &target);
                    if persist {
                        self.save_state();
                    }
                    self.say(ctx, &format!("Now using {target} ({})", vstr(&snap["status"]))).await?;
                }
                _ => {
                    self.say(ctx, "Unknown command. Try /help.").await?;
                }
            }
            return Ok(());
        }
        if ack && send_immediately {
            self.say(ctx, "⏳ On it.").await?;
        }
        let announce = if ack && !send_immediately { Announce::Yes } else { Announce::No };
        self.dispatch_content(ctx, vec![json!({"type": "input_text", "text": text})], override_session, persist, send_immediately, announce).await
    }

    // ------------------------------------------------------------ callbacks

    async fn handle_callback(self: &Arc<Self>, callback: &Value) -> Result<()> {
        let cid = &callback["id"];
        let data = callback["data"].as_str().unwrap_or("");
        let message = &callback["message"];
        let ctx = self.ctx(message["chat"]["id"].clone(), message["message_thread_id"].as_i64(), None);
        if !self.allowed_sender(&ctx.chat_id, &callback["from"]) {
            self.answer_callback(cid, Some("Not allowed")).await;
            return Ok(());
        }
        let guard = |label: &'static str| move |e: anyhow::Error| (label, e);
        let res: std::result::Result<(), (&str, anyhow::Error)> = if data.starts_with("approval:") {
            self.handle_approval_callback(&ctx, cid, data).await.map_err(guard("Approval failed"))
        } else if let Some(rest) = data.strip_prefix("mode:") {
            let (sid, mode) = rest.rsplit_once(':').unwrap_or(("", ""));
            self.answer_callback(cid, Some(mode)).await;
            self.set_mode(&ctx, sid, mode, Some(message)).await.map_err(guard("Mode change failed"))
        } else if data.starts_with("en:") {
            self.handle_enable_callback(&ctx, cid, data).await.map_err(guard("Approval failed"))
        } else if data.starts_with("pair:") {
            self.handle_pair_callback(&ctx, cid, data).await.map_err(guard("Approval failed"))
        } else if data.starts_with("mh:") || data.starts_with("mx:") {
            self.handle_model_harness_callback(&ctx, cid, message, data).await.map_err(guard("Model switch failed"))
        } else if data.starts_with("model:") || data.starts_with("me:") {
            self.handle_model_callback(&ctx, cid, message, data).await.map_err(guard("Model switch failed"))
        } else if data.starts_with("q:") {
            self.handle_question_callback(&ctx, cid, data).await.map_err(guard("Answer failed"))
        } else if data.starts_with("mcp:") {
            self.handle_mcp_callback(&ctx, cid, message, data).await.map_err(guard("MCP action failed"))
        } else if data.starts_with("cmp:") {
            if let Err(e) = self.handle_compact_callback(&ctx, cid, message, data).await {
                log(format!("compact callback failed: {e}"));
                self.say(&ctx, &format!("Compact failed: {e}")).await?;
            }
            return Ok(());
        } else {
            let mut page = 0;
            if let Some(target) = data.strip_prefix("use:") {
                match self.session_snapshot(target).await {
                    Ok(snap) => {
                        self.select_session(&ctx, target);
                        self.save_state();
                        self.answer_callback(cid, Some(&format!("Selected {} ({})", take(target, 8), vstr(&snap["status"])))).await;
                    }
                    Err(e) => {
                        self.answer_callback(cid, Some(&format!("Cannot use {}: {e}", take(target, 8)))).await;
                        return Ok(());
                    }
                }
            } else if data == "sched" || data.starts_with("wc:") {
                if let Some(id) = data.strip_prefix("wc:").and_then(|i| i.parse::<i64>().ok()) {
                    match self.k(json!({"op": "wake_cancel", "id": id})).await {
                        Ok(_) => self.answer_callback(cid, Some(&format!("Cancelled wake #{id}"))).await,
                        Err(e) => self.answer_callback(cid, Some(&format!("Cannot cancel #{id}: {e}"))).await,
                    }
                } else {
                    self.answer_callback(cid, Some("Updated")).await;
                }
                let (text, markup) = self.schedule_card().await;
                if let Some(mid) = message["message_id"].as_i64() {
                    if let Err(e) = self.tg1("editMessageText", json!({"chat_id": ctx.chat_id, "message_id": mid, "text": text,
                            "disable_web_page_preview": true, "reply_markup": markup.unwrap_or(json!({"inline_keyboard": []}))})).await {
                        log(format!("edit schedule failed: {e}"));
                    }
                }
                return Ok(());
            } else if data == "list" || data.starts_with("list:") {
                page = data.strip_prefix("list:").and_then(|p| p.parse().ok()).unwrap_or(0);
                self.answer_callback(cid, Some("Updated")).await;
            } else {
                self.answer_callback(cid, Some("Unknown action")).await;
                return Ok(());
            }
            let (text, markup) = self.session_overview(&ctx, page).await?;
            if let Some(mid) = message["message_id"].as_i64() {
                let mut p = json!({"chat_id": ctx.chat_id, "message_id": mid, "text": text});
                if let Some(m) = &markup {
                    p["reply_markup"] = m.clone();
                }
                match self.tg1("editMessageText", p).await {
                    Ok(_) => return Ok(()),
                    Err(e) => log(format!("edit list failed: {e}")),
                }
            }
            self.send_markup(&ctx, &ctx.chat_id, &text, markup).await?;
            return Ok(());
        };
        if let Err((label, e)) = res {
            log(format!("{} callback failed: {e}", label.split(' ').next().unwrap_or("").to_lowercase()));
            self.answer_callback(cid, Some(label)).await;
        }
        Ok(())
    }

    // ------------------------------------------------------------ the update loop

    fn enqueue_update_job(&self, update_id: i64, entry: &Value, ack: bool) {
        let ctx = self.ctx(entry["chat_id"].clone(), entry["thread_id"].as_i64(), Some(update_id));
        let kind = if entry["kind"] == "attachment" {
            JobKind::Attachment { attachment: entry["attachment"].clone(), caption: entry["caption"].as_str().unwrap_or("").to_string() }
        } else {
            JobKind::Text { text: entry["text"].as_str().unwrap_or("").to_string(), ack }
        };
        let label = format!("{} update {update_id}", if entry["kind"] == "attachment" { "attachment" } else { "message" });
        self.enqueue(Job { ctx, label, kind, override_session: None });
    }

    fn replay_pending_updates(&self) {
        let pending: Vec<(i64, Value)> = self.with_state(|st| st.get("pending_updates").and_then(|p| p.as_object()).map(|m| {
            m.iter().filter_map(|(k, v)| k.parse::<i64>().ok().map(|k| (k, v.clone()))).collect()
        }).unwrap_or_default());
        let mut pending = pending;
        pending.sort_by_key(|(k, _)| *k);
        for (k, entry) in pending {
            log(format!("replaying unposted update {k}"));
            self.enqueue_update_job(k, &entry, false);
        }
    }

    async fn initial_offset(&self) -> i64 {
        if let Some(o) = self.with_state(|st| st.get("offset").and_then(Value::as_i64)) {
            return o;
        }
        let r = self.tg("getUpdates", Some(json!({"offset": -1, "timeout": 0, "allowed_updates": ALLOWED_UPDATES})), 60, Some(120)).await;
        match r {
            Ok(Value::Array(u)) if !u.is_empty() => u.iter().filter_map(|x| x["update_id"].as_i64()).max().unwrap_or(-1) + 1,
            _ => 0,
        }
    }

    fn set_offset(&self, offset: i64) {
        self.with_state(|st| st.insert("offset".into(), json!(offset)));
        self.save_state();
    }

    pub(crate) async fn run_forever(self: Arc<Self>) {
        let mut offset = self.initial_offset().await;
        self.set_offset(offset);
        log(format!("bridge started; session={} chat={}", self.current_session(&self.dm()).unwrap_or_else(|| "None".into()), self.cfg.chat_id));
        match self.sync_bot_commands().await {
            Ok(()) => log("Telegram command menu synced"),
            Err(e) => log(format!("Telegram command menu sync failed: {e}")),
        }
        let broadcaster = {
            let b = self.clone();
            async move { b.broadcast_loop().await }
        };
        self.replay_pending_updates();
        let poller = async {
            let mut failures: u32 = 0;
            loop {
                let mut current: Option<i64> = None;
                let r: Result<()> = async {
                    let updates = self.tg("getUpdates", Some(json!({"offset": offset, "timeout": 50, "allowed_updates": ALLOWED_UPDATES})), 60, Some(0)).await?;
                    failures = 0;
                    if let Some(o) = self.take_outage() {
                        let dm = self.dm();
                        self.send_notice(&dm, &self.chat_id(), &outage_notice_text(o)).await;
                    }
                    for update in updates.as_array().cloned().unwrap_or_default() {
                        let update_id = update["update_id"].as_i64().unwrap_or(0);
                        current = Some(update_id);
                        offset = update_id + 1;
                        self.set_offset(offset);
                        self.handle_update(update_id, &update).await?;
                        self.set_offset(offset);
                    }
                    Ok(())
                }.await;
                if let Err(e) = r {
                    log(format!("poll failed: {e}"));
                    if let Some(c) = current.filter(|c| is_network_error(&e) && !self.is_pending(*c)) {
                        // Never skip an update we could not finish handling: fetch it again.
                        offset = c;
                        self.set_offset(c);
                        log(format!("rewound offset to unhandled update {c}"));
                    }
                    failures += 1;
                    tokio::time::sleep(Duration::from_secs((5 * 2u64.pow(failures.saturating_sub(1).min(3))).min(30))).await;
                }
            }
        };
        tokio::join!(broadcaster, poller);
    }

    async fn handle_update(self: &Arc<Self>, update_id: i64, update: &Value) -> Result<()> {
        if let Some(cb) = update.get("callback_query").filter(|c| !c.is_null()) {
            if let Err(e) = self.handle_callback(cb).await {
                if is_network_error(&e) {
                    return Err(e);
                }
                log(format!("callback handling failed: {e}"));
            }
            return Ok(());
        }
        if let Some(m) = update.get("my_chat_member").filter(|c| !c.is_null()) {
            let chat = &m["chat"];
            log(format!("membership update chat={} type={} title={} status={}", vstr(&chat["id"]), vstr(&chat["type"]), vstr(&chat["title"]), vstr(&m["new_chat_member"]["status"])));
            self.with_state(|st| {
                let chats = st.entry("chats").or_insert(json!({}));
                chats[vstr(&chat["id"])] = json!({"type": chat["type"], "title": chat["title"], "status": m["new_chat_member"]["status"]});
            });
            self.save_state();
            return Ok(());
        }
        let is_channel_post = update.get("channel_post").is_some_and(|c| !c.is_null());
        let mut message = update.get("message").filter(|m| !m.is_null()).or(update.get("channel_post")).cloned().unwrap_or(json!({}));
        let chat_id = message["chat"]["id"].clone();
        let ctx = self.ctx(chat_id.clone(), message["message_thread_id"].as_i64(), Some(update_id));
        let origin_chat = message["forward_origin"]["chat"].clone();
        let origin_chat = if origin_chat.is_object() { origin_chat } else { message["forward_from_chat"].clone() };
        if !origin_chat["id"].is_null() {
            log(format!("forwarded from chat={} type={} title={}", vstr(&origin_chat["id"]), vstr(&origin_chat["type"]), vstr(&origin_chat["title"])));
            self.with_state(|st| {
                let chats = st.entry("chats").or_insert(json!({}));
                chats[vstr(&origin_chat["id"])] = json!({"type": origin_chat["type"], "title": origin_chat["title"]});
            });
            self.save_state();
        }
        let mut text = message["text"].as_str().map(String::from);
        let attachment = attachment_from_message(&message);
        if is_channel_post {
            if message["chat"]["type"] != "channel" {
                return Ok(());
            }
            let explicit = text.as_deref().unwrap_or("").trim().starts_with('/');
            if !explicit && !self.bot_mentioned(&message).await {
                log(format!("ignoring channel post {update_id} chat={} (bot not mentioned)", vstr(&chat_id)));
                return Ok(());
            }
            let clean_text = self.strip_bot_mention(text.as_deref()).await;
            let clean_caption = self.strip_bot_mention(message["caption"].as_str()).await;
            if clean_text != text || clean_caption.as_deref() != message["caption"].as_str() {
                message["text"] = json!(clean_text);
                message["caption"] = json!(clean_caption);
                text = clean_text;
            }
        } else if !self.allowed_sender(&chat_id, &message["from"]) {
            return Ok(());
        }
        if let Some(att) = attachment {
            log(format!("incoming attachment update {update_id} chat={} name={}", vstr(&chat_id), vstr(&att["name"])));
            let entry = json!({"kind": "attachment", "chat_id": chat_id, "thread_id": message["message_thread_id"], "attachment": att,
                               "caption": message["caption"].as_str().unwrap_or("")});
            self.record_pending(update_id, entry.clone());
            if self.work_pending() {
                self.send_notice(&ctx, &chat_id, "⏳ Attachment queued. Tap /send_now to send it now.").await;
            }
            self.enqueue_update_job(update_id, &entry, false);
            return Ok(());
        }
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            if vstr(&chat_id) == self.cfg.chat_id || message["chat"]["type"] == "private" {
                self.send_text(&ctx, &chat_id, "Send text, a photo, or a file.").await?;
            }
            return Ok(());
        };
        log(format!("incoming update {update_id} chat={} chars={}", vstr(&chat_id), clen(&text)));
        if text.trim_start().starts_with('/') {
            if let Err(e) = self.handle_text(&ctx, &text, None, true, true, false).await {
                if is_network_error(&e) {
                    return Err(e);
                }
                log(format!("message handling failed: {e}"));
                let _ = self.send_text(&ctx, &chat_id, &format!("Bridge error: {e}")).await;
            }
        } else if self.answer_question_with_text(&ctx, &message, &text).await {
            log(format!("update {update_id} answered a pending question"));
        } else {
            let text = quoted_message_text(&message, &text);
            let pending = self.work_pending();
            let entry = json!({"kind": "text", "chat_id": chat_id, "thread_id": message["message_thread_id"], "text": text});
            // Kept on disk until posted to its session, so a failed reply or a restart cannot lose it.
            self.record_pending(update_id, entry.clone());
            if pending {
                self.send_notice(&ctx, &chat_id, "⏳ Queued — finishing the current task first. Tap /send_now to send it now.").await;
            }
            // Otherwise the worker acknowledges once it knows whether the session is free or busy.
            self.enqueue_update_job(update_id, &entry, !pending);
        }
        Ok(())
    }
}

const ALLOWED_UPDATES: &str = r#"["message", "channel_post", "my_chat_member", "callback_query"]"#;

// ------------------------------------------------------------------ helpers

/// The first N.N[.N] in a version string.
pub fn short_version(text: &str) -> String {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            let mut dots = 0;
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || (b[j] == b'.' && dots < 2 && j + 1 < b.len() && b[j + 1].is_ascii_digit())) {
                if b[j] == b'.' {
                    dots += 1;
                }
                j += 1;
            }
            if dots >= 1 {
                return text[start..j].to_string();
            }
            i = j;
        } else {
            i += 1;
        }
    }
    String::new()
}

pub fn page_bounds(total: usize, page: i64) -> (i64, usize) {
    let pages = total.div_ceil(LIST_PAGE_SIZE).max(1) as i64;
    let page = page.clamp(0, pages - 1);
    (page, page as usize * LIST_PAGE_SIZE)
}

/// A folder for a phone-width line: ~ for home, and only the last parts when long.
pub fn short_path(path: &str, home: Option<&str>) -> String {
    let mut p = path.trim_end_matches('/').to_string();
    if let Some(h) = home.map(|h| h.trim_end_matches('/')).filter(|h| !h.is_empty()) {
        if p == h {
            p = "~".into();
        } else if let Some(rest) = p.strip_prefix(&format!("{h}/")) {
            p = format!("~/{rest}");
        }
    }
    if clen(&p) <= 40 {
        return p;
    }
    let parts: Vec<&str> = p.split('/').filter(|x| !x.is_empty()).collect();
    let mut tail: Vec<&str> = vec![];
    for part in parts.iter().rev() {
        let len: usize = tail.iter().map(|t| clen(t) + 1).sum::<usize>() + clen(part);
        if !tail.is_empty() && len > 36 {
            break;
        }
        tail.insert(0, part);
    }
    format!("…/{}", tail.join("/"))
}

/// Relative time to a future (or overdue) unix time: "in 2h 05m", "in 3d 4h", "overdue 5m".
pub fn relative_to(at: i64, now: i64) -> String {
    let d = at - now;
    let s = d.abs();
    let (days, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    let span = if days > 0 { format!("{days}d {h}h") } else if h > 0 { format!("{h}h {m:02}m") } else if m > 0 { format!("{m}m") } else { format!("{s}s") };
    if d >= 0 { format!("in {span}") } else { format!("overdue {span}") }
}

/// Text and Cancel buttons for /schedule from the server's wake rows (pending only, soonest first).
pub fn schedule_text(wakes: &[Value], now: i64) -> (String, Option<Value>) {
    let mut pending: Vec<&Value> = wakes.iter().filter(|w| w["status"].as_str().unwrap_or("pending") == "pending").collect();
    pending.sort_by_key(|w| w["at"].as_i64().unwrap_or(i64::MAX));
    if pending.is_empty() {
        return ("⏰ No pending wakes.".into(), Some(json!({"inline_keyboard": [[{"text": "🔄 Refresh", "callback_data": "sched"}]]})));
    }
    let mut lines = vec![format!("⏰ Pending wakes ({})", pending.len()), String::new()];
    let mut buttons = vec![];
    for w in pending {
        let id = w["id"].as_i64().unwrap_or(0);
        let target = w["session_name"].as_str().filter(|n| !n.is_empty()).or(w["session"].as_str()).unwrap_or("?");
        let at = w["at"].as_i64().unwrap_or(0);
        let local = w["at_local"].as_str().map(String::from).unwrap_or_else(|| crate::wakes::show(at));
        lines.push(format!("#{id} → {target} · {local} ({})", relative_to(at, now)));
        if let Some(r) = w["repeat"].as_str().filter(|r| !r.is_empty()) {
            lines.push(format!("   🔁 {r}"));
        }
        let mut msg = squash(w["message"].as_str().unwrap_or(""));
        if clen(&msg) > 80 {
            msg = format!("{}…", take(&msg, 79));
        }
        lines.push(format!("   {msg}"));
        buttons.push(json!([{"text": format!("✖ Cancel #{id} ({})", take(target, 20)), "callback_data": format!("wc:{id}")}]));
    }
    buttons.push(json!([{"text": "🔄 Refresh", "callback_data": "sched"}]));
    (lines.join("\n"), Some(json!({"inline_keyboard": buttons})))
}

pub fn duration_ago(ts: f64) -> String {
    let s = (now_f() - ts).max(0.0) as i64;
    let (d, r) = (s / 86400, s % 86400);
    let (h, r) = (r / 3600, r % 3600);
    let (m, sec) = (r / 60, r % 60);
    format!("{d}d {h:02}h {m:02}m {sec:02}s ago")
}

/// Phone-width model name: claude-opus-5-5 -> opus-5.5.
pub fn short_model(model: &str) -> String {
    let name = model.trim();
    if name.is_empty() {
        return "default".into();
    }
    let mut name = name.rsplit('/').next().unwrap_or(name).to_string();
    if let Some(n) = name.strip_prefix("claude-") {
        name = n.to_string();
    }
    // strip -YYYYMMDD
    if name.len() > 9 {
        let (head, tail) = name.split_at(name.len() - 9);
        if tail.starts_with('-') && tail[1..].chars().all(|c| c.is_ascii_digit()) {
            name = head.to_string();
        }
    }
    // -(\d+)-(\d+)$ -> -\1.\2
    let parts: Vec<&str> = name.rsplitn(3, '-').collect();
    if parts.len() == 3 && !parts[0].is_empty() && !parts[1].is_empty() && parts[0].chars().all(|c| c.is_ascii_digit()) && parts[1].chars().all(|c| c.is_ascii_digit()) {
        return format!("{}-{}.{}", parts[2], parts[1], parts[0]);
    }
    name
}

pub fn harness_label(snap: &Value, session: Option<&Value>) -> Option<String> {
    let raw = snap["kurisu_harness"].as_str().filter(|h| !h.is_empty())
        .or(snap["harness"].as_str().filter(|h| !h.is_empty()))
        .or(session.and_then(|s| s["harness"].as_str()).filter(|h| !h.is_empty()))?;
    Some(if raw == "pi-native" { "pi".into() } else { raw.to_string() })
}

fn price_for(model: &str) -> Option<(f64, f64)> {
    let n = model.to_lowercase();
    [("opus", (5.0, 25.0)), ("sonnet", (3.0, 15.0)), ("haiku", (1.0, 5.0))].into_iter().find(|(f, _)| n.contains(f)).map(|(_, p)| p)
}

/// API-equivalent spend, or None.
pub fn estimated_cost(snap: &Value) -> Option<f64> {
    if let Some(t) = snap["total_cost_usd"].as_f64().filter(|t| *t > 0.0) {
        return Some(t);
    }
    let usage = snap["usage_by_model"].as_object()?;
    let (mut cost, mut priced) = (0.0, false);
    for (model, u) in usage {
        let Some((inp, out)) = price_for(model) else { continue };
        let n = |k: &str| u[k].as_f64().unwrap_or(0.0);
        let tokens = n("input_tokens") + n("output_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
        if tokens == 0.0 {
            continue;
        }
        priced = true;
        cost += (n("input_tokens") * inp + n("output_tokens") * out + n("cache_creation_input_tokens") * inp * 1.25 + n("cache_read_input_tokens") * inp * 0.1) / 1_000_000.0;
    }
    priced.then_some(cost)
}

pub fn short_tokens(count: f64) -> String {
    let c = count as i64;
    if c >= 1_000_000 {
        let t = format!("{:.1}", c as f64 / 1_000_000.0);
        let t = t.trim_end_matches('0').trim_end_matches('.');
        return format!("{t}M");
    }
    if c >= 1000 {
        return format!("{}K", (c as f64 / 1000.0).round_ties_even() as i64);
    }
    c.to_string()
}

pub fn context_label(snap: &Value) -> Option<String> {
    let used = snap["last_total_tokens"].as_f64();
    let window = snap["context_window"].as_f64().filter(|w| *w > 0.0);
    if snap["context_pending"] == json!(true) && used.is_none() {
        return Some(match window {
            Some(w) => format!("pending/{}", short_tokens(w)),
            None => "pending".into(),
        });
    }
    let used = used.filter(|u| *u > 0.0)?;
    Some(match window {
        Some(w) => format!("{}/{}", short_tokens(used), short_tokens(w)),
        None => short_tokens(used),
    })
}

pub fn short_list(names: &[String], limit: usize) -> String {
    if names.len() <= limit {
        return names.join(", ");
    }
    format!("{} +{}", names[..limit].join(", "), names.len() - limit)
}

pub fn format_context_usage(snap: &Value) -> String {
    let used = snap["last_total_tokens"].as_i64().filter(|u| *u >= 0);
    let window = snap["context_window"].as_i64().filter(|w| *w > 0);
    if snap["context_pending"] == json!(true) && used.is_none() {
        return match window {
            Some(w) => format!("pending until the next turn (window: {} tokens)", fmt_int(w)),
            None => "pending until the next turn".into(),
        };
    }
    let Some(used) = used else {
        return match window {
            Some(w) => format!("not reported (window: {} tokens)", fmt_int(w)),
            None => "not reported yet".into(),
        };
    };
    match window {
        Some(w) => format!("{} / {} tokens ({:.1}% used)", fmt_int(used), fmt_int(w), used as f64 * 100.0 / w as f64),
        None => format!("{} tokens used (window unknown)", fmt_int(used)),
    }
}

/// Effort choices on step 3 of /model: "default" (keep the current setting), then the
/// harness's levels. Index 0 is default; i>0 is effort_levels(harness)[i-1].
pub fn effort_choices(harness: &str) -> Vec<&'static str> {
    let h = if harness == "codex" { "codex" } else { "claude" };
    std::iter::once("default").chain(crate::store::effort_levels(h).iter().copied()).collect()
}

/// Text and keyboard for step 3 (effort) of /model.
fn effort_step(tok: String, harness: &str, model: &str, current: Option<&str>) -> (String, Value) {
    let cur = current.filter(|c| !c.is_empty()).unwrap_or("default");
    let text = format!("Step 3 of 3 — effort for {}\nCurrent effort: {cur}\nApplied from the next turn; \"default\" keeps the current setting.",
                       if model.is_empty() { format!("{harness} default model") } else { model.to_string() });
    let row: Vec<Value> = effort_choices(harness).iter().enumerate()
        .map(|(i, e)| json!({"text": format!("{}{e}", if *e == cur && i > 0 { "✓ " } else { "" }), "callback_data": format!("me:{tok}:{i}")})).collect();
    (text, json!({"inline_keyboard": row.chunks(3).collect::<Vec<_>>()}))
}

/// The effort picked on step 3 (None = default / keep).
fn picked_effort(card: &Value, data: &str) -> Option<String> {
    let h = card["harness"].as_str().filter(|h| !h.is_empty()).unwrap_or(card["current_harness"].as_str().unwrap_or("claude"));
    let i: usize = data.strip_prefix("me:")?.rsplit(':').next()?.parse().ok()?;
    effort_choices(h).get(i).filter(|e| **e != "default").map(|e| e.to_string())
}

pub fn model_update_text(snap: &Value, requested: &str) -> String {
    let reset = ["default", "off", "reset"].contains(&requested.trim().to_lowercase().as_str());
    let selection = if reset { "agent default".to_string() } else { snap["model_override"].as_str().filter(|m| !m.is_empty()).unwrap_or(requested).to_string() };
    let title = squash(snap["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)"));
    let sid = snap["id"].as_str().unwrap_or("?");
    format!("✅ Model updated for {title} ({}).\nModel: {selection}\nUsed for subsequent turns.", take(sid, 8))
}

fn countdown(seconds: i64) -> String {
    let s = seconds.max(0);
    let (d, r) = (s / 86400, s % 86400);
    let (h, r) = (r / 3600, r % 3600);
    let m = r / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

fn usage_age(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 90 {
        format!("{s}s")
    } else if s < 5400 {
        format!("{} min", (s as f64 / 60.0).round_ties_even() as i64)
    } else {
        countdown(s)
    }
}

/// claude-monitor's local estimate (cost, tokens, burn rate), if it is installed.
pub async fn local_usage_estimate() -> Option<Value> {
    let exe = crate::stt::which_bin("claude-monitor")
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".local/bin/claude-monitor")))?;
    let out = tokio::time::timeout(Duration::from_secs(6),
        tokio::process::Command::new(exe).args(["--once", "--output", "json", "--plan", "pro"]).kill_on_drop(true).output()).await;
    let out = match out {
        Ok(Ok(o)) => o,
        other => {
            log(format!("claude-monitor estimate skipped: {}", if other.is_err() { "timeout".to_string() } else { "not runnable".to_string() }));
            return None;
        }
    };
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let local = v.get("local").filter(|l| l.is_object())?;
    let (cost, tokens, burn) = (local["cost_usd"].clone(), local["tokens"]["total_tokens"].clone(), local["burn_rate_cost_per_hour"].clone());
    if ![&cost, &tokens, &burn].iter().any(|x| x.is_number()) {
        return None;
    }
    Some(json!({"cost_usd": cost, "total_tokens": tokens, "burn_cost_per_hour": burn}))
}

/// Plan limits from the server's `usage` op, shaped for /usage (percent, reset, staleness).
pub fn shape_official(r: &Value, now: i64) -> Value {
    let mut out = json!({"source": "kurisu_statusline", "confidence": "unavailable", "captured_at": null, "age_seconds": null,
                         "stale": true, "five_hour": null, "seven_day": null, "sessions_seen": 0});
    out["sessions_seen"] = json!(r["sessions"].as_object().map(|m| m.len()).unwrap_or(0));
    let limits = &r["rate_limits"];
    if !limits.is_object() {
        return out;
    }
    out["confidence"] = json!("official");
    if let Some(c) = limits["captured_at"].as_f64() {
        out["captured_at"] = json!(c as i64);
        out["age_seconds"] = json!(now - c as i64);
    }
    for key in ["five_hour", "seven_day"] {
        let w = &limits[key];
        let (Some(pct), Some(rst)) = (w["used_percentage"].as_f64(), w["resets_at"].as_f64()) else { continue };
        let rst = rst as i64;
        out[key] = json!({"used_percentage": w["used_percentage"], "pct": pct, "resets_at": rst, "window_over": rst <= now,
                          "resets_in_seconds": (rst - now).max(0)});
    }
    let age = out["age_seconds"].as_i64();
    let over = ["five_hour", "seven_day"].iter().any(|k| out[*k]["window_over"] == json!(true));
    out["stale"] = json!(age.is_none_or(|a| a > USAGE_STALE_AFTER_S) || over);
    out
}

/// One line: "Claude: 5h 34% (resets 23:00, in 1h 02m) · 7d 12% (resets Mon 10 Nov 09:00, in 2d 11h)".
pub fn usage_summary(official: &Value, now: i64) -> String {
    let mut parts = vec![];
    for (key, label, f) in [("five_hour", "5h", "%H:%M"), ("seven_day", "7d", "%a %d %b %H:%M")] {
        let w = &official[key];
        if !w.is_object() {
            continue;
        }
        let pct = fmt_g(w["used_percentage"].as_f64().unwrap_or(0.0));
        if w["window_over"] == json!(true) {
            parts.push(format!("{label} {pct}% (window already reset)"));
        } else {
            let rst = w["resets_at"].as_i64().unwrap_or(0);
            parts.push(format!("{label} {pct}% (resets {}, in {})", local_fmt(rst, f), countdown(rst - now)));
        }
    }
    if parts.is_empty() {
        return "Claude: no plan limits reported yet (they appear after a Claude session's first turn on a subscription login); Codex: not tracked".into();
    }
    let stale = if official["stale"] == json!(true) { " ⚠️ stale" } else { "" };
    format!("Claude: {}{stale}; Codex: not tracked", parts.join(" · "))
}

/// The /usage report: what Telegram shows (`text`), a one-line `summary`, and the data.
pub fn usage_report(raw: &Value, estimate: Option<Value>, now: i64) -> Value {
    let official = shape_official(raw, now);
    json!({
        "summary": usage_summary(&official, now),
        "text": format_usage(&official, estimate.as_ref(), now),
        "harnesses": {
            "claude": official,
            "codex": {"confidence": "unavailable", "note": "Kurisu does not track Codex plan limits"}
        },
        "local_estimate": estimate,
    })
}

pub fn format_usage(official: &Value, estimate: Option<&Value>, now: i64) -> String {
    let mut lines = vec!["📊 Claude usage (Pro, official)".to_string()];
    let five = official["five_hour"].is_object();
    let seven = official["seven_day"].is_object();
    if official["confidence"] == "unavailable" || (!five && !seven) {
        lines.push("No live Claude session has reported limits yet.".into());
    } else {
        for (key, label, f) in [("five_hour", "5-hour", "%H:%M"), ("seven_day", "7-day", "%a %d %b %H:%M")] {
            let w = &official[key];
            if !w.is_object() {
                lines.push(format!("{label}: not reported"));
                continue;
            }
            let pct = fmt_g(w["used_percentage"].as_f64().unwrap_or(0.0));
            if w["window_over"] == json!(true) {
                lines.push(format!("{label}: {pct}% used (window already reset; figure is old)"));
                continue;
            }
            let rst = w["resets_at"].as_i64().unwrap_or(0);
            lines.push(format!("{label}: {pct}% used · resets {} (in {})", local_fmt(rst, f), countdown(rst - now)));
        }
        if let Some(age) = official["age_seconds"].as_i64() {
            if official["stale"] == json!(true) {
                lines.push(format!("⚠️ Stale — last known {} ago.", usage_age(age)));
            } else {
                lines.push(format!("Captured {} ago.", usage_age(age)));
            }
        }
    }
    if let Some(e) = estimate {
        lines.push(String::new());
        lines.push("Local estimate (this machine's Claude Code logs)".into());
        if let Some(c) = e["cost_usd"].as_f64() {
            lines.push(format!("API-price equivalent: ${} (estimate; not billed on Pro)", fmt_money(c)));
        }
        if let Some(t) = e["total_tokens"].as_f64() {
            lines.push(format!("Tokens: {} (estimate)", fmt_int(t as i64)));
        }
        if let Some(b) = e["burn_cost_per_hour"].as_f64() {
            lines.push(format!("Burn rate: ${}/h (estimate)", fmt_money(b)));
        }
    }
    lines.join("\n")
}

pub fn mime_for_path(path: &str) -> String {
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "gif" => "image/gif", "jpeg" | "jpg" => "image/jpeg", "png" => "image/png", "webp" => "image/webp",
        "pdf" => "application/pdf", "txt" => "text/plain", "md" => "text/markdown", "json" => "application/json",
        "csv" => "text/csv", "html" | "htm" => "text/html", "zip" => "application/zip",
        "mp3" => "audio/mpeg", "ogg" | "oga" | "opus" => "audio/ogg", "wav" => "audio/x-wav", "m4a" => "audio/mp4",
        "flac" => "audio/flac", "mp4" => "video/mp4", "webm" => "video/webm",
        _ => "application/octet-stream",
    }.to_string()
}

pub fn attachment_from_message(message: &Value) -> Option<Value> {
    if let Some(photos) = message["photo"].as_array().filter(|p| !p.is_empty()) {
        let best = photos.iter().max_by_key(|p| (p["file_size"].as_i64().unwrap_or(0), p["width"].as_i64().unwrap_or(0) * p["height"].as_i64().unwrap_or(0)))?;
        if let Some(fid) = best["file_id"].as_str() {
            return Some(json!({"file_id": fid, "name": "photo.jpg", "mime": "image/jpeg"}));
        }
    }
    for (key, fallback) in [("document", "file"), ("audio", "audio.mp3"), ("voice", "voice.ogg"), ("video", "video.mp4"),
                            ("video_note", "video-note.mp4"), ("animation", "animation.mp4")] {
        let media = &message[key];
        let Some(fid) = media["file_id"].as_str().filter(|f| !f.is_empty()) else { continue };
        let name = media["file_name"].as_str().filter(|n| !n.is_empty()).unwrap_or(fallback).to_string();
        let mime = media["mime_type"].as_str().map(str::to_lowercase).filter(|m| !m.is_empty()).unwrap_or_else(|| mime_for_path(&name));
        return Some(json!({"file_id": fid, "name": name, "mime": mime}));
    }
    None
}

pub fn is_audio_attachment(name: &str, mime: &str) -> bool {
    let ext = std::path::Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    mime.starts_with("audio/") || ["ogg", "oga", "opus", "mp3", "m4a", "wav", "flac", "aac", "webm"].contains(&ext.as_str())
}

/// A file name Kurisu's speech-to-text accepts (an audio extension).
fn audio_name(name: &str, mime: &str) -> String {
    if crate::stt::is_audio(name) {
        return name.to_string();
    }
    let ext = match mime {
        "audio/mpeg" => "mp3", "audio/mp4" | "audio/x-m4a" => "m4a", "audio/wav" | "audio/x-wav" => "wav",
        "audio/flac" => "flac", "audio/aac" => "aac", "audio/webm" => "webm", _ => "ogg",
    };
    format!("{name}.{ext}")
}

pub fn voice_transcript_text(name: &str, path: &str, transcript: &str, caption: &str) -> String {
    let mut text = format!("The user sent a voice/audio message (\"{name}\"). Local speech-to-text transcript:\n\n{}\n\n(Audio file: {path} — transcription may contain mistakes.)",
                           if transcript.is_empty() { "(no speech detected)" } else { transcript });
    if !caption.is_empty() {
        text.push_str(&format!("\n\nUser note: {caption}"));
    }
    text
}

/// A top-level `key = "value"` from a TOML file (before any [table]).
fn toml_top_level_string(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            return None;
        }
        if let Some((k, v)) = l.split_once('=') {
            if k.trim() == key {
                let v = v.trim();
                let v = v.split(" #").next().unwrap_or(v).trim();
                return Some(v.trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn usage_report_matches_telegram_and_summarizes() {
        let now = 1_000_000;
        let raw = json!({"sessions": {"a": {}, "b": {}}, "rate_limits": {"captured_at": now - 60,
            "five_hour": {"used_percentage": 34.0, "resets_at": now + 3720}, "seven_day": {"used_percentage": 12.5, "resets_at": now + 2 * 86400}}});
        let r = usage_report(&raw, Some(json!({"cost_usd": 1.5, "total_tokens": 1000, "burn_cost_per_hour": 0.5})), now);
        let official = shape_official(&raw, now);
        assert_eq!(r["text"], format_usage(&official, r["local_estimate"].as_object().map(|_| &r["local_estimate"]), now));
        assert_eq!(r["harnesses"]["claude"]["five_hour"]["pct"], 34.0);
        assert_eq!(r["harnesses"]["claude"]["sessions_seen"], 2);
        let s = r["summary"].as_str().unwrap();
        assert!(s.starts_with("Claude: 5h 34% (resets ") && s.contains("in 1h") && s.contains("7d 12.5%") && !s.contains('\n'), "{s}");
        let empty = usage_report(&json!({"sessions": {}, "rate_limits": null}), None, now);
        assert!(empty["summary"].as_str().unwrap().contains("no plan limits reported yet"));
        assert!(empty["text"].as_str().unwrap().contains("No live Claude session has reported limits yet."));
    }

    #[test]
    fn short_path_uses_tilde_and_tail() {
        assert_eq!(short_path("/home/me/proj", Some("/home/me")), "~/proj");
        assert_eq!(short_path("/home/me/", Some("/home/me")), "~");
        assert_eq!(short_path("/srv/x", Some("/home/me")), "/srv/x");
        assert_eq!(short_path("/home/meow/x", Some("/home/me")), "/home/meow/x");
        let long = "/run/media/deck/SDCARD/distrobox/omni/dev/kurisu-live-test/kurisu-home/workspaces/kurisu-dev3";
        let s = short_path(long, None);
        assert!(s.starts_with("…/") && s.ends_with("workspaces/kurisu-dev3") && clen(&s) <= 40, "{s}");
        assert_eq!(short_path("/a/this-is-one-very-long-folder-name-without-any-slashes-at-all", None).matches('/').count(), 1);
    }

    #[test]
    fn schedule_lists_pending_wakes_soonest_first() {
        let now = 1_000_000;
        let wakes = vec![
            serde_json::json!({"id": 7, "session_name": "lead", "at": now + 7500, "at_local": "2026-11-08 06:00 EST", "repeat": "0 6 * * *",
                               "message": "daily standup ".repeat(10), "status": "pending"}),
            serde_json::json!({"id": 3, "session_name": "w1", "at": now + 300, "message": "check CI", "status": "pending"}),
            serde_json::json!({"id": 1, "session_name": "w1", "at": now - 10, "message": "old", "status": "fired"}),
        ];
        let (text, kb) = schedule_text(&wakes, now);
        let i3 = text.find("#3 → w1").unwrap();
        let i7 = text.find("#7 → lead · 2026-11-08 06:00 EST (in 2h 05m)").unwrap();
        assert!(i3 < i7, "{text}");
        assert!(text.contains("(in 5m)") && text.contains("🔁 0 6 * * *") && text.contains("…") && !text.contains("#1 "), "{text}");
        let kb = kb.unwrap();
        assert_eq!(kb["inline_keyboard"][0][0]["callback_data"], "wc:3");
        assert_eq!(kb["inline_keyboard"][2][0]["callback_data"], "sched");
        assert!(schedule_text(&[], now).0.contains("No pending wakes"));
        assert_eq!(relative_to(now - 120, now), "overdue 2m");
        assert_eq!(relative_to(now + 3 * 86400 + 4 * 3600, now), "in 3d 4h");
    }

    #[test]
    fn model_picker_effort_step() {
        assert_eq!(effort_choices("claude"), ["default", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(effort_choices("codex"), ["default", "low", "medium", "high", "xhigh"]);
        let (text, kb) = effort_step("T".into(), "codex", "gpt-x", Some("high"));
        assert!(text.contains("Step 3 of 3") && text.contains("Current effort: high"), "{text}");
        let btns: Vec<Value> = kb["inline_keyboard"].as_array().unwrap().iter().flat_map(|r| r.as_array().unwrap().clone()).collect();
        assert_eq!(btns.len(), 5);
        assert_eq!(btns[0]["callback_data"], "me:T:0");
        assert_eq!(btns[3]["text"], "✓ high");
        assert!(btns.iter().all(|b| b["text"] != "max"));
        let card = serde_json::json!({"harness": "", "current_harness": "claude"});
        assert_eq!(picked_effort(&card, "me:T:0"), None);
        assert_eq!(picked_effort(&card, "me:T:5").as_deref(), Some("max"));
        assert_eq!(picked_effort(&serde_json::json!({"harness": "codex"}), "me:T:5"), None);
        assert_eq!(picked_effort(&card, "model:T:1"), None);
    }

    use super::*;

    #[test]
    fn formatting_matches_the_bridge() {
        assert_eq!(short_model("claude-opus-5-5"), "opus-5.5");
        assert_eq!(short_model("claude-haiku-4-5-20251001"), "haiku-4.5");
        assert_eq!(short_model("ollama/deepseek-v4.1-flash:cloud"), "deepseek-v4.1-flash:cloud");
        assert_eq!(short_model(""), "default");
        assert_eq!(short_tokens(230_000.0), "230K");
        assert_eq!(short_tokens(1_000_000.0), "1M");
        assert_eq!(short_tokens(1_500_000.0), "1.5M");
        assert_eq!(short_tokens(500.0), "500");
        assert_eq!(fmt_int(1234567), "1,234,567");
        assert_eq!(fmt_money(4.2), "4.20");
        assert_eq!(fmt_g(38.0), "38");
        assert_eq!(fmt_g(38.25), "38.2");
        assert_eq!(countdown(3 * 3600 + 5 * 60), "3h 05m");
        assert_eq!(countdown(2 * 86400 + 3600), "2d 1h");
        assert_eq!(usage_age(30), "30s");
        assert_eq!(usage_age(600), "10 min");
        assert_eq!(context_label(&json!({"last_total_tokens": 35468, "context_window": 200000})).unwrap(), "35K/200K");
        assert_eq!(context_label(&json!({"context_pending": true, "context_window": 200000})).unwrap(), "pending/200K");
        assert_eq!(format_context_usage(&json!({"last_total_tokens": 35468, "context_window": 200000})), "35,468 / 200,000 tokens (17.7% used)");
        assert_eq!(format_context_usage(&json!({})), "not reported yet");
        assert_eq!(page_bounds(12, 9), (2, 10));
        assert_eq!(short_version("2.1.319 (Claude Code)"), "2.1.319");
        assert_eq!(short_version("codex-cli 0.166.0"), "0.166.0");
        assert_eq!(short_version("v12"), "");
        assert_eq!(page_bounds(0, 3), (0, 0));
        assert_eq!(short_list(&["a".into(), "b".into(), "c".into(), "d".into()], 3), "a, b, c +1");
        assert_eq!(estimated_cost(&json!({"usage_by_model": {"claude-haiku-5-5": {"input_tokens": 1_000_000, "output_tokens": 0}}})), Some(1.0));
        assert_eq!(toml_top_level_string("model = \"deepseek:cloud\"\n[tui]\nmodel = \"x\"", "model").unwrap(), "deepseek:cloud");
        let usage = json!({"confidence": "official", "five_hour": {"used_percentage": 38, "resets_at": 1000 + 3600, "window_over": false},
                           "seven_day": null, "age_seconds": 30, "stale": false});
        let t = format_usage(&usage, None, 1000);
        assert!(t.contains("5-hour: 38% used · resets ") && t.contains("(in 1h 00m)") && t.contains("7-day: not reported") && t.ends_with("Captured 30s ago."), "{t}");
    }

    #[test]
    fn attachments_and_voice() {
        let m = json!({"photo": [{"file_id": "s", "file_size": 10}, {"file_id": "b", "file_size": 99}]});
        assert_eq!(attachment_from_message(&m).unwrap()["file_id"], "b");
        let v = json!({"voice": {"file_id": "v", "mime_type": "audio/ogg"}});
        assert_eq!(attachment_from_message(&v).unwrap(), json!({"file_id": "v", "name": "voice.ogg", "mime": "audio/ogg"}));
        assert!(is_audio_attachment("voice.ogg", "") && is_audio_attachment("x", "audio/mpeg") && !is_audio_attachment("a.txt", "text/plain"));
        assert_eq!(audio_name("rec", "audio/mpeg"), "rec.mp3");
        assert!(voice_transcript_text("voice.ogg", "/p", "", "note").contains("(no speech detected)"));
    }
}
