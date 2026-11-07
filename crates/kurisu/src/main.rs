//! `kurisu` (alias `ku`): CLI-first, self-hosted lead + peer sessions on your own CLIs.
//!
//!   (every <name> below may also be the session's UUID)
//!   kurisu status                         host CLIs (path, version, login), server, sessions
//!   kurisu lead "<task>"                  start (or reuse) the Lead and give it a task
//!   kurisu new <name> [--model M] [--cwd DIR] [--harness claude|codex] [--role lead|worker] [--description D]
//!   kurisu send <name> "<message>"        message any session and print its reply
//!   kurisu sessions                       list sessions
//!   kurisu history <name>                 a session's messages (user, agent, peer:<name>)
//!   kurisu update <name> [--task T] [--model M] [--effort E] [--description D] [--role R] [--lifecycle L] [--name N]
//!   kurisu audit [<name>]                 who changed what
//!   kurisu usage [<name>]                 token usage from the CLIs' transcripts
//!   kurisu attach <name>                  watch a session's live terminal (detach: Ctrl-b d)
//!   kurisu stop <name> | kurisu shutdown  stop one CLI / stop everything
//!   kurisu web [--port 8787] [--host 127.0.0.1]   local web UI (prints a URL with a token)
//!   kurisu serve                          run the server in the foreground
//!                                         (other commands start it in the background on demand)
//!   kurisu smoke <claude|codex> <cwd>     live driver check (new, prompt, kill, resume, prompt)
//!   kurisu mcp | kurisu hook <file>       (internal) session tool server / Claude hook sink

