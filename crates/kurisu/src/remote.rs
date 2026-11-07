//! Remote access from the phone: `/terminal` (a PTY shell in the browser), and later
//! `/browser`. Off until `ku terminal enable`.
//!
//! A link alone grants nothing:
//!   1. `/terminal` in Telegram returns a link with a random token (5 min, at most 3 attempts).
//!   2. Opening it starts an *attempt*: the page shows a 6-character code and waits; the
//!      Telegram chat gets an approval card with the requester's IP, user agent and code.
//!   3. Approve unlocks that browser (an HttpOnly cookie) and starts the shell; the link is
//!      then used up. Deny, or no answer in 2 min, kills the link. A refresh is a new attempt.
//! Idle sessions close after 30 min; `/terminal close` closes everything. Every step is
//! audited. Links and tokens are never logged.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path as AxPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::server::Server;

pub const LINK_TTL: Duration = Duration::from_secs(5 * 60);
pub const ATTEMPT_TTL: Duration = Duration::from_secs(2 * 60);
pub const MAX_ATTEMPTS: u32 = 3;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Cookie per kind (cookies ignore the port, and both listeners may share a host).
fn cookie_name(kind: &str) -> &'static str {
    if kind == "browser" { "ku_browser" } else { "ku_terminal" }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub enabled: bool,
    /// Address to listen on: loopback, a LAN or Tailscale address (never 0.0.0.0 unless forced).
    pub listen: String,
    /// The URL in links (e.g. http://my-box.tailnet.ts.net:8790); default http://<listen>.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("terminal.json")
}

pub fn load(home: &Path) -> Option<Config> {
    serde_json::from_slice(&std::fs::read(config_path(home)).ok()?).ok()
}

/// Loopback, private LAN (RFC 1918 / ULA) or Tailscale (100.64/10, fd7a:115c:a1e0::/48).
pub fn address_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || (v.octets()[0] == 100 && (v.octets()[1] & 0xC0) == 64),
        IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00,
    }
}

pub fn check_listen(listen: &str, force: bool) -> Result<SocketAddr> {
    let addr: SocketAddr = listen.parse().with_context(|| format!("listen address {listen:?} must be IP:PORT"))?;
    if addr.ip().is_unspecified() && !force {
        bail!("{listen} listens on every interface; use 127.0.0.1, a LAN or a Tailscale address (or --force)");
    }
    if !addr.ip().is_unspecified() && !address_allowed(addr.ip()) && !force {
        bail!("{} is a public address; use 127.0.0.1, a LAN or a Tailscale address (or --force)", addr.ip());
    }
    Ok(addr)
}

/// Where a phone can reach this machine: the Tailscale address (with its MagicDNS
/// name when enabled), else a LAN address. Never loopback or 0.0.0.0.
#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    pub ip: IpAddr,
    pub host: Option<String>,
    pub via: &'static str,
}

impl Detected {
    pub fn url(&self, port: u16) -> String {
        match &self.host {
            Some(h) => format!("http://{h}:{port}"),
            None => match self.ip {
                IpAddr::V6(v) => format!("http://[{v}]:{port}"),
                ip => format!("http://{ip}:{port}"),
            },
        }
    }

    pub fn listen(&self, port: u16) -> String {
        SocketAddr::new(self.ip, port).to_string()
    }

    pub fn describe(&self, port: u16) -> String {
        match &self.host {
            Some(h) => format!("{h}:{port} ({}, {})", self.ip, self.via),
            None => format!("{}:{port} ({})", self.ip, self.via),
        }
    }
}

pub const NO_ADDRESS: &str = "No Tailscale or LAN address found; install Tailscale on this machine and your phone.";

fn tailscale_ip(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v) if v.octets()[0] == 100 && (v.octets()[1] & 0xC0) == 64)
}

/// `tailscale status --json` from the CLI, directly or (in a distrobox) on the host.
fn tailscale_status() -> Option<Value> {
    let cands = ["tailscale", "/opt/tailscale/tailscale", "/usr/bin/tailscale", "/usr/local/bin/tailscale",
                 "/Applications/Tailscale.app/Contents/MacOS/Tailscale"];
    let run = |argv: &[&str]| -> Option<Value> {
        let out = std::process::Command::new(argv[0]).args(&argv[1..]).stderr(std::process::Stdio::null()).output().ok()?;
        out.status.success().then(|| serde_json::from_slice::<Value>(&out.stdout).ok()).flatten()
    };
    for c in cands {
        if let Some(v) = run(&[c, "status", "--json"]) {
            return Some(v);
        }
    }
    if crate::stt::which_bin("distrobox-host-exec").is_some() {
        for c in cands {
            if let Some(v) = run(&["distrobox-host-exec", c, "status", "--json"]) {
                return Some(v);
            }
        }
    }
    None
}

