//! Zero-downtime install: prove a candidate in a shadow, hand over atomically, drain
//! the old server, and roll back by swapping the socket back (never by rebuilding).
//!
//! Layout the handover uses, next to the public `kurisu.sock`:
//!
//!   kurisu.sock     a symlink clients follow; repointed atomically by rename
//!   kurisu.sock.a   one concrete server socket (the old or the new server)
//!   kurisu.sock.b   the other one
//!
//! A deploy:
//!   1. shadow: copy the store to a temp home, run the candidate there with its own
//!      socket and no sidecars, and require version + session count + store reads.
//!   2. handover: start the candidate on the real home in standby (own slot, no
//!      Telegram/web/browser), probe it, then repoint the public symlink in one rename.
//!   3. drain: the old server stops accepting, releases Telegram/web/browser, lets
//!      in-flight turns finish (bounded), then exits. The new server then promotes
//!      (Telegram, web, resumed turns).
//!   4. rollback: if the post-swap probe fails, repoint the symlink back; the old
//!      process never stopped, so there is nothing to rebuild.
//!
//! Explicit non-goal: a session's CLI process does not survive the restart. Sessions
//! may restart; their history is in the store, and a turn cut off by the restart is
//! resumed on the new server. No session is ever left running against a dead socket.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::paths::Paths;
use crate::server;

/// The public socket name (also what `Paths::socket` returns).
const PUBLIC: &str = "kurisu.sock";
const SLOT_A: &str = "kurisu.sock.a";
const SLOT_B: &str = "kurisu.sock.b";

#[derive(Clone, Debug)]
pub struct InstallOpts {
    pub from: PathBuf,
    pub target: PathBuf,
    pub yes: bool,
    pub keep_current: bool,
    /// Let in-flight turns finish this long before the old server stops anyway.
    pub drain: Duration,
    /// Fault injection for deploy drills: "shadow" or "post_swap".
    pub fault: Option<String>,
}

impl InstallOpts {
    pub fn new(from: PathBuf, target: PathBuf, yes: bool, keep_current: bool) -> Self {
        Self {
            from,
            target,
            yes,
            keep_current,
            drain: Duration::from_secs(server::DRAIN_TIMEOUT_SECS),
            fault: std::env::var("KURISU_DEPLOY_FAULT").ok().filter(|s| !s.is_empty()),
        }
    }
}

#[derive(Debug)]
pub struct Report {
    pub commit: String,
    pub shadow_ms: u128,
    pub handover_ms: u128,
    pub sessions: usize,
    pub drained: bool,
    pub telegram: String,
}