mod codex;
mod driver;
mod hostcheck;
mod mcp;
mod paths;
mod server;
mod store;
mod web;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::v1::schema::SessionId;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use codex::{CodexConfig, CodexDriver};
use driver::{agent_text, ClaudeDriver, Driver};
use paths::Paths;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn positional(args: &[String], n: usize) -> Option<String> {
    let mut seen = 0;
    let mut skip = false;
    for a in args.iter().skip(2) {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            skip = true;
            continue;
        }
        if seen == n {
            return Some(a.clone());
        }
        seen += 1;
    }
    None
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("kurisu: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let paths = Paths::from_env();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    match cmd {
        "hook" => claude_cli_acp::run_hook(Path::new(args.get(2).context("events path")?)),
        "serve" => server::serve(paths).await,
        "mcp" => mcp::serve_stdio(paths.socket()).await,
        "smoke" => smoke(args.get(2).context("harness")?, PathBuf::from(args.get(3).context("cwd")?)).await,
        "status" => status(&paths).await,
        "web" => {
            let host = flag(&args, "--host").unwrap_or_else(|| "127.0.0.1".into());
            let port: u16 = flag(&args, "--port").map(|p| p.parse()).transpose().context("--port")?.unwrap_or(8787);
            web::serve(paths, &host, port).await
        }
        "sessions" => {
            let v = req(&paths, json!({"op": "peers", "include_archived": args.iter().any(|a| a == "--all")})).await?;
            print_sessions(&v);
            Ok(())
        }
        "new" => {
            let name = positional(&args, 0).context("usage: kurisu new <name>")?;
            let v = req(&paths, json!({"op": "create", "name": name, "model": flag(&args, "--model"),
                                       "cwd": flag(&args, "--cwd"), "harness": flag(&args, "--harness"),
                                       "role": flag(&args, "--role"), "description": flag(&args, "--description")})).await?;
            println!("created {} ({}) id {}  model {}  folder {}", v["name"].as_str().unwrap_or(""), v["role"].as_str().unwrap_or(""),
                     v["id"].as_str().unwrap_or(""), v["model"].as_str().unwrap_or(""), v["cwd"].as_str().unwrap_or(""));
            if let Some(w) = v["warning"].as_str() {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        "lead" => {
            let task = positional(&args, 0).context("usage: kurisu lead \"<task>\"")?;
            let v = req(&paths, json!({"op": "lead", "text": task, "model": flag(&args, "--model")})).await?;
            println!("{}", v["reply"].as_str().unwrap_or(""));
            Ok(())
        }
        "send" => {
            let name = positional(&args, 0).context("usage: kurisu send <name> \"<message>\"")?;
            let text = positional(&args, 1).context("usage: kurisu send <name> \"<message>\"")?;
            send(&paths, &name, &text).await
        }
        "history" => {
            let name = positional(&args, 0).context("usage: kurisu history <name>")?;
            let v = req(&paths, json!({"op": "history", "name": name})).await?;
            for m in v.as_array().into_iter().flatten() {
                println!("[{}] {}: {}", m["seq"], m["role"].as_str().unwrap_or(""), m["text"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        "update" => {
            let name = positional(&args, 0).context("usage: kurisu update <name> --effort low --model M ...")?;
            let mut fields = serde_json::Map::new();
            for f in ["task", "description", "model", "effort", "role", "lifecycle", "name"] {
                if let Some(v) = flag(&args, &format!("--{f}")) {
                    fields.insert(f.into(), json!(v));
                }
            }
            if fields.is_empty() {
                bail!("nothing to update: pass --model, --effort, --description, --task, --role, --lifecycle or --name");
            }
            let v = req(&paths, json!({"op": "update", "name": name, "fields": fields})).await?;
            print_update(&v);
            Ok(())
        }
        "audit" => {
            for a in req(&paths, json!({"op": "audit", "name": positional(&args, 0)})).await?.as_array().into_iter().flatten() {
                if let Some(ev) = a["field"].as_str().and_then(|f| f.strip_prefix("event:")) {
                    println!("[{}] {} {} {} {}", a["seq"], a["actor_name"].as_str().unwrap_or(""), ev, a["target_name"].as_str().unwrap_or(""), a["new"].as_str().unwrap_or(""));
                    continue;
                }
                println!("[{}] {} changed {}.{}: {:?} -> {:?}", a["seq"], a["actor_name"].as_str().unwrap_or(""), a["target_name"].as_str().unwrap_or(""),
                         a["field"].as_str().unwrap_or(""), a["old"].as_str().unwrap_or(""), a["new"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        "usage" => {
            let v = req(&paths, json!({"op": "usage", "name": positional(&args, 0)})).await?;
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_usage(&v);
            }
            Ok(())
        }
        "attach" => {
            let name = positional(&args, 0).context("usage: kurisu attach <name>")?;
            let st = store::Store::open(&paths.db())?;
            let sid = st.resolve_req(&name)?;
            let id = st.get(&sid)?.and_then(|s| s.harness_session_id).context("that session has not started yet")?;
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new("tmux")
                .args(["-L", &paths.tmux_socket(), "attach", "-t", &format!("claude-{}", &id[..8.min(id.len())])])
                .exec();
            Err(err).context("exec tmux")
        }
        "stop" => {
            let name = positional(&args, 0).context("usage: kurisu stop <name>")?;
            let v = req(&paths, json!({"op": "stop", "name": name})).await?;
            println!("{} {}", v["name"].as_str().unwrap_or(""), if v["was_running"] == true { "stopped (it resumes on its next message)" } else { "was not running" });
            Ok(())
        }
        "shutdown" => {
            if tokio::net::UnixStream::connect(paths.socket()).await.is_ok() {
                server::call(&paths.socket(), json!({"op": "shutdown"})).await?;
                println!("kurisu server stopping (its CLI sessions are closed; transcripts are kept)");
            } else {
                println!("kurisu server is not running");
            }
            Ok(())
        }
        _ => {
            println!("{}", include_str!("main.rs").lines().take_while(|l| l.starts_with("//!")).map(|l| l.trim_start_matches("//!").strip_prefix(' ').unwrap_or("")).collect::<Vec<_>>().join("\n"));
            Ok(())
        }
    }
}

async fn send(paths: &Paths, name: &str, text: &str) -> Result<()> {
    let v = req(paths, json!({"op": "send", "name": name, "text": text})).await?;
    println!("{}", v["reply"].as_str().unwrap_or(""));
    Ok(())
}

/// Send a request to the server, starting it in the background if needed.
async fn req(paths: &Paths, r: Value) -> Result<Value> {
    ensure_server(paths).await?;
    server::call(&paths.socket(), r).await
}

async fn ensure_server(paths: &Paths) -> Result<()> {
    if tokio::net::UnixStream::connect(paths.socket()).await.is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(&paths.home)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths.log())?;
    use std::os::unix::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .arg("serve")
        .env("KURISU_HOME", &paths.home)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0) // survives the terminal's Ctrl-C
        .spawn()
        .context("start kurisu server")?;
    for _ in 0..50 {
        if tokio::net::UnixStream::connect(paths.socket()).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("kurisu server did not start; see {}", paths.log().display())
}

fn print_update(v: &Value) {
    let name = v["name"].as_str().unwrap_or("");
    if let Some(w) = v["warning"].as_str() {
        eprintln!("warning: {w}");
    }
    let changes = v["changes"].as_array().cloned().unwrap_or_default();
    if changes.is_empty() {
        println!("{name}: nothing changed");
    }
    for c in changes {
        println!("{name}: {} → {}", c["field"].as_str().unwrap_or(""), c["new"].as_str().unwrap_or(""));
    }
    for u in v["unchanged"].as_array().into_iter().flatten() {
        println!("{name}: {} unchanged", u.as_str().unwrap_or(""));
    }
    if v["changes"].as_array().is_some_and(|c| c.iter().any(|x| x["field"] == "model" || x["field"] == "effort")) {
        println!("(model/effort apply from its next turn)");
    }
}

fn print_usage(v: &Value) {
    println!("  {:<14} {:>5} {:>8} {:>8} {:>12} {:>12}  {}", "NAME", "MSGS", "INPUT", "OUTPUT", "CACHE-WRITE", "CACHE-READ", "MODELS");
    let row = |n: &str, u: &Value| {
        let g = |k: &str| u[k].as_u64().unwrap_or(0);
        let models: Vec<&str> = u["models"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        println!("  {:<14} {:>5} {:>8} {:>8} {:>12} {:>12}  {}", n, g("messages"), g("input_tokens"), g("output_tokens"),
                 g("cache_creation_input_tokens"), g("cache_read_input_tokens"), models.join(","));
    };
    if let Some(m) = v["sessions"].as_object() {
        for (n, u) in m {
            row(n, u);
        }
    }
    row("TOTAL", &v["total"]);
}

fn print_sessions(v: &Value) {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("  (no sessions)");
        return;
    }
    println!("  {:<12} {:<8} {:<7} {:<10} {:<17} {:<6} {:<8} {:<30} {}", "NAME", "ID", "ROLE", "CREATED_BY", "MODEL", "EFFORT", "STATUS", "DESCRIPTION", "NOTE");
    let cut = |v: &Value, n: usize| {
        let t = v.as_str().unwrap_or("");
        if t.chars().count() > n { t.chars().take(n.saturating_sub(1)).collect::<String>() + "…" } else { t.to_string() }
    };
    for s in rows {
        println!(
            "  {:<12} {:<8} {:<7} {:<10} {:<17} {:<6} {:<8} {:<30} {}",
            cut(&s["name"], 12), s["id"].as_str().unwrap_or("").chars().take(8).collect::<String>(), cut(&s["role"], 7), s["created_by_name"].as_str().unwrap_or("(user)"), cut(&s["model"], 17),
            s["effort"].as_str().unwrap_or("-"),
            if s["lifecycle"] == "active" { cut(&s["status"], 8) } else { cut(&s["lifecycle"], 8) },
            cut(&s["description"], 30), cut(&s["note"], 40)
        );
    }
}

async fn status(paths: &Paths) -> Result<()> {
    println!("Host CLIs");
    for c in [hostcheck::claude().await, hostcheck::codex().await] {
        println!("  {c}");
    }
    println!("\nKurisu server ({})", paths.home.display());
    if tokio::net::UnixStream::connect(paths.socket()).await.is_ok() {
        let p = server::call(&paths.socket(), json!({"op": "ping"})).await?;
        println!("  running: pid {}  rss {} kB", p["pid"], p["rss_kb"]);
        println!("\nSessions");
        print_sessions(&server::call(&paths.socket(), json!({"op": "peers", "include_archived": true})).await?);
    } else {
        println!("  not running (starts on demand)");
        if paths.db().exists() {
            let st = store::Store::open(&paths.db())?;
            println!("\nSessions");
            print_sessions(&json!(st.list()?));
        }
    }
    Ok(())
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

async fn smoke(harness: &str, cwd: PathBuf) -> Result<()> {
    let q1 = "Reply with exactly the word PONG and nothing else.";
    let q2 = "What single word did you reply with last time? Answer with just that word.";
    match harness {
        "claude" => {
            let me = std::env::current_exe()?.to_string_lossy().into_owned();
            let mut cfg = claude_cli_acp::Config::new(std::env::temp_dir().join("kurisu-smoke"), vec![me, "hook".into()]);
            cfg.extra_args = vec!["--model".into(), store::DEFAULT_MODEL.into()];
            let d = ClaudeDriver(claude_cli_acp::ClaudeCli::new(cfg));
            let id = d.new_session(cwd.clone()).await?;
            let r1 = ask(&d, &id, q1).await?;
            d.close(&id).await?;
            d.load_session(&id, cwd).await?;
            let r2 = ask(&d, &id, q2).await?;
            d.close(&id).await?;
            println!("reply1={r1:?} reply2={r2:?}");
            anyhow::ensure!(r1.trim() == "PONG" && r2.contains("PONG"), "smoke failed");
        }
        "codex" => {
            let cfg = CodexConfig { sandbox: "read-only".into(), ..Default::default() };
            let d = CodexDriver::spawn(cfg.clone()).await?;
            let id = d.new_session(cwd.clone()).await?;
            let r1 = ask(&d, &id, q1).await?;
            drop(d);
            let d = CodexDriver::spawn(cfg).await?;
            d.load_session(&id, cwd).await?;
            let r2 = ask(&d, &id, q2).await?;
            println!("reply1={r1:?} reply2={r2:?}");
            anyhow::ensure!(r1.trim() == "PONG" && r2.contains("PONG"), "smoke failed");
        }
        h => bail!("unknown harness {h}"),
    }
    println!("SMOKE OK ({harness})");
    Ok(())
}
