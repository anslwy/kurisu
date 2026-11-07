//! The Kurisu server: owns the session store and the drivers, and serves
//! line-JSON requests on a 0600 unix socket (used by the CLI and by the
//! `kurisu mcp` tool server that every session gets).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::v1::schema::{
    LoadSessionRequest, NewSessionRequest, PermissionOptionId, RequestPermissionOutcome, RequestPermissionRequest,
    SelectedPermissionOutcome, SessionId,
};
use anyhow::{anyhow, bail, Context, Result};
use claude_cli_acp::{ClaudeCli, SessionOpts};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex, Notify, OnceCell};

use crate::codex::{CodexConfig, CodexDriver};
use crate::driver::{agent_text, Driver};
use crate::paths::Paths;
use crate::store::{self, Store};

pub const LEAD_PROMPT: &str = "\
You are the Lead of a Kurisu team. Kurisu runs every agent as a separate, \
long-lived PEER session; there are no sub-agents, so do not use the Task or \
Agent tools. The `kurisu-mcp` tools are your live roster and messaging: \
call whoami and peers_list to see who exists (never rely on a roster file); \
session_create {name, description} starts a worker (Claude, Haiku by default); \
session_send {to, msg} sends a peer a message and returns its reply; \
session_history, board_post and board_read help you coordinate. Keep your own \
status current with set_my_status. When you delegate, find the workers with \
peers_list, give each a short self-contained task, collect the replies, and \
answer the user with a concise summary that says which worker said what.";

pub fn worker_prompt(name: &str) -> String {
    format!(
        "You are `{name}`, a peer session in a Kurisu team (no sub-agents: do not \
         use the Task or Agent tools). Messages from other sessions arrive as \
         normal prompts. Answer concisely and directly. The `kurisu-mcp` tools \
         (whoami, peers_list, session_send, set_my_status, board_post) tell you \
         who you are and who your peers are; keep your status current."
    )
}

/// Seconds an approval request waits for an answer (`KURISU_APPROVAL_TTL`, default 600).
pub fn approval_ttl() -> u64 {
    std::env::var("KURISU_APPROVAL_TTL").ok().and_then(|v| v.parse().ok()).filter(|v| *v >= 10).unwrap_or(600)
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn outcome(allow: bool) -> RequestPermissionOutcome {
    let id = if allow { claude_cli_acp::ALLOW_ONCE } else { claude_cli_acp::REJECT_ONCE };
    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(PermissionOptionId::new(id)))
}

/// Text prepended to a prompt when the session's previous turn failed: the CLI
/// still holds the failed message, so say plainly that it must not be answered.
pub fn failed_turn_preamble(err: &str) -> String {
    let err: String = err.chars().take(160).collect();
    format!(
        "[Kurisu: your previous turn failed ({err}) and that message was abandoned. \
         Do not answer it. Reply only to the message below.]\n\n"
    )
}

pub struct Server {
    paths: Paths,
    store: Mutex<Store>,
    claude: ClaudeCli,
    codex: OnceCell<Arc<CodexDriver>>,
    live: Mutex<HashSet<String>>,
    /// Sessions whose model/effort/name changed: relaunch (resume) before their next turn.
    relaunch: Mutex<HashSet<String>>,
    busy: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    stop: Notify,
}

impl Server {
    pub fn new(paths: Paths) -> Result<Self> {
        std::fs::create_dir_all(&paths.home)?;
        let store = Store::open(&paths.db())?;
        for s in store.list()? {
            // Never-run sessions stay "new"; everything else is not running after a restart.
            if s.harness_session_id.is_some() {
                store.set_status(&s.id, "stopped")?;
            }
        }
        let me = std::env::current_exe()?.to_string_lossy().into_owned();
        let mut cfg = claude_cli_acp::Config::new(paths.home.join("claude"), vec![me, "hook".into()]);
        cfg.tmux_socket = paths.tmux_socket();
        if let Ok(bin) = std::env::var("KURISU_CLAUDE_BIN") {
            cfg.claude_bin = bin;
        }
        // The hook's own deadline is a fallback: the server expires requests 10 s earlier.
        cfg.permission_timeout = std::time::Duration::from_secs(approval_ttl() + 10);
        // Approvals still pending from before a restart can no longer be answered.
        for a in store.approvals(None, false)? {
            store.approval_decide(&a.id, "cancelled", "kurisu", Some("the server restarted"), None)?;
        }
        Ok(Self {
            paths,
            store: Mutex::new(store),
            claude: ClaudeCli::new(cfg),
            codex: OnceCell::new(),
            live: Mutex::default(),
            relaunch: Mutex::default(),
            busy: Mutex::default(),
            stop: Notify::new(),
        })
    }

    async fn codex(&self) -> Result<Arc<CodexDriver>> {
        self.codex
            .get_or_try_init(|| async { CodexDriver::spawn(CodexConfig::default()).await.map(Arc::new) })
            .await
            .cloned()
    }

