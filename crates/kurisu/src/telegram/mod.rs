//! Built-in Telegram channel: a 1:1 port of the Python Telegram bridge's Kurisu
//! backend (bridge up to `f47ea4e`: includes the /status CLI line `91a8ddc` and the
//! approval/question card closing `f47ea4e`). Optional and off until `ku telegram setup`; it runs
//! inside the ku server and calls the server's ops in-process.
//!
//! The shared logic keeps the bridge's Omnigent-shaped data (sessions, items and
//! elicitation events as JSON) so each function maps to its Python original.

mod backend;
mod bridge;
mod cards;
mod tg;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::Server;

/// `<KURISU_HOME>/telegram.json` (mode 0600). The token never goes to logs or git.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub token: String,
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<String>,
    /// Folder for sessions created with /new (KURISU_CWD in the Python bridge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_session_cwd: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("telegram.json")
}

pub fn state_dir(home: &Path) -> PathBuf {
    home.join("telegram")
}

pub fn load_config(home: &Path) -> Option<Config> {
    serde_json::from_slice(&std::fs::read(config_path(home)).ok()?).ok()
}

/// Write a file readable only by the user (atomic: temp file + rename).
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all().ok();
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn save_config(home: &Path, cfg: &Config) -> Result<()> {
    write_private(&config_path(home), &serde_json::to_vec_pretty(cfg)?)
}

/// Read KEY=VALUE lines (quotes stripped), as the Python bridge's load_config.
fn read_env_file(path: &Path) -> Result<HashMap<String, String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let (k, v) = line.split_once('=').unwrap();
        let k = k.trim().trim_start_matches("export ").trim();
        out.insert(k.to_string(), v.trim().trim_matches('"').trim_matches('\'').to_string());
    }
    Ok(out)
}

fn prompt(label: &str) -> Result<String> {
    use std::io::Write;
    eprint!("{label}: ");
    std::io::stderr().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

/// `ku telegram setup|status|off|on`.
pub async fn cli(paths: &crate::paths::Paths, args: &[String]) -> Result<()> {
    let home = paths.home.clone();
    let sub = args.get(2).map(String::as_str).unwrap_or("status");
    match sub {
        "setup" => {
            // Values from env files (--from-env FILE, repeatable), then prompts for anything missing.
            let mut vals: HashMap<String, String> = HashMap::new();
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--from-env" => {
                        let f = args.get(i + 1).context("--from-env FILE")?;
                        vals.extend(read_env_file(Path::new(f))?);
                        i += 2;
                    }
                    "--chat-id" => {
                        vals.insert("TG_CHAT_ID".into(), args.get(i + 1).context("--chat-id ID")?.clone());
                        i += 2;
                    }
                    "--cwd" => {
                        vals.insert("KURISU_CWD".into(), args.get(i + 1).context("--cwd DIR")?.clone());
                        i += 2;
                    }
                    other => bail!("unknown option {other}"),
                }
            }
            let token = match vals.get("TG_BOT_TOKEN").or(vals.get("TELEGRAM_BOT_TOKEN")).filter(|v| !v.is_empty()) {
                Some(t) => t.clone(),
                None => prompt("Bot token (from @BotFather)")?,
            };
            if !token.contains(':') || token.len() < 20 {
                bail!("that does not look like a Telegram bot token");
            }
            let chat_id = match vals.get("TG_CHAT_ID").filter(|v| !v.is_empty()) {
                Some(c) => c.clone(),
                None => prompt("Allowed chat id (your user id for a private chat)")?,
            };
            if chat_id.parse::<i64>().is_err() {
                bail!("the chat id must be a number");
            }
            let cfg = Config {
                token,
                chat_id,
                owner_user_id: vals.get("TG_OWNER_USER_ID").filter(|v| !v.is_empty()).cloned(),
                new_session_cwd: vals.get("KURISU_CWD").filter(|v| !v.is_empty()).cloned(),
                enabled: true,
            };
            save_config(&home, &cfg)?;
            println!("telegram: configured for chat {} (token saved in {}, mode 600)", cfg.chat_id, config_path(&home).display());
            reload(paths).await;
            Ok(())
        }
        "off" | "on" => {
            let mut cfg = load_config(&home).context("telegram is not set up: run `ku telegram setup`")?;
            cfg.enabled = sub == "on";
            save_config(&home, &cfg)?;
            println!("telegram: {}", if cfg.enabled { "on" } else { "off" });
            reload(paths).await;
            Ok(())
        }
        "notify" => {
            let text = args.get(3).filter(|t| !t.trim().is_empty()).context("usage: kurisu telegram notify \"<text>\"")?;
            crate::server::call(&paths.socket(), json!({"op": "telegram_notify", "text": text})).await?;
            println!("telegram: sent");
            Ok(())
        }
        "status" => {
            match load_config(&home) {
                None => println!("telegram: not set up (optional). Turn it on with `ku telegram setup`."),
                Some(c) => {
                    let running = crate::server::call(&paths.socket(), json!({"op": "telegram_status"})).await.ok();
                    println!("telegram: {} for chat {}{}", if c.enabled { "on" } else { "off" }, c.chat_id,
                             running.map(|r| format!(" ({})", r["state"].as_str().unwrap_or("?"))).unwrap_or_default());
                }
            }
            Ok(())
        }
        _ => bail!("usage: kurisu telegram setup [--from-env FILE]... [--chat-id ID] [--cwd DIR] | status | off | on | notify \"<text>\""),
    }
}