/// Install `from` over `target` with a shadow probe and an atomic handover.
pub async fn install(paths: &Paths, opts: &InstallOpts) -> Result<Report> {
    if !opts.from.is_file() {
        bail!("no built binary at {} (ku source build)", opts.from.display());
    }
    // A static check first: this never opens a home or the database.
    let out = Command::new(&opts.from).arg("--version").output().context("run the new binary")?;
    if !out.status.success() {
        bail!("the new binary does not run (--version failed); nothing was changed");
    }
    let commit = commit_of(&opts.from).context("the new binary did not report its source commit")?;
    println!("new: {} ({})", String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("kurisu"), &commit[..commit.len().min(10)]);

    let live = probe_live(paths).await?;
    println!("live: {} session(s), commit {}", live.sessions, &live.commit[..live.commit.len().min(10)]);
    if live.commit == commit {
        println!("the running build already is {}", &commit[..commit.len().min(10)]);
    }

    // ---------------------------------------------------------------- 1. shadow
    let t0 = Instant::now();
    let shadow = Shadow::start(paths, &opts.from, &commit, live.sessions).await?;
    let shadow_ms = t0.elapsed().as_millis();
    println!("shadow probe: ok ({} session(s), version + store reads) in {}ms", shadow.sessions, shadow_ms);
    shadow.stop().await;
    if opts.fault.as_deref() == Some("shadow") {
        bail!("fault injection: shadow passed, aborting before touching the live instance");
    }

    if !crate::source::confirm("Install it in place of the running ku (the current one is kept)?", opts.yes) {
        return Ok(Report { commit, shadow_ms, handover_ms: 0, sessions: live.sessions, drained: true, telegram: "not started".into() });
    }

    // Keep the current binary before replacing the path (the running process keeps its
    // own mapping, so this is never a live outage).
    let bin = paths.home.join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::write(bin.join("target"), opts.target.to_string_lossy().as_bytes())?;
    if opts.keep_current {
        if !bin.join("ku.orig").exists() {
            std::fs::copy(&opts.target, bin.join("ku.orig"))?;
        }
        std::fs::copy(&opts.target, bin.join("ku.prev"))?;
    }

    // ---------------------------------------------------------------- 2. handover
    let t1 = Instant::now();
    // Talk to the old server over connections opened before the public path moves.
    let old_pid = ping_pid(&paths.socket()).await;
    let mut control = Conn::open(&paths.socket()).await.ok();
    let mut watch = Conn::open(&paths.socket()).await.ok();
    let public = PublicSocket::prepare(paths)?;
    println!("socket: {} -> {}", paths.socket().display(), public.next.display());

    let log = paths.home.join("deploy-candidate.log");
    let mut child = spawn_standby(&opts.from, &paths.home, &public.next, &log)?;
    let next_name = public.next.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let previous_name = public.previous.file_name().unwrap_or_default().to_string_lossy().into_owned();

    // The candidate must answer on the real home before it is allowed to own the path.
    let probe = match probe_until(&public.next, &commit, live.sessions, Duration::from_secs(20)).await {
        Ok(p) => p,
        Err(e) => {
            let _ = stop_child(&mut child);
            bail!("the candidate did not pass its pre-swap probe on the live home ({e:#}); the live instance was not touched");
        }
    };
    println!("pre-swap probe: ok (pid {}, {} session(s))", probe.pid, probe.sessions);

    replace(&opts.target, &opts.from)?;
    println!("installed {}", opts.target.display());

    repoint(&paths.socket(), &next_name)?;
    let switched = Instant::now();

    // ---------------------------------------------------------------- 3. verify + drain
    let post = probe_until(&paths.socket(), &commit, live.sessions, Duration::from_secs(10)).await;
    let post = match post {
        Ok(_) if opts.fault.as_deref() == Some("post_swap") => {
            let _ = rollback(paths, &public, &mut child, &previous_name, opts, old_pid).await;
            bail!("fault injection: post-swap probe failed after {}ms; rolled back", switched.elapsed().as_millis());
        }
        Ok(p) => p,
        Err(e) => {
            let _ = rollback(paths, &public, &mut child, &previous_name, opts, old_pid).await;
            bail!("the new server failed its post-swap probe ({e:#}); the previous one is back in place (the live instance never stopped)");
        }
    };
    println!("post-swap probe: ok (pid {}, commit {}, {} session(s)) in {}ms", post.pid, &commit[..commit.len().min(10)], post.sessions, switched.elapsed().as_millis());

    let mut drained = true;
    if let Some(old) = old_pid {
        let did_drain = drain_old(&mut control, &mut watch, old, opts.drain).await;
        drained = did_drain;
    }
    // An older server exiting deletes the public path it knew (`kurisu.sock`), which is
    // now the symlink: always make sure it points at the promoted slot.
    repoint(&paths.socket(), &next_name)?;
    if public.previous != paths.socket() {
        let _ = std::fs::remove_file(&public.previous);
    }

    // ---------------------------------------------------------------- 4. promote
    let promote = server::call(&public.next, json!({"op": "promote"})).await;
    let promote = match promote {
        Ok(v) => v,
        Err(e) => {
            let _ = rollback(paths, &public, &mut child, &previous_name, opts, old_pid).await;
            bail!("the new server could not take over ({e:#}); the previous binary was restored");
        }
    };
    let telegram = promote["telegram"].as_str().unwrap_or("unknown").to_string();
    if telegram.starts_with("error") {
        let _ = rollback(paths, &public, &mut child, &previous_name, opts, old_pid).await;
        bail!("the new server's Telegram channel did not start ({telegram}); the previous binary was restored");
    }
    let handover_ms = t1.elapsed().as_millis();
    println!("handover: {}ms, drained: {}, telegram: {}", handover_ms, drained, telegram);

    Ok(Report { commit, shadow_ms, handover_ms, sessions: post.sessions, drained, telegram })
}

// ------------------------------------------------------------------ probe

struct Live {
    commit: String,
    sessions: usize,
}

async fn probe_live(paths: &Paths) -> Result<Live> {
    let v = server::call(&paths.socket(), json!({"op": "version"})).await
        .context("ask the running server for its version (is it up?)")?;
    let peers = server::call(&paths.socket(), json!({"op": "peers", "include_archived": true})).await?;
    Ok(Live {
        commit: v["git_sha"].as_str().unwrap_or_default().to_string(),
        sessions: peers.as_array().map(Vec::len).unwrap_or(0),
    })
}

struct Probe {
    pid: i64,
    sessions: usize,
}

