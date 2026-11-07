//! The Kurisu server: owns the session store and the drivers, and serves
//! line-JSON requests on a 0600 unix socket (used by the CLI and by the
//! `kurisu mcp` tool server that every session gets).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::v1::schema::{LoadSessionRequest, NewSessionRequest, SessionId};
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
        let harness = {
            // Record the message first, so observers see it immediately.
            let st = self.store.lock().await;
            let s = st.get(name)?.ok_or_else(|| anyhow!("no session {name}"))?;
            if s.lifecycle != "active" {
                bail!("session {} is {}", s.name, s.lifecycle);
            }
            st.append(name, from, text)?;
            st.set_status(name, "starting")?;
            s.harness
        };
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
                self.claude.prompt(req, tx).await
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
        Ok(json!({"id": name, "name": display, "reply": reply, "stop_reason": format!("{:?}", resp.stop_reason)}))
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
            "stop" => {
                let name = s("name")?;
                if let Some(id) = self.store.lock().await.get(&name)?.and_then(|x| x.harness_session_id) {
                    let _ = self.claude.close(&SessionId::new(id)).await;
                }
                let was_live = self.live.lock().await.remove(&name);
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
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let server = server.clone();
                tokio::spawn(async move { let _ = connection(server, stream).await; });
            }
            _ = server.stop.notified() => break,
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
}
