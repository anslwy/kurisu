//! Optional Live Browser add-on (github.com/anslwy/live-browser, binary `lb`).
//! Nothing happens until `ku browser setup`. Then:
//!   - every session (Claude and Codex) gets the `lb` MCP server through Kurisu's own
//!     per-session MCP config (never ~/.claude.json or ~/.codex/config.toml), pre-approved;
//!   - Kurisu runs `lb web` on its own port with a random passcode (lb listens on every
//!     interface and the passcode is its guard), starts it on demand and stops it after
//!     30 min without live sessions or viewers, and on shutdown;
//!   - `/browser` in Telegram opens the live view through Kurisu's paired, proxied link
//!     (the phone never sees the passcode).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// The lb release `ku browser setup` downloads when none is installed.
pub const LB_PINNED: &str = "v0.4.18";
pub const IDLE: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub enabled: bool,
    pub lb: String,
    /// Port of Kurisu's own `lb web` (not lb's default 8765).
    pub port: u16,
    pub passcode: String,
    /// Where the `/browser` link listens (loopback, LAN or Tailscale).
    pub listen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("browser.json")
}

pub fn load(home: &Path) -> Option<Config> {
    serde_json::from_slice(&std::fs::read(config_path(home)).ok()?).ok()
}

pub fn active(home: &Path) -> Option<Config> {
    load(home).filter(|c| c.enabled && Path::new(&c.lb).is_file())
}

impl Config {
    pub fn server_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The `mcpServers` entry every session gets.
    pub fn mcp_entry(&self) -> Value {
        json!({"type": "stdio", "command": self.lb, "args": ["mcp"],
               "env": {"LB_SERVER": self.server_url(), "LB_PASSCODE": self.passcode}})
    }
}

fn find_lb() -> Option<PathBuf> {
    crate::stt::which_bin("lb").or_else(|| {
        let h = PathBuf::from(std::env::var("HOME").ok()?);
        [".local/bin/lb", ".local/share/live-browser/lb", ".kurisu/lb/lb"].iter().map(|p| h.join(p)).find(|p| p.is_file())
    })
}

fn lb_version(lb: &Path) -> Option<String> {
    let out = std::process::Command::new(lb).arg("--version").output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1).map(String::from)
}

/// Chrome engines lb can use (from `lb browsers`), e.g. "System Chrome: /usr/bin/chromium".
fn chrome_engines(lb: &Path) -> Vec<String> {
    let out = std::process::Command::new(lb).arg("browsers").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    out.lines().filter(|l| l.contains(": /")).map(|l| l.trim().to_string()).collect()
}

async fn download_lb(home: &Path) -> Result<PathBuf> {
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x86_64",
        (os, arch) => bail!("no Live Browser release for {os}/{arch}; install lb yourself (github.com/anslwy/live-browser) and run this again"),
    };
    let url = format!("https://github.com/anslwy/live-browser/releases/download/{LB_PINNED}/live-browser-{LB_PINNED}-{asset}.tar.gz");
    let bytes = reqwest::get(&url).await?.error_for_status()?.bytes().await?;
    let dir = home.join("lb");
    std::fs::create_dir_all(&dir)?;
    let tgz = dir.join("lb.tar.gz");
    std::fs::write(&tgz, &bytes)?;
    let st = std::process::Command::new("tar").arg("-xzf").arg(&tgz).arg("-C").arg(&dir).status()?;
    let _ = std::fs::remove_file(&tgz);
    if !st.success() {
        bail!("could not unpack {url}");
    }
    // The archive holds `lb` (possibly in a folder) and its web assets.
    let found = walk_find(&dir, "lb").context("the release archive has no lb binary")?;
    Ok(found)
}

fn walk_find(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = walk_find(&p, name) {
                return Some(f);
            }
        } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(p);
        }
    }
    None
}

fn random_passcode() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