/// Ask a server (shadow or live) for what a deploy must prove, over its socket:
/// version, the session count, and store reads that touch the schema.
async fn probe(sock: &Path, expect_commit: &str, least_sessions: usize) -> Result<Probe> {
    let v = server::call(sock, json!({"op": "version"})).await?;
    let sha = v["git_sha"].as_str().unwrap_or_default();
    if !expect_commit.is_empty() && sha != expect_commit {
        bail!("version reports commit {sha}, expected {expect_commit}");
    }
    let peers = server::call(sock, json!({"op": "peers", "include_archived": true})).await?;
    let rows = peers.as_array().cloned().unwrap_or_default();
    if rows.len() < least_sessions {
        bail!("sees {} session(s), expected at least {least_sessions}", rows.len());
    }
    // Reads across messages + audit + approvals (the tables a schema break would hit).
    let changes = server::call(sock, json!({"op": "changes", "msg_after": -1, "audit_after": -1})).await?;
    if changes["msg_seq"].as_i64().is_none() || changes["audit_seq"].as_i64().is_none() {
        bail!("store read returned no sequence numbers");
    }
    // One session's history: proves the messages table is readable, not just present.
    if let Some(id) = rows.first().and_then(|r| r["id"].as_str()) {
        let h = server::call(sock, json!({"op": "history", "name": id, "tail": 1})).await?;
        if !h.is_array() {
            bail!("history read for {id} did not return a list");
        }
    }
    let ping = server::call(sock, json!({"op": "ping"})).await?;
    Ok(Probe { pid: ping["pid"].as_i64().unwrap_or(0), sessions: rows.len() })
}