    /// Claude launch options for one session: model, peer tools, no sub-agents.
    fn claude_opts(&self, s: &store::Session) -> Result<SessionOpts> {
        let dir = self.paths.session_dir(&s.id);
        std::fs::create_dir_all(&dir)?;
        let me = std::env::current_exe()?.to_string_lossy().into_owned();
        let mcp = json!({"mcpServers": {crate::mcp::SERVER_NAME: {
            "type": "stdio", "command": me, "args": ["mcp"],
            "env": {"KURISU_HOME": self.paths.home, "KURISU_SESSION": s.id}
        }}});
        let mcp_path = dir.join("mcp.json");
        std::fs::write(&mcp_path, serde_json::to_vec_pretty(&mcp)?)?;
        let prompt_path = dir.join("system-prompt.md");
        let prompt = if s.role == "lead" { LEAD_PROMPT.to_string() } else { worker_prompt(&s.name) };
        std::fs::write(&prompt_path, prompt)?;
        let p = |x: &Path| x.to_string_lossy().into_owned();
        let mut effort = vec![];
        if let Some(e) = &s.effort {
            effort = vec!["--effort".to_string(), e.clone()];
        }
        Ok(SessionOpts {
            args: [effort, vec![
                "--model".into(), s.model.clone(),
                // Explicit, so the user's default (e.g. auto mode and its setup dialog) is not inherited.
                "--permission-mode".into(), "default".into(),
                "--mcp-config".into(), p(&mcp_path), "--strict-mcp-config".into(),
                "--allowedTools".into(), format!("mcp__{}", crate::mcp::SERVER_NAME),
                "--disallowedTools".into(), "Task,Agent".into(),
                "--append-system-prompt-file".into(), p(&prompt_path),
            ]]
            .concat(),
        })
    }

    async fn ensure_live(&self, name: &str, actor: &str) -> Result<SessionId> {
        let s = self.store.lock().await.get(name)?.ok_or_else(|| anyhow!("no session {name}"))?;
        if s.lifecycle != "active" {
            bail!("session {name} is {}", s.lifecycle);
        }
        if self.relaunch.lock().await.remove(name) && self.live.lock().await.remove(name) {
            // Model/effort/name changed: close now, resume below with the new launch options.
            if let (Some(id), "claude") = (&s.harness_session_id, s.harness.as_str()) {
                let _ = self.claude.close(&SessionId::new(id.clone())).await;
            }
        }
        let cwd = PathBuf::from(&s.cwd);
        std::fs::create_dir_all(&cwd).ok();
        if let Some(id) = &s.harness_session_id {
            let id = SessionId::new(id.clone());
            if !self.live.lock().await.contains(name) {
                match s.harness.as_str() {
                    "claude" => {
                        let opts = self.claude_opts(&s)?;
                        self.claude.load_session_with(LoadSessionRequest::new(id.clone(), cwd), &opts).await?
                    }
                    "codex" => self.codex().await?.load_session(&id, cwd).await?,
                    h => bail!("harness {h} not implemented"),
                }
                self.live.lock().await.insert(name.into());
                let st = self.store.lock().await;
                st.set_status(name, "idle")?;
                st.log_event(actor, name, "resumed", &format!("{} {}", s.model, s.effort.clone().unwrap_or_default()))?;
            }
            return Ok(id);
        }
        let id = match s.harness.as_str() {
            "claude" => {
                let opts = self.claude_opts(&s)?;
                self.claude.new_session_with(NewSessionRequest::new(cwd), &opts).await?.session_id
            }
            "codex" => self.codex().await?.new_session(cwd).await?,
            h => bail!("harness {h} not implemented"),
        };
        let st = self.store.lock().await;
        st.bind_harness_id(name, &id.to_string())?;
        st.set_status(name, "idle")?;
        st.log_event(actor, name, "started", &s.model)?;
        drop(st);
        self.live.lock().await.insert(name.into());
        Ok(id)
    }

    async fn send(&self, name: &str, text: &str, from: &str, actor: &str) -> Result<Value> {
        let lock = self.busy.lock().await.entry(name.into()).or_default().clone();
        let _turn = lock.lock().await;
        let (harness, previous_failure) = {
            // Record the message first, so observers see it immediately.
            let st = self.store.lock().await;
            let s = st.get(name)?.ok_or_else(|| anyhow!("no session {name}"))?;
            if s.lifecycle != "active" {
                bail!("session {} is {}", s.name, s.lifecycle);
            }
            // A failed turn leaves its message in the CLI's context; the next prompt says so.
            let previous_failure = st
                .last_message(name)?
                .filter(|(_, role, text, _)| role == "error" && text.starts_with("turn failed"))
                .map(|(_, _, text, _)| text.trim_start_matches("turn failed: ").to_string());
            st.append(name, from, text)?;
            st.set_status(name, "starting")?;
            (s.harness, previous_failure)
        };
        let notice = previous_failure.as_ref().map(|e| {
            let e: String = e.chars().take(160).collect();
            format!("the previous message to this session failed ({e}) and was not answered; this reply answers only the new message")
        });
        let prompt_text = match &previous_failure {
            Some(e) => format!("{}{text}", failed_turn_preamble(e)),
            None => text.to_string(),
        };
        let text = prompt_text.as_str();
        let id = match self.ensure_live(name, actor).await {
            Ok(id) => id,
            Err(e) => {
                let st = self.store.lock().await;
                st.append(name, "error", &format!("could not start the session: {e:#}"))?;
                st.set_status(name, "error")?;
                st.log_event(actor, name, "failed", &format!("{e:#}"))?;
                return Err(e);
            }
        };
        self.store.lock().await.set_status(name, "working")?;
        let (tx, mut rx) = mpsc::channel(256);
        let collect = tokio::spawn(async move {
            let mut all = vec![];
            while let Some(n) = rx.recv().await {
                all.push(n);
            }
            all
        });
        let res = match harness.as_str() {
            "claude" => {
                let req = agent_client_protocol::v1::schema::PromptRequest::new(
                    id.clone(),
                    vec![agent_client_protocol::v1::schema::ContentBlock::Text(
                        agent_client_protocol::v1::schema::TextContent::new(text),
                    )],
                );
                // Permission prompts arrive on `ptx` while the turn runs.
                let (ptx, mut prx) = mpsc::channel::<RequestPermissionRequest>(16);
                let perms = async {
                    while let Some(p) = prx.recv().await {
                        if let Err(e) = self.on_permission(name, p).await {
                            eprintln!("approval handling failed for {name}: {e:#}");
                        }
                    }
                };
                let (res, ()) = tokio::join!(self.claude.prompt_with_permissions(req, tx, Some(ptx)), perms);
                self.cancel_pending(name, "the turn ended").await;
                res
            }
            _ => self.codex().await?.prompt(&id, text, tx).await,
        };
        let updates = collect.await?;
        let reply = agent_text(&updates);
        let st = self.store.lock().await;
        st.set_status(name, if res.is_ok() { "idle" } else { "error" })?;
        if let Err(e) = &res {
            st.append(name, "error", &format!("turn failed: {e:#}"))?;
            st.log_event(actor, name, "failed", &format!("{e:#}"))?;
        }
        let resp = res?;
        st.append(name, "agent", &reply)?;
        let display = st.get(name)?.map(|x| x.name).unwrap_or_default();
        Ok(json!({"id": name, "name": display, "reply": reply, "stop_reason": format!("{:?}", resp.stop_reason), "notice": notice}))
    }

