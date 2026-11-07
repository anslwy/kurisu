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
    ContentBlock, ContentChunk, PromptResponse, SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
    ToolCall,
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

pub struct CodexDriver {
    cfg: CodexConfig,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Pending,
    subs: Subs,
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
                Some(SessionUpdate::ToolCall(ToolCall::new(id, ty.to_string())))
            } else {
                None
            }
        }
        _ => None,
    }
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
        let drv = Self { cfg, stdin: Mutex::new(stdin), next_id: AtomicI64::new(1), pending: pending.clone(), subs: subs.clone(), _child: child };
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
                    // Phase 1: translate to ACP session/request_permission and ask the user.
                    m if m.ends_with("/requestApproval") => self.respond(req_id, json!({"decision": "decline"})).await?,
                    _ => self.respond_error(req_id, "unsupported by kurisu").await?,
                }
                continue;
            }
            if let Some(u) = to_update(method, &params) {
                let _ = updates.send(SessionNotification::new(id.clone(), u)).await;
            }
            if method == "turn/completed" {
                let status = params.pointer("/turn/status").and_then(Value::as_str).unwrap_or("failed");
                break stop_reason(status).map(PromptResponse::new);
            }
        };
        self.subs.lock().await.remove(&thread);
        result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_deltas_tools_and_status() {
        let u = to_update("item/agentMessage/delta", &json!({"delta": "PO", "threadId": "t"}));
        assert!(matches!(u, Some(SessionUpdate::AgentMessageChunk(_))));
        let u = to_update("item/started", &json!({"item": {"type": "commandExecution", "id": "c1"}}));
        assert!(matches!(u, Some(SessionUpdate::ToolCall(_))));
        assert!(to_update("item/started", &json!({"item": {"type": "agentMessage", "id": "m"}})).is_none());
        assert!(to_update("thread/tokenUsage/updated", &json!({})).is_none());
        assert!(matches!(stop_reason("completed").unwrap(), StopReason::EndTurn));
        assert!(matches!(stop_reason("interrupted").unwrap(), StopReason::Cancelled));
        assert!(stop_reason("failed").is_err());
    }
}