/// Set up the add-on: find or download lb, check for Chrome, pick the /browser address
/// (the Tailscale/LAN address unless given) and write browser.json. Returns a summary.
pub async fn setup(home: &Path, listen: Option<String>, url: Option<String>, port: u16, lb: Option<PathBuf>, verbose: bool) -> Result<String> {
    let lb = match lb.filter(|p| p.is_file()).or_else(find_lb) {
        Some(p) => p,
        None => {
            if verbose {
                println!("downloading Live Browser {LB_PINNED} ...");
            }
            download_lb(home).await?
        }
    };
    let ver = lb_version(&lb).context("that lb does not run")?;
    if !ver.starts_with("0.4") && !ver.starts_with("0.5") {
        bail!("lb {ver} is not a version Kurisu knows (0.4.x/0.5.x)");
    }
    let engines = chrome_engines(&lb);
    if engines.is_empty() {
        bail!("Live Browser found no Chrome on this machine. Install Google Chrome or Chromium there, or run `{} browsers download` on it, then try again.", lb.display());
    }
    let (listen, public_url) = match listen {
        Some(l) => (l, url),
        None => {
            let d = crate::remote::detect_address().await.context(crate::remote::NO_ADDRESS)?;
            (d.listen(8793), url.or_else(|| d.host.as_ref().map(|_| d.url(8793))))
        }
    };
    let old = load(home);
    let cfg = Config { enabled: true, lb: lb.to_string_lossy().into_owned(), port,
                       passcode: old.map(|o| o.passcode).filter(|p| p.len() >= 32).unwrap_or_else(random_passcode),
                       listen: listen.clone(), public_url: public_url.clone() };
    crate::telegram::write_private(&config_path(home), &serde_json::to_vec_pretty(&cfg)?)?;
    Ok(format!("Live Browser: on (lb {ver}; {}). /browser links: {}.", engines.first().cloned().unwrap_or_default(),
               public_url.unwrap_or_else(|| format!("http://{listen}"))))
}

pub async fn cli(paths: &crate::paths::Paths, args: &[String]) -> Result<()> {
    let home = &paths.home;
    let force = args.iter().any(|a| a == "--force");
    let flag = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    match args.get(2).map(String::as_str).unwrap_or("status") {
        "setup" => {
            let listen = flag("--listen");
            if let Some(l) = &listen {
                crate::remote::check_listen(l, force)?;
            }
            let port: u16 = flag("--port").map(|p| p.parse()).transpose().context("--port")?.unwrap_or(8792);
            let summary = setup(home, listen, flag("--url"), port, flag("--lb").map(PathBuf::from), true).await?;
            println!("{summary}");
            println!("Every session gets the lb MCP tools (from its next start). In Telegram, /browser opens the live view (approve the pairing code on the card).");
            let _ = crate::server::call(&paths.socket(), json!({"op": "browser_reload"})).await;
            Ok(())
        }
        "off" | "on" => {
            let mut c = load(home).context("Live Browser is not set up: ku browser setup")?;
            c.enabled = sub_on(args);
            crate::telegram::write_private(&config_path(home), &serde_json::to_vec_pretty(&c)?)?;
            let _ = crate::server::call(&paths.socket(), json!({"op": "browser_reload"})).await;
            println!("Live Browser: {} (sessions pick it up from their next start)", if c.enabled { "on" } else { "off" });
            Ok(())
        }
        "status" => {
            println!("{}", status_line(home, crate::server::call(&paths.socket(), json!({"op": "browser_status"})).await.ok().map(|v| v["running"] == json!(true))));
            Ok(())
        }
        _ => bail!("usage: kurisu browser setup [--listen IP:PORT] [--port N] [--url URL] [--lb PATH] | status | off | on"),
    }
}

fn sub_on(args: &[String]) -> bool {
    args.get(2).map(String::as_str) == Some("on")
}

/// "browser: on (lb 0.4.18, running)" or the optional hint.
pub fn status_line(home: &Path, running: Option<bool>) -> String {
    match load(home) {
        Some(c) if c.enabled => {
            let ver = lb_version(Path::new(&c.lb)).unwrap_or_else(|| "missing".into());
            format!("browser: on (lb {ver}, {}; /browser link on {})", match running { Some(true) => "running", Some(false) => "stopped until needed", None => "server not running" }, c.listen)
        }
        Some(_) => "browser: off (ku browser on)".into(),
        None => "browser: not set up (optional: ku browser setup gives every session a real Chrome, and /browser the live view)".into(),
    }
}

