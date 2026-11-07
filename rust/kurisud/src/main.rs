//! kurisud: Kurisu's lean session server (spike).
//!
//!   kurisud serve --data <dir>          run the server (unix socket <dir>/kurisud.sock)
//!   kurisud call --data <dir> '<json>'  send one control request (testing)
//!   kurisud mcp --data <dir>            MCP stdio server with the Lead's session tools
//!   kurisud smoke <claude|codex> <cwd>  live driver check: new, prompt, kill, resume, prompt
//!   kurisud hook <events>               (internal) Claude Code hook sink

mod codex;
mod driver;
mod mcp;
mod store;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::v1::schema::SessionId;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex, OnceCell};

use codex::{CodexConfig, CodexDriver};
use driver::{agent_text, ClaudeDriver, Driver};
use store::Store;

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn claude_driver(state: &Path) -> ClaudeDriver {
    let me = std::env::current_exe().expect("current_exe").to_string_lossy().into_owned();
    let mut cfg = claude_cli_acp::Config::new(state.join("claude"), vec![me, "hook".into()]);
    if let Ok(extra) = std::env::var("KURISU_CLAUDE_ARGS") {
        cfg.extra_args = extra.split_whitespace().map(String::from).collect();
    }
    ClaudeDriver(claude_cli_acp::ClaudeCli::new(cfg))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data = || PathBuf::from(arg(&args, "--data").expect("--data <dir>"));
    match args.get(1).map(String::as_str) {
        Some("hook") => claude_cli_acp::run_hook(Path::new(args.get(2).context("events path")?)),
        Some("serve") => serve(data()).await,
        Some("call") => {
            let reply = call(&data().join("kurisud.sock"), serde_json::from_str(args.last().unwrap())?).await?;
            println!("{}", serde_json::to_string_pretty(&reply)?);
            Ok(())
        }
        Some("mcp") => mcp::serve_stdio(data().join("kurisud.sock")).await,
        Some("smoke") => smoke(args.get(2).context("harness")?, PathBuf::from(args.get(3).context("cwd")?)).await,
        _ => bail!("usage: kurisud serve|call|mcp|smoke|hook"),
    }
}

// ----------------------------------------------------------------- server