/// IPv4 addresses per interface (`ip -o -4 addr`, else `ifconfig`).
fn interface_addrs() -> Vec<(String, IpAddr)> {
    let mut out = vec![];
    if let Ok(o) = std::process::Command::new("ip").args(["-o", "-4", "addr", "show"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if let (Some(name), Some(i)) = (f.get(1), f.iter().position(|w| *w == "inet")) {
                if let Some(ip) = f.get(i + 1).and_then(|a| a.split('/').next()).and_then(|a| a.parse().ok()) {
                    out.push((name.to_string(), ip));
                }
            }
        }
    }
    if out.is_empty() {
        if let Ok(o) = std::process::Command::new("ifconfig").output() {
            let mut name = String::new();
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if !line.starts_with(char::is_whitespace) {
                    name = line.split(':').next().unwrap_or("").to_string();
                } else if let Some(rest) = line.trim().strip_prefix("inet ") {
                    if let Some(ip) = rest.split_whitespace().next().and_then(|a| a.parse().ok()) {
                        out.push((name.clone(), ip));
                    }
                }
            }
        }
    }
    out
}

pub async fn detect_address() -> Option<Detected> {
    tokio::task::spawn_blocking(|| {
        if let Some(v) = tailscale_status() {
            let ip = v["Self"]["TailscaleIPs"].as_array().into_iter().flatten().filter_map(|x| x.as_str()?.parse::<IpAddr>().ok()).find(|ip| ip.is_ipv4());
            if let Some(ip) = ip {
                let magic = v["CurrentTailnet"]["MagicDNSEnabled"] == json!(true);
                let host = v["Self"]["DNSName"].as_str().map(|h| h.trim_end_matches('.').to_string()).filter(|h| magic && !h.is_empty());
                return Some(Detected { ip, host, via: "Tailscale" });
            }
        }
        let addrs = interface_addrs();
        if let Some((_, ip)) = addrs.iter().find(|(n, ip)| n.starts_with("tailscale") || tailscale_ip(*ip)) {
            return Some(Detected { ip: *ip, host: None, via: "Tailscale" });
        }
        let virtual_if = |n: &str| ["docker", "br-", "veth", "virbr", "podman", "cni", "lo"].iter().any(|p| n.starts_with(p));
        addrs.iter().filter(|(n, ip)| !virtual_if(n) && matches!(ip, IpAddr::V4(v) if v.is_private()))
            .max_by_key(|(_, ip)| matches!(ip, IpAddr::V4(v) if v.octets()[0] == 192)) // prefer 192.168 home LANs
            .map(|(_, ip)| Detected { ip: *ip, host: None, via: "LAN" })
    }).await.ok().flatten()
}

fn is_local_only(listen: &str) -> bool {
    listen.parse::<SocketAddr>().map(|a| a.ip().is_loopback() || a.ip().is_unspecified()).unwrap_or(true)
}

/// Turn on `/terminal` (from Telegram after approval, or `ku terminal enable`).
pub fn enable_terminal(home: &Path, d: &Detected, port: u16) -> Result<Config> {
    let cfg = Config { enabled: true, listen: d.listen(port), public_url: d.host.as_ref().map(|_| d.url(port)) };
    crate::telegram::write_private(&config_path(home), &serde_json::to_vec_pretty(&cfg)?)?;
    Ok(cfg)
}

pub fn disable_terminal(home: &Path) -> Result<bool> {
    let Some(mut c) = load(home) else { return Ok(false) };
    let was = c.enabled;
    c.enabled = false;
    crate::telegram::write_private(&config_path(home), &serde_json::to_vec_pretty(&c)?)?;
    Ok(was)
}

