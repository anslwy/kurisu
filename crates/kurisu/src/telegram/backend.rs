//! The bridge's Kurisu backend, in-process: Kurisu sessions, messages and approvals
//! in the Omnigent shapes the shared bridge logic reads (Python `kurisu_*`).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::{log, now_i, squash, take, token, Bridge, Ctx};

pub const KURISU_MCP_CACHE_TTL_S: u64 = 30;
pub const LB_PATH_REL: &str = ".local/bin/lb";

fn kurisu_status(s: &str) -> &str {
    match s {
        "working" => "running",
        "starting" => "starting",
        "awaiting_approval" => "waiting",
        "error" => "failed",
        "idle" | "new" | "stopped" => "idle",
        other => other,
    }
}

fn kurisu_harness(h: &str) -> &str {
    match h {
        "claude" => "claude-native",
        other => other,
    }
}

/// A Kurisu Approval as an Omnigent pending_elicitations event.
pub fn approval_event(a: &Value) -> Value {
    let mut params = json!({
        "message": a["title"].as_str().filter(|s| !s.is_empty()).or(a["tool"].as_str()).unwrap_or("Approval required"),
        "content_preview": a["preview"].as_str().unwrap_or(""),
        "target_session_id": a["session"],
    });
    if a["kind"] == "question" {
        if let Some(qs) = a["questions"].as_array() {
            let qs: Vec<Value> = qs.iter().filter(|q| q.is_object()).map(|q| json!({
                "question": q["question"], "header": q["header"],
                "options": if q["options"].is_array() { q["options"].clone() } else { json!([]) },
                "multiSelect": q["multiSelect"] == json!(true),
            })).collect();
            params["ask_user_question"] = json!({"questions": qs});
        }
    }
    json!({"elicitation_id": a["id"], "params": params, "expires_at": a["expires_at"]})
}

/// A Kurisu Session in the Omnigent field names the bridge reads.
pub fn session_dict(s: &Value, approvals: &[Value]) -> Value {
    let model = s["model"].clone();
    let pending: Vec<&Value> = approvals.iter().filter(|a| a["session"] == s["id"]).collect();
    let usage = s["usage"].as_object();
    let status = s["status"].as_str().unwrap_or("");
    let mut out = json!({
        "id": s["id"], "title": s["name"], "name": s["name"],
        "status": if status.is_empty() { "idle".to_string() } else { kurisu_status(status).to_string() },
        "kurisu_status": s["status"], "role": s["role"],
        "parent_session_id": s["created_by_session"],
        "note": s["note"],
        "created_at": s["created_at"],
        "updated_at": if s["last_activity"].is_null() || s["last_activity"] == json!(0) { s["created_at"].clone() } else { s["last_activity"].clone() },
        "last_user_at": s["last_user_at"],
        "model_override": model, "llm_model": model,
        "reasoning_effort": s["effort"],
        "harness": s["harness"].as_str().map(|h| json!(kurisu_harness(h))).unwrap_or(Value::Null),
        "kurisu_harness": s["harness"],
        "permission_mode": s["permission_mode_effective"],
        "workspace": s["cwd"],
        "last_total_tokens": s["context_tokens"],
        "context_window": s["context_window"],
        "total_cost_usd": s["cost_usd"],
        "context_pending": s["context_pending"] == json!(true),
        "usage_by_model": match usage {
            Some(u) => json!({ model.as_str().filter(|m| !m.is_empty()).unwrap_or("unknown"): {
                "input_tokens": u.get("input"), "output_tokens": u.get("output"),
                "cache_read_input_tokens": u.get("cache_read"), "cache_creation_input_tokens": u.get("cache_write"),
            }}),
            None => Value::Null,
        },
        "activity": s["activity"],
        "pending_elicitations_count": pending.len(),
        "pending_elicitations": pending.iter().map(|a| approval_event(a)).collect::<Vec<_>>(),
        "backend": "kurisu",
    });
    if status == "error" {
        let last = &s["last"];
        out["last_task_error"] = json!({"message": if last["role"] == "error" { last["text"].clone() } else { json!("session error") }});
    }
    out
}