struct Server {
    store: Mutex<Store>,
    claude: ClaudeDriver,
    codex: OnceCell<Arc<CodexDriver>>, // spawned on first use: no codex process while idle
    live: Mutex<HashSet<String>>,
    busy: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Server {
    async fn codex(&self) -> Result<Arc<CodexDriver>> {
        self.codex
            .get_or_try_init(|| async { CodexDriver::spawn(CodexConfig::default()).await.map(Arc::new) })
            .await
            .cloned()
    }

    async fn ensure_live(&self, name: &str) -> Result<SessionId> {
        let s = self.store.lock().await.get(name)?.ok_or_else(|| anyhow!("no session {name}"))?;
        let cwd = PathBuf::from(&s.cwd);
        if let Some(id) = &s.harness_session_id {
            let id = SessionId::new(id.clone());
            if !self.live.lock().await.contains(name) {
                match s.harness.as_str() {
                    "claude" => self.claude.load_session(&id, cwd).await?,
                    "codex" => self.codex().await?.load_session(&id, cwd).await?,
                    h => bail!("harness {h} not implemented"),
                }
                self.live.lock().await.insert(name.into());
            }
            return Ok(id);
        }
        let id = match s.harness.as_str() {
            "claude" => self.claude.new_session(cwd).await?,
            "codex" => self.codex().await?.new_session(cwd).await?,
            h => bail!("harness {h} not implemented"),
        };
        self.store.lock().await.bind_harness_id(name, &id.to_string())?;
        self.live.lock().await.insert(name.into());
        Ok(id)
    }

    async fn send(&self, name: &str, text: &str, from: &str) -> Result<Value> {
        let lock = self.busy.lock().await.entry(name.into()).or_default().clone();
        let _turn = lock.lock().await;
        let id = self.ensure_live(name).await?;
        let harness = self.store.lock().await.get(name)?.unwrap().harness;
        {
            let st = self.store.lock().await;
            st.append(name, from, text)?;
            st.set_status(name, "working")?;
        }
        let (tx, mut rx) = mpsc::channel(256);
        let collect = tokio::spawn(async move {
            let mut all = vec![];
            while let Some(n) = rx.recv().await {
                all.push(n);
            }
            all
        });
        let res = match harness.as_str() {
            "claude" => self.claude.prompt(&id, text, tx).await,
            _ => self.codex().await?.prompt(&id, text, tx).await,
        };
        let updates = collect.await?;
        let reply = agent_text(&updates);
        let st = self.store.lock().await;
        st.set_status(name, if res.is_ok() { "idle" } else { "error" })?;
        let resp = res?;
        st.append(name, "agent", &reply)?;
        Ok(json!({"name": name, "reply": reply, "stop_reason": format!("{:?}", resp.stop_reason)}))
    }

    async fn handle(&self, req: Value) -> Result<Value> {
        let s = |k: &str| req.get(k).and_then(Value::as_str).map(String::from).ok_or_else(|| anyhow!("missing {k}"));
        match s("op")?.as_str() {
            "list" => Ok(json!(self.store.lock().await.list()?)),
            "create" => Ok(json!(self.store.lock().await.create(&s("name")?, &s("harness")?, &s("cwd")?)?)),
            "status" => Ok(json!(self.store.lock().await.get(&s("name")?)?)),
            "history" => {
                let after = req.get("after").and_then(Value::as_i64).unwrap_or(0);
                Ok(json!(self.store.lock().await.history(&s("name")?, after, 200)?))
            }
            "send" => self.send(&s("name")?, &s("text")?, &s("from").unwrap_or_else(|_| "user".into())).await,
            op => bail!("unknown op {op}"),
        }
    }
}

async fn serve(data: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&data)?;
    let sock = data.join("kurisud.sock");
    let _ = std::fs::remove_file(&sock);
    let old = unsafe { libc_umask(0o177) };
    let listener = UnixListener::bind(&sock)?;
    unsafe { libc_umask(old) };
    let server = Arc::new(Server {
        store: Mutex::new(Store::open(&data.join("kurisu.db"))?),
        claude: claude_driver(&data),
        codex: OnceCell::new(),
        live: Mutex::default(),
        busy: Mutex::default(),
    });
    eprintln!("kurisud listening on {}", sock.display());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let server = server.clone();
                tokio::spawn(async move { let _ = connection(server, stream).await; });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

extern "C" {
    #[link_name = "umask"]
    fn libc_umask(mask: u32) -> u32;
}

async fn connection(server: Arc<Server>, stream: UnixStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => match server.handle(req).await {
                Ok(v) => json!({"ok": v}),
                Err(e) => json!({"error": e.to_string()}),
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

// ------------------------------------------------------------------ smoke

async fn ask<D: Driver>(d: &D, id: &SessionId, text: &str) -> Result<String> {
    let (tx, mut rx) = mpsc::channel(256);
    let resp = d.prompt(id, text, tx).await?;
    let mut all = vec![];
    while let Ok(n) = rx.try_recv() {
        all.push(n);
    }
    eprintln!("  stop_reason={:?}", resp.stop_reason);
    Ok(agent_text(&all))
}

async fn smoke_with<D: Driver>(d: &D, cwd: PathBuf) -> Result<()> {
    let t = std::time::Instant::now();
    let id = d.new_session(cwd.clone()).await?;
    eprintln!("{}: session {id} ready in {:?}", d.harness(), t.elapsed());
    let r1 = ask(d, &id, "Reply with exactly the word PONG and nothing else.").await?;
    println!("reply1={r1:?}");
    d.close(&id).await?;
    d.load_session(&id, cwd).await?;
    let r2 = ask(d, &id, "What single word did you reply with last time? Answer with just that word.").await?;
    println!("reply2={r2:?}");
    d.close(&id).await?;
    anyhow::ensure!(r1.trim() == "PONG" && r2.contains("PONG"), "smoke failed");
    println!("SMOKE OK ({})", d.harness());
    Ok(())
}

async fn smoke(harness: &str, cwd: PathBuf) -> Result<()> {
    let state = std::env::temp_dir().join("kurisud-smoke");
    match harness {
        "claude" => smoke_with(&claude_driver(&state), cwd).await,
        "codex" => {
            // A fresh app-server process for the resume step proves restart identity.
            let d = CodexDriver::spawn(CodexConfig { sandbox: "read-only".into(), ..Default::default() }).await?;
            let id = d.new_session(cwd.clone()).await?;
            eprintln!("codex: thread {id}");
            let r1 = ask(&d, &id, "Reply with exactly the word PONG and nothing else.").await?;
            println!("reply1={r1:?}");
            drop(d);
            let d = CodexDriver::spawn(CodexConfig { sandbox: "read-only".into(), ..Default::default() }).await?;
            d.load_session(&id, cwd).await?;
            let r2 = ask(&d, &id, "What single word did you reply with last time? Answer with just that word.").await?;
            println!("reply2={r2:?}");
            anyhow::ensure!(r1.trim() == "PONG" && r2.contains("PONG"), "smoke failed");
            println!("SMOKE OK (codex, resumed in a new app-server process)");
            Ok(())
        }
        h => bail!("unknown harness {h}"),
    }
}