fn random_hex(n: usize) -> String {
    let mut s = String::new();
    while s.len() < n {
        s.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    s.truncate(n);
    s
}

/// 6 characters without look-alikes (0/O, 1/I/L).
pub fn pairing_code() -> String {
    const A: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let bytes = uuid::Uuid::new_v4().into_bytes();
    bytes.iter().take(6).map(|b| A[*b as usize % A.len()] as char).collect()
}

struct Link {
    kind: &'static str,
    expires: Instant,
    attempts: u32,
    dead: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub enum AttemptState {
    Pending,
    Approved,
    Denied,
    Expired,
    Replaced,
}

struct Attempt {
    token: String,
    kind: &'static str,
    cookie: String,
    code: String,
    ip: String,
    ua: String,
    state: AttemptState,
    expires: Instant,
    expires_unix: i64,
}

struct Session {
    kind: &'static str,
    ip: String,
    attempt: String,
    last: Instant,
    shell: Option<Arc<Shell>>,
}

pub struct Shell {
    writer: std::sync::Mutex<Box<dyn std::io::Write + Send>>,
    master: std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    child: std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    out: tokio::sync::broadcast::Sender<Vec<u8>>,
}

impl Shell {
    fn kill(&self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

#[derive(Default)]
struct Inner {
    links: HashMap<String, Link>,
    attempts: HashMap<String, Attempt>,
    sessions: HashMap<String, Session>,
    listeners: HashMap<&'static str, (String, tokio::task::JoinHandle<()>)>,
}

/// Remote access state, owned by the server.
#[derive(Default)]
pub struct Hub {
    inner: Mutex<Inner>,
}

#[derive(Clone)]
struct Web {
    server: Weak<Server>,
    kind: &'static str,
}

impl Hub {
    /// `/terminal`: a new link (starts the listener if needed).
    pub async fn link(&self, server: &Arc<Server>, kind: &'static str) -> Result<Value> {
        let home = server.home();
        let (mut listen, mut public_url) = if kind == "browser" {
            let b = crate::browser::active(&home).context("Live Browser is not set up")?;
            // Start lb web now, so the live view is there right after approval.
            server.browser.ensure(&home, &b).await?;
            (b.listen, b.public_url)
        } else {
            let c = load(&home).filter(|c| c.enabled).context("the remote terminal is off")?;
            (c.listen, c.public_url)
        };
        // A phone cannot open 127.0.0.1: move a local-only setup to the Tailscale/LAN address.
        if is_local_only(&listen) {
            let port = listen.parse::<SocketAddr>().map(|a| a.port()).unwrap_or(if kind == "browser" { 8793 } else { 8790 });
            let d = detect_address().await.context(NO_ADDRESS)?;
            listen = d.listen(port);
            public_url = d.host.as_ref().map(|_| d.url(port));
            if kind == "browser" {
                if let Some(mut b) = crate::browser::load(&home) {
                    b.listen = listen.clone();
                    b.public_url = public_url.clone();
                    crate::telegram::write_private(&crate::browser::config_path(&home), &serde_json::to_vec_pretty(&b)?)?;
                }
            } else {
                enable_terminal(&home, &d, port)?;
            }
            server.audit("kurisu", "terminal", "remote_listen_moved", &format!("{kind}: from a local-only address to {}", d.describe(port))).await;
        }
        struct Cfg { listen: String, public_url: Option<String> }
        let cfg = Cfg { listen, public_url };
        let addr = check_listen(&cfg.listen, true)?;
        self.ensure_listener(server, kind, &cfg.listen, addr).await?;
        let token = random_hex(40);
        let mut g = self.inner.lock().await;
        g.links.insert(token.clone(), Link { kind, expires: Instant::now() + LINK_TTL, attempts: 0, dead: false });
        drop(g);
        let base = cfg.public_url.clone().unwrap_or_else(|| format!("http://{}", cfg.listen));
        let base = base.trim_end_matches('/');
        server.audit("user", "terminal", "remote_link_issued", &format!("{kind} link, expires in 5 min")).await;
        let path = if kind == "browser" { "/_ku/r/" } else { "/r/" };
        Ok(json!({"url": format!("{base}{path}{token}"), "expires_in": LINK_TTL.as_secs(), "listen": cfg.listen}))
    }

    async fn ensure_listener(&self, server: &Arc<Server>, kind: &'static str, listen: &str, addr: SocketAddr) -> Result<()> {
        let mut g = self.inner.lock().await;
        if let Some((l, h)) = g.listeners.get(kind) {
            if l == listen && !h.is_finished() {
                return Ok(());
            }
            h.abort();
        }
        let tcp = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("listen on {listen}"))?;
        let web = Web { server: Arc::downgrade(server), kind };
        let app = if kind == "browser" { browser_router(web) } else { router(web) };
        let h = tokio::spawn(async move {
            let _ = axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await;
        });
        g.listeners.insert(kind, (listen.to_string(), h));
        Ok(())
    }

    /// Any approved browser viewer (keeps lb web alive).
    pub async fn browser_viewers(&self) -> bool {
        self.inner.lock().await.sessions.values().any(|s| s.kind == "browser")
    }

    /// Close every remote session and kill every link (`/terminal close`, `ku terminal disable`).
    pub async fn close_all(&self, server: &Server, kind: Option<&str>, why: &str) -> usize {
        let mut g = self.inner.lock().await;
        let keys: Vec<String> = g.sessions.iter().filter(|(_, s)| kind.is_none_or(|k| s.kind == k)).map(|(k, _)| k.clone()).collect();
        let n = keys.len();
        for k in keys {
            if let Some(s) = g.sessions.remove(&k) {
                if let Some(sh) = s.shell {
                    sh.kill();
                }
            }
        }
        for l in g.links.values_mut().filter(|l| kind.is_none_or(|k| l.kind == k)) {
            l.dead = true;
        }
        let pending: Vec<String> = g.attempts.iter().filter(|(_, a)| a.state == AttemptState::Pending && kind.is_none_or(|k| a.kind == k)).map(|(k, _)| k.clone()).collect();
        for id in &pending {
            if let Some(a) = g.attempts.get_mut(id) {
                a.state = AttemptState::Expired;
            }
        }
        drop(g);
        for id in pending {
            server.telegram.pair_close(&id, "⌛ Closed — the link is dead.").await;
        }
        if n > 0 {
            server.audit("user", "terminal", "remote_closed", &format!("{n} session(s): {why}")).await;
        }
        n
    }

    pub async fn stop(&self) {
        let mut g = self.inner.lock().await;
        for (_, s) in g.sessions.drain() {
            if let Some(sh) = s.shell {
                sh.kill();
            }
        }
        for (_, (_, h)) in g.listeners.drain() {
            h.abort();
        }
    }

    /// Approve or deny an attempt (the Telegram card). Err when it is no longer pending.
    pub async fn decide(&self, server: &Server, attempt: &str, approve: bool) -> Result<String> {
        let mut g = self.inner.lock().await;
        let Some(a) = g.attempts.get_mut(attempt) else { bail!("expired") };
        if a.state != AttemptState::Pending || Instant::now() > a.expires {
            bail!("expired");
        }
        let (token, cookie, ip, kind, ua) = (a.token.clone(), a.cookie.clone(), a.ip.clone(), a.kind, short_ua(&a.ua));
        if approve {
            a.state = AttemptState::Approved;
            if let Some(l) = g.links.get_mut(&token) {
                l.dead = true; // used up
            }
            g.sessions.insert(cookie, Session { kind, ip: ip.clone(), attempt: attempt.to_string(), last: Instant::now(), shell: None });
            drop(g);
            server.audit("user", "terminal", "remote_approved", &format!("{kind} from {ip} ({ua})")).await;
            Ok(ip)
        } else {
            a.state = AttemptState::Denied;
            if let Some(l) = g.links.get_mut(&token) {
                l.dead = true;
            }
            drop(g);
            server.audit("user", "terminal", "remote_denied", &format!("{kind} from {ip}; the link is dead")).await;
            Ok(ip)
        }
    }

    /// Close one approved session (the card's Close button).
    pub async fn close_attempt(&self, server: &Server, attempt: &str) -> bool {
        let mut g = self.inner.lock().await;
        let key = g.sessions.iter().find(|(_, s)| s.attempt == attempt).map(|(k, _)| k.clone());
        let Some(k) = key else { return false };
        if let Some(s) = g.sessions.remove(&k) {
            if let Some(sh) = s.shell {
                sh.kill();
            }
            drop(g);
            server.audit("user", "terminal", "remote_closed", &format!("{} from {} (Close button)", s.kind, s.ip)).await;
        }
        true
    }

    /// Server tick: expire attempts (killing their link) and idle sessions.
    pub async fn tick(&self, server: &Server) {
        let now = Instant::now();
        let mut g = self.inner.lock().await;
        let expired: Vec<(String, String)> = g.attempts.iter()
            .filter(|(_, a)| a.state == AttemptState::Pending && now > a.expires).map(|(k, a)| (k.clone(), a.token.clone())).collect();
        for (id, tok) in &expired {
            if let Some(a) = g.attempts.get_mut(id) {
                a.state = AttemptState::Expired;
            }
            if let Some(l) = g.links.get_mut(tok) {
                l.dead = true;
            }
        }
        let idle: Vec<String> = g.sessions.iter().filter(|(_, s)| now.duration_since(s.last) > IDLE_TIMEOUT).map(|(k, _)| k.clone()).collect();
        let mut closed = vec![];
        for k in idle {
            if let Some(s) = g.sessions.remove(&k) {
                if let Some(sh) = &s.shell {
                    sh.kill();
                }
                closed.push(s);
            }
        }
        g.links.retain(|_, l| now < l.expires + Duration::from_secs(600));
        g.attempts.retain(|_, a| a.state == AttemptState::Pending || now < a.expires + Duration::from_secs(3600));
        drop(g);
        for (id, _) in expired {
            server.audit("kurisu", "terminal", "remote_expired", "no answer in 2 min; the link is dead").await;
            server.telegram.pair_close(&id, "⌛ Expired — no answer in 2 min; the link is dead.").await;
        }
        for s in closed {
            server.audit("kurisu", "terminal", "remote_closed", &format!("{} from {} (idle 30 min)", s.kind, s.ip)).await;
            server.telegram.pair_close(&s.attempt, "⏹ Closed after 30 min idle.").await;
        }
    }

    /// Opening a link: a new attempt for that browser, or why not (audited either way).
    async fn open(&self, server: &Server, token: &str, ip: &str, ua: &str, cookie: Option<String>) -> std::result::Result<(String, String, i64), &'static str> {
        let r = self.open_inner(server, token, ip, ua, cookie).await;
        if let Err(why) = &r {
            server.audit("kurisu", "terminal", "remote_refused", &format!("from {ip} ({}): {why}", short_ua(ua))).await;
        }
        r
    }

    async fn open_inner(&self, server: &Server, token: &str, ip: &str, ua: &str, cookie: Option<String>) -> std::result::Result<(String, String, i64), &'static str> {
        let mut g = self.inner.lock().await;
        let now = Instant::now();
        let Some(l) = g.links.get_mut(token) else { return Err("This link is not valid.") };
        if l.dead || now > l.expires {
            return Err("This link has expired or was already used. Send the command again in Telegram.");
        }
        if l.attempts >= MAX_ATTEMPTS {
            l.dead = true;
            return Err("Too many attempts with this link. Send the command again in Telegram.");
        }
        l.attempts += 1;
        let (kind, n) = (l.kind, l.attempts);
        // A refresh replaces the previous pending attempt of this link.
        let replaced: Vec<String> = g.attempts.iter().filter(|(_, a)| a.token == token && a.state == AttemptState::Pending).map(|(k, _)| k.clone()).collect();
        for r in &replaced {
            if let Some(a) = g.attempts.get_mut(r) {
                a.state = AttemptState::Replaced;
            }
        }
        let id = random_hex(12);
        let cookie = cookie.filter(|c| c.len() == 48 && !g.sessions.contains_key(c)).unwrap_or_else(|| random_hex(48));
        let code = pairing_code();
        let expires_unix = chrono::Utc::now().timestamp() + ATTEMPT_TTL.as_secs() as i64;
        g.attempts.insert(id.clone(), Attempt { token: token.into(), kind, cookie: cookie.clone(), code: code.clone(), ip: ip.into(),
                                                 ua: ua.into(), state: AttemptState::Pending, expires: now + ATTEMPT_TTL, expires_unix });
        drop(g);
        for r in replaced {
            server.telegram.pair_close(&r, "↻ Replaced by a new attempt (page refreshed).").await;
        }
        server.audit("kurisu", "terminal", "remote_attempt", &format!("{kind} attempt {n}/{MAX_ATTEMPTS} from {ip} ({})", short_ua(ua))).await;
        let posted = server.telegram.pair_card(&id, kind, ip, &short_ua(ua), &code, expires_unix).await;
        if let Err(e) = posted {
            eprintln!("remote: approval card not sent: {e:#}");
            let mut g = self.inner.lock().await;
            if let Some(a) = g.attempts.get_mut(&id) {
                a.state = AttemptState::Expired;
            }
            return Err("Could not ask for approval in Telegram (is the Telegram channel on?).");
        }
        Ok((cookie, code, expires_unix))
    }

    async fn state_of(&self, cookie: &str) -> (&'static str, Option<String>) {
        let g = self.inner.lock().await;
        if g.sessions.contains_key(cookie) {
            return ("approved", None);
        }
        match g.attempts.values().filter(|a| a.cookie == cookie).max_by_key(|a| a.expires_unix) {
            Some(a) => (match a.state {
                AttemptState::Pending => "pending",
                AttemptState::Approved => "closed",
                AttemptState::Denied => "denied",
                AttemptState::Expired => "expired",
                AttemptState::Replaced => "replaced",
            }, Some(a.code.clone())),
            None => ("none", None),
        }
    }

    /// The approved session's shell (started on first use).
    async fn shell(&self, server: &Server, cookie: &str) -> Result<Arc<Shell>> {
        let mut g = self.inner.lock().await;
        let s = g.sessions.get_mut(cookie).filter(|s| s.kind == "terminal").context("not approved")?;
        s.last = Instant::now();
        if let Some(sh) = &s.shell {
            return Ok(sh.clone());
        }
        let sh = Arc::new(spawn_shell(&server.home())?);
        s.shell = Some(sh.clone());
        let ip = s.ip.clone();
        drop(g);
        server.audit("user", "terminal", "remote_shell_started", &format!("from {ip}")).await;
        Ok(sh)
    }

    async fn touch_kind(&self, cookie: &str, kind: &str) -> bool {
        let mut g = self.inner.lock().await;
        match g.sessions.get_mut(cookie).filter(|s| s.kind == kind) {
            Some(s) => {
                s.last = Instant::now();
                true
            }
            None => false,
        }
    }

    async fn touch(&self, cookie: &str) -> bool {
        let mut g = self.inner.lock().await;
        match g.sessions.get_mut(cookie) {
            Some(s) => {
                s.last = Instant::now();
                true
            }
            None => false,
        }
    }
}

fn short_ua(ua: &str) -> String {
    let ua = ua.trim();
    if ua.is_empty() {
        return "unknown browser".into();
    }
    let mut parts = vec![];
    for (needle, name) in [("Android", "Android"), ("iPhone", "iPhone"), ("iPad", "iPad"), ("Mac OS X", "macOS"), ("Windows", "Windows"), ("Linux", "Linux")] {
        if ua.contains(needle) {
            parts.push(name);
            break;
        }
    }
    for (needle, name) in [("Edg/", "Edge"), ("Firefox/", "Firefox"), ("Chrome/", "Chrome"), ("Safari/", "Safari")] {
        if ua.contains(needle) {
            parts.push(name);
            break;
        }
    }
    if parts.is_empty() { ua.chars().take(60).collect() } else { parts.join(" · ") }
}

fn spawn_shell(home: &Path) -> Result<Shell> {
    use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
    let pty = NativePtySystem::default().openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })?;
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let cwd = home.join("workspaces");
    std::fs::create_dir_all(&cwd)?;
    let mut cmd = CommandBuilder::new(&shell);
    cmd.arg("-l");
    cmd.cwd(&cwd);
    cmd.env("TERM", "xterm-256color");
    let child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader()?;
    let writer = pty.master.take_writer()?;
    let (tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(256);
    let out = tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = out.send(buf[..n].to_vec());
                }
            }
        }
        let _ = out.send(vec![]); // end of output
    });
    Ok(Shell { writer: std::sync::Mutex::new(writer), master: std::sync::Mutex::new(pty.master), child: std::sync::Mutex::new(child), out: tx })
}

