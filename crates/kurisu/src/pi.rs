//! pi (the `pi` coding agent) as a Kurisu harness.
//!
//! pi has no ACP server; it speaks its own JSONL protocol (`pi --mode rpc`).
//! `kurisu pi-acp` is a small bridge: an ACP agent on stdio (so the server
//! drives it exactly like fx/agy through `AcpDriver`) that runs one
//! `pi --mode rpc` child per ACP session and translates between the two.
//!
//! - MCP: each session gets its own pi config dir (`PI_CODING_AGENT_DIR`) under
//!   `<KURISU_HOME>/pi/agents/<session>`, which links every entry of the user's
//!   real `~/.pi/agent` (logins, models, settings, extensions) except
//!   `mcp.json`, which Kurisu writes from the ACP `mcpServers` it was handed.
//! - Approvals: pi has no permission system of its own. Kurisu loads a gate
//!   extension that asks `ctx.ui.confirm("kurisu:approve", ...)` before every
//!   tool call; in RPC mode that arrives here as an `extension_ui_request`, and
//!   the bridge answers it from the session's permission mode, or turns it into
//!   an ACP `session/request_permission` (a Kurisu approval row).
//! - Plugins: every file or folder in `<KURISU_HOME>/pi/extensions` is passed
//!   to pi with `-e`, so Kurisu controls which extensions a pi session loads.
//!
//! Kurisu model ids for pi are pi's own `provider/model`
//! (`ollama/deepseek-v4.1-flash:cloud`), or `default` for pi's saved choice.
use crate::acp::AcpDriver;
use agent_client_protocol::v1::schema::SessionId;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

/// Title the gate extension uses for its confirm dialog.
pub const GATE_TITLE: &str = "kurisu:approve";
/// Kurisu's permission modes pi supports (the bridge enforces them).
pub const MODES: &[&str] = &["default", "auto", "acceptEdits", "plan", "bypassPermissions"];
/// pi's thinking levels Kurisu offers as effort.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

const GATE_TS: &str = r#"// Written by Kurisu: asks the Kurisu bridge before every tool call.
export default function (pi: any) {
  pi.on("tool_call", async (event: any, ctx: any) => {
    const ok = await ctx.ui.confirm("kurisu:approve", JSON.stringify({ toolCallId: event.toolCallId, toolName: event.toolName, input: event.input }));
    if (!ok) return { block: true, reason: `${event.toolName} was not approved` };
  });
}
"#;

/// The approval gate is on unless `KURISU_PI_GATE` is 0/false/off: then pi
/// runs every tool with no card, whatever the session's permission mode.
pub fn gate_enabled(v: Option<&str>) -> bool {
    !matches!(v.map(str::to_ascii_lowercase).as_deref(), Some("0" | "false" | "off" | "no"))
}

pub fn bin() -> String {
    crate::harness::resolve("pi")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| std::env::var("KURISU_PI_BIN").unwrap_or_else(|_| "pi".into()))
}

/// Kurisu's pi folder (gate, per-session config dirs, sessions, extensions).
pub fn home_for(kurisu_home: &Path) -> PathBuf {
    kurisu_home.join("pi")
}

/// Environment the server gives `kurisu pi-acp`.
pub fn bridge_env(kurisu_home: &Path) -> Vec<(String, String)> {
    vec![
        ("KURISU_PI_HOME".into(), home_for(kurisu_home).to_string_lossy().into_owned()),
        ("KURISU_PI_BIN".into(), bin()),
    ]
}

/// `(provider, model)` for pi's `set_model`; None for `default`.
pub fn split_model(model: &str) -> Option<(String, String)> {
    if model.is_empty() || model == "default" {
        return None;
    }
    match model.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => Some((p.into(), m.into())),
        _ => None,
    }
}

pub fn check_mode(mode: Option<&str>) -> Result<()> {
    let m = mode.unwrap_or("default");
    if !MODES.contains(&m) {
        bail!("pi permission mode {m:?} unsupported; use {}", MODES.join("|"));
    }
    Ok(())
}

