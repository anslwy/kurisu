//! Codex driver: the official `codex app-server` (JSON-RPC over stdio, the
//! interface behind the Codex IDE extension), translated to ACP updates.
//!
//! One app-server child per Kurisu server process; threads are Codex sessions.
//! Auth is whatever the user set up with `codex login`. Kurisu never sends or
//! refreshes credentials: an `account/chatgptAuthTokens/refresh` request from
//! the server is answered with an error, so tokens stay inside Codex.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, Meta, PermissionOption, PermissionOptionKind, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionId, SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::driver::{Driver, Updates};

#[derive(Clone, Debug)]
pub struct CodexConfig {
    pub codex_bin: String,
    /// `read-only` | `workspace-write` | `danger-full-access`
    pub sandbox: String,
    /// `untrusted` | `on-request` | `never`
    pub approval_policy: String,
    pub turn_timeout: Duration,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            codex_bin: "codex".into(),
            sandbox: "workspace-write".into(),
            approval_policy: "on-request".into(),
            turn_timeout: Duration::from_secs(30 * 60),
        }
    }
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value>>>>>;
type Subs = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>>;
/// Approval requests waiting for the user: request id -> (thread, answer).
type Approvals = Arc<Mutex<HashMap<String, (String, oneshot::Sender<RequestPermissionOutcome>)>>>;
/// Where a turn's approval requests go (the server answers each with `respond_permission`).
pub type Perms = mpsc::Sender<RequestPermissionRequest>;

/// Option id for "allow this and the rest of the session".
pub const ALLOW_ALWAYS: &str = "allow_always";

pub struct CodexDriver {
    cfg: CodexConfig,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Pending,
    subs: Subs,
    approvals: Approvals,
    _child: Child,
}

/// Map one app-server notification to an ACP update (if any).
pub fn to_update(method: &str, params: &Value) -> Option<SessionUpdate> {
    match method {
        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|d| SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(d))))),
        "item/started" => {
            let item = params.get("item")?;
            let ty = item.get("type")?.as_str()?;
            if matches!(ty, "commandExecution" | "fileChange" | "mcpToolCall" | "webSearch") {
                let id = item.get("id").and_then(Value::as_str).unwrap_or("item").to_string();
                let (title, input) = item_title(ty, item);
                Some(SessionUpdate::ToolCall(ToolCall::new(id, title).name(ty.to_string()).status(ToolCallStatus::InProgress).raw_input(input)))
            } else {
                None
            }
        }
        "item/completed" => {
            let item = params.get("item")?;
            let ty = item.get("type")?.as_str()?;
            if !matches!(ty, "commandExecution" | "fileChange" | "mcpToolCall" | "webSearch") {
                return None;
            }
            let id = item.get("id").and_then(Value::as_str).unwrap_or("item").to_string();
            let failed = matches!(item.get("status").and_then(Value::as_str), Some("failed" | "declined"));
            let out = match ty {
                "commandExecution" => json!({"output": item.get("aggregatedOutput"), "exit_code": item.get("exitCode")}),
                "mcpToolCall" => json!({"result": item.get("result"), "error": item.get("error")}),
                _ => item.clone(),
            };
            let fields = ToolCallUpdateFields::new().name(ty.to_string())
                .status(if failed { ToolCallStatus::Failed } else { ToolCallStatus::Completed }).raw_output(out);
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields)))
        }
        _ => None,
    }
}

/// A readable title and the input of a Codex tool item.
fn item_title(ty: &str, item: &Value) -> (String, Value) {
    match ty {
        "commandExecution" => {
            let cmd = item.get("command").cloned().unwrap_or(Value::Null);
            let text = cmd.as_str().map(String::from).unwrap_or_else(|| cmd.to_string());
            (format!("Shell: {text}"), json!({"command": cmd, "cwd": item.get("cwd")}))
        }
        "mcpToolCall" => {
            let server = item.get("server").and_then(Value::as_str).unwrap_or("");
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
            (format!("MCP: {server}.{tool}"), json!({"server": server, "tool": tool, "arguments": item.get("arguments")}))
        }
        "fileChange" => {
            let paths: Vec<&str> = item.get("changes").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.get("path").and_then(Value::as_str)).collect()).unwrap_or_default();
            (format!("Edit: {}", paths.join(", ")), json!({"changes": item.get("changes")}))
        }
        _ => (ty.to_string(), item.clone()),
    }
}