/// Tell a running server to (re)start or stop its Telegram channel.
async fn reload(paths: &crate::paths::Paths) {
    match crate::server::call(&paths.socket(), json!({"op": "telegram_reload"})).await {
        Ok(v) => println!("telegram: {}", v["state"].as_str().unwrap_or("reloaded")),
        Err(_) => println!("telegram: it starts with the Kurisu server (`ku web` or any ku command)"),
    }
}

// ------------------------------------------------------------------ the bridge

/// Where a reply goes (Python REPLY_CONTEXT + CURRENT_UPDATE).
#[derive(Debug, Clone)]
pub struct Ctx {
    pub chat_id: Value,
    pub thread_id: Option<i64>,
    pub scope: String,
    pub update_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum JobKind {
    Text { text: String, ack: bool },
    Attachment { attachment: Value, caption: String },
    CatchUp,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub ctx: Ctx,
    pub label: String,
    pub kind: JobKind,
    pub override_session: Option<String>,
}

#[derive(Default)]
pub struct Active {
    pub active: HashSet<String>,
    pub relaying: HashSet<String>,
    pub stop: HashSet<String>,
    pub waiting: HashSet<String>,
    pub send_now: HashSet<String>,
}

pub struct Bridge {
    pub server: Arc<Server>,
    pub cfg: Config,
    pub home: PathBuf,
    pub(crate) api: String,
    pub(crate) http: reqwest::Client,
    pub(crate) state: Mutex<Value>,
    pub(crate) relayed: Mutex<(HashSet<String>, VecDeque<String>)>,
    pub(crate) net: Mutex<(Option<f64>, Vec<(f64, f64)>)>,
    pub(crate) act: Mutex<Active>,
    pub(crate) jobs: Mutex<VecDeque<Job>>,
    pub(crate) queued: AtomicUsize,
    pub(crate) job_notify: tokio::sync::Notify,
    pub(crate) compactions: Mutex<HashSet<String>>,
    pub(crate) question_lock: tokio::sync::Mutex<()>,
    pub(crate) mcp_cache: Mutex<HashMap<String, (std::time::Instant, Value)>>,
    pub(crate) codex_model: Mutex<(f64, Option<String>)>,
    pub(crate) bot_info: Mutex<Value>,
    pub(crate) skills_cache: Mutex<HashMap<String, (Vec<Option<std::time::SystemTime>>, Vec<String>)>>,
    /// Auto-detected `stt` CLI and when it was checked (60 s cache, as the bridge).
    pub(crate) stt_probe: Mutex<(f64, Option<crate::stt::Config>)>,
}

pub(crate) fn now_f() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub(crate) fn now_i() -> i64 {
    now_f() as i64
}

pub(crate) fn log(msg: impl AsRef<str>) {
    eprintln!("{} telegram: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg.as_ref());
}

impl Bridge {
    fn state_file(&self) -> PathBuf {
        state_dir(&self.home).join("state.json")
    }

    fn relayed_file(&self) -> PathBuf {
        state_dir(&self.home).join("state-relayed.json")
    }

    pub(crate) fn upload_dir(&self) -> PathBuf {
        state_dir(&self.home).join("uploads")
    }

    pub(crate) fn save_state(&self) {
        let bytes = {
            let st = self.state.lock().unwrap();
            serde_json::to_vec_pretty(&*st).unwrap_or_default()
        };
        if let Err(e) = write_private(&self.state_file(), &bytes) {
            log(format!("could not save state: {e:#}"));
        }
    }

    /// Run `f` on the state (a JSON object), like Python's dict access under STATE_LOCK.
    pub(crate) fn with_state<T>(&self, f: impl FnOnce(&mut serde_json::Map<String, Value>) -> T) -> T {
        let mut st = self.state.lock().unwrap();
        if !st.is_object() {
            *st = json!({});
        }
        f(st.as_object_mut().unwrap())
    }

    pub(crate) fn chat_id(&self) -> Value {
        match self.cfg.chat_id.parse::<i64>() {
            Ok(n) => json!(n),
            Err(_) => json!(self.cfg.chat_id),
        }
    }

    pub(crate) fn scope_for(&self, chat_id: &Value, thread_id: Option<i64>) -> String {
        if vstr(chat_id) == self.cfg.chat_id {
            return "dm".into();
        }
        format!("chat:{}:{}", vstr(chat_id), thread_id.map(|t| t.to_string()).unwrap_or_else(|| "main".into()))
    }

    pub(crate) fn ctx(&self, chat_id: Value, thread_id: Option<i64>, update_id: Option<i64>) -> Ctx {
        let scope = self.scope_for(&chat_id, thread_id);
        Ctx { chat_id, thread_id, scope, update_id }
    }

    pub(crate) fn dm(&self) -> Ctx {
        self.ctx(self.chat_id(), None, None)
    }

    pub(crate) fn is_owner_dm(&self, chat_id: &Value) -> bool {
        vstr(chat_id) == self.cfg.chat_id
    }

    pub(crate) fn allowed_sender(&self, chat_id: &Value, sender: &Value) -> bool {
        if sender["is_bot"].as_bool() == Some(true) {
            return false;
        }
        if vstr(chat_id) == self.cfg.chat_id {
            return true;
        }
        let owner = self.cfg.owner_user_id.clone().unwrap_or_else(|| self.cfg.chat_id.clone());
        vstr(&sender["id"]) == owner
    }

    // --- relayed ids (persisted so a restart never re-broadcasts a reply already sent)

    fn load_relayed(&self) {
        let ids: Vec<String> = std::fs::read(self.relayed_file()).ok()
            .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok()).unwrap_or_default()
            .into_iter().filter_map(|v| v.as_str().map(String::from)).collect();
        let mut r = self.relayed.lock().unwrap();
        for id in ids {
            if r.0.insert(id.clone()) {
                r.1.push_back(id);
            }
        }
    }

    pub(crate) fn mark_relayed(&self, item_id: &str) {
        if item_id.is_empty() {
            return;
        }
        let snapshot: Vec<String> = {
            let mut r = self.relayed.lock().unwrap();
            if !r.0.insert(item_id.to_string()) {
                return;
            }
            r.1.push_back(item_id.to_string());
            while r.1.len() > 1000 {
                if let Some(old) = r.1.pop_front() {
                    r.0.remove(&old);
                }
            }
            let n = r.1.len();
            r.1.iter().skip(n.saturating_sub(300)).cloned().collect()
        };
        if let Err(e) = write_private(&self.relayed_file(), &serde_json::to_vec(&snapshot).unwrap_or_default()) {
            log(format!("could not persist relayed ids: {e:#}"));
        }
    }

    pub(crate) fn was_relayed(&self, item_id: &str) -> bool {
        !item_id.is_empty() && self.relayed.lock().unwrap().0.contains(item_id)
    }
}

/// str(value) as Python would print an id (numbers without quotes).
pub(crate) fn vstr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// " ".join(text.split()): collapse whitespace.
pub(crate) fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Python's s[:n] on characters.
pub(crate) fn take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub(crate) fn clen(s: &str) -> usize {
    s.chars().count()
}

/// A random hex token (uuid4().hex[:n]).
pub(crate) fn token(n: usize) -> String {
    take(&uuid::Uuid::new_v4().simple().to_string(), n)
}

// ------------------------------------------------------------------ lifecycle

/// The server's handle on its Telegram channel.
#[derive(Default)]
pub struct Channel {
    inner: tokio::sync::Mutex<Option<(Vec<tokio::task::JoinHandle<()>>, Arc<Bridge>)>>,
}

impl Channel {
    pub async fn state(&self) -> &'static str {
        if self.inner.lock().await.is_some() { "running" } else { "stopped" }
    }