/// A Kurisu Message as an Omnigent item (cursor = str(seq)).
pub fn kurisu_item(m: &Value) -> Value {
    let role = m["role"].as_str().unwrap_or("");
    let text = m["text"].as_str().unwrap_or("");
    let mut item = json!({"id": m["seq"].to_string(), "created_at": m["at"]});
    let o = item.as_object_mut().unwrap();
    if role == "agent" {
        o.insert("type".into(), json!("message"));
        o.insert("role".into(), json!("assistant"));
        o.insert("content".into(), json!([{"type": "output_text", "text": text}]));
    } else if role == "user" {
        o.insert("type".into(), json!("message"));
        o.insert("role".into(), json!("user"));
        o.insert("content".into(), json!([{"type": "input_text", "text": text}]));
    } else if let Some(name) = role.strip_prefix("peer:") {
        // Like Omnigent's runtime notices: a turn the bridge did not start.
        o.insert("type".into(), json!("message"));
        o.insert("role".into(), json!("user"));
        o.insert("content".into(), json!([{"type": "input_text", "text": format!("[from {name}] {text}")}]));
    } else if role == "error" {
        o.insert("type".into(), json!("message"));
        o.insert("role".into(), json!("assistant"));
        o.insert("content".into(), json!([{"type": "output_text", "text": format!("⚠️ {text}")}]));
    } else if role == "compaction" {
        o.insert("type".into(), json!("compaction"));
        o.insert("summary".into(), json!(text));
    } else {
        o.insert("type".into(), json!(if role.is_empty() { "event" } else { role }));
        o.insert("text".into(), json!(text));
    }
    item
}