/// The ACP permission request for one app-server approval request, or `None` when
/// Kurisu does not ask the user (unknown shapes, and `item/permissions/*` grants,
/// which have a different answer shape).
///
/// Tool names follow the Claude hook's (`Bash`, `Edit`) so that the user's
/// pre-approval rules apply unchanged.
pub fn approval_request(thread: &SessionId, method: &str, params: &Value, rid: &str) -> Option<RequestPermissionRequest> {
    if method.starts_with("item/permissions/") {
        return None;
    }
    let paths: Vec<String> = params
        .get("changes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|c| c.get("path").and_then(Value::as_str).map(String::from)).collect())
        .unwrap_or_default();
    let command = match params.get("command") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    };
    let (name, title, input) = if !command.is_empty() {
        let fields = json!({"command": command, "cwd": params.get("cwd"), "description": params.get("reason")});
        ("Bash", format!("Shell: {command}"), fields)
    } else if !paths.is_empty() {
        let fields = json!({"file_path": paths[0], "changes": params.get("changes")});
        ("Edit", format!("Edit: {}", paths.join(", ")), fields)
    } else {
        return None;
    };
    let title: String = title.chars().take(200).collect();
    let mut meta = Meta::new();
    if let Some(cwd) = params.get("cwd") {
        meta.insert("cwd".into(), cwd.clone());
    }
    let fields = ToolCallUpdateFields::new().title(title).name(name.to_string()).raw_input(input);
    Some(
        RequestPermissionRequest::new(
            thread.clone(),
            ToolCallUpdate::new(rid.to_string(), fields),
            vec![
                PermissionOption::new(claude_cli_acp::ALLOW_ONCE, "Allow", PermissionOptionKind::AllowOnce),
                PermissionOption::new(ALLOW_ALWAYS, "Always allow (this session)", PermissionOptionKind::AllowAlways),
                PermissionOption::new(claude_cli_acp::REJECT_ONCE, "Deny", PermissionOptionKind::RejectOnce),
            ],
        )
        .meta(meta),
    )
}

/// The app-server answer for the user's outcome: `accept` | `acceptForSession` |
/// `decline` | `cancel` (Codex turns a decline into `Rejected("rejected by user")`).
pub fn approval_decision(outcome: &RequestPermissionOutcome) -> Value {
    let id: &str = match outcome {
        RequestPermissionOutcome::Selected(s) => &s.option_id.0,
        _ => return json!({"decision": "cancel"}),
    };
    if id == claude_cli_acp::ALLOW_ONCE {
        json!({"decision": "accept"})
    } else if id == ALLOW_ALWAYS {
        json!({"decision": "acceptForSession"})
    } else {
        json!({"decision": "decline"})
    }
}

/// Token usage from a Codex rollout file (`$CODEX_HOME/sessions/**/rollout-*-<thread>.jsonl`):
/// the newest `token_count` event. No credentials are involved.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RolloutUsage {
    pub input_uncached: u64,
    pub cached_input: u64,
    pub cache_write: u64,
    pub output: u64,
    pub turns: u64,
    /// Tokens in the context after the last turn, and the model's window.
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

pub fn rollout_usage(text: &str) -> Option<RolloutUsage> {
    let mut out: Option<RolloutUsage> = None;
    let mut turns = 0;
    for line in text.lines().filter(|l| l.contains("\"token_count\"")) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let info = &v["payload"]["info"];
        let total = &info["total_token_usage"];
        if !total.is_object() {
            continue;
        }
        turns += 1;
        let n = |x: &Value, k: &str| x[k].as_u64().unwrap_or(0);
        let input = n(total, "input_tokens");
        let cached = n(total, "cached_input_tokens");
        out = Some(RolloutUsage {
            input_uncached: input.saturating_sub(cached),
            cached_input: cached,
            cache_write: n(total, "cache_write_input_tokens"),
            output: n(total, "output_tokens"),
            turns,
            context_tokens: info["last_token_usage"]["total_tokens"].as_u64(),
            context_window: info["model_context_window"].as_u64(),
        });
    }
    out
}

/// The rollout file of a Codex thread, if Codex wrote one.
pub fn find_rollout(thread_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))?;
    let suffix = format!("-{thread_id}.jsonl");
    fn walk(dir: &std::path::Path, suffix: &str, depth: usize) -> Option<PathBuf> {
        let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
        // Newest first: dated folders sort by name.
        entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
        for e in entries {
            let p = e.path();
            if p.is_dir() && depth < 4 {
                if let Some(f) = walk(&p, suffix, depth + 1) {
                    return Some(f);
                }
            } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-") && n.ends_with(suffix)) {
                return Some(p);
            }
        }
        None
    }
    walk(&home.join("sessions"), &suffix, 0)
}