    /// (Re)start from the saved config; stop when it is missing or off.
    pub async fn reload(&self, server: Arc<Server>, home: PathBuf) -> Result<&'static str> {
        self.stop().await;
        let Some(cfg) = load_config(&home) else { return Ok("not set up") };
        if !cfg.enabled {
            return Ok("off");
        }
        let api = std::env::var("KURISU_TELEGRAM_API").unwrap_or_else(|_| "https://api.telegram.org".into());
        let http = reqwest::Client::builder().user_agent("kurisu-telegram").build()?;
        let b = Arc::new(Bridge {
            server, cfg, home, api, http,
            state: Mutex::new(json!({})),
            relayed: Mutex::default(), net: Mutex::default(), act: Mutex::default(),
            jobs: Mutex::default(), queued: AtomicUsize::new(0), job_notify: tokio::sync::Notify::new(),
            compactions: Mutex::default(), question_lock: tokio::sync::Mutex::new(()),
            mcp_cache: Mutex::default(), codex_model: Mutex::new((0.0, None)), bot_info: Mutex::new(json!({})),
            skills_cache: Mutex::default(), stt_probe: Mutex::new((0.0, None)),
        });
        let st: Value = std::fs::read(b.state_file()).ok().and_then(|x| serde_json::from_slice(&x).ok()).filter(Value::is_object).unwrap_or(json!({}));
        *b.state.lock().unwrap() = st;
        b.load_relayed();
        let tasks = vec![
            tokio::spawn({ let b = b.clone(); async move { b.worker_loop().await } }),
            tokio::spawn({ let b = b.clone(); async move { b.run_forever().await } }),
        ];
        *self.inner.lock().await = Some((tasks, b));
        Ok("running")
    }

    /// A session was replaced (harness switch): chats that used it follow the new one.
    pub async fn session_replaced(&self, old: &str, new: &str) {
        if let Some((_, b)) = self.inner.lock().await.as_ref() {
            b.with_state(|st| {
                if st.get("session_id").and_then(Value::as_str) == Some(old) {
                    st.insert("session_id".into(), json!(new));
                }
                if let Some(scopes) = st.get_mut("scopes").and_then(Value::as_object_mut) {
                    for v in scopes.values_mut() {
                        if v.as_str() == Some(old) {
                            *v = json!(new);
                        }
                    }
                }
            });
            b.save_state();
        }
    }

    /// Send a one-off notice to the chat (e.g. before and after a restart).
    pub async fn notify(&self, text: &str) -> Result<()> {
        let b = self.inner.lock().await.as_ref().map(|(_, b)| b.clone()).ok_or_else(|| anyhow::anyhow!("the Telegram channel is not running"))?;
        let dm = b.dm();
        b.send_text(&dm, &b.chat_id(), text).await.map(|_| ())
    }

    pub async fn stop(&self) {
        if let Some((tasks, _)) = self.inner.lock().await.take() {
            for t in tasks {
                t.abort();
            }
        }
    }
}