    /// A permission prompt from a session's CLI: settle it by rule, or queue it for the user.
    async fn on_permission(&self, name: &str, p: RequestPermissionRequest) -> Result<()> {
        let rid = p.tool_call.tool_call_id.to_string();
        let tool = p.tool_call.fields.name.clone().unwrap_or_else(|| "tool".into());
        let title = p.tool_call.fields.title.clone().unwrap_or_else(|| tool.clone());
        let input = p.tool_call.fields.raw_input.clone().unwrap_or(Value::Null);
        let meta = p.meta.clone().unwrap_or_default();
        let cwd = meta.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
        let hook_deadline = meta.get("expires_at").and_then(Value::as_i64).unwrap_or(i64::MAX);
        let expires = (now() + approval_ttl() as i64).min(hook_deadline);
        let st = self.store.lock().await;
        let s = st.get(name)?.ok_or_else(|| anyhow!("no session {name}"))?;
        let a = st.approval_create(name, &rid, &tool, &title, &input, &cwd, expires)?;
        let rules = st.rules()?;
        if let Some(rule) = crate::approvals::decide(&rules, &s, &tool, &input, &cwd) {
            let allow = rule.decision == "allow";
            let by = format!("rule#{}", rule.id);
            let status = if allow { "approved" } else { "denied" };
            st.approval_decide(&a.id, status, &by, Some(&format!("{} by rule #{}", status, rule.id)), Some(rule.id))?;
            st.log_event(&by, name, &format!("approval_{status}"), &format!("{} {title}", a.id))?;
            drop(st);
            let msg = format!("This tool call was denied by the user's Kurisu rule #{}.", rule.id);
            self.claude.respond_permission(&SessionId::new(s.harness_session_id.unwrap_or_default()), &rid, &outcome(allow), Some(&msg)).await?;
            return Ok(());
        }
        st.append(name, "approval", &format!("[approval:{}] {title}", a.id))?;
        st.log_event(name, name, "approval_requested", &format!("{} {title}", a.id))?;
        st.set_status(name, "awaiting_approval")?;
        Ok(())
    }

    /// The user's answer to a pending approval (single use).
    async fn decide(&self, key: &str, allow: bool, always: bool, reason: Option<String>) -> Result<Value> {
        let st = self.store.lock().await;
        let a = st.approval_resolve(key)?;
        if a.status != "pending" {
            bail!("approval {} is already {}", a.id, a.status);
        }
        let status = if allow { "approved" } else { "denied" };
        let mut rule = None;
        if allow && always {
            // "Always" = a session rule for this tool, limited to the session's folder.
            rule = Some(st.rule_add("session", &a.session, &a.tool, true, None, "allow", "user")?);
        }
        if !st.approval_decide(&a.id, status, "user", reason.as_deref(), rule.as_ref().map(|r| r.id))? {
            bail!("approval {} is no longer pending", a.id);
        }
        st.log_event("user", &a.session, &format!("approval_{status}"), &format!("{} {}{}", a.id, a.title,
                     reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()))?;
        if let Some(r) = &rule {
            st.log_event("user", &a.session, "rule_added", &format!("#{} allow {} in its folder", r.id, r.tool))?;
        }
        let hid = st.get(&a.session)?.and_then(|s| s.harness_session_id).unwrap_or_default();
        let still_pending = !st.approvals(Some(&a.session), false)?.is_empty();
        if !still_pending {
            st.set_status(&a.session, "working")?;
        }
        drop(st);
        let msg = match &reason {
            Some(r) => format!("The user denied this tool call: {r}"),
            None => "The user denied this tool call.".into(),
        };
        self.claude.respond_permission(&SessionId::new(hid), &a.request_id, &outcome(allow), Some(&msg)).await?;
        Ok(json!({"id": a.id, "status": status, "session": a.session, "session_name": a.session_name, "title": a.title,
                  "rule": rule}))
    }