async fn probe_until(sock: &Path, commit: &str, least_sessions: usize, budget: Duration) -> Result<Probe> {
    let deadline = Instant::now() + budget;
    loop {
        match probe(sock, commit, least_sessions).await {
            Ok(p) => return Ok(p),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ------------------------------------------------------------------ shadow

struct Shadow {
    home: PathBuf,
    sock: PathBuf,
    child: Child,
    sessions: usize,
}

impl Shadow {
    async fn start(paths: &Paths, candidate: &Path, commit: &str, prod_sessions: usize) -> Result<Self> {
        let mut last = None;
        for attempt in 0..2 {
            let home = PathBuf::from(format!("/tmp/kurisu-shadow-{}-{attempt}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home)?;
            let sock = home.join(PUBLIC);
            snapshot_db(&paths.db(), &home.join("kurisu.db"))
                .context("copy the store for the shadow probe")?;
            let log = home.join("shadow.log");
            let mut child = spawn_standby(candidate, &home, &sock, &log)?;
            match probe_until(&sock, commit, prod_sessions, Duration::from_secs(20)).await {
                Ok(p) => {
                    let sessions = p.sessions;
                    if sessions == prod_sessions {
                        return Ok(Self { home, sock, child, sessions });
                    }
                    last = Some(anyhow::anyhow!("shadow sees {sessions} session(s), live has {prod_sessions}"));
                }
                Err(e) => last = Some(e.context(if attempt == 0 { "shadow probe" } else { "shadow probe (retry)" })),
            }
            let _ = stop_child(&mut child);
            let _ = std::fs::remove_dir_all(&home);
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("shadow probe failed")))
    }

    async fn stop(mut self) {
        let _ = server::call(&self.sock, json!({"op": "shutdown"})).await;
        let _ = stop_child(&mut self.child);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Consistent copy of a live SQLite store (WAL included) via SQLite's backup API.
fn snapshot_db(from: &Path, to: &Path) -> Result<()> {
    use rusqlite::{Connection, OpenFlags};
    let src = Connection::open_with_flags(from, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut dst = Connection::open(to)?;
    let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
    backup.run_to_completion(20, Duration::from_millis(25), None)?;
    Ok(())
}

// ------------------------------------------------------------------ process + socket helpers

fn spawn_standby(bin: &Path, home: &Path, sock: &Path, log: &Path) -> Result<Child> {
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    let child = Command::new(bin)
        .args(["serve", "--standby", "--socket"])
        .arg(sock)
        .env("KURISU_HOME", home)
        // A standby never resumes turns, but a copied store may look "working".
        .env("KURISU_NO_RESUME", "1")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .with_context(|| format!("start {}", bin.display()))?;
    Ok(child)
}

fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    Ok(())
}

fn commit_of(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|l| l.strip_prefix("source: ")).and_then(|s| s.split("commit ").nth(1)).map(|s| s.trim().to_string())
}

async fn ping_pid(sock: &Path) -> Option<u32> {
    let v = tokio::time::timeout(Duration::from_millis(800), server::call(sock, json!({"op": "ping"}))).await.ok()?.ok()?;
    v["pid"].as_i64().and_then(|p| u32::try_from(p).ok())
}

/// Copy `from` next to `target` and rename it in place (atomic for readers).
fn replace(target: &Path, from: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = target.with_extension("new");
    std::fs::copy(from, &tmp).with_context(|| format!("copy to {} (is that folder writable?)", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

struct PublicSocket {
    previous: PathBuf,
    next: PathBuf,
}

impl PublicSocket {
    /// Make sure `kurisu.sock` is a symlink to a slot, then pick the other slot for the
    /// candidate. On a pre-handover install (a real socket) the old server's path is
    /// moved to `.a` and the public name becomes a relative symlink to it.
    fn prepare(paths: &Paths) -> Result<Self> {
        let link = paths.socket();
        let a = paths.home.join(SLOT_A);
        let b = paths.home.join(SLOT_B);
        match std::fs::symlink_metadata(&link) {
            Ok(m) if m.file_type().is_symlink() => {
                let current = crate::paths::concrete_socket_path(&link)?;
                let previous = current.file_name().map(|n| paths.home.join(n)).unwrap_or_else(|| a.clone());
                let next = if previous == a { b } else { a };
                Ok(Self { previous, next })
            }
            Ok(_) => {
                // A real socket file: move it (the old listener follows the inode) and
                // leave a symlink in its place. Clients retry through the microsecond gap.
                let _ = std::fs::remove_file(&b);
                std::fs::rename(&link, &a).context("move the running server's socket to its slot")?;
                std::os::unix::fs::symlink(SLOT_A, &link).context("point the public socket at its slot")?;
                Ok(Self { previous: a, next: b })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No live socket: bind the public path directly, no draining needed.
                Ok(Self { previous: link.clone(), next: b })
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// Atomically repoint the public symlink (rename a new link over the old one).
fn repoint(link: &Path, target_name: &str) -> Result<()> {
    let tmp = link.with_file_name(format!(".kurisu.sock.new.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target_name, &tmp)?;
    std::fs::rename(&tmp, link)?;
    Ok(())
}

// ------------------------------------------------------------------ old server

/// A persistent connection to one server; survives the public path moving.
struct Conn {
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    w: tokio::net::unix::OwnedWriteHalf,
}

impl Conn {
    async fn open(sock: &Path) -> Result<Self> {
        use tokio::io::AsyncBufReadExt;
        let stream = server::connect_retry(sock, Duration::from_secs(2)).await?;
        let (r, w) = stream.into_split();
        Ok(Self { lines: tokio::io::BufReader::new(r).lines(), w })
    }

    async fn request(&mut self, req: Value) -> Result<Value> {
        use tokio::io::AsyncWriteExt;
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        self.w.write_all(&line).await?;
        let reply = self.lines.next_line().await?.context("no reply")?;
        let reply: Value = serde_json::from_str(&reply)?;
        match reply.get("error") {
            Some(e) => bail!("{}", e.as_str().unwrap_or("error")),
            None => Ok(reply["ok"].clone()),
        }
    }
}

/// Stop the old server: no new clients, sidecars released, in-flight turns finished
/// (bounded), then exit. Falls back to watching its session list and shutting it down
/// when the old build does not know `drain` yet.
async fn drain_old(control: &mut Option<Conn>, watch: &mut Option<Conn>, pid: u32, limit: Duration) -> bool {
    let mut fell_back = false;
    if let Some(c) = control.as_mut() {
        match c.request(json!({"op": "drain", "timeout": limit.as_secs()})).await {
            Ok(v) => {
                let drained = v["drained"].as_bool().unwrap_or(false);
                wait_gone(pid, Duration::from_secs(limit.as_secs() + 30)).await;
                return drained;
            }
            Err(e) if e.to_string().contains("unknown op") => fell_back = true,
            Err(_) => {
                // The server may have exited before replying: that is still a drain.
                if wait_gone(pid, Duration::from_secs(5)).await {
                    return true;
                }
                fell_back = true;
            }
        }
    }
    let _ = fell_back;
    // Older build: observe its busy sessions over the connection opened before the
    // path moved, give turns the same bounded time, then shut it down.
    let deadline = Instant::now() + limit;
    loop {
        let busy = match watch.as_mut() {
            Some(c) => match c.request(json!({"op": "peers", "include_archived": false})).await {
                Ok(v) => v.as_array().cloned().unwrap_or_default().into_iter()
                    .filter(|s| matches!(s["status"].as_str(), Some("starting" | "working" | "awaiting_approval")))
                    .count(),
                Err(_) => 0,
            },
            None => 0,
        };
        if busy == 0 || Instant::now() >= deadline {
            let drained = busy == 0;
            if let Some(c) = control.as_mut() {
                let _ = c.request(json!({"op": "shutdown"})).await;
            }
            wait_gone(pid, Duration::from_secs(30)).await;
            return drained;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_gone(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Last resort: never leave two servers owning one home.
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    tokio::time::sleep(Duration::from_millis(200)).await;
    !std::path::Path::new(&format!("/proc/{pid}")).exists()
}

// ------------------------------------------------------------------ rollback

/// Put the previous server back in place. The old process is normally still running
/// (nothing to restart); if the drain already took it down, start its kept binary on
/// its old slot again.
async fn rollback(paths: &Paths, public: &PublicSocket, child: &mut Child, previous_name: &str,
                  opts: &InstallOpts, old_pid: Option<u32>) -> Result<()> {
    println!("rollback: repointing {} -> {}", paths.socket().display(), previous_name);
    // Kill the candidate first only if the old one is still there; otherwise restart old.
    let old_alive = old_pid.is_some_and(|p| std::path::Path::new(&format!("/proc/{p}")).exists());
    if !old_alive {
        let kept = paths.home.join("bin/ku.prev");
        if kept.is_file() {
            let log = paths.home.join("deploy-rollback.log");
            let mut restart = spawn_standby(&kept, &paths.home, &public.previous, &log)?;
            let ok = probe_until(&public.previous, &commit_of(&kept).unwrap_or_default(), 0, Duration::from_secs(20)).await.is_ok();
            if !ok {
                let _ = stop_child(&mut restart);
                bail!("could not restart the previous binary at {}", public.previous.display());
            }
            // The restarted old server is primary: let it take the sidecars back.
            let _ = server::call(&public.previous, json!({"op": "promote"})).await;
        }
    }
    repoint(&paths.socket(), previous_name)?;
    let _ = server::call(&public.next, json!({"op": "shutdown"})).await;
    let _ = stop_child(child);
    let _ = std::fs::remove_file(&public.next);
    if opts.keep_current {
        let prev = paths.home.join("bin/ku.prev");
        if prev.is_file() {
            let _ = replace(&opts.target, &prev);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_is_read_from_version_output() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("fake-ku");
        std::fs::write(&bin, "#!/bin/sh\nprintf 'kurisu 0.0.1 (abcdef1234, x86_64)\\nsource: https://example/r commit abcdef1234567890\\n'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(commit_of(&bin).as_deref(), Some("abcdef1234567890"));
    }

    #[test]
    fn public_socket_becomes_a_symlink_and_switches_atomically() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths { home: d.path().to_path_buf() };
        // A pre-handover install has a real socket file.
        let listener = std::os::unix::net::UnixListener::bind(paths.socket()).unwrap();
        let public = PublicSocket::prepare(&paths).unwrap();
        assert_eq!(public.previous.file_name().unwrap(), SLOT_A);
        assert_eq!(public.next.file_name().unwrap(), SLOT_B);
        assert_eq!(std::fs::read_link(paths.socket()).unwrap(), std::path::Path::new(SLOT_A));
        // Repoint, then back: the link is the only thing that changes.
        repoint(&paths.socket(), SLOT_B).unwrap();
        assert_eq!(std::fs::read_link(paths.socket()).unwrap(), std::path::Path::new(SLOT_B));
        repoint(&paths.socket(), SLOT_A).unwrap();
        assert_eq!(std::fs::read_link(paths.socket()).unwrap(), std::path::Path::new(SLOT_A));
        // The old listener (whose file moved) is still there at its slot.
        assert!(public.previous.exists());
        drop(listener);
    }

    #[test]
    fn snapshot_copies_schema_and_rows() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src.db");
        let store = crate::store::Store::open(&src).unwrap();
        store.board_post("tester", "hello").unwrap();
        let copy = d.path().join("copy.db");
        snapshot_db(&src, &copy).unwrap();
        let other = crate::store::Store::open(&copy).unwrap();
        assert_eq!(other.board_read(10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn client_retry_waits_for_a_socket_that_appears_later() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("late.sock");
        let s = sock.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(350)).await;
            let _l = std::os::unix::net::UnixListener::bind(&s).unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let c = server::connect_retry(&sock, Duration::from_secs(2)).await;
        assert!(c.is_ok(), "connect_retry should ride out the gap: {c:?}");
    }
}