pub fn item_text(item: &Value) -> String {
    let mut parts: Vec<String> = vec![];
    match &item["content"] {
        Value::String(s) => parts.push(s.clone()),
        Value::Array(blocks) => {
            for b in blocks.iter().filter(|b| b.is_object()) {
                let t = [&b["text"], &b["output_text"], &b["input_text"]].into_iter()
                    .find_map(|v| v.as_str().filter(|s| !s.is_empty()));
                if let Some(t) = t.filter(|t| !t.trim().is_empty()) {
                    parts.push(t.to_string());
                }
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        if let Some(t) = item["text"].as_str() {
            parts.push(t.to_string());
        }
    }
    parts.join("\n").trim().to_string()
}

impl Bridge {
    /// One server op, in-process; errors read like the HTTP API's ("HTTP 400: …").
    pub(crate) async fn k(&self, req: Value) -> Result<Value> {
        crate::channels::op(&self.server, req).await.map_err(|e| anyhow!("HTTP 400: {e:#}"))
    }

    pub(crate) async fn pending_approvals(&self, session: Option<&str>) -> Vec<Value> {
        match self.k(json!({"op": "approvals", "all": false, "name": session})).await {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
            Err(e) => {
                log(format!("kurisu approvals failed: {e}"));
                vec![]
            }
        }
    }

    pub(crate) async fn fetch_sessions(&self) -> Result<Vec<Value>> {
        let sessions = self.k(json!({"op": "peers", "include_archived": false})).await?;
        let approvals = self.pending_approvals(None).await;
        Ok(sessions.as_array().into_iter().flatten()
            .filter(|s| s["lifecycle"].as_str().unwrap_or("active") != "archived")
            .map(|s| session_dict(s, &approvals)).collect())
    }

    pub(crate) async fn session_snapshot(&self, id: &str) -> Result<Value> {
        let s = self.k(json!({"op": "status", "name": id})).await?;
        let sid = s["id"].as_str().unwrap_or(id).to_string();
        let approvals = self.pending_approvals(Some(&sid)).await;
        Ok(session_dict(&s, &approvals))
    }

    pub(crate) async fn status_of(&self, id: &str) -> Option<String> {
        self.session_snapshot(id).await.ok().and_then(|s| s["status"].as_str().map(String::from))
    }

    /// {data: items, has_more}; desc = newest first (tail), asc = after a cursor.
    pub(crate) async fn list_items(&self, id: &str, after: Option<&str>, limit: usize, desc: bool) -> Result<Value> {
        if desc {
            let tail = limit.clamp(1, 200);
            let msgs = self.k(json!({"op": "history", "name": id, "tail": tail})).await?;
            let items: Vec<Value> = msgs.as_array().into_iter().flatten().rev().map(kurisu_item).collect();
            return Ok(json!({"data": items, "has_more": false}));
        }
        let after_seq = after.filter(|a| !a.is_empty() && *a != "__empty__").and_then(|a| a.parse::<i64>().ok()).unwrap_or(0);
        let msgs = self.k(json!({"op": "history", "name": id, "after": after_seq})).await?;
        let items: Vec<Value> = msgs.as_array().into_iter().flatten().map(kurisu_item).collect();
        let more = items.len() > limit;
        Ok(json!({"data": items.into_iter().take(limit).collect::<Vec<_>>(), "has_more": more}))
    }

    pub(crate) async fn latest_item_id(&self, id: &str) -> Result<Option<String>> {
        let page = self.list_items(id, None, 1, true).await?;
        Ok(page["data"][0]["id"].as_str().map(String::from))
    }

    pub(crate) async fn post_message(&self, id: &str, content: &[Value], steer: bool) -> Result<Value> {
        let person=self.server.channels.person("telegram",&self.cfg.chat_id).ok().flatten().unwrap_or_else(||"owner".into());
        if let Err(e)=self.server.channels.route(id,&crate::channels::Address{channel:"telegram".into(),destination:self.cfg.chat_id.clone(),person}) {
            log(format!("could not save shared delivery route: {e}"));
        }
        let (mut texts, mut images, mut attached) = (vec![], vec![], vec![]);
        for b in content {
            match b["type"].as_str() {
                Some("input_text") => {
                    if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                        texts.push(t.to_string());
                    }
                }
                Some("input_image") => {
                    let url = b["image_url"].as_str().unwrap_or("");
                    let mime = url.strip_prefix("data:").map(|r| r.split(';').next().unwrap_or("")).unwrap_or("image/jpeg");
                    images.push(json!({"mime": mime, "name": "telegram-photo", "data_b64": url}));
                }
                Some("input_file") => {
                    if let Some(f) = b["file_id"].as_str().filter(|f| !f.is_empty()) {
                        attached.push(f.to_string());
                    }
                }
                _ => {}
            }
        }
        if !attached.is_empty() {
            texts.insert(0, format!("[Attached (in your folder; read them with the Read tool): {}]", attached.join(", ")));
        }
        let text = if texts.is_empty() { if images.is_empty() { String::new() } else { "(image)".into() } } else { texts.join("\n\n") };
        let mut req = json!({"op": "send", "name": id, "text": text, "wait": false});
        if !images.is_empty() {
            req["images"] = json!(images);
        }
        if steer {
            req["mode"] = json!("steer");
        }
        self.k(req).await
    }

    pub(crate) async fn create_session(&self, ctx: &Ctx) -> Result<String> {
        let mut name = format!("telegram-{}-{}", chrono::Local::now().format("%Y%m%d-%H%M"), token(4));
        if ctx.scope != "dm" {
            name.push_str("-group");
        }
        let mut req = json!({"op": "create", "name": name, "description": "Telegram chat", "role": "worker"});
        // /new preserves the selected harness/model instead of silently returning to Claude.
        if let Some(id)=self.current_session(ctx) {
            if let Ok(s)=self.session_snapshot(&id).await {
                req["harness"]=s["kurisu_harness"].clone();
                req["model"]=s["model_override"].clone();
                req["permission_mode"]=s["permission_mode"].clone();
            }
        }
        if let Some(c) = &self.cfg.new_session_cwd {
            req["cwd"] = json!(c);
        }
        let r = self.k(req).await?;
        r["id"].as_str().map(String::from).ok_or_else(|| anyhow!("Kurisu session create returned no id"))
    }

    pub(crate) async fn lead_session(&self) -> Result<String> {
        let lead = self.k(json!({"op": "lead_get", "create": true})).await?;
        lead["id"].as_str().map(String::from).ok_or_else(|| anyhow!("Kurisu has no Lead session"))
    }

    pub(crate) async fn resolve_elicitation(&self, approval_id: &str, action: &str, content: Option<Value>) -> Result<Value> {
        match (action, content) {
            ("accept", Some(c)) => self.k(json!({"op": "answer", "id": approval_id, "answers": c})).await,
            ("accept", None) => self.k(json!({"op": "approve", "id": approval_id, "always": false})).await,
            ("accept_session", _) => self.k(json!({"op": "approve", "id": approval_id, "always": true, "session_wide": true})).await,
            _ => self.k(json!({"op": "deny", "id": approval_id, "reason": "Dismissed from Telegram"})).await,
        }
    }

    pub(crate) async fn upload_file(&self, id: &str, filename: &str, data: &[u8]) -> Result<Option<String>> {
        use base64::Engine;
        let name = std::path::Path::new(filename).file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).unwrap_or("file");
        let r = self.k(json!({"op": "upload", "name": id, "filename": name,
                              "data_b64": base64::engine::general_purpose::STANDARD.encode(data)})).await?;
        Ok(r["path"].as_str().map(String::from))
    }

    pub(crate) async fn model_options(&self, snapshot: &Value) -> Result<(Vec<Value>, Option<String>)> {
        let harness = snapshot["kurisu_harness"].as_str().or(snapshot["harness"].as_str()).filter(|h|*h!="claude-native").unwrap_or("claude");
        let r = self.k(json!({"op": "models", "harness": harness})).await?;
        let mut options = vec![];
        for e in r["models"].as_array().into_iter().flatten() {
            let id = e["id"].as_str().or(e.as_str()).unwrap_or("");
            if id.is_empty() || disabled_model(id) {
                continue;
            }
            let label = e["label"].as_str().filter(|l| !l.is_empty()).unwrap_or(id);
            options.push(json!({"id": id, "label": label}));
        }
        let note = if options.is_empty() { Some(r["note"].as_str().filter(|n| !n.is_empty()).unwrap_or("Kurisu has no model catalogue for this harness.").to_string()) } else { None };
        Ok((options, note))
    }

    pub(crate) async fn default_model(&self, harness: &str) -> Option<String> {
        let r = self.k(json!({"op": "models", "harness": harness})).await.ok()?;
        r["models"].as_array()?.iter().find(|e| e["default"] == json!(true)).and_then(|e| e["id"].as_str().map(String::from))
    }

    pub(crate) async fn kurisu_switch_model(&self, snapshot: &Value, model_id: &str) -> Result<Value> {
        let id = snapshot["id"].as_str().unwrap_or("").to_string();
        let mut model = model_id.to_string();
        if ["default", "off", "reset"].contains(&model_id.trim().to_lowercase().as_str()) {
            let harness = snapshot["kurisu_harness"].as_str().or(snapshot["harness"].as_str()).filter(|h|*h!="claude-native").unwrap_or("claude");
            model = self.default_model(harness).await.ok_or_else(|| anyhow!("Kurisu reported no default model"))?;
        }
        self.k(json!({"op": "update", "name": id, "fields": {"model": model}})).await?;
        self.session_snapshot(&id).await
    }

    pub(crate) async fn official_usage(&self) -> Result<Value> {
        Ok(super::bridge::shape_official(&self.k(json!({"op": "usage"})).await?, now_i()))
    }

    pub(crate) async fn kurisu_mcp(&self, id: &str, fresh: bool) -> Result<Value> {
        let now = std::time::Instant::now();
        if !fresh {
            if let Some((exp, v)) = self.mcp_cache.lock().unwrap().get(id) {
                if *exp > now {
                    return Ok(v.clone());
                }
            }
        }
        let r = self.k(json!({"op": "mcp_list", "name": id})).await?;
        self.mcp_cache.lock().unwrap().insert(id.to_string(), (now + std::time::Duration::from_secs(KURISU_MCP_CACHE_TTL_S), r.clone()));
        Ok(r)
    }

    pub(crate) async fn kurisu_mcp_names(&self, id: &str) -> Vec<String> {
        let r = match self.kurisu_mcp(id, false).await {
            Ok(r) => r,
            Err(e) => {
                log(format!("kurisu mcp list failed: {e}"));
                return vec![];
            }
        };
        let mut names: Vec<String> = r["servers"].as_array().into_iter().flatten()
            .filter(|s| s["enabled"] == json!(true)).filter_map(|s| s["name"].as_str().filter(|n| !n.is_empty()).map(String::from)).collect();
        names.sort();
        let builtin: Vec<String> = r["builtin"].as_array().map(|b| b.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_else(|| vec!["kurisu-mcp".into()]);
        names.extend(builtin);
        names
    }

    pub(crate) fn lb_path(&self) -> Option<std::path::PathBuf> {
        let p = std::path::PathBuf::from(std::env::var("HOME").ok()?).join(LB_PATH_REL);
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&p).ok().filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).map(|_| p)
    }

    pub(crate) async fn kurisu_mcp_overview(&self, session: &Value, notice: Option<&str>, interactive: bool) -> Result<(String, Option<Value>)> {
        let sid = session["id"].as_str().unwrap_or("").to_string();
        let (mut servers, mut builtin, mut note) = (vec![], vec![json!("kurisu-mcp")], String::new());
        if !sid.is_empty() {
            let r = self.kurisu_mcp(&sid, true).await?;
            servers = r["servers"].as_array().cloned().unwrap_or_default();
            if let Some(b) = r["builtin"].as_array().filter(|b| !b.is_empty()) {
                builtin = b.clone();
            }
            note = r["note"].as_str().unwrap_or("").to_string();
        }
        let title = take(&squash(session["title"].as_str().unwrap_or("(no session)")), 60);
        let mut lines = vec!["🔌 MCP servers".to_string()];
        if let Some(n) = notice {
            lines.extend(["".into(), n.to_string()]);
        }
        let short = if sid.is_empty() { "-".to_string() } else { take(&sid, 8) };
        lines.extend(["".into(), format!("Session: {title} ({short}) · Kurisu"), "".into(), "This session (stored in Kurisu)".into()]);
        for name in &builtin {
            lines.push(format!("• {} — ✅ on · built into every Kurisu session (fixed)", super::vstr(name)));
        }
        for s in &servers {
            // Stored switch vs. actually loaded in the running CLI: never claim a tool it lacks.
            let state = if s["enabled"] != json!(true) { "⛔ off".to_string() }
                else if s["loaded"] == json!(true) { "✅ on · loaded now".to_string() }
                else { "✅ on · applies from its next turn".to_string() };
            lines.push(format!("• {} — {} · {}", super::vstr(&s["name"]), state, mcp_describe(&s["config"])));
        }
        if servers.is_empty() {
            lines.push("• no other servers".into());
        }
        lines.extend(["".into(), if note.is_empty() { "Changes apply when Kurisu restarts (resumes) the session before its next turn.".into() } else { note }]);
        let text = take(&lines.join("\n"), 3900);
        if !interactive || sid.is_empty() {
            return Ok((text, None));
        }
        let (mut actions, mut buttons): (Vec<Value>, Vec<Vec<Value>>) = (vec![], vec![]);
        for s in &servers {
            let enable = s["enabled"] != json!(true);
            actions.push(json!({"op": "toggle", "harness": "kurisu", "name": s["name"], "enable": enable, "session_id": sid}));
            buttons.push(vec![json!({"text": take(&format!("{} {} · Kurisu", if enable { "✅ Enable" } else { "⛔ Disable" }, super::vstr(&s["name"])), 64), "callback_data": null})]);
        }
        if self.lb_path().is_some() && !servers.iter().any(|s| s["name"] == "lb") {
            actions.push(json!({"op": "add", "harness": "kurisu", "session_id": sid}));
            buttons.push(vec![json!({"text": "➕ Add lb (Live Browser) · Kurisu", "callback_data": null})]);
        }
        if !actions.is_empty() {
            let tok = token(16);
            self.with_state(|st| {
                let cards = st.entry("mcp_cards").or_insert(json!({}));
                if !cards.is_object() {
                    *cards = json!({});
                }
                let m = cards.as_object_mut().unwrap();
                m.insert(tok.clone(), json!({"session_id": sid, "actions": actions, "created_at": now_i()}));
                while m.len() > super::bridge::MCP_CARD_LIMIT {
                    let first = m.keys().next().cloned().unwrap();
                    m.remove(&first);
                }
            });
            self.save_state();
            for (i, row) in buttons.iter_mut().enumerate() {
                row[0]["callback_data"] = json!(format!("mcp:{tok}:{i}"));
            }
        }
        buttons.push(vec![json!({"text": "🔄 Refresh", "callback_data": "mcp:r"})]);
        Ok((text, Some(json!({"inline_keyboard": buttons}))))
    }

    pub(crate) async fn kurisu_mcp_apply(&self, action: &Value) -> Result<String> {
        let sid = action["session_id"].as_str().unwrap_or("").to_string();
        let (r, head) = if action["op"] == "add" {
            let lb = self.lb_path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let r = self.k(json!({"op": "mcp_set", "name": sid, "server": "lb", "config": {"command": lb, "args": ["mcp"]}, "enabled": true})).await?;
            (r, "➕ Kurisu: lb (Live Browser) added to this session. It needs `lb web` running to work.".to_string())
        } else {
            let enabled = action["enable"] == json!(true);
            let name = super::vstr(&action["name"]);
            let r = self.k(json!({"op": "mcp_set", "name": sid, "server": name, "enabled": enabled})).await?;
            let head = if r["changed"] == json!(false) {
                format!("Kurisu: {name} was already {}.", if enabled { "enabled" } else { "disabled" })
            } else {
                format!("{} Kurisu: {name} {}.", if enabled { "✅" } else { "⛔" }, if enabled { "enabled" } else { "disabled" })
            };
            (r, head)
        };
        self.mcp_cache.lock().unwrap().remove(&sid);
        Ok(format!("{head}\n{}", r["note"].as_str().filter(|n| !n.is_empty()).unwrap_or("Kurisu restarts (resumes) the session's CLI before its next turn.")))
    }

    // --- current session per scope

    pub(crate) fn current_session(&self, ctx: &Ctx) -> Option<String> {
        let shared = if ctx.scope == "dm" {
            self.server.channels.person("telegram", &self.cfg.chat_id).ok().flatten()
                .and_then(|p| self.server.channels.session(&p).ok().flatten())
        } else { self.server.channels.scope(&ctx.scope).ok().flatten() };
        if shared.is_some() { return shared; }
        self.with_state(|st| {
            if ctx.scope != "dm" {
                return st.get("scopes").and_then(|s| s.get(&ctx.scope)).and_then(Value::as_str).map(String::from);
            }
            st.get("session_id").and_then(Value::as_str).map(String::from)
        })
    }

    pub(crate) fn select_session(&self, ctx: &Ctx, id: &str) {
        let saved = if ctx.scope == "dm" {
            let person = self.server.channels.person("telegram", &self.cfg.chat_id).ok().flatten().unwrap_or_else(|| "owner".into());
            self.server.channels.select(&person, id)
        } else { self.server.channels.select_scope(&ctx.scope, id) };
        if let Err(e) = saved { log(format!("could not save shared session binding: {e}")); }
        self.with_state(|st| {
            if ctx.scope == "dm" {
                st.insert("session_id".into(), json!(id));
            } else {
                let scopes = st.entry("scopes").or_insert(json!({}));
                if !scopes.is_object() {
                    *scopes = json!({});
                }
                scopes[&ctx.scope] = json!(id);
            }
        });
    }

    /// The scope's session; by default the Kurisu Lead.
    pub(crate) async fn ensure_session(&self, ctx: &Ctx) -> Result<String> {
        if let Some(s) = self.current_session(ctx) {
            return Ok(s);
        }
        let id = self.lead_session().await?;
        self.select_session(ctx, &id);
        self.save_state();
        Ok(id)
    }
}