    /// Cancel a session's pending approvals (its turn ended or was stopped).
    async fn cancel_pending(&self, name: &str, why: &str) {
        let st = self.store.lock().await;
        for a in st.approvals(Some(name), false).unwrap_or_default() {
            if st.approval_decide(&a.id, "cancelled", "kurisu", Some(why), None).unwrap_or(false) {
                let _ = st.log_event("kurisu", name, "approval_cancelled", &format!("{} {} ({why})", a.id, a.title));
            }
        }
    }

    /// Deny approvals past their expiry (the hook also denies on its own timer).
    async fn expire_approvals(&self) -> Result<()> {
        let expired = self.store.lock().await.approvals_expired(now())?;
        for a in expired {
            let st = self.store.lock().await;
            if !st.approval_decide(&a.id, "expired", "kurisu", Some("no answer before it expired"), None)? {
                continue;
            }
            st.log_event("kurisu", &a.session, "approval_expired", &format!("{} {}", a.id, a.title))?;
            let hid = st.get(&a.session)?.and_then(|s| s.harness_session_id).unwrap_or_default();
            if st.approvals(Some(&a.session), false)?.is_empty() && st.get(&a.session)?.is_some_and(|s| s.status == "awaiting_approval") {
                st.set_status(&a.session, "working")?;
            }
            drop(st);
            let _ = self.claude.respond_permission(&SessionId::new(hid), &a.request_id, &outcome(false), Some("This tool call was denied: the user did not answer the approval request in time.")).await;
        }
        Ok(())
    }

    async fn usage(&self, name: Option<&str>) -> Result<Value> {
        let sessions = self.store.lock().await.list()?;
        let mut total = claude_cli_acp::usage::Usage::default();
        let mut per = serde_json::Map::new();
        for s in sessions.iter().filter(|s| name.is_none_or(|n| n == s.id)) {
            let (Some(id), "claude") = (&s.harness_session_id, s.harness.as_str()) else { continue };
            let Some(path) = self.claude.transcript_path(&SessionId::new(id.clone())).await else { continue };
            let u = claude_cli_acp::usage::from_transcript(&std::fs::read_to_string(&path).unwrap_or_default());
            total.add(&u);
            per.insert(s.name.clone(), json!(u));
        }
        Ok(json!({"sessions": per, "total": total}))
    }