/// Model, effort and mode go over ACP config options; the bridge applies them.
pub async fn configure(driver: &AcpDriver, id: &SessionId, model: &str, effort: Option<&str>, mode: Option<&str>) -> Result<()> {
    check_mode(mode)?;
    if split_model(model).is_some() {
        driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"model","value":model})).await?;
    }
    if let Some(e) = effort.filter(|e| *e != "default") {
        driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"effort","value":e})).await?;
    }
    driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"mode","value":mode.unwrap_or("default")})).await?;
    Ok(())
}

/// Parse `pi --list-models` (a table: provider, model, ...).
pub fn parse_models(out: &str) -> Vec<String> {
    out.lines()
        .skip_while(|l| !l.trim_start().starts_with("provider"))
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some(format!("{}/{}", it.next()?, it.next()?))
        })
        .collect()
}

pub fn catalogue() -> Vec<(String, String)> {
    let mut out = vec![("default".to_string(), "pi default".to_string())];
    let listed = std::process::Command::new(bin())
        .arg("--list-models")
        .env("PI_OFFLINE", "1")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    if let Ok(o) = listed {
        for id in parse_models(&String::from_utf8_lossy(&o.stdout)) {
            out.push((id.clone(), id));
        }
    }
    out
}

/// What the bridge does with a tool call under a permission mode.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

const READ_ONLY: &[&str] = &["read", "grep", "find", "ls"];
const EDITS: &[&str] = &["edit", "write"];

pub fn decide(mode: &str, tool: &str, always: &HashSet<String>) -> Decision {
    if READ_ONLY.contains(&tool) {
        return Decision::Allow;
    }
    match mode {
        "auto" | "bypassPermissions" => Decision::Allow,
        "plan" => Decision::Deny,
        _ if always.contains(tool) => Decision::Allow,
        "acceptEdits" if EDITS.contains(&tool) => Decision::Allow,
        _ => Decision::Ask,
    }
}

/// The ACP `session/request_permission` params for one pi tool call.
pub fn permission_params(sid: &str, call: &Value) -> Value {
    let tool = call["toolName"].as_str().unwrap_or("");
    json!({"sessionId": sid, "toolCall": {"toolCallId": call["toolCallId"], "name": tool, "title": tool, "kind": tool_kind(tool), "status":"pending", "rawInput": call["input"]},
        "options": [
            {"optionId":"allow_once","name":"Allow","kind":"allow_once"},
            {"optionId":"allow_always","name":"Always allow","kind":"allow_always"},
            {"optionId":"reject_once","name":"Deny","kind":"reject_once"}]})
}

pub fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "read" | "ls" => "read",
        "edit" | "write" => "edit",
        "grep" | "find" => "search",
        "bash" | "powershell" => "execute",
        _ => "other",
    }
}

/// pi's `mcp.json` for the ACP `mcpServers` list. Tools are exposed directly
/// (`mcp__<server>__<tool>`), as Claude and the other harnesses name them.
pub fn mcp_json(servers: &Value) -> Value {
    let pairs = |v: &Value| -> serde_json::Map<String, Value> {
        v.as_array().into_iter().flatten().filter_map(|e| Some((e["name"].as_str()?.to_string(), e["value"].clone()))).collect()
    };
    let mut out = serde_json::Map::new();
    for s in servers.as_array().into_iter().flatten() {
        let Some(name) = s["name"].as_str() else { continue };
        let entry = if let Some(url) = s["url"].as_str() {
            json!({"url": url, "headers": pairs(&s["headers"]), "exposure": "direct"})
        } else if s["command"].is_string() {
            json!({"command": s["command"], "args": s.get("args").cloned().unwrap_or(json!([])), "env": pairs(&s["env"]), "exposure": "direct"})
        } else {
            continue;
        };
        out.insert(name.to_string(), entry);
    }
    json!({ "mcpServers": out })
}