pub fn disabled_model(id: &str) -> bool {
    id.to_lowercase().contains("kimi")
}

fn secret_key(s: &str) -> bool {
    let l = s.to_lowercase();
    ["token", "secret", "key", "pass", "auth", "bearer", "cookie"].iter().any(|k| l.contains(k))
}

pub fn mcp_mask_args(args: &Value) -> Vec<String> {
    let mut out = vec![];
    let mut hide_next = false;
    for a in args.as_array().into_iter().flatten() {
        let text = super::vstr(a);
        if hide_next {
            out.push("***".into());
            hide_next = false;
            continue;
        }
        if let Some((key, _)) = text.split_once('=').filter(|(k, _)| secret_key(k)) {
            out.push(format!("{key}=***"));
        } else if text.starts_with('-') && secret_key(&text) {
            out.push(text.clone());
            hide_next = true;
        } else if text.len() >= 32 && text.chars().all(|c| c.is_ascii_alphanumeric() || "_-.+/=".contains(c)) {
            out.push("***".into());
        } else {
            out.push(text);
        }
    }
    out
}

/// One-line, secret-free description: command/args or URL host+path.
pub fn mcp_describe(config: &Value) -> String {
    if !config.is_object() {
        return String::new();
    }
    let mut text = if let Some(url) = config["url"].as_str().filter(|u| !u.is_empty()) {
        let shown = match url.split_once("://") {
            Some((scheme, rest)) => {
                let (hostport, path) = match rest.find('/') {
                    Some(i) => (&rest[..i], &rest[i..]),
                    None => (rest, ""),
                };
                let hostport = hostport.rsplit('@').next().unwrap_or(hostport);
                let path = path.split(['?', '#']).next().unwrap_or("");
                format!("{scheme}://{}{path}", hostport.to_lowercase())
            }
            None => "(url)".into(),
        };
        format!("{} {shown}", config["type"].as_str().unwrap_or("http"))
    } else {
        let cmd = config["command"].as_str().unwrap_or("?");
        let base = std::path::Path::new(cmd).file_name().and_then(|n| n.to_str()).unwrap_or(cmd).to_string();
        std::iter::once(base).chain(mcp_mask_args(&config["args"])).collect::<Vec<_>>().join(" ")
    };
    let mut hidden = vec![];
    if let Some(m) = config["env"].as_object().filter(|m| !m.is_empty()) {
        hidden.push(format!("{} env hidden", m.len()));
    }
    for key in ["headers", "http_headers", "env_http_headers"] {
        if let Some(m) = config[key].as_object().filter(|m| !m.is_empty()) {
            for (k, v) in m {
                hidden.push(format!("{k}: {}", crate::server::mask_header_value(v.as_str().unwrap_or(""))));
            }
        }
    }
    if !hidden.is_empty() {
        text = format!("{text} · {}", hidden.join(", "));
    }
    take(&text, 140)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_sessions_items_and_mcp() {
        let s = json!({"id": "a1", "name": "lead", "status": "awaiting_approval", "harness": "claude", "cwd": "/w",
                       "model": "claude-haiku-5-5", "usage": {"input": 5, "output": 7}, "created_at": 1, "last_activity": null});
        let d = session_dict(&s, &[json!({"session": "a1", "id": "x", "title": "Bash: ls", "kind": "permission"})]);
        assert_eq!((d["status"].as_str(), d["harness"].as_str(), d["title"].as_str()), (Some("waiting"), Some("claude-native"), Some("lead")));
        assert_eq!(d["pending_elicitations"][0]["params"]["message"], "Bash: ls");
        assert_eq!(d["usage_by_model"]["claude-haiku-5-5"]["output_tokens"], 7);
        assert_eq!(d["updated_at"], 1);
        assert_eq!(kurisu_item(&json!({"seq": 4, "role": "error", "text": "x", "at": 9}))["content"][0]["text"], "⚠️ x");
        assert_eq!(kurisu_item(&json!({"seq": 4, "role": "agent", "text": "hi"}))["id"], "4");
        assert_eq!(item_text(&kurisu_item(&json!({"seq": 1, "role": "agent", "text": " hi "}))), "hi");
        assert_eq!(mcp_describe(&json!({"command": "/usr/bin/gh-mcp", "args": ["--token", "abc", "API_KEY=1"], "env": {"A": "b"}})),
                   "gh-mcp --token *** API_KEY=*** · 1 env hidden");
        assert_eq!(mcp_describe(&json!({"type": "sse", "url": "https://u:p@Ex.com:8443/mcp?x=1"})), "sse https://ex.com:8443/mcp");
        let raw = json!({"type": "http", "url": "https://mcp.notion.com/v1/mcp", "headers": {"Authorization": "Bearer ntn_secret_9876"}});
        let desc = mcp_describe(&raw);
        assert!(!desc.contains("ntn_secret"), "{desc}");
        assert_eq!(desc, "http https://mcp.notion.com/v1/mcp · Authorization: ****9876");
        assert_eq!(mcp_describe(&crate::server::redact_mcp(&raw)), desc);
    }
}