/// Map a completed turn's status to an ACP stop reason.
pub fn stop_reason(status: &str) -> Result<StopReason> {
    match status {
        "completed" => Ok(StopReason::EndTurn),
        "interrupted" => Ok(StopReason::Cancelled),
        other => bail!("codex turn ended with status {other}"),
    }
}

impl CodexDriver {
    pub async fn spawn(cfg: CodexConfig) -> Result<Self> {
        let mut child = Command::new(&cfg.codex_bin)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawn codex app-server")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending: Pending = Arc::default();
        let subs: Subs = Arc::default();
        let approvals: Approvals = Arc::default();
        let drv = Self {
            cfg, stdin: Mutex::new(stdin), next_id: AtomicI64::new(1), pending: pending.clone(), subs: subs.clone(),
            approvals, _child: child,
        };
        tokio::spawn(read_loop(BufReader::new(stdout), pending, subs));
        drv.request("initialize", json!({"clientInfo": {"name": "kurisu", "title": "Kurisu", "version": env!("CARGO_PKG_VERSION")}})).await?;
        drv.notify("initialized", json!({})).await?;
        Ok(drv)
    }

    async fn send(&self, msg: Value) -> Result<()> {
        let mut line = serde_json::to_vec(&msg)?;
        line.push(b'\n');
        let mut w = self.stdin.lock().await;
        w.write_all(&line).await?;
        w.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"method": method, "params": params})).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.send(json!({"id": id, "method": method, "params": params})).await?;
        tokio::time::timeout(Duration::from_secs(120), rx).await.context("app-server timeout")??
    }

    fn thread_params(&self, cwd: &PathBuf) -> Value {
        json!({"cwd": cwd, "sandbox": self.cfg.sandbox, "approvalPolicy": self.cfg.approval_policy})
    }

    /// Start a thread with extra thread/start params (config overrides, developer instructions).
    pub async fn new_session_with(&self, cwd: PathBuf, extra: &Value) -> Result<SessionId> {
        let mut p = self.thread_params(&cwd);
        merge(&mut p, extra);
        let r = self.request("thread/start", p).await?;
        let id = r.pointer("/thread/id").and_then(Value::as_str).ok_or_else(|| anyhow!("no thread id"))?;
        Ok(SessionId::new(id.to_string()))
    }

    pub async fn load_session_with(&self, id: &SessionId, cwd: PathBuf, extra: &Value) -> Result<()> {
        let mut p = self.thread_params(&cwd);
        merge(&mut p, extra);
        p["threadId"] = json!(id.to_string());
        p["excludeTurns"] = json!(true);
        self.request("thread/resume", p).await.map(|_| ())
    }
}

fn merge(p: &mut Value, extra: &Value) {
    if let (Some(p), Some(e)) = (p.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
}

/// Reader: routes responses to waiters, and notifications and server requests
/// to the subscriber of their thread (answered in `prompt`).
async fn read_loop(
    mut out: BufReader<tokio::process::ChildStdout>,
    pending: Pending,
    subs: Subs,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match out.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let id = msg.get("id").and_then(Value::as_i64);
        let method = msg.get("method").and_then(Value::as_str);
        match (id, method) {
            (Some(id), None) => {
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let res = match msg.get("error") {
                        Some(e) => Err(anyhow!("app-server error: {e}")),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(res);
                }
            }
            (_, Some(_)) => {
                // Notifications and server requests: hand to the thread's subscriber.
                let thread = msg
                    .pointer("/params/threadId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(tx) = subs.lock().await.get(&thread) {
                    let _ = tx.send(msg);
                }
            }
            _ => {}
        }
    }
}

impl CodexDriver {
    async fn respond(&self, id: &Value, result: Value) -> Result<()> {
        self.send(json!({"id": id, "result": result})).await
    }
    async fn respond_error(&self, id: &Value, message: &str) -> Result<()> {
        self.send(json!({"id": id, "error": {"code": -32601, "message": message}})).await
    }
}

impl Driver for CodexDriver {
    fn harness(&self) -> &'static str {
        "codex"
    }

    async fn new_session(&self, cwd: PathBuf) -> Result<SessionId> {
        let r = self.request("thread/start", self.thread_params(&cwd)).await?;
        let id = r.pointer("/thread/id").and_then(Value::as_str).ok_or_else(|| anyhow!("no thread id"))?;
        Ok(SessionId::new(id.to_string()))
    }

    async fn load_session(&self, id: &SessionId, cwd: PathBuf) -> Result<()> {
        let mut p = self.thread_params(&cwd);
        p["threadId"] = json!(id.to_string());
        p["excludeTurns"] = json!(true);
        self.request("thread/resume", p).await.map(|_| ())
    }

    async fn prompt(&self, id: &SessionId, text: &str, updates: Updates) -> Result<PromptResponse> {
        self.prompt_with_permissions(id, text, updates, None).await
    }

    async fn cancel(&self, id: &SessionId) -> Result<()> {
        // turn/interrupt needs the turn id; Phase 1 tracks it (spike: no-op).
        let _ = id;
        Ok(())
    }

    async fn close(&self, _id: &SessionId) -> Result<()> {
        Ok(())
    }
}