    async fn handle(&self, req: Value) -> Result<Value> {
        let s = |k: &str| req.get(k).and_then(Value::as_str).map(String::from).ok_or_else(|| anyhow!("missing {k}"));
        let opt = |k: &str| req.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(String::from);
        // Calls coming from a session's MCP tools carry the caller's id (KURISU_SESSION).
        let caller = match opt("caller") {
            Some(c) => Some(self.store.lock().await.resolve_req(&c)?),
            None => None,
        };
        // Any `name` argument may be a UUID or a live name: resolve it to the id here.
        let op = s("op")?;
        let key = if matches!(op.as_str(), "create" | "lead") { None } else { opt("name") };
        let target = match &key {
            Some(k) => Some(self.store.lock().await.resolve_req(k)?),
            None => None,
        };
        let s = |k: &str| -> Result<String> {
            if k == "name" {
                if let Some(t) = &target {
                    return Ok(t.clone());
                }
            }
            req.get(k).and_then(Value::as_str).map(String::from).ok_or_else(|| anyhow!("missing {k}"))
        };
        match op.as_str() {
            "ping" => Ok(json!({"pid": std::process::id(), "rss_kb": crate::paths::rss_kb(), "home": self.paths.home})),
            "list" => Ok(json!(self.store.lock().await.list()?)),
            "create" => {
                let name = s("name")?;
                let role = opt("role").unwrap_or_else(|| "worker".into());
                if caller.is_some() && role != "worker" {
                    bail!("sessions created by a session are workers; only the user creates a lead");
                }
                let cwd = opt("cwd").map(PathBuf::from).unwrap_or_else(|| self.paths.workspace(&name));
                let model = opt("model").unwrap_or_else(|| store::DEFAULT_MODEL.into());
                let harness = opt("harness").unwrap_or_else(|| "claude".into());
                let description = opt("description").unwrap_or_default();
                let mut v = json!(self.store.lock().await.create(&name, &harness, &cwd.to_string_lossy(), &role, &model,
                                                                 &description, caller.as_deref())?);
                if let Some(w) = store::model_warning(&harness, &model) {
                    v["warning"] = json!(w);
                }
                Ok(v)
            }
            "lead" => {
                // Start (or reuse) the user's Lead and give it a task.
                let lead = {
                    let st = self.store.lock().await;
                    match st.resolve(&opt("lead").unwrap_or_else(|| "lead".into()))? {
                        Some(id) => id,
                        None => {
                            let model = opt("model").unwrap_or_else(|| store::DEFAULT_MODEL.into());
                            st.create("lead", "claude", &self.paths.workspace("lead").to_string_lossy(), "lead", &model,
                                      "Lead: talks to the user, delegates to peers, reports back", None)?
                                .id
                        }
                    }
                };
                self.send(&lead, &s("text")?, "user", "user").await
            }
            "update" => {
                let target = s("name")?;
                let fields = req.get("fields").and_then(Value::as_object).cloned().unwrap_or_default();
                self.update(caller.as_deref(), &target, &fields).await
            }
            "audit" => Ok(json!(self.store.lock().await.audit(target.as_deref())?)),
            "changes" => {
                // Incremental sync for live clients: new messages and audit rows since the
                // given global sequence numbers, plus the current session list.
                let st = self.store.lock().await;
                let msg_after = req.get("msg_after").and_then(Value::as_i64).unwrap_or(-1);
                let audit_after = req.get("audit_after").and_then(Value::as_i64).unwrap_or(-1);
                let (messages, audit) = if msg_after < 0 || audit_after < 0 {
                    (json!([]), json!([]))
                } else {
                    (json!(st.messages_after(msg_after, 500)?), json!(st.audit_after(None, audit_after)?))
                };
                Ok(json!({"msg_seq": st.max_message_seq()?, "audit_seq": st.max_audit_seq()?, "messages": messages, "audit": audit,
                          "approvals": st.approvals(None, false)?}))
            }
            "peers" | "list_full" => {
                let st = self.store.lock().await;
                let all = req.get("include_archived").and_then(Value::as_bool).unwrap_or(false);
                let mut out = vec![];
                for x in st.list()? {
                    if x.lifecycle == "archived" && !all {
                        continue;
                    }
                    let mut v = json!(x);
                    v["children"] = json!(st.children(&x.id)?);
                    v["replies"] = json!(st.reply_count(&x.id)?);
                    if let Some((seq, role, text, at)) = st.last_message(&x.id)? {
                        let preview: String = text.chars().take(120).collect();
                        v["last"] = json!({"seq": seq, "role": role, "text": preview, "at": at});
                    }
                    out.push(v);
                }
                Ok(json!(out))
            }
            "status" => {
                let st = self.store.lock().await;
                let name = s("name")?;
                let mut v = json!(st.get(&name)?.ok_or_else(|| anyhow!("no session {name}"))?);
                v["children"] = json!(st.children(&name)?);
                Ok(v)
            }
            "history" => {
                let st = self.store.lock().await;
                match req.get("tail").and_then(Value::as_i64) {
                    Some(tail) => {
                        let offset = req.get("offset").and_then(Value::as_i64).unwrap_or(0);
                        Ok(json!(st.history_tail(&s("name")?, tail.clamp(1, 200), offset.max(0))?))
                    }
                    None if caller.is_some() => Ok(json!(st.history_tail(&s("name")?, 20, 0)?)),
                    None => Ok(json!(st.history(&s("name")?, req.get("after").and_then(Value::as_i64).unwrap_or(0), 500)?)),
                }
            }
            "set_description" => {
                let name = s("name")?;
                self.store.lock().await.set_description(&name, &s("text")?)?;
                Ok(json!({"ok": name}))
            }
            "set_note" => {
                let name = s("name")?;
                self.store.lock().await.set_note(&name, &s("text")?)?;
                Ok(json!({"ok": name}))
            }
            "board_post" => {
                let author = match &caller {
                    Some(c) => self.store.lock().await.get(c)?.map(|x| x.name).unwrap_or_default(),
                    None => "user".into(),
                };
                Ok(json!({"seq": self.store.lock().await.board_post(&author, &s("text")?)?}))
            }
            "board_read" => {
                let tail = req.get("tail").and_then(Value::as_i64).unwrap_or(20).clamp(1, 200);
                Ok(json!(self.store.lock().await.board_read(tail)?))
            }
            "send" => {
                let name = s("name")?;
                if caller.as_deref() == Some(name.as_str()) {
                    bail!("a session cannot send to itself");
                }
                let from = match &caller {
                    Some(c) => format!("peer:{}", self.store.lock().await.get(c)?.map(|x| x.name).unwrap_or_default()),
                    None => "user".into(),
                };
                let actor = caller.clone().unwrap_or_else(|| "user".into());
                self.send(&name, &s("text")?, &from, &actor).await
            }
            "usage" => self.usage(target.as_deref()).await,
            "approvals" => {
                let all = req.get("all").and_then(Value::as_bool).unwrap_or(false);
                Ok(json!(self.store.lock().await.approvals(target.as_deref(), all)?))
            }
            "approve" | "deny" => {
                if caller.is_some() {
                    bail!("permission denied: only the user answers approvals");
                }
                let always = req.get("always").and_then(Value::as_bool).unwrap_or(false);
                self.decide(&s("id")?, op == "approve", always, opt("reason")).await
            }
            "rules" => Ok(json!(self.store.lock().await.rules()?)),
            "rule_add" => {
                if caller.is_some() {
                    bail!("permission denied: only the user sets approval rules");
                }
                let st = self.store.lock().await;
                let scope = opt("scope").unwrap_or_else(|| "role".into());
                let raw = s("target")?;
                let target_id = if scope == "session" { st.resolve_req(&raw)? } else { raw.clone() };
                let in_cwd = req.get("in_cwd").and_then(Value::as_bool).unwrap_or(false);
                let decision = opt("decision").unwrap_or_else(|| "allow".into());
                let r = st.rule_add(&scope, &target_id, &s("tool")?, in_cwd, opt("command_prefix").as_deref(), &decision, "user")?;
                let detail = format!("#{} {} {} for {} {}{}{}", r.id, r.decision, r.tool, scope, raw,
                                     if in_cwd { " in its folder" } else { "" },
                                     r.command_prefix.as_deref().map(|p| format!(" (command starts with {p:?})")).unwrap_or_default());
                // Audit rows need a session target: record role rules against every session with that role.
                let targets: Vec<String> = if scope == "session" { vec![target_id.clone()] }
                    else { st.list()?.into_iter().filter(|x| x.role == raw).map(|x| x.id).collect() };
                for t in targets {
                    st.log_event("user", &t, "rule_added", &detail)?;
                }
                Ok(json!(r))
            }
            "rule_remove" => {
                if caller.is_some() {
                    bail!("permission denied: only the user sets approval rules");
                }
                let id = req.get("id").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|x| x.trim_start_matches('#').parse().ok())))
                    .ok_or_else(|| anyhow!("missing rule id"))?;
                let st = self.store.lock().await;
                let rule = st.rules()?.into_iter().find(|r| r.id == id).ok_or_else(|| anyhow!("no rule #{id}"))?;
                st.rule_remove(id)?;
                let targets: Vec<String> = if rule.scope == "session" { vec![rule.target.clone()] }
                    else { st.list()?.into_iter().filter(|x| x.role == rule.target).map(|x| x.id).collect() };
                for t in targets {
                    st.log_event("user", &t, "rule_removed", &format!("#{} {} {}", id, rule.decision, rule.tool))?;
                }
                Ok(json!({"removed": id}))
            }
            "stop" => {
                let name = s("name")?;
                if let Some(id) = self.store.lock().await.get(&name)?.and_then(|x| x.harness_session_id) {
                    let _ = self.claude.close(&SessionId::new(id)).await;
                }
                let was_live = self.live.lock().await.remove(&name);
                self.cancel_pending(&name, "the session was stopped").await;
                let st = self.store.lock().await;
                if st.get(&name)?.is_some_and(|x| x.harness_session_id.is_some()) {
                    st.set_status(&name, "stopped")?;
                }
                if was_live {
                    st.log_event(caller.as_deref().unwrap_or("user"), &name, "stopped", "")?;
                }
                let display = st.get(&name)?.map(|x| x.name).unwrap_or_default();
                Ok(json!({"stopped": name, "name": display, "was_running": was_live}))
            }
            "shutdown" => {
                self.stop.notify_one();
                Ok(json!({"shutting_down": true}))
            }
            op => bail!("unknown op {op}"),
        }
    }

    /// session_update: the user may change anything; a session may change only its descendants.
    async fn update(&self, caller: Option<&str>, target: &str, fields: &serde_json::Map<String, Value>) -> Result<Value> {
        let actor = caller.unwrap_or("user").to_string();
        {
            let st = self.store.lock().await;
            st.get(target)?.ok_or_else(|| anyhow!("no session {target}"))?;
            if let Some(c) = caller {
                if !st.is_ancestor(c, target)? {
                    let n = |id: &str| st.get(id).ok().flatten().map(|x| x.name).unwrap_or_else(|| id.to_string());
                    bail!("permission denied: {} is not an ancestor of {} (only the user or a session's creator chain may modify it)", n(c), n(target));
                }
                if fields.get("role").and_then(Value::as_str) == Some("lead") {
                    bail!("permission denied: only the user can make a session a lead");
                }
            }
        }
        if fields.is_empty() {
            bail!("nothing to update");
        }
        let order = ["task", "description", "model", "effort", "role", "lifecycle", "name"];
        if let Some(bad) = fields.keys().find(|k| !order.contains(&k.as_str())) {
            bail!("field {bad} cannot be updated (allowed: {})", order.join(", "));
        }
        let name = target.to_string();
        let mut applied = vec![];
        let mut unchanged = vec![];
        for f in order {
            let Some(v) = fields.get(f) else { continue };
            let v = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
            if !self.store.lock().await.update_field(&actor, &name, f, &v)? {
                unchanged.push(f);
                continue;
            }
            match f {
                "model" | "effort" => {
                    self.relaunch.lock().await.insert(name.clone());
                }
                "lifecycle" if v != "active" => {
                    if let Some(id) = self.store.lock().await.get(&name)?.and_then(|x| x.harness_session_id) {
                        let _ = self.claude.close(&SessionId::new(id)).await;
                    }
                    self.live.lock().await.remove(&name);
                    self.store.lock().await.set_status(&name, "stopped")?;
                }
                _ => {}
            }
            applied.push(json!({"field": f, "new": v}));
        }
        let cur = self.store.lock().await.get(&name)?;
        let display = cur.as_ref().map(|x| x.name.clone()).unwrap_or_default();
        let warning = fields.get("model").and_then(Value::as_str).and_then(|m| store::model_warning(cur.as_ref().map(|x| x.harness.as_str()).unwrap_or("claude"), m));
        Ok(json!({"updated": name, "name": display, "changes": applied, "unchanged": unchanged, "by": actor, "warning": warning,
                  "note": "model/effort changes take effect from the session's next turn; the id never changes"}))
    }

    async fn shutdown_children(&self) {
        let names: Vec<String> = self.live.lock().await.drain().collect();
        for name in names {
            let _ = self.store.lock().await.set_status(&name, "stopped");
        }
        // One private tmux server per Kurisu home: stopping it closes every CLI.
        self.claude.kill_all().await;
    }
}