/// Kurisu's own `lb web`: started on demand, stopped when idle or on shutdown.
#[derive(Default)]
pub struct Supervisor {
    child: Mutex<Option<Child>>,
    last_use: Mutex<Option<Instant>>,
}

impl Supervisor {
    pub async fn healthy(cfg: &Config) -> bool {
        reqwest::Client::new().get(format!("{}/api/health", cfg.server_url())).header("x-lb-passcode", &cfg.passcode)
            .timeout(Duration::from_secs(3)).send().await.is_ok_and(|r| r.status().is_success())
    }

    pub async fn running(&self) -> bool {
        self.child.lock().await.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_none())
    }

    /// Make sure `lb web` answers; start it (own process group, so it and its Chrome are
    /// stopped with Kurisu and never left behind) if it does not.
    pub async fn ensure(&self, home: &Path, cfg: &Config) -> Result<()> {
        *self.last_use.lock().await = Some(Instant::now());
        if Self::healthy(cfg).await {
            return Ok(());
        }
        let mut g = self.child.lock().await;
        if g.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_none()) {
            drop(g);
        } else {
            let dir = home.join("browser");
            std::fs::create_dir_all(&dir)?;
            let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("lb-web.log"))?;
            let mut cmd = Command::new(&cfg.lb);
            cmd.args(["web", "--port", &cfg.port.to_string(), "--foreground", "--no-open", "--profile"]).arg(dir.join("profile"))
                .env("LB_PASSCODE", &cfg.passcode).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).kill_on_drop(true);
            #[cfg(unix)]
            cmd.process_group(0);
            *g = Some(cmd.spawn().with_context(|| format!("start {} web", cfg.lb))?);
            drop(g);
        }
        for _ in 0..60 {
            if Self::healthy(cfg).await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        bail!("lb web did not start (see {}/browser/lb-web.log)", home.display())
    }

    pub async fn touch(&self) {
        *self.last_use.lock().await = Some(Instant::now());
    }

    /// Stop it: SIGTERM to its process group (lb reaps its browsers), then SIGKILL.
    pub async fn stop(&self) {
        let Some(mut c) = self.child.lock().await.take() else { return };
        #[cfg(unix)]
        if let Some(pid) = c.id() {
            // The whole group (lb and its Chrome) must go, not just lb.
            let group = format!("-{pid}");
            let alive = || std::process::Command::new("kill").args(["-0", "--", &group]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
            let _ = std::process::Command::new("kill").args(["-TERM", "--", &group]).stderr(Stdio::null()).status();
            for _ in 0..30 {
                let _ = c.try_wait();
                if !alive() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let _ = std::process::Command::new("kill").args(["-KILL", "--", &group]).stderr(Stdio::null()).status();
        }
        let _ = c.kill().await;
    }

    /// Stop after `IDLE` with no live sessions and no viewers.
    pub async fn tick(&self, busy: bool) {
        if busy {
            self.touch().await;
            return;
        }
        let idle = self.last_use.lock().await.is_some_and(|t| t.elapsed() > IDLE);
        if idle && self.running().await {
            eprintln!("browser: stopping lb web (idle 30 min)");
            self.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_entry_and_status() {
        let d = tempfile::tempdir().unwrap();
        assert!(status_line(d.path(), None).contains("not set up"));
        let c = Config { enabled: true, lb: "/x/lb".into(), port: 8792, passcode: random_passcode(), listen: "127.0.0.1:8793".into(), public_url: None };
        assert_eq!(c.passcode.len(), 64);
        let e = c.mcp_entry();
        assert_eq!(e["args"], json!(["mcp"]));
        assert_eq!(e["env"]["LB_SERVER"], "http://127.0.0.1:8792");
        assert!(active(d.path()).is_none());
    }
}