impl CodexDriver {
    /// Like `prompt`, but approval requests go to `perms` as ACP
    /// `RequestPermissionRequest`s; answer each with `respond_permission`.
    /// Without `perms` they are declined, so a turn never hangs.
    pub async fn prompt_with_permissions(
        &self,
        id: &SessionId,
        text: &str,
        updates: Updates,
        perms: Option<Perms>,
    ) -> Result<PromptResponse> {
        let thread = id.to_string();
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.subs.lock().await.insert(thread.clone(), tx);
        let started = self
            .request("turn/start", json!({"threadId": thread, "input": [{"type": "text", "text": text}]}))
            .await;
        if let Err(e) = started {
            self.subs.lock().await.remove(&thread);
            return Err(e);
        }
        let deadline = tokio::time::Instant::now() + self.cfg.turn_timeout;
        let mut sent_text = false;
        let result = loop {
            let msg = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(m)) => m,
                Ok(None) => break Err(anyhow!("app-server closed")),
                Err(_) => break Err(anyhow!("turn timed out")),
            };
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(req_id) = msg.get("id") {
                // Server -> client request.
                match method {
                    "account/chatgptAuthTokens/refresh" => {
                        self.respond_error(req_id, "kurisu does not handle credentials").await?
                    }
                    m if m.ends_with("/requestApproval") => {
                        let answer = self.ask_approval(id, m, &params, req_id, &perms, deadline).await;
                        self.respond(req_id, answer).await?
                    }
                    _ => self.respond_error(req_id, "unsupported by kurisu").await?,
                }
                continue;
            }
            // A new agent message item after earlier text: keep the messages apart.
            if method == "item/started" && params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") && sent_text {
                let sep = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new("\n\n"))));
                let _ = updates.send(SessionNotification::new(id.clone(), sep)).await;
            }
            if let Some(u) = to_update(method, &params) {
                sent_text |= matches!(u, SessionUpdate::AgentMessageChunk(_));
                let _ = updates.send(SessionNotification::new(id.clone(), u)).await;
            }
            if method == "turn/completed" {
                let status = params.pointer("/turn/status").and_then(Value::as_str).unwrap_or("failed");
                break stop_reason(status).map(PromptResponse::new);
            }
        };
        self.subs.lock().await.remove(&thread);
        // The turn is over: nothing will answer these (the server cancels what it queued).
        self.approvals.lock().await.retain(|_, (t, _)| t != &thread);
        result
    }

    /// Ask the user about one app-server approval request; returns the JSON answer.
    async fn ask_approval(
        &self,
        thread: &SessionId,
        method: &str,
        params: &Value,
        req_id: &Value,
        perms: &Option<Perms>,
        deadline: tokio::time::Instant,
    ) -> Value {
        let decline = json!({"decision": "decline"});
        let rid = format!("codex-{thread}-{req_id}");
        let (Some(perms), Some(req)) = (perms, approval_request(thread, method, params, &rid)) else {
            return decline;
        };
        let (tx, rx) = oneshot::channel();
        self.approvals.lock().await.insert(rid.clone(), (thread.to_string(), tx));
        if perms.send(req).await.is_err() {
            // No approver connected any more.
            self.approvals.lock().await.remove(&rid);
            return decline;
        }
        // The server answers through `respond_permission`; expiry and a stopped turn
        // both land there too, so this cannot wait past the turn's deadline.
        let answer = tokio::time::timeout_at(deadline, rx).await;
        self.approvals.lock().await.remove(&rid);
        match answer {
            Ok(Ok(outcome)) => approval_decision(&outcome),
            _ => decline,
        }
    }

    /// Answer a pending approval request (`request_id` is the tool call id the
    /// server was given). Unknown ids (already expired, turn over) are ignored.
    pub async fn respond_permission(&self, request_id: &str, outcome: &RequestPermissionOutcome) -> Result<()> {
        if let Some((_, tx)) = self.approvals.lock().await.remove(request_id) {
            let _ = tx.send(outcome.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{PermissionOptionId, SelectedPermissionOutcome};

    #[test]
    fn approval_requests_become_permission_cards() {
        let t = SessionId::new("th");
        let p = json!({"threadId": "th", "itemId": "i1", "command": ["rm", "-rf", "/x"], "cwd": "/w", "reason": "cleanup"});
        let r = approval_request(&t, "item/commandExecution/requestApproval", &p, "rid1").unwrap();
        assert_eq!(r.tool_call.tool_call_id.to_string(), "rid1");
        assert_eq!(r.tool_call.fields.name.as_deref(), Some("Bash"));
        let input = r.tool_call.fields.raw_input.as_ref().unwrap();
        assert_eq!(input["command"], "rm -rf /x");
        assert_eq!(input["cwd"], "/w");
        assert_eq!(r.meta.as_ref().unwrap().get("cwd").unwrap(), "/w");
        assert!(r.options.iter().any(|o| &*o.option_id.0 == ALLOW_ALWAYS));
        let p = json!({"changes": [{"path": "/w/a.rs"}, {"path": "/w/b.rs"}]});
        let r = approval_request(&t, "item/fileChange/requestApproval", &p, "rid2").unwrap();
        assert_eq!(r.tool_call.fields.name.as_deref(), Some("Edit"));
        assert_eq!(r.tool_call.fields.raw_input.as_ref().unwrap()["file_path"], "/w/a.rs");
        // Extra-permission grants have another answer shape (and empty requests are not asked).
        assert!(approval_request(&t, "item/permissions/requestApproval", &json!({"permissions": {}}), "r").is_none());
        assert!(approval_request(&t, "item/fileChange/requestApproval", &json!({}), "r").is_none());
    }

    #[test]
    fn maps_permission_outcomes_to_codex_decisions() {
        let sel = |id: &str| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(PermissionOptionId::new(id)));
        assert_eq!(approval_decision(&sel(claude_cli_acp::ALLOW_ONCE)), json!({"decision": "accept"}));
        assert_eq!(approval_decision(&sel(ALLOW_ALWAYS)), json!({"decision": "acceptForSession"}));
        assert_eq!(approval_decision(&sel(claude_cli_acp::REJECT_ONCE)), json!({"decision": "decline"}));
        assert_eq!(approval_decision(&RequestPermissionOutcome::Cancelled), json!({"decision": "cancel"}));
    }

    #[test]
    fn maps_deltas_tools_and_status() {
        let u = to_update("item/agentMessage/delta", &json!({"delta": "PO", "threadId": "t"}));
        assert!(matches!(u, Some(SessionUpdate::AgentMessageChunk(_))));
        let u = to_update("item/started", &json!({"item": {"type": "commandExecution", "id": "c1"}}));
        assert!(matches!(u, Some(SessionUpdate::ToolCall(_))));
        assert!(to_update("item/started", &json!({"item": {"type": "agentMessage", "id": "m"}})).is_none());
        assert!(to_update("thread/tokenUsage/updated", &json!({})).is_none());
        let u = to_update("item/completed", &json!({"item": {"type": "commandExecution", "id": "c1", "command": "ls", "status": "completed", "aggregatedOutput": "a\n", "exitCode": 0}}));
        let Some(SessionUpdate::ToolCallUpdate(u)) = u else { panic!() };
        assert_eq!(u.fields.raw_output.unwrap()["output"], "a\n");
        let Some(SessionUpdate::ToolCall(t)) = to_update("item/started", &json!({"item": {"type": "mcpToolCall", "id": "m", "server": "kurisu-mcp", "tool": "session_handoff", "arguments": {"id": "x"}}})) else { panic!() };
        assert_eq!(t.title, "MCP: kurisu-mcp.session_handoff");
        assert!(matches!(stop_reason("completed").unwrap(), StopReason::EndTurn));
        assert!(matches!(stop_reason("interrupted").unwrap(), StopReason::Cancelled));
        assert!(stop_reason("failed").is_err());
        let t = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":5,"total_tokens":105},"last_token_usage":{"total_tokens":105},"model_context_window":258400}}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":250,"cached_input_tokens":200,"cache_write_input_tokens":3,"output_tokens":9,"total_tokens":259},"last_token_usage":{"total_tokens":154},"model_context_window":258400}}}"#);
        let u = rollout_usage(t).unwrap();
        assert_eq!((u.input_uncached, u.cached_input, u.cache_write, u.output, u.turns), (50, 200, 3, 9, 2));
        assert_eq!((u.context_tokens, u.context_window), (Some(154), Some(258400)));
        assert!(rollout_usage("{}").is_none());
    }
}