use std::io::Read as _;

// ------------------------------------------------------------------ HTTP

fn router(web: Web) -> Router {
    Router::new()
        .route("/r/{token}", get(open_link))
        .route("/state", get(state))
        .route("/ws", get(ws))
        .route("/xterm.js", get(|| async { asset("application/javascript", include_str!("../terminal/xterm-5.5.0.js")) }))
        .route("/xterm.css", get(|| async { asset("text/css", include_str!("../terminal/xterm-5.5.0.css")) }))
        .route("/fit.js", get(|| async { asset("application/javascript", include_str!("../terminal/addon-fit-0.10.0.js")) }))
        .with_state(web)
}

/// `/browser`: pairing under /_ku/, everything else is lb web's UI, proxied for an approved
/// browser with the passcode added here (the phone never gets it).
fn browser_router(web: Web) -> Router {
    Router::new()
        .route("/_ku/r/{token}", get(open_link))
        .route("/_ku/state", get(state))
        .fallback(proxy)
        .with_state(web)
}

async fn proxy(State(web): State<Web>, req: axum::extract::Request) -> Response {
    let Some(server) = web.server.upgrade() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let Some(cookie) = cookie_of(req.headers(), "browser") else {
        return page("Kurisu", "<h2>Kurisu</h2><p>Not paired. Send /browser in Telegram.</p>");
    };
    if !server.remote.touch_kind(&cookie, "browser").await {
        return page("Kurisu", "<h2>Kurisu</h2><p>Not paired, or closed. Send /browser in Telegram.</p>");
    }
    let Some(cfg) = crate::browser::active(&server.home()) else {
        return page("Kurisu", "<h2>Kurisu</h2><p>Live Browser is off on the computer.</p>");
    };
    server.browser.touch().await;
    let path = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut out = reqwest::Client::new().request(method, format!("{}{path}", cfg.server_url())).timeout(Duration::from_secs(60));
    for (k, v) in req.headers() {
        let n = k.as_str();
        if matches!(n, "host" | "cookie" | "authorization" | "x-lb-passcode" | "connection" | "content-length" | "origin" | "referer") {
            continue;
        }
        out = out.header(n, v.as_bytes());
    }
    out = out.header("x-lb-passcode", &cfg.passcode);
    let body = match axum::body::to_bytes(req.into_body(), 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let resp = match out.body(body.to_vec()).send().await {
        Ok(r) => r,
        Err(_) => {
            // lb web may have been stopped as idle: start it again.
            let _ = server.browser.ensure(&server.home(), &cfg).await;
            return (StatusCode::BAD_GATEWAY, "Live Browser is starting; retry in a moment").into_response();
        }
    };
    let mut r = Response::builder().status(resp.status().as_u16());
    for (k, v) in resp.headers() {
        if matches!(k.as_str(), "set-cookie" | "transfer-encoding" | "connection" | "content-length") {
            continue;
        }
        r = r.header(k.as_str(), v.as_bytes());
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    let mut r = r.body(axum::body::Body::from(bytes)).unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    r.headers_mut().insert("referrer-policy", "no-referrer".parse().unwrap());
    r.headers_mut().insert("x-frame-options", "DENY".parse().unwrap());
    r
}

fn browser_wait_page(code: &str) -> Response {
    let html = r##"<!doctype html><html><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>Kurisu browser</title><style>body{font:16px system-ui;background:#111;color:#eee;margin:0;padding:24px}
#code{font:700 40px ui-monospace,monospace;letter-spacing:.2em;margin:12px 0}</style></head><body>
<h2>Kurisu live browser</h2><p>Pairing code</p><div id=code>__CODE__</div>
<p id=msg>Waiting for approval in Telegram… Check that the code on the card matches, then tap Approve.</p>
<script>const texts={denied:'Denied in Telegram. This link is dead.',expired:'No approval in time. This link is dead; send /browser again.',
replaced:'This page was replaced by a newer attempt.',closed:'Closed.',none:'Not paired.'};
async function poll(){try{const s=(await (await fetch('/_ku/state',{cache:'no-store'})).json()).state;
if(s==='approved'){location.replace('/');return}if(s!=='pending'){document.getElementById('msg').textContent=texts[s]||s;return}}catch(e){}setTimeout(poll,1500)}
poll();</script></body></html>"##;
    secure_headers(Html(html.replace("__CODE__", code)).into_response())
}

fn asset(ct: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "public, max-age=86400")], body).into_response()
}