pub async fn serve(paths: Paths) -> Result<()> {
    let server = Arc::new(Server::new(paths.clone())?);
    let sock = paths.socket();
    let _ = std::fs::remove_file(&sock);
    let listener = crate::paths::bind_private(&sock)?;
    eprintln!("kurisu server listening on {} (pid {})", sock.display(), std::process::id());
    let mut expiry = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let server = server.clone();
                tokio::spawn(async move { let _ = connection(server, stream).await; });
            }
            _ = server.stop.notified() => break,
            _ = expiry.tick() => {
                let _ = server.expire_approvals().await;
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    server.shutdown_children().await;
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

async fn connection(server: Arc<Server>, stream: UnixStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => match server.handle(req).await {
                Ok(v) => json!({"ok": v}),
                Err(e) => json!({"error": format!("{e:#}")}),
            },
            Err(e) => json!({"error": format!("bad json: {e}")}),
        };
        let mut out = serde_json::to_vec(&reply)?;
        out.push(b'\n');
        w.write_all(&out).await?;
    }
    Ok(())
}

pub async fn call(sock: &Path, req: Value) -> Result<Value> {
    let stream = UnixStream::connect(sock).await.with_context(|| format!("connect {}", sock.display()))?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_vec(&req)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    let mut lines = BufReader::new(r).lines();
    let reply: Value = serde_json::from_str(&lines.next_line().await?.context("no reply")?)?;
    match reply.get("error") {
        Some(e) => bail!("{}", e.as_str().unwrap_or("error")),
        None => Ok(reply["ok"].clone()),
    }
}

