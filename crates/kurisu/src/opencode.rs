//! Installed OpenCode, exclusively through ACP JSON-RPC over stdio.
use crate::driver::{Driver, Updates};
use agent_client_protocol::v1::schema::{
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    SessionNotification,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, Mutex},
};
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
type Permissions = Arc<Mutex<HashMap<String, (Value, RequestPermissionRequest)>>>;
pub struct OpenCodeDriver {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pending: Pending,
    updates: Arc<Mutex<HashMap<String, Updates>>>,
    perms: Arc<Mutex<HashMap<String, mpsc::Sender<RequestPermissionRequest>>>>,
    pub alive: Arc<AtomicBool>,
    pub configs: Arc<Mutex<HashMap<String, Value>>>,
    commands: Arc<Mutex<HashMap<String, Value>>>,
    compactions: Arc<Mutex<HashMap<String, Value>>>,
    approvals: Permissions,
    next: AtomicU64,
    child: Mutex<Child>,
}
impl OpenCodeDriver {
    pub async fn spawn() -> Result<Self> {
        let mut child = Command::new(
            std::env::var("KURISU_OPENCODE_BIN").unwrap_or_else(|_| "opencode".into()),
        )
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start opencode acp")?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let pending: Pending = Default::default();
        let updates: Arc<Mutex<HashMap<String, Updates>>> = Default::default();
        let perms: Arc<Mutex<HashMap<String, mpsc::Sender<RequestPermissionRequest>>>> =
            Default::default();
        let approvals: Permissions = Default::default();
        let alive = Arc::new(AtomicBool::new(true));
        let is_alive = alive.clone();
        let configs: Arc<Mutex<HashMap<String, Value>>> = Default::default();
        let commands: Arc<Mutex<HashMap<String, Value>>> = Default::default();
        let compactions: Arc<Mutex<HashMap<String, Value>>> = Default::default();
        let (cc, cm, cp) = (configs.clone(), commands.clone(), compactions.clone());
        let (p, u, ps, a, w) = (
            pending.clone(),
            updates.clone(),
            perms.clone(),
            approvals.clone(),
            stdin.clone(),
        );
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    break;
                };
                if let Some(method) = v["method"].as_str() {
                    match method {
                        "session/update" => {
                            let sid = v["params"]["sessionId"].as_str().unwrap_or("").to_string();
                            let update = &v["params"]["update"];
                            if update["sessionUpdate"] == "available_commands_update" {
                                cm.lock()
                                    .await
                                    .insert(sid.clone(), update["availableCommands"].clone());
                            }
                            if update["_meta"]["opencode/compaction"].is_object() {
                                cp.lock().await.insert(
                                    sid.clone(),
                                    update["_meta"]["opencode/compaction"].clone(),
                                );
                            }
                            if update["sessionUpdate"] == "config_option_update" {
                                cc.lock().await.insert(sid, update["configOptions"].clone());
                            }
                            if let Ok(n) =
                                serde_json::from_value::<SessionNotification>(v["params"].clone())
                            {
                                let tx = u.lock().await.get(&n.session_id.to_string()).cloned();
                                if let Some(tx) = tx {
                                    let _ = tx.send(n).await;
                                }
                            }
                        }
                        "session/request_permission" => {
                            if let Ok(req) = serde_json::from_value::<RequestPermissionRequest>(
                                v["params"].clone(),
                            ) {
                                let key = req.tool_call.tool_call_id.to_string();
                                let tx = ps.lock().await.get(&req.session_id.to_string()).cloned();
                                a.lock()
                                    .await
                                    .insert(key.clone(), (v["id"].clone(), req.clone()));
                                if let Some(tx) = tx {
                                    if tx.send(req).await.is_ok() {
                                        continue;
                                    }
                                }
                                a.lock().await.remove(&key);
                            }
                            let _ = write(&w, json!({"jsonrpc":"2.0","id":v["id"],"result":{"outcome":{"outcome":"cancelled"}}})).await;
                        }
                        _ if v.get("id").is_some() => {
                            let _ = write(&w,json!({"jsonrpc":"2.0","id":v["id"],"error":{"code":-32601,"message":"Unsupported client capability"}})).await;
                        }
                        _ => {}
                    }
                } else if let Some(id) = v["id"].as_u64() {
                    if let Some(tx) = p.lock().await.remove(&id) {
                        let result = if v.get("error").is_some() {
                            Err(anyhow!("OpenCode ACP: {}", v["error"]))
                        } else {
                            Ok(v["result"].clone())
                        };
                        let _ = tx.send(result);
                    }
                }
            }
            is_alive.store(false, Ordering::Release);
            for (_, tx) in p.lock().await.drain() {
                let _ = tx.send(Err(anyhow!("OpenCode ACP disconnected")));
            }
            u.lock().await.clear();
            ps.lock().await.clear();
            a.lock().await.clear();
        });
        let d = Self {
            stdin,
            pending,
            updates,
            perms,
            approvals,
            alive,
            configs,
            commands,
            compactions,
            next: AtomicU64::new(1),
            child: Mutex::new(child),
        };
        let init=d.rpc("initialize",json!({"protocolVersion":1,"clientInfo":{"name":"kurisu","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{}})).await?;
        anyhow::ensure!(
            init["protocolVersion"] == 1,
            "OpenCode ACP protocol version must be 1"
        );
        Ok(d)
    }
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        if !self.alive.load(Ordering::Acquire) {
            bail!("OpenCode ACP disconnected; retry to restart the agent");
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if let Err(e) = write(
            &self.stdin,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .await
        {
            self.pending.lock().await.remove(&id);
            self.shutdown().await;
            return Err(e);
        }
        match tokio::time::timeout(
            Duration::from_secs(if method == "session/prompt" {
                std::env::var("KURISU_OPENCODE_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1800)
            } else {
                60
            }),
            rx,
        )
        .await
        {
            Ok(Ok(r)) => r,
            _ => {
                self.pending.lock().await.remove(&id);
                self.alive.store(false, Ordering::Release);
                self.stdin.lock().await.take();
                bail!("OpenCode ACP {method} timed out or disconnected")
            }
        }
    }
    pub async fn configure(
        &self,
        id: &SessionId,
        model: &str,
        effort: Option<&str>,
        mode: Option<&str>,
    ) -> Result<()> {
        if model != "default" {
            let result = self
                .rpc(
                    "session/set_config_option",
                    json!({"sessionId":id,"configId":"model","value":model}),
                )
                .await?;
            self.configs
                .lock()
                .await
                .insert(id.to_string(), result["configOptions"].clone());
        }
        let has_effort = self
            .configs
            .lock()
            .await
            .get(&id.to_string())
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|c| c["id"] == "effort"));
        if let Some(e) = effort.filter(|e| *e != "default" || has_effort) {
            self.rpc(
                "session/set_config_option",
                json!({"sessionId":id,"configId":"effort","value":e}),
            )
            .await?;
        }
        self.rpc(
            "session/set_mode",
            json!({"sessionId":id,"modeId":if mode == Some("plan") {"plan"} else {"build"}}),
        )
        .await?;
        Ok(())
    }
    pub async fn session(&self, id: Option<&SessionId>, cwd: PathBuf, mcp: Value) -> Result<Value> {
        let mut p = json!({"cwd":cwd,"mcpServers":mcp});
        if let Some(id) = id {
            p["sessionId"] = json!(id);
        }
        let result = self
            .rpc(
                if id.is_some() {
                    "session/load"
                } else {
                    "session/new"
                },
                p,
            )
            .await?;
        let sid = id
            .map(ToString::to_string)
            .or_else(|| result["sessionId"].as_str().map(String::from));
        if let Some(sid) = sid {
            self.configs
                .lock()
                .await
                .insert(sid, result["configOptions"].clone());
        }
        Ok(result)
    }
    pub async fn prompt_with_permissions(
        &self,
        id: &SessionId,
        text: &str,
        updates: Updates,
        perms: Option<mpsc::Sender<RequestPermissionRequest>>,
    ) -> Result<PromptResponse> {
        self.prompt_with_context(id, text, updates, perms, None)
            .await
    }
    pub async fn prompt_with_context(
        &self,
        id: &SessionId,
        text: &str,
        updates: Updates,
        perms: Option<mpsc::Sender<RequestPermissionRequest>>,
        context: Option<&str>,
    ) -> Result<PromptResponse> {
        let key = id.to_string();
        {
            let mut active = self.updates.lock().await;
            if active.contains_key(&key) {
                bail!("OpenCode session is busy");
            }
            active.insert(key.clone(), updates);
        }
        if let Some(p) = perms {
            self.perms.lock().await.insert(key.clone(), p);
        }
        let mut prompt = vec![];
        if let Some(context) = context {
            prompt.push(
                json!({"type":"text","text":context,"annotations":{"audience":["assistant"]}}),
            );
        }
        prompt.push(json!({"type":"text","text":text}));
        let result = self
            .rpc("session/prompt", json!({"sessionId":id,"prompt":prompt}))
            .await;
        if result.is_err() {
            let _ = self.cancel(id).await;
        }
        self.updates.lock().await.remove(&key);
        self.perms.lock().await.remove(&key);
        serde_json::from_value(result?).context("OpenCode ACP prompt response")
    }
    /// A cancelled ACP prompt must finish before its turn can be released. Some
    /// agents never answer it after session/cancel; terminate this session's
    /// isolated process rather than declaring an upstream turn idle locally.
    pub async fn stop_stuck_turn(&self) -> Result<()> {
        self.alive.store(false, Ordering::Release);
        // Hold pending across termination so EOF cannot replace the stopped
        // outcome with a generic disconnect error in the reader task.
        let mut pending = self.pending.lock().await;
        self.child.lock().await.kill().await.context("terminate stuck OpenCode ACP")?;
        self.stdin.lock().await.take();
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(anyhow!(claude_cli_acp::TURN_STOPPED)));
        }
        self.updates.lock().await.clear();
        self.perms.lock().await.clear();
        self.approvals.lock().await.clear();
        Ok(())
    }
    pub async fn shutdown(&self) {
        self.alive.store(false, Ordering::Release);
        self.stdin.lock().await.take();
    }
    pub async fn discover(&self, cwd: PathBuf, model: Option<&str>) -> Result<Value> {
        let v = self.session(None, cwd, json!([])).await?;
        let id = SessionId::new(
            v["sessionId"]
                .as_str()
                .context("missing discovery sessionId")?
                .to_string(),
        );
        let result = async {
            if let Some(model) = model.filter(|m| *m != "default") {
                return self
                    .rpc(
                        "session/set_config_option",
                        json!({"sessionId":id,"configId":"model","value":model}),
                    )
                    .await;
            }
            Ok(v)
        }
        .await;
        let _ = self.rpc("session/delete", json!({"sessionId":id})).await;
        result
    }
    pub async fn compact(&self, id: &SessionId) -> Result<claude_cli_acp::Compaction> {
        let available = self
            .commands
            .lock()
            .await
            .get(&id.to_string())
            .cloned()
            .unwrap_or(json!([]));
        if !available
            .as_array()
            .is_some_and(|a| a.iter().any(|c| c["name"] == "compact"))
        {
            bail!("Installed OpenCode ACP did not advertise /compact");
        }
        self.compactions.lock().await.remove(&id.to_string());
        let (tx, mut rx) = mpsc::channel(256);
        let collect = async move { while rx.recv().await.is_some() {} };
        let (result, ()) = tokio::join!(
            self.prompt_with_permissions(id, "/compact", tx, None),
            collect
        );
        let response = result?;
        anyhow::ensure!(
            matches!(
                response.stop_reason,
                agent_client_protocol::v1::schema::StopReason::EndTurn
            ),
            "OpenCode compaction did not complete: {:?}",
            response.stop_reason
        );
        let state = self
            .compactions
            .lock()
            .await
            .get(&id.to_string())
            .cloned()
            .unwrap_or(Value::Null);
        anyhow::ensure!(
            state["status"] == "completed",
            "OpenCode did not report completed compaction: {}",
            state
        );
        Ok(claude_cli_acp::Compaction {
            trigger: Some("manual".into()),
            ..Default::default()
        })
    }
    pub async fn respond_permission(
        &self,
        key: &str,
        outcome: &RequestPermissionOutcome,
    ) -> Result<()> {
        let (id, req) = self
            .approvals
            .lock()
            .await
            .remove(key)
            .context("OpenCode permission is no longer pending")?;
        // Kurisu uses stable allow/deny IDs; map to the IDs advertised by this agent.
        let v = serde_json::to_value(outcome)?;
        if v["outcome"] == "cancelled" {
            return write(
                &self.stdin,
                json!({"jsonrpc":"2.0","id":id,"result":{"outcome":v}}),
            )
            .await;
        }
        let selected = v["optionId"].as_str().unwrap_or("");
        let kind = if selected == claude_cli_acp::ALLOW_ONCE {
            "allow_once"
        } else if selected == crate::codex::ALLOW_ALWAYS {
            "allow_always"
        } else {
            "reject_once"
        };
        let opts = serde_json::to_value(req.options)?;
        let option = opts
            .as_array()
            .and_then(|a| a.iter().find(|o| o["kind"] == kind))
            .and_then(|o| o["optionId"].as_str());
        let answer = option
            .map(|o| json!({"outcome":"selected","optionId":o}))
            .unwrap_or(json!({"outcome":"cancelled"}));
        write(
            &self.stdin,
            json!({"jsonrpc":"2.0","id":id,"result":{"outcome":answer}}),
        )
        .await
    }
}
async fn write(w: &Mutex<Option<ChildStdin>>, v: Value) -> Result<()> {
    let mut guard = w.lock().await;
    let w = guard.as_mut().context("OpenCode ACP transport is closed")?;
    w.write_all(format!("{v}\n").as_bytes()).await?;
    w.flush().await?;
    Ok(())
}
impl Driver for OpenCodeDriver {
    fn harness(&self) -> &'static str {
        "opencode"
    }
    async fn new_session(&self, cwd: PathBuf) -> Result<SessionId> {
        let v = self.session(None, cwd, json!([])).await?;
        Ok(SessionId::new(
            v["sessionId"]
                .as_str()
                .context("missing ACP sessionId")?
                .to_string(),
        ))
    }
    async fn load_session(&self, id: &SessionId, cwd: PathBuf) -> Result<()> {
        self.session(Some(id), cwd, json!([])).await?;
        Ok(())
    }
    async fn prompt(&self, id: &SessionId, text: &str, updates: Updates) -> Result<PromptResponse> {
        self.prompt_with_permissions(id, text, updates, None).await
    }
    async fn cancel(&self, id: &SessionId) -> Result<()> {
        write(
            &self.stdin,
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}),
        )
        .await
    }
    async fn close(&self, id: &SessionId) -> Result<()> {
        self.rpc("session/close", json!({"sessionId":id})).await?;
        Ok(())
    }
}