fn cookie_of(h: &HeaderMap, kind: &str) -> Option<String> {
    let name = cookie_name(kind);
    h.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(&format!("{name}=")).map(String::from))
}

fn secure_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    h.insert("content-security-policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'".parse().unwrap());
    r
}

fn page(title: &str, body: &str) -> Response {
    secure_headers(Html(format!("<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
<title>{title}</title><style>body{{font:16px system-ui;background:#111;color:#eee;margin:0;padding:24px}}.code{{font:700 40px ui-monospace,monospace;letter-spacing:.2em;margin:16px 0}}</style>\
</head><body>{body}</body></html>")).into_response())
}

async fn open_link(State(web): State<Web>, AxPath(token): AxPath<String>, ConnectInfo(peer): ConnectInfo<SocketAddr>, h: HeaderMap) -> Response {
    let Some(server) = web.server.upgrade() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let ua = h.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    match server.remote.open(&server, &token, &peer.ip().to_string(), &ua, cookie_of(&h, web.kind)).await {
        Err(why) => page("Kurisu", &format!("<h2>Kurisu</h2><p>{why}</p>")),
        Ok((cookie, code, _)) => {
            let mut r = if web.kind == "browser" { browser_wait_page(&code) } else { terminal_page(&code) };
            r.headers_mut().insert(header::SET_COOKIE, format!("{}={cookie}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400", cookie_name(web.kind)).parse().unwrap());
            r
        }
    }
}

async fn state(State(web): State<Web>, h: HeaderMap) -> Response {
    let Some(server) = web.server.upgrade() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let Some(c) = cookie_of(&h, web.kind) else { return axum::Json(json!({"state": "none"})).into_response() };
    let (st, _) = server.remote.state_of(&c).await;
    secure_headers(axum::Json(json!({"state": st})).into_response())
}

async fn ws(State(web): State<Web>, h: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let Some(server) = web.server.upgrade() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    // Same-origin only.
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let origin = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("");
    if origin.split("://").nth(1) != Some(host) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(cookie) = cookie_of(&h, "terminal") else { return StatusCode::UNAUTHORIZED.into_response() };
    let shell = match server.remote.shell(&server, &cookie).await {
        Ok(s) => s,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    upgrade.on_upgrade(move |sock| pump(sock, shell, server, cookie))
}

async fn pump(mut sock: WebSocket, shell: Arc<Shell>, server: Arc<Server>, cookie: String) {
    let mut rx = shell.out.subscribe();
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Ok(b) if b.is_empty() => {
                    let _ = sock.send(Message::Close(None)).await;
                    break;
                }
                Ok(b) => {
                    if sock.send(Message::Binary(b.into())).await.is_err() {
                        break;
                    }
                    server.remote.touch(&cookie).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
            msg = sock.recv() => {
                let Some(Ok(msg)) = msg else { break };
                if !server.remote.touch(&cookie).await {
                    let _ = sock.send(Message::Close(None)).await;
                    break; // closed from Telegram or idle
                }
                match msg {
                    Message::Text(t) => {
                        let t = t.to_string();
                        if let Some(ctl) = t.strip_prefix('\u{0}') {
                            if let Ok(v) = serde_json::from_str::<Value>(ctl) {
                                if let (Some(c), Some(r)) = (v["cols"].as_u64(), v["rows"].as_u64()) {
                                    let _ = shell.master.lock().unwrap().resize(portable_pty::PtySize { rows: r.clamp(5, 500) as u16, cols: c.clamp(10, 500) as u16, pixel_width: 0, pixel_height: 0 });
                                }
                            }
                        } else {
                            let _ = shell.writer.lock().unwrap().write_all(t.as_bytes());
                        }
                    }
                    Message::Binary(b) => {
                        let _ = shell.writer.lock().unwrap().write_all(&b);
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }
}

use std::io::Write as _;

fn terminal_page(code: &str) -> Response {
    let html = r##"<!doctype html><html><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1,interactive-widget=resizes-content">
<title>Kurisu terminal</title><link rel=stylesheet href="/xterm.css">
<style>html,body{margin:0;height:100%;background:#111;color:#eee;font:16px system-ui}
#wait{padding:24px}#code{font:700 40px ui-monospace,monospace;letter-spacing:.2em;margin:12px 0}
#app{display:none;flex-direction:column;height:100%}#t{flex:1;min-height:0;padding:4px}
#keys{display:flex;gap:4px;padding:4px;background:#222;overflow-x:auto}
#keys button{flex:1;min-width:44px;padding:10px 6px;background:#333;color:#eee;border:0;border-radius:6px;font:15px system-ui}
#keys button.on{background:#2b6}</style></head><body>
<div id=wait><h2>Kurisu terminal</h2><p>Pairing code</p><div id=code>__CODE__</div>
<p id=msg>Waiting for approval in Telegram… Check that the code on the card matches, then tap Approve.</p></div>
<div id=app><div id=t></div><div id=keys>
<button data-k="\x1b">Esc</button><button data-k="\t">Tab</button><button id=ctrl>Ctrl</button>
<button data-k="\x1b[D">←</button><button data-k="\x1b[A">↑</button><button data-k="\x1b[B">↓</button><button data-k="\x1b[C">→</button>
</div></div>
<script src="/xterm.js"></script><script src="/fit.js"></script><script>
const msg=document.getElementById('msg');let ctrl=false,ws=null;
const texts={denied:'Denied in Telegram. This link is dead.',expired:'No approval in time. This link is dead; send /terminal again.',
 replaced:'This page was replaced by a newer attempt.',closed:'Closed.',none:'Not paired.'};
async function poll(){try{const r=await fetch('/state',{cache:'no-store'});const s=(await r.json()).state;
 if(s==='approved')return start();if(s!=='pending'){msg.textContent=texts[s]||s;return}}catch(e){}setTimeout(poll,1500)}
function start(){document.getElementById('wait').style.display='none';document.getElementById('app').style.display='flex';
 const term=new Terminal({fontSize:14,cursorBlink:true,scrollback:5000});const fit=new FitAddon.FitAddon();term.loadAddon(fit);
 term.open(document.getElementById('t'));fit.fit();window.term=term;
 ws=new WebSocket((location.protocol==='https:'?'wss://':'ws://')+location.host+'/ws');ws.binaryType='arraybuffer';
 const size=()=>{fit.fit();if(ws.readyState===1)ws.send('\0'+JSON.stringify({cols:term.cols,rows:term.rows}))};
 ws.onopen=size;ws.onmessage=e=>term.write(new Uint8Array(e.data));
 ws.onclose=()=>term.write('\r\n\x1b[33m[terminal closed]\x1b[0m\r\n');
 const send=d=>{if(ws.readyState!==1)return;if(ctrl&&d.length===1){const c=d.toUpperCase().charCodeAt(0);if(c>=64&&c<96)d=String.fromCharCode(c-64);ctrl=false;cb.classList.remove('on')}ws.send(d)};
 term.onData(send);addEventListener('resize',size);const cb=document.getElementById('ctrl');
 cb.onclick=()=>{ctrl=!ctrl;cb.classList.toggle('on',ctrl);term.focus()};
 document.querySelectorAll('#keys [data-k]').forEach(b=>b.onclick=()=>{ws.send(JSON.parse('"'+b.dataset.k+'"'));term.focus()});
 term.focus()}
poll();</script></body></html>"##;
    let mut r = Html(html.replace("__CODE__", code)).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    h.insert("content-security-policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'".parse().unwrap());
    r
}

// ------------------------------------------------------------------ CLI

pub async fn cli(paths: &crate::paths::Paths, args: &[String]) -> Result<()> {
    let home = &paths.home;
    let force = args.iter().any(|a| a == "--force");
    let flag = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    match args.get(2).map(String::as_str).unwrap_or("status") {
        "enable" => {
            let cfg = match flag("--listen") {
                Some(listen) => {
                    check_listen(&listen, force)?;
                    let cfg = Config { enabled: true, listen, public_url: flag("--url") };
                    crate::telegram::write_private(&config_path(home), &serde_json::to_vec_pretty(&cfg)?)?;
                    cfg
                }
                None => {
                    let d = detect_address().await.context(NO_ADDRESS)?;
                    enable_terminal(home, &d, 8790)?
                }
            };
            println!("remote terminal: on, listening on {} when a link is used{}. In Telegram, send /terminal.",
                     cfg.listen, cfg.public_url.as_ref().map(|u| format!(" (links: {u})")).unwrap_or_default());
            Ok(())
        }
        "disable" => {
            disable_terminal(home)?;
            let _ = crate::server::call(&paths.socket(), json!({"op": "remote_close", "why": "ku terminal disable"})).await;
            println!("remote terminal: off (open sessions closed)");
            Ok(())
        }
        "status" => {
            match load(home) {
                Some(c) if c.enabled => println!("remote terminal: on (listen {}{})", c.listen, c.public_url.map(|u| format!(", links {u}")).unwrap_or_default()),
                _ => println!("remote terminal: off (optional). Turn it on with `ku terminal enable [--listen IP:PORT]`."),
            }
            Ok(())
        }
        _ => bail!("usage: kurisu terminal enable [--listen IP:PORT] [--url URL] [--force] | disable | status"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected_urls_and_local_only() {
        let d = Detected { ip: "100.118.224.35".parse().unwrap(), host: Some("mybox.tail.ts.net".into()), via: "Tailscale" };
        assert_eq!(d.url(8790), "http://mybox.tail.ts.net:8790");
        assert_eq!(d.listen(8790), "100.118.224.35:8790");
        let lan = Detected { ip: "192.168.0.58".parse().unwrap(), host: None, via: "LAN" };
        assert_eq!(lan.url(8793), "http://192.168.0.58:8793");
        assert!(is_local_only("127.0.0.1:8790") && is_local_only("0.0.0.0:8790") && !is_local_only("100.1.2.3:8790"));
        assert!(tailscale_ip("100.118.224.35".parse().unwrap()) && !tailscale_ip("100.200.0.1".parse().unwrap()));
    }

    #[test]
    fn listen_addresses_and_codes() {
        assert!(check_listen("127.0.0.1:8790", false).is_ok());
        assert!(check_listen("100.101.102.103:8790", false).is_ok());
        assert!(check_listen("192.168.1.5:8790", false).is_ok());
        assert!(check_listen("0.0.0.0:8790", false).unwrap_err().to_string().contains("every interface"));
        assert!(check_listen("0.0.0.0:8790", true).is_ok());
        assert!(check_listen("8.8.8.8:8790", false).unwrap_err().to_string().contains("public"));
        assert!(check_listen("[fd7a:115c:a1e0::1]:8790", false).is_ok());
        assert!(check_listen("nope", false).is_err());
        let c = pairing_code();
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|ch| "ABCDEFGHJKMNPQRSTUVWXYZ23456789".contains(ch)));
        assert_eq!(short_ua("Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/131.0 Mobile Safari/537.36"), "Android · Chrome");
    }
}