/// Codex is wired but not part of Milestone 1's demo; keep the trait in use.
#[allow(dead_code)]
fn _assert_driver<D: Driver>() {}

#[cfg(test)]
mod tests {
    use super::*;

    async fn h(s: &Server, v: Value) -> Result<Value> {
        s.handle(v).await
    }

    #[tokio::test]
    async fn lineage_authority_and_audit() {
        let dir = tempfile::tempdir().unwrap();
        let s = Server::new(Paths { home: dir.path().to_path_buf() }).unwrap();
        h(&s, json!({"op": "create", "name": "lead", "role": "lead"})).await.unwrap();
        let w = h(&s, json!({"op": "create", "name": "worker-a", "description": "math", "caller": "lead"})).await.unwrap();
        assert_eq!(w["created_by_name"], "lead");
        assert!(crate::store::looks_like_uuid(w["id"].as_str().unwrap()));
        // by UUID works the same as by name
        let wid = w["id"].as_str().unwrap().to_string();
        assert_eq!(h(&s, json!({"op": "status", "name": wid})).await.unwrap()["name"], "worker-a");
        assert_eq!(w["model"], "claude-haiku-5-5");
        h(&s, json!({"op": "create", "name": "sub", "caller": "worker-a"})).await.unwrap();
        // a session cannot mint a lead
        assert!(h(&s, json!({"op": "create", "name": "x", "role": "lead", "caller": "lead"})).await.is_err());
        // ancestors may modify descendants (parent and grandparent)
        h(&s, json!({"op": "update", "name": "worker-a", "caller": "lead", "fields": {"effort": "low", "description": "arithmetic"}})).await.unwrap();
        h(&s, json!({"op": "update", "name": "sub", "caller": "lead", "fields": {"task": "idle"}})).await.unwrap();
        // non-ancestors are refused with a clear error
        let e = h(&s, json!({"op": "update", "name": "lead", "caller": "worker-a", "fields": {"description": "pwned"}})).await.unwrap_err();
        assert!(e.to_string().contains("permission denied: worker-a is not an ancestor of lead"), "{e}");
        assert!(h(&s, json!({"op": "update", "name": "worker-a", "caller": "sub", "fields": {"effort": "max"}})).await.is_err());
        assert!(h(&s, json!({"op": "update", "name": "worker-a", "caller": "lead", "fields": {"role": "lead"}})).await.is_err());
        assert!(h(&s, json!({"op": "update", "name": "worker-a", "caller": "lead", "fields": {"cwd": "/"}})).await.is_err());
        // the user may modify anything
        h(&s, json!({"op": "update", "name": "lead", "fields": {"effort": "high"}})).await.unwrap();
        // model/effort changes are queued for the next turn
        assert!(s.relaunch.lock().await.contains(&wid));
        let peers = h(&s, json!({"op": "peers"})).await.unwrap();
        let lead = peers.as_array().unwrap().iter().find(|p| p["name"] == "lead").unwrap().clone();
        assert_eq!(lead["children"][0]["name"], "worker-a");
        assert!(lead["created_by_session"].is_null());
        // pause blocks sends; archive hides from peers_list
        h(&s, json!({"op": "update", "name": "sub", "caller": "worker-a", "fields": {"lifecycle": "paused"}})).await.unwrap();
        let e = h(&s, json!({"op": "send", "name": "sub", "text": "hi", "caller": "lead"})).await.unwrap_err();
        assert!(e.to_string().contains("paused"), "{e}");
        h(&s, json!({"op": "update", "name": "sub", "fields": {"lifecycle": "archived"}})).await.unwrap();
        assert_eq!(h(&s, json!({"op": "peers"})).await.unwrap().as_array().unwrap().len(), 2);
        let audit = h(&s, json!({"op": "audit"})).await.unwrap();
        let actors: Vec<_> = audit.as_array().unwrap().iter().map(|a| (a["actor_name"].as_str().unwrap().to_string(), a["field"].as_str().unwrap().to_string())).collect();
        // rename keeps the id; the new name resolves, the old one does not
        h(&s, json!({"op": "update", "name": "worker-a", "caller": "lead", "fields": {"name": "math"}})).await.unwrap();
        assert_eq!(h(&s, json!({"op": "status", "name": "math"})).await.unwrap()["id"], wid);
        assert!(h(&s, json!({"op": "status", "name": "worker-a"})).await.is_err());
        assert!(actors.contains(&("lead".into(), "effort".into())));
        assert!(actors.contains(&("user".into(), "effort".into())));
        // sending to yourself is refused
        assert!(h(&s, json!({"op": "send", "name": "lead", "text": "x", "caller": "lead"})).await.is_err());
    }