/// `-e` paths: every file or folder directly in `dir`, sorted, hidden ones skipped.
pub fn extension_paths(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// Text of an ACP prompt (context block first, as sent).
pub fn prompt_text(prompt: &Value) -> String {
    prompt
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| match b["type"].as_str() {
            Some("text") => b["text"].as_str().map(String::from),
            Some("resource_link") => b["uri"].as_str().map(|u| format!("@{u}")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ACP `session/update` payload for one pi event, when it maps to one.
pub fn translate(ev: &Value) -> Option<Value> {
    match ev["type"].as_str()? {
        "message_update" => {
            let e = &ev["assistantMessageEvent"];
            match e["type"].as_str()? {
                "text_delta" => Some(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":e["delta"]}})),
                "thinking_delta" => Some(json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":e["delta"]}})),
                _ => None,
            }
        }
        "tool_execution_start" => {
            let name = ev["toolName"].as_str().unwrap_or("tool");
            Some(json!({"sessionUpdate":"tool_call","toolCallId":ev["toolCallId"],"title":name,"kind":tool_kind(name),"status":"in_progress","rawInput":ev["args"]}))
        }
        "tool_execution_end" => {
            let text = result_text(&ev["result"]);
            Some(json!({"sessionUpdate":"tool_call_update","toolCallId":ev["toolCallId"],
                "status": if ev["isError"] == true {"failed"} else {"completed"},
                "content":[{"type":"content","content":{"type":"text","text":text}}],
                "rawOutput": ev["result"]}))
        }
        "message_end" if ev["message"]["role"] == "assistant" && ev["message"]["stopReason"] == "error" => {
            let msg = ev["message"]["errorMessage"].as_str().unwrap_or("pi: provider error");
            Some(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("\n[pi error] {msg}")}}))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- bridge

type Out = mpsc::UnboundedSender<Value>;
type Waiters = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

struct PiSession {
    id: String,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    mode: Mutex<String>,
    always: Mutex<HashSet<String>>,
    /// The ACP request id of the running prompt.
    prompt: Mutex<Option<Value>>,
    cancelled: AtomicBool,
    pending: Waiters,
    next: AtomicU64,
}

impl PiSession {
    async fn send(&self, v: Value) -> Result<()> {
        let mut w = self.stdin.lock().await;
        w.write_all(format!("{v}\n").as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }
    async fn call(&self, mut v: Value) -> Result<Value> {
        let id = format!("k{}", self.next.fetch_add(1, Ordering::Relaxed));
        v["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        self.send(v).await?;
        let r = tokio::time::timeout(std::time::Duration::from_secs(60), rx).await.context("pi command timed out")?.context("pi exited")?;
        if r["success"] == false {
            bail!("pi: {}", r["error"].as_str().unwrap_or("command failed"));
        }
        Ok(r)
    }
}

struct Bridge {
    out: Out,
    home: PathBuf,
    pi: String,
    sessions: Mutex<HashMap<String, Arc<PiSession>>>,
    client: Waiters,
    next: AtomicU64,
}

impl Bridge {
    fn reply(&self, id: &Value, result: Value) {
        let _ = self.out.send(json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
    fn error(&self, id: &Value, msg: String) {
        let _ = self.out.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":msg}}));
    }
    fn update(&self, sid: &str, update: Value) {
        let _ = self.out.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,"update":update}}));
    }
    async fn ask_client(&self, method: &str, params: Value) -> Result<Value> {
        let id = format!("pi{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.client.lock().await.insert(id.clone(), tx);
        let _ = self.out.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let v = rx.await.context("client went away")?;
        if v.get("error").is_some() {
            bail!("client: {}", v["error"]);
        }
        Ok(v["result"].clone())
    }

    /// The per-session pi config dir: links to the real one, Kurisu's mcp.json.
    fn agent_dir(&self, sid: &str, mcp: &Value) -> Result<PathBuf> {
        let dir = self.home.join("agents").join(sid);
        std::fs::create_dir_all(&dir)?;
        let real = std::env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi/agent")));
        if let Some(real) = real.filter(|r| r.is_dir()) {
            for e in std::fs::read_dir(&real)?.flatten() {
                let name = e.file_name();
                if name == "mcp.json" || name == "sessions" {
                    continue;
                }
                let link = dir.join(&name);
                if link.symlink_metadata().is_err() {
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(e.path(), &link)?;
                }
            }
        }
        let tmp = dir.join("mcp.json.kurisu-tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&mcp_json(mcp))?)?;
        std::fs::rename(&tmp, dir.join("mcp.json"))?;
        Ok(dir)
    }

    async fn start(self: &Arc<Self>, sid: String, cwd: &str, mcp: &Value) -> Result<()> {
        if let Some(old) = self.sessions.lock().await.remove(&sid) {
            let _ = old.child.lock().await.kill().await;
        }
        let agent = self.agent_dir(&sid, mcp)?;
        let gate = self.home.join("kurisu-gate.ts");
        std::fs::write(&gate, GATE_TS)?;
        let ext_dir = self.home.join("extensions");
        std::fs::create_dir_all(&ext_dir)?;
        let mut cmd = Command::new(&self.pi);
        cmd.args(["--mode", "rpc", "--offline", "--session-dir"]).arg(self.home.join("sessions"));
        cmd.args(["--session-id", &sid]);
        if gate_enabled(std::env::var("KURISU_PI_GATE").ok().as_deref()) {
            cmd.arg("-e").arg(&gate);
        }
        for e in extension_paths(&ext_dir) {
            cmd.arg("-e").arg(e);
        }
        let mut child = cmd
            .current_dir(cwd)
            .env("PI_CODING_AGENT_DIR", &agent)
            .env("KURISU_PI_SESSION", &sid)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {} --mode rpc", self.pi))?;
        let stdout = child.stdout.take().context("pi stdout")?;
        let s = Arc::new(PiSession {
            id: sid.clone(),
            stdin: Mutex::new(child.stdin.take().context("pi stdin")?),
            child: Mutex::new(child),
            mode: Mutex::new("default".into()),
            always: Default::default(),
            prompt: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            pending: Default::default(),
            next: AtomicU64::new(1),
        });
        self.sessions.lock().await.insert(sid.clone(), s.clone());
        let me = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(ev) = serde_json::from_str::<Value>(&line) else { continue };
                me.on_pi(&s, ev).await;
            }
            for (_, tx) in s.pending.lock().await.drain() {
                let _ = tx.send(json!({"success":false,"error":"pi exited"}));
            }
            if let Some(id) = s.prompt.lock().await.take() {
                me.error(&id, "pi exited during the turn".into());
            }
        });
        // Wait until pi answers a command, so MCP/extension startup is done.
        s_ready(self, &sid).await
    }

    async fn on_pi(self: &Arc<Self>, s: &Arc<PiSession>, ev: Value) {
        match ev["type"].as_str() {
            Some("response") => {
                if let Some(id) = ev["id"].as_str() {
                    if let Some(tx) = s.pending.lock().await.remove(id) {
                        let _ = tx.send(ev);
                    }
                }
            }
            Some("extension_ui_request") => {
                let method = ev["method"].as_str().unwrap_or("").to_string();
                if !matches!(method.as_str(), "confirm" | "select" | "input" | "editor") {
                    return;
                }
                let (me, s) = (self.clone(), s.clone());
                tokio::spawn(async move {
                    let answer = if method == "confirm" && ev["title"] == GATE_TITLE {
                        json!({"confirmed": me.approve(&s, &ev).await})
                    } else {
                        json!({"cancelled": true})
                    };
                    let mut r = json!({"type":"extension_ui_response","id":ev["id"]});
                    r.as_object_mut().unwrap().extend(answer.as_object().unwrap().clone());
                    let _ = s.send(r).await;
                });
            }
            Some("agent_settled") => {
                if let Some(id) = s.prompt.lock().await.take() {
                    let reason = if s.cancelled.swap(false, Ordering::AcqRel) || ev["aborted"] == true { "cancelled" } else { "end_turn" };
                    self.reply(&id, json!({"stopReason": reason}));
                }
            }
            _ => {
                if let Some(u) = translate(&ev) {
                    self.update(&s.id, u);
                }
            }
        }
    }

    async fn approve(&self, s: &PiSession, ev: &Value) -> bool {
        let call: Value = ev["message"].as_str().and_then(|m| serde_json::from_str(m).ok()).unwrap_or(Value::Null);
        let tool = call["toolName"].as_str().unwrap_or("").to_string();
        let mode = s.mode.lock().await.clone();
        match decide(&mode, &tool, &*s.always.lock().await) {
            Decision::Allow => return true,
            Decision::Deny => return false,
            Decision::Ask => {}
        }
        let params = permission_params(&s.id, &call);
        let Ok(r) = self.ask_client("session/request_permission", params).await else { return false };
        match r["outcome"]["optionId"].as_str() {
            Some("allow_always") if r["outcome"]["outcome"] == "selected" => {
                s.always.lock().await.insert(tool);
                true
            }
            Some("allow_once") if r["outcome"]["outcome"] == "selected" => true,
            _ => false,
        }
    }

    async fn session(&self, p: &Value) -> Result<Arc<PiSession>> {
        let sid = p["sessionId"].as_str().context("sessionId")?;
        self.sessions.lock().await.get(sid).cloned().with_context(|| format!("unknown pi session {sid}"))
    }

    async fn handle(self: &Arc<Self>, id: Value, method: &str, p: Value) {
        let r: Result<Option<Value>> = async {
            Ok(Some(match method {
                "initialize" => json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"kurisu-pi","version":env!("CARGO_PKG_VERSION")},"authMethods":[]}),
                "session/new" => {
                    let sid = uuid::Uuid::new_v4().to_string();
                    self.start(sid.clone(), p["cwd"].as_str().unwrap_or("."), &p["mcpServers"]).await?;
                    json!({"sessionId": sid, "configOptions": []})
                }
                "session/load" => {
                    let sid = p["sessionId"].as_str().context("sessionId")?.to_string();
                    self.start(sid, p["cwd"].as_str().unwrap_or("."), &p["mcpServers"]).await?;
                    json!({"configOptions": []})
                }
                "session/set_config_option" => {
                    let s = self.session(&p).await?;
                    let value = p["value"].as_str().unwrap_or("");
                    match p["configId"].as_str() {
                        Some("model") => {
                            if let Some((provider, model)) = split_model(value) {
                                s.call(json!({"type":"set_model","provider":provider,"modelId":model})).await?;
                            }
                        }
                        Some("effort") => {
                            s.call(json!({"type":"set_thinking_level","level":value})).await?;
                        }
                        Some("mode") => {
                            check_mode(Some(value))?;
                            *s.mode.lock().await = value.to_string();
                        }
                        other => bail!("pi has no config option {other:?}"),
                    }
                    json!({"configOptions": []})
                }
                "session/set_mode" => {
                    let s = self.session(&p).await?;
                    let m = p["modeId"].as_str().unwrap_or("default");
                    check_mode(Some(m))?;
                    *s.mode.lock().await = m.to_string();
                    json!({})
                }
                "session/prompt" => {
                    let s = self.session(&p).await?;
                    {
                        let mut cur = s.prompt.lock().await;
                        if cur.is_some() {
                            bail!("pi session is busy");
                        }
                        *cur = Some(id.clone());
                    }
                    s.cancelled.store(false, Ordering::Release);
                    let sent = s.call(json!({"type":"prompt","message":prompt_text(&p["prompt"])})).await;
                    match sent {
                        Ok(r) if r["data"]["disposition"] == "started" || r["data"]["disposition"].is_null() => return Ok(None),
                        Ok(_) => {
                            s.prompt.lock().await.take();
                            json!({"stopReason":"end_turn"})
                        }
                        Err(e) => {
                            s.prompt.lock().await.take();
                            return Err(e);
                        }
                    }
                }
                "session/close" | "session/delete" => {
                    if let Some(sid) = p["sessionId"].as_str() {
                        if let Some(s) = self.sessions.lock().await.remove(sid) {
                            let _ = s.child.lock().await.kill().await;
                        }
                    }
                    json!({})
                }
                other => bail!("method not found: {other}"),
            }))
        }
        .await;
        match r {
            Ok(Some(v)) => self.reply(&id, v),
            Ok(None) => {}
            Err(e) => self.error(&id, format!("{e:#}")),
        }
    }

    async fn notify(self: &Arc<Self>, method: &str, p: Value) {
        if method == "session/cancel" {
            if let Ok(s) = self.session(&p).await {
                if s.prompt.lock().await.is_some() {
                    s.cancelled.store(true, Ordering::Release);
                    let _ = s.send(json!({"type":"abort"})).await;
                }
            }
        }
    }
}

async fn s_ready(b: &Arc<Bridge>, sid: &str) -> Result<()> {
    let s = b.sessions.lock().await.get(sid).cloned().context("pi session vanished")?;
    s.call(json!({"type":"get_state"})).await.map(|_| ())
}

/// `kurisu pi-acp`: the ACP agent on stdio.
pub async fn serve_bridge() -> Result<()> {
    let home = std::env::var_os("KURISU_PI_HOME").map(PathBuf::from).unwrap_or_else(|| home_for(&crate::paths::Paths::from_env().home));
    std::fs::create_dir_all(&home)?;
    let (out, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(v) = rx.recv().await {
            if stdout.write_all(format!("{v}\n").as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let b = Arc::new(Bridge {
        out,
        home,
        pi: std::env::var("KURISU_PI_BIN").unwrap_or_else(|_| bin()),
        sessions: Default::default(),
        client: Default::default(),
        next: AtomicU64::new(1),
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match (v["method"].as_str(), v.get("id")) {
            (Some(m), Some(id)) => {
                let (b, id, m, p) = (b.clone(), id.clone(), m.to_string(), v["params"].clone());
                tokio::spawn(async move { b.handle(id, &m, p).await });
            }
            (Some(m), None) => b.notify(m, v["params"].clone()).await,
            (None, Some(id)) => {
                let key = id.as_str().map(String::from).unwrap_or_else(|| id.to_string());
                if let Some(tx) = b.client.lock().await.remove(&key) {
                    let _ = tx.send(v);
                }
            }
            _ => {}
        }
    }
    for (_, s) in b.sessions.lock().await.drain() {
        let _ = s.child.lock().await.kill().await;
    }
    drop(b);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_request_names_the_tool() {
        let p = permission_params("s1", &json!({"toolCallId":"c1","toolName":"bash","input":{"command":"ls"}}));
        let req: agent_client_protocol::v1::schema::RequestPermissionRequest = serde_json::from_value(p).unwrap();
        assert_eq!(req.tool_call.fields.name.as_deref(), Some("bash"));
        assert_eq!(req.options.len(), 3);
    }

    #[test]
    fn gate_switch() {
        assert!(gate_enabled(None));
        assert!(gate_enabled(Some("1")));
        assert!(!gate_enabled(Some("0")));
        assert!(!gate_enabled(Some("OFF")));
    }

    #[test]
    fn models_split_on_first_slash() {
        assert_eq!(split_model("ollama/deepseek-v4.1-flash:cloud"), Some(("ollama".into(), "deepseek-v4.1-flash:cloud".into())));
        assert_eq!(split_model("openrouter/anthropic/claude"), Some(("openrouter".into(), "anthropic/claude".into())));
        assert_eq!(split_model("default"), None);
        assert_eq!(split_model("bare"), None);
    }

    #[test]
    fn list_models_table_parses() {
        let out = "provider  model                      context\nollama    deepseek-v4.1-flash:cloud  1.0M\nopenai    gpt-6  400K\n";
        assert_eq!(parse_models(out), vec!["ollama/deepseek-v4.1-flash:cloud", "openai/gpt-6"]);
        assert!(parse_models("").is_empty());
    }

    #[test]
    fn modes_decide_tool_calls() {
        let none = HashSet::new();
        assert_eq!(decide("auto", "bash", &none), Decision::Allow);
        assert_eq!(decide("bypassPermissions", "write", &none), Decision::Allow);
        assert_eq!(decide("default", "bash", &none), Decision::Ask);
        assert_eq!(decide("default", "mcp__kurisu-mcp__board_post", &none), Decision::Ask);
        assert_eq!(decide("default", "read", &none), Decision::Allow);
        assert_eq!(decide("acceptEdits", "write", &none), Decision::Allow);
        assert_eq!(decide("acceptEdits", "bash", &none), Decision::Ask);
        assert_eq!(decide("plan", "bash", &none), Decision::Deny);
        assert_eq!(decide("plan", "grep", &none), Decision::Allow);
        let always: HashSet<String> = ["bash".to_string()].into();
        assert_eq!(decide("default", "bash", &always), Decision::Allow);
        assert!(check_mode(Some("plan")).is_ok());
        assert!(check_mode(Some("yolo")).is_err());
    }

    #[test]
    fn acp_mcp_servers_become_pi_mcp_json() {
        let acp = json!([
            {"name":"kurisu-mcp","command":"/bin/kurisu","args":["mcp"],"env":[{"name":"KURISU_SESSION","value":"s1"}]},
            {"type":"http","name":"web","url":"https://x/mcp","headers":[{"name":"Authorization","value":"Bearer t"}]},
            {"name":"broken"}
        ]);
        let v = mcp_json(&acp);
        let k = &v["mcpServers"]["kurisu-mcp"];
        assert_eq!(k["command"], "/bin/kurisu");
        assert_eq!(k["args"], json!(["mcp"]));
        assert_eq!(k["env"]["KURISU_SESSION"], "s1");
        assert_eq!(k["exposure"], "direct");
        assert_eq!(v["mcpServers"]["web"]["headers"]["Authorization"], "Bearer t");
        assert!(v["mcpServers"].get("broken").is_none());
    }

    #[test]
    fn pi_events_translate_to_acp_updates() {
        let t = translate(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hi"}})).unwrap();
        assert_eq!(t["sessionUpdate"], "agent_message_chunk");
        assert_eq!(t["content"]["text"], "hi");
        let t = translate(&json!({"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{"command":"ls"}})).unwrap();
        assert_eq!((t["sessionUpdate"].as_str(), t["kind"].as_str(), t["status"].as_str()), (Some("tool_call"), Some("execute"), Some("in_progress")));
        let t = translate(&json!({"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","result":{"content":[{"type":"text","text":"out"}]},"isError":true})).unwrap();
        assert_eq!(t["status"], "failed");
        assert_eq!(t["content"][0]["content"]["text"], "out");
        assert!(translate(&json!({"type":"agent_start"})).is_none());
        // Every update is a valid ACP SessionNotification.
        for u in [
            translate(&json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"x"}})).unwrap(),
            translate(&json!({"type":"tool_execution_start","toolCallId":"c1","toolName":"mcp__kurisu-mcp__peers_list","args":{}})).unwrap(),
            t,
        ] {
            serde_json::from_value::<agent_client_protocol::v1::schema::SessionNotification>(json!({"sessionId":"s","update":u})).unwrap();
        }
    }

    #[test]
    fn prompt_joins_text_blocks() {
        let p = json!([{"type":"text","text":"ctx","annotations":{"audience":["assistant"]}},{"type":"text","text":"do it"}]);
        assert_eq!(prompt_text(&p), "ctx\n\ndo it");
    }

    #[test]
    fn extensions_dir_lists_visible_entries_sorted() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.ts"), "").unwrap();
        std::fs::write(d.path().join("a.js"), "").unwrap();
        std::fs::write(d.path().join(".hidden.ts"), "").unwrap();
        std::fs::create_dir(d.path().join("pkg")).unwrap();
        let names: Vec<String> = extension_paths(d.path()).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["a.js", "b.ts", "pkg"]);
        assert!(extension_paths(&d.path().join("missing")).is_empty());
    }
}