    fn perm(rid: &str, tool: &str, input: Value, cwd: &str) -> RequestPermissionRequest {
        let ev = &claude_cli_acp::parse_events(&json!({"hook_event_name": "PermissionRequest", "tool_name": tool, "tool_input": input,
            "cwd": cwd, "kurisu_request_id": rid, "expires_at": now() + 600}).to_string())[0];
        claude_cli_acp::permission_request(&SessionId::new("x"), ev).unwrap()
    }

    #[tokio::test]
    async fn approvals_rules_decisions_and_audit() {
        let dir = tempfile::tempdir().unwrap();
        let s = Server::new(Paths { home: dir.path().to_path_buf() }).unwrap();
        let w = h(&s, json!({"op": "create", "name": "dev", "cwd": "/w/dev"})).await.unwrap();
        let wid = w["id"].as_str().unwrap().to_string();
        let hid = "11111111-2222-3333-4444-555555555555";
        s.store.lock().await.bind_harness_id(&wid, hid).unwrap();
        let decision = |rid: &str| -> Option<Value> {
            let p = dir.path().join("claude").join(hid).join("decisions").join(format!("{rid}.json"));
            std::fs::read(p).ok().map(|b| serde_json::from_slice(&b).unwrap())
        };
        // no rule: queued for the user, session awaits approval
        s.on_permission(&wid, perm("r-1", "Bash", json!({"command": "ls"}), "/w/dev")).await.unwrap();
        let pending = h(&s, json!({"op": "approvals"})).await.unwrap();
        assert_eq!(pending.as_array().unwrap().len(), 1);
        assert_eq!(h(&s, json!({"op": "status", "name": "dev"})).await.unwrap()["status"], "awaiting_approval");
        let id = pending[0]["id"].as_str().unwrap().to_string();
        assert_eq!(pending[0]["title"], "Bash: ls");
        // sessions cannot answer approvals; the user can, once
        assert!(h(&s, json!({"op": "approve", "id": id, "caller": "dev"})).await.is_err());
        let r = h(&s, json!({"op": "approve", "id": &id[..5]})).await.unwrap();
        assert_eq!(r["status"], "approved");
        assert_eq!(decision("r-1").unwrap()["behavior"], "allow");
        assert!(h(&s, json!({"op": "deny", "id": id})).await.unwrap_err().to_string().contains("already approved"));
        assert_eq!(h(&s, json!({"op": "status", "name": "dev"})).await.unwrap()["status"], "working");
        // deny with a reason
        s.on_permission(&wid, perm("r-2", "Write", json!({"file_path": "/etc/x"}), "/w/dev")).await.unwrap();
        let id2 = h(&s, json!({"op": "approvals"})).await.unwrap()[0]["id"].as_str().unwrap().to_string();
        h(&s, json!({"op": "deny", "id": id2, "reason": "not there"})).await.unwrap();
        assert_eq!(decision("r-2").unwrap(), json!({"behavior": "deny", "message": "The user denied this tool call: not there"}));
        // a role rule settles matching calls without asking; others still ask
        h(&s, json!({"op": "rule_add", "scope": "role", "target": "worker", "tool": "Bash", "in_cwd": true})).await.unwrap();
        s.on_permission(&wid, perm("r-3", "Bash", json!({"command": "cat src/a.rs"}), "/w/dev")).await.unwrap();
        assert_eq!(decision("r-3").unwrap()["behavior"], "allow");
        s.on_permission(&wid, perm("r-4", "Bash", json!({"command": "cat /etc/passwd"}), "/w/dev")).await.unwrap();
        assert!(decision("r-4").is_none());
        assert_eq!(h(&s, json!({"op": "approvals"})).await.unwrap().as_array().unwrap().len(), 1);
        // the turn ending cancels what is left
        s.cancel_pending(&wid, "the turn ended").await;
        assert!(h(&s, json!({"op": "approvals"})).await.unwrap().as_array().unwrap().is_empty());
        let all = h(&s, json!({"op": "approvals", "all": true})).await.unwrap();
        let statuses: Vec<_> = all.as_array().unwrap().iter().map(|a| a["status"].as_str().unwrap().to_string()).collect();
        assert_eq!(statuses, ["cancelled", "approved", "denied", "approved"]);
        assert_eq!(all[1]["decided_by"], "rule#1");
        // every decision is in the audit trail
        let audit = h(&s, json!({"op": "audit", "name": "dev"})).await.unwrap();
        let evs: Vec<_> = audit.as_array().unwrap().iter().map(|a| format!("{}:{}", a["actor_name"].as_str().unwrap(), a["field"].as_str().unwrap())).collect();
        for e in ["dev:event:approval_requested", "user:event:approval_approved", "user:event:approval_denied", "user:event:rule_added",
                  "rule#1:event:approval_approved", "kurisu:event:approval_cancelled"] {
            assert!(evs.contains(&e.to_string()), "{e} not in {evs:?}");
        }
        // sessions cannot add rules
        assert!(h(&s, json!({"op": "rule_add", "scope": "role", "target": "worker", "tool": "Write", "caller": "dev"})).await.is_err());
    }

    #[test]
    fn failed_turn_preamble_names_the_error() {
        let p = failed_turn_preamble("rate_limit");
        assert!(p.contains("rate_limit") && p.contains("Do not answer it"));
    }
}
