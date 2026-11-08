//! `kurisu` (alias `ku`): CLI-first, self-hosted lead + peer sessions on your own CLIs.
//!
//!   (every <name> below may also be the session's UUID)
//!   kurisu status                         host CLIs (path, version, login), server, sessions
//!   kurisu lead "<task>" [--harness claude|codex|opencode] [--model M]
//!                                         start (or reuse) the Lead and give it a task
//!                                         (default harness: the installed, logged-in CLI; Claude if both)
//!   kurisu new <name> [--harness claude|codex|opencode] [--model M] [--mode auto|default|acceptEdits|plan] [--cwd DIR] [--role lead|worker] [--description D]
//!   kurisu send <name> "<message>" [--no-wait] [--steer]   message a session and print its reply
//!                                         (--no-wait: queue and return; --steer: type it into the running turn)
//!   kurisu sessions                       list sessions
//!   kurisu models <claude|codex|opencode> [--model ID]   model catalogue and model-specific ACP effort choices
//!   kurisu history <name> [--tools] [--search T] [--limit N] [--before SEQ]
//!                                         a session's history (any harness; tool calls with --tools)
//!   kurisu handoff <name>                 what another agent needs to continue that session (JSON)
//!   kurisu update <name> [--task T] [--model M] [--effort E] [--mode M] [--description D] [--role R] [--lifecycle L] [--name N]
//!                                         (--mode: permission mode; auto lets routine actions run and asks for risky ones)
//!   kurisu update <name> --harness claude|codex|opencode [--model M]   switch harness: archive it, start a same-name replacement that reads its history
//!   kurisu audit [<name>]                 who changed what
//!   kurisu approvals [<name>] [--all]     pending tool approvals (--all: recent decisions too)
//!   kurisu approve <id> [--always]        allow one pending call (--always: also allow that tool in the session's folder)
//!   kurisu deny <id> [--reason R]         deny one pending call (or dismiss a question)
//!   kurisu answer <id> "<answer>" [...]   answer a question (one answer per question, in order)
//!   kurisu wake add <name> "<message>" (--at WHEN | --repeat "CRON")   schedule a message
//!                                         (WHEN: "in 30m", "18:30", "2026-11-08 06:00", RFC 3339; CRON: 5 fields, local time)
//!   kurisu wake ls [<name>] [--all]       scheduled wakes (--all: also fired/cancelled)
//!   kurisu wake rm <id>                   cancel a wake
//!   kurisu stt setup [stt setup options]  opt in to local speech-to-text (uses the `stt` CLI;
//!                                         runs its one-time model download if needed)
//!   kurisu stt use --command "CMD [{file}]" | --mcp "CMD ARGS" [--tool transcribe]
//!                                         use any other speech-to-text command or stdio MCP server
//!   kurisu stt status | kurisu stt off | kurisu stt <audio file>
//!   kurisu telegram setup [--from-env FILE]... [--chat-id ID] [--cwd DIR]
//!                                         opt in to the built-in Telegram channel (token stored mode 600)
//!   kurisu telegram status | off | on
//!   kurisu import omnigent [--session ID]... [--all] [--dry-run] [--model M] [--effort E] [--roster FILE] [--db FILE]
//!                                         copy Omnigent sessions (default: Omni Lead's roster) into Kurisu
//!   kurisu source [status] | check [--install] | get | build | install | rollback | reset
//!                                         rebuild Kurisu from the exact commit this binary came from
//!                                         (keeps the previous binary; reset goes back to the shipped one)
//!   kurisu terminal enable [--listen IP:PORT] [--url URL] | disable | status
//!                                         allow /terminal in Telegram (off by default; approval per attempt)
//!   kurisu web-link enable [--listen IP:PORT] [--url URL] | disable | status
//!                                         allow /web: a chat UI for sessions (same approval flow)
//!   kurisu browser setup [--listen IP:PORT] [--port N] [--lb PATH] | status | off | on
//!                                         optional Live Browser: every session gets a real Chrome (lb MCP),
//!                                         /browser in Telegram shows it live (approval per attempt)
//!   kurisu rules                          pre-approval rules
//!   kurisu rule add (--role R | --session S) --tool T [--in-cwd] [--command-prefix P] [--deny]
//!   kurisu rule rm <id>                   remove a rule
//!   kurisu version [--json]              running server build (no auto-start)
//!   kurisu usage [<name>]                 token usage from the CLIs' transcripts
//!   kurisu attach <name>                  watch a session's live terminal (detach: Ctrl-b d)
//!   kurisu mcp <name>                     a session's MCP servers (kurisu-mcp is always on)
//!   kurisu mcp add <name> <server> (--command "cmd args..." | --url URL [--type http|sse])
//!   kurisu mcp enable|disable|rm <name> <server>   (changes apply from its next turn)
//!   kurisu compact <name>                 compact a session's context (the CLI's /compact)
//!   kurisu stop <name> | kurisu shutdown  stop one CLI / stop everything
//!   kurisu web                          open the running instance's /web chat (one-time link)
//!   kurisu web --serve [--port N] [--host H]   old standalone web UI
//!                                         (--port/--host alone also serve, legacy)
//!   kurisu serve                          run the server in the foreground
//!                                         (other commands start it in the background on demand)
//!   kurisu smoke <claude|codex|opencode> <cwd>     live driver check (new, prompt, kill, resume, prompt)
//!   kurisu mcp | kurisu hook <file>       (internal) session tool server / Claude hook sink
//!   kurisu --version

mod activity;
mod approvals;
mod browser;
mod chat;
mod codex;
mod opencode;
mod driver;
mod hostcheck;
mod import;
mod mcp;
mod paths;
mod remote;
mod server;
mod source;
mod store;
mod stt;
mod telegram;
mod version;
mod wakes;
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

/// `ku stt ...`: speech-to-text is optional; nothing changes until the user opts in here.
async fn stt_cli(paths: &Paths, args: &[String]) -> Result<()> {
    let home = paths.home.clone();
    match positional(args, 0).as_deref() {
        Some("setup") => {
            // Reuse the external `stt` CLI: run its one-time setup (venv + model) unless it is set up already.
            let bin = flag(args, "--bin").map(PathBuf::from).or_else(|| which("stt"))
                .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/bin/stt")).filter(|p| p.is_file()))
                .context("no `stt` command found (PATH or ~/.local/bin).\n\
`stt` is an optional, separate local speech-to-text tool (faster-whisper with a warm daemon) from the Telegram bridge \
repo (tools/stt/stt): copy it to ~/.local/bin/stt and run this again (its one-time setup downloads a model).\n\
Or use any speech-to-text you already have: ku stt use --command \"whisper-cli -f {file}\" (prints the transcript), \
or ku stt use --mcp \"CMD ARGS\" --tool NAME (a stdio MCP server whose tool takes {path}).\n\
Without it Kurisu works as before: voice files are passed through unchanged.")?;
            let ok = std::process::Command::new(&bin).arg("status").stdout(std::process::Stdio::null()).status()?.success();
            if !ok {
                let mut extra: Vec<&String> = vec![];
                let mut it = args.iter().skip(3);
                while let Some(a) = it.next() {
                    if a == "--bin" {
                        it.next();
                    } else {
                        extra.push(a);
                    }
                }
                println!("running `{} setup` (one-time; downloads a model, this can take a while)...", bin.display());
                let st = std::process::Command::new(&bin).arg("setup").args(extra).status()?;
                if !st.success() {
                    bail!("`{} setup` failed ({st}); Kurisu's speech-to-text stays off", bin.display());
                }
            }
            stt::save(&home, &stt::Config::Command { command: vec![bin.to_string_lossy().into_owned()] })?;
            println!("speech-to-text: on (command {}). Voice attachments are now transcribed on send; sessions have the transcribe tool.", bin.display());
            Ok(())
        }
        Some("use") => {
            let cfg = match (flag(args, "--command"), flag(args, "--mcp")) {
                (Some(c), None) => stt::Config::Command { command: stt::split_command(&c) },
                (None, Some(m)) => stt::Config::Mcp { command: stt::split_command(&m), tool: flag(args, "--tool").unwrap_or_else(|| "transcribe".into()) },
                _ => bail!("usage: kurisu stt use --command \"CMD [{{file}}]\" | --mcp \"CMD ARGS\" [--tool NAME]"),
            };
            let argv = match &cfg {
                stt::Config::Command { command } | stt::Config::Mcp { command, .. } => command.clone(),
                stt::Config::Off => vec![],
            };
            let bin = argv.first().context("empty command")?;
            if which(bin).is_none() && !Path::new(bin).is_file() {
                bail!("{bin}: not found");
            }
            stt::save(&home, &cfg)?;
            println!("speech-to-text: on ({})", serde_json::to_string(&cfg)?);
            Ok(())
        }
        Some("off") => {
            let was = stt::disable(&home)?;
            println!("speech-to-text: off{}", if was { "" } else { " (it was not on)" });
            Ok(())
        }
        Some("status") | None => {
            match stt::load(&home) {
                Some(stt::Config::Off) => println!("speech-to-text: off (`ku stt setup` or `ku stt use` turns it on again)"),
                None => println!("speech-to-text: not configured. The Telegram channel uses a working `stt` CLI if it finds one; \
voice attachments on send are transcribed only after `ku stt setup`. `ku stt off` turns both off."),
                Some(c) => {
                    println!("speech-to-text: on ({})", serde_json::to_string(&c)?);
                    if let stt::Config::Command { command } = &c {
                        if command.len() == 1 && command[0].ends_with("stt") {
                            let _ = std::process::Command::new(&command[0]).arg("status").status();
                        }
                    }
                }
            }
            Ok(())
        }
        Some(file) => {
            let cfg = stt::active(&home).context("speech-to-text is off: run `ku stt setup` first")?;
            let p = std::fs::canonicalize(file).with_context(|| format!("no such file: {file}"))?;
            println!("{}", stt::transcribe(&cfg, &p).await?);
            Ok(())
        }
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        return Path::new(bin).is_file().then(|| PathBuf::from(bin));
    }
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(bin)).find(|c| c.is_file()))
}

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
            // Boolean flags take no value.
            skip = !["--all", "--json", "--always", "--in-cwd", "--deny", "--no-wait", "--steer", "--tools"].contains(&a.as_str());
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
        "hook" => claude_cli_acp::run_hook_args(&args[2..]),
        "--version" | "-V" => {
            println!("kurisu {}\nsource: {} commit {}", source::version_line(), source::REPO, source::COMMIT);
            Ok(())
        }
        "source" => source::cli(&paths, &args).await,
        "terminal" => remote::cli(&paths, &args).await,
        "web-link" | "weblink" => remote::web_cli(&paths, &args).await,
        "browser" => browser::cli(&paths, &args).await,
        "approvals" => {
            let v = req(&paths, json!({"op": "approvals", "name": positional(&args, 0), "all": args.iter().any(|a| a == "--all")})).await?;
            print_approvals(&v);
            Ok(())
        }
        "approve" | "deny" => {
            let id = positional(&args, 0).with_context(|| format!("usage: kurisu {cmd} <approval-id>"))?;
            let v = req(&paths, json!({"op": cmd, "id": id, "always": args.iter().any(|a| a == "--always"),
                                       "reason": flag(&args, "--reason")})).await?;
            println!("{} {} ({}: {})", v["id"].as_str().unwrap_or(""), v["status"].as_str().unwrap_or(""),
                     v["session_name"].as_str().unwrap_or(""), v["title"].as_str().unwrap_or(""));
            if let Some(r) = v["rule"].as_object() {
                println!("added rule #{}: allow {} for this session in its folder", r["id"], r["tool"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        "answer" => {
            let id = positional(&args, 0).context("usage: kurisu answer <id> \"<answer>\" ...")?;
            let mut answers = serde_json::Map::new();
            for i in 0.. {
                let Some(a) = positional(&args, i + 1) else { break };
                answers.insert(i.to_string(), json!(a));
            }
            if answers.is_empty() {
                bail!("usage: kurisu answer <id> \"<answer>\" ... (see ku approvals for the question)");
            }
            let v = req(&paths, json!({"op": "answer", "id": id, "answers": answers})).await?;
            println!("{} answered: {}", v["id"].as_str().unwrap_or(""), v["answer"]);
            Ok(())
        }
        "rules" => {
            print_rules(&req(&paths, json!({"op": "rules"})).await?);
            Ok(())
        }
        "stt" => stt_cli(&paths, &args).await,
        "telegram" => telegram::cli(&paths, &args).await,
        "import" => import::cli(&paths, &args).await,
        "wake" => match positional(&args, 0).as_deref() {
            Some("add") => {
                let usage = "usage: kurisu wake add <name> \"<message>\" (--at WHEN | --repeat \"CRON\" | both)";
                let name = positional(&args, 1).context(usage)?;
                let message = positional(&args, 2).context(usage)?;
                let w = req(&paths, json!({"op": "wake_add", "name": name, "message": message,
                                           "at": flag(&args, "--at"), "repeat": flag(&args, "--repeat")})).await?;
                println!("wake #{} for {} at {}{}", w["id"], w["session_name"].as_str().unwrap_or(""), w["at_local"].as_str().unwrap_or(""),
                         w["repeat"].as_str().map(|r| format!(" (repeats '{r}')")).unwrap_or_default());
                Ok(())
            }
            Some("ls") | None => {
                let v = req(&paths, json!({"op": "wakes", "name": positional(&args, 1), "all": args.iter().any(|a| a == "--all")})).await?;
                let list = v.as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    println!("(no wakes)");
                }
                for w in list {
                    let late = if w["last_late"] == json!(true) { " late" } else { "" };
                    println!("#{:<4} {:<9} {:<12} {:<22} {:<14} by {:<10} {}{}", w["id"], w["status"].as_str().unwrap_or(""),
                             w["session_name"].as_str().unwrap_or(""), w["at_local"].as_str().unwrap_or(""),
                             w["repeat"].as_str().unwrap_or("-"), w["created_by_name"].as_str().unwrap_or(""),
                             w["message"].as_str().unwrap_or("").chars().take(60).collect::<String>(), late);
                }
                Ok(())
            }
            Some("rm") => {
                let id = positional(&args, 1).context("usage: kurisu wake rm <id>")?;
                req(&paths, json!({"op": "wake_cancel", "id": id})).await?;
                println!("cancelled wake #{}", id.trim_start_matches('#'));
                Ok(())
            }
            _ => bail!("usage: kurisu wake add <name> \"<message>\" --at WHEN [--repeat CRON] | kurisu wake ls [<name>] [--all] | kurisu wake rm <id>"),
        },
        "rule" => match positional(&args, 0).as_deref() {
            Some("add") => {
                let (scope, target) = match (flag(&args, "--role"), flag(&args, "--session")) {
                    (Some(r), None) => ("role", r),
                    (None, Some(s)) => ("session", s),
                    _ => bail!("usage: kurisu rule add (--role R | --session S) --tool T [--in-cwd] [--command-prefix P] [--deny]"),
                };
                let v = req(&paths, json!({"op": "rule_add", "scope": scope, "target": target, "tool": flag(&args, "--tool").context("--tool")?,
                                           "in_cwd": args.iter().any(|a| a == "--in-cwd"), "command_prefix": flag(&args, "--command-prefix"),
                                           "decision": if args.iter().any(|a| a == "--deny") { "deny" } else { "allow" }})).await?;
                print_rules(&json!([v]));
                Ok(())
            }
            Some("rm") => {
                let id = positional(&args, 1).context("usage: kurisu rule rm <id>")?;
                req(&paths, json!({"op": "rule_remove", "id": id})).await?;
                println!("removed rule #{}", id.trim_start_matches('#'));
                Ok(())
            }
            _ => bail!("usage: kurisu rule add ... | kurisu rule rm <id>"),
        },
        "serve" => server::serve(paths).await,
        // Bare `mcp` is the per-session tool server (stdio); `mcp <args>` is the CLI below.
        "mcp" if args.len() == 2 => mcp::serve_stdio(paths.socket()).await,
        "smoke" => smoke(args.get(2).context("harness")?, PathBuf::from(args.get(3).context("cwd")?)).await,
        "status" => status(&paths).await,
        "web" => match web_cmd(&args)? {
            WebCmd::Link => web_link(&paths).await,
            WebCmd::Serve { host, port, legacy } => {
                if legacy {
                    eprintln!("kurisu: `ku web --host/--port` is the legacy form; use `ku web --serve ...` (serving anyway)");
                }
                web::serve(paths, &host, port).await
            }
        },
        "models" => {
            let harness=positional(&args,0).unwrap_or_else(||"claude".into());
            let v=req(&paths,json!({"op":"models","harness":harness,"model":flag(&args,"--model")})).await?;
            println!("{}",serde_json::to_string_pretty(&v)?); Ok(())
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
                                       "role": flag(&args, "--role"), "description": flag(&args, "--description"),
                                       "permission_mode": flag(&args, "--mode")})).await?;
            println!("created {} ({}) id {}  model {}  folder {}", v["name"].as_str().unwrap_or(""), v["role"].as_str().unwrap_or(""),
                     v["id"].as_str().unwrap_or(""), v["model"].as_str().unwrap_or(""), v["cwd"].as_str().unwrap_or(""));
            if let Some(w) = v["warning"].as_str() {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        "lead" => {
            let task = positional(&args, 0).context("usage: kurisu lead \"<task>\" [--harness claude|codex|opencode] [--model M]")?;
            let v = req(&paths, json!({"op": "lead", "text": task, "model": flag(&args, "--model"), "harness": flag(&args, "--harness")})).await?;
            println!("{}", v["reply"].as_str().unwrap_or(""));
            Ok(())
        }
        "send" => {
            let name = positional(&args, 0).context("usage: kurisu send <name> \"<message>\"")?;
            let text = positional(&args, 1).context("usage: kurisu send <name> \"<message>\"")?;
            if args.iter().any(|a| a == "--no-wait" || a == "--steer") {
                let mode = if args.iter().any(|a| a == "--steer") { "steer" } else { "queue" };
                let v = req(&paths, json!({"op": "send", "name": name, "text": text, "wait": false, "mode": mode})).await?;
                let how = if v["steered"] == true { "typed into the running turn" } else if v["queued"] == true { "queued after the running turn" } else { "sent" };
                println!("{} message {} {how}", v["name"].as_str().unwrap_or(""), v["seq"]);
                return Ok(());
            }
            send(&paths, &name, &text).await
        }
        "history" => {
            let name = positional(&args, 0).context("usage: kurisu history <name>")?;
            let mut r = json!({"op": "history_page", "name": name, "tools": args.iter().any(|a| a == "--tools"),
                               "limit": flag(&args, "--limit").and_then(|v| v.parse::<i64>().ok()).unwrap_or(200)});
            if let Some(t) = flag(&args, "--search") {
                r["search"] = json!(t);
            }
            if let Some(b) = flag(&args, "--before").and_then(|v| v.parse::<i64>().ok()) {
                r["before"] = json!(b);
            }
            let v = req(&paths, r).await?;
            for m in v["messages"].as_array().into_iter().flatten() {
                let role = m["role"].as_str().unwrap_or("");
                match role {
                    "tool" => println!("[{}] tool: {} ({})", m["seq"], m["text"].as_str().unwrap_or(""),
                                       m["data"]["status"].as_str().unwrap_or("?")),
                    _ => println!("[{}] {}: {}", m["seq"], role, m["text"].as_str().unwrap_or("")),
                }
            }
            if v["has_more"] == json!(true) {
                println!("(older messages: --before {})", v["first_seq"]);
            }
            Ok(())
        }
        "handoff" => {
            let name = positional(&args, 0).context("usage: kurisu handoff <name>")?;
            let v = req(&paths, json!({"op": "handoff", "name": name})).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        "update" => {
            let name = positional(&args, 0).context("usage: kurisu update <name> --effort low --model M ...")?;
            let mut fields = serde_json::Map::new();
            for f in ["task", "description", "model", "effort", "role", "lifecycle", "name", "harness"] {
                if let Some(v) = flag(&args, &format!("--{f}")) {
                    fields.insert(f.into(), json!(v));
                }
            }
            if let Some(v) = flag(&args, "--mode") {
                fields.insert("permission_mode".into(), json!(v));
            }
            if fields.is_empty() {
                bail!("nothing to update: pass --model, --effort, --mode, --description, --task, --role, --lifecycle or --name");
            }
            let v = req(&paths, json!({"op": "update", "name": name, "fields": fields})).await?;
            if v.get("new").is_some() {
                let (o, n, m) = (&v["old"], &v["new"], &v["moved"]);
                println!("switched {}: {} ({} {}) archived -> new session {} ({} {})", o["name"].as_str().unwrap_or(""),
                         o["id"].as_str().unwrap_or(""), o["harness"].as_str().unwrap_or(""), o["model"].as_str().unwrap_or(""),
                         n["id"].as_str().unwrap_or(""), n["harness"].as_str().unwrap_or(""), n["model"].as_str().unwrap_or(""));
                println!("moved: {} children, {} wakes, {} MCP servers, {} rules; {}", m["children"], m["wakes"], m["mcp_servers"], m["rules"],
                         if v["seed"].is_null() { "not seeded" } else { "it is reading the old history now (see `ku history`)" });
                return Ok(());
            }
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
        "version" => {
            let v = server::call(&paths.socket(), json!({"op": "version"})).await?;
            if args.iter().any(|a| a == "--json") { println!("{}", v); }
            else { println!("{}", version::format(&v)); }
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
        "mcp" => {
            let sub = positional(&args, 0).context("usage: kurisu mcp <name> | kurisu mcp add|enable|disable|rm <name> <server>")?;
            if !["add", "enable", "disable", "rm"].contains(&sub.as_str()) {
                let v = req(&paths, json!({"op": "mcp_list", "name": sub})).await?;
                println!("  kurisu-mcp      on   (built in)");
                for s in v["servers"].as_array().into_iter().flatten() {
                    println!("  {:<15} {:<4} {}", s["name"].as_str().unwrap_or(""), if s["enabled"] == true { "on" } else { "off" }, s["config"]);
                }
                return Ok(());
            }
            let name = positional(&args, 1).context("session name")?;
            let server = positional(&args, 2).context("server name")?;
            let r = match sub.as_str() {
                "add" => {
                    let config = match (flag(&args, "--command"), flag(&args, "--url")) {
                        (Some(c), None) => {
                            let mut parts = c.split_whitespace().map(String::from);
                            json!({"command": parts.next().unwrap_or_default(), "args": parts.collect::<Vec<_>>()})
                        }
                        (None, Some(u)) => json!({"type": flag(&args, "--type").unwrap_or_else(|| "http".into()), "url": u}),
                        _ => bail!("pass --command \"cmd args...\" or --url URL"),
                    };
                    req(&paths, json!({"op": "mcp_set", "name": name, "server": server, "config": config})).await?
                }
                "rm" => req(&paths, json!({"op": "mcp_remove", "name": name, "server": server})).await?,
                on => req(&paths, json!({"op": "mcp_set", "name": name, "server": server, "enabled": on == "enable"})).await?,
            };
            println!("{} {}: {}", name, server, if r["changed"] == true { "done (applies from its next turn)" } else { "nothing changed" });
            Ok(())
        }
        "compact" => {
            let name = positional(&args, 0).context("usage: kurisu compact <name>")?;
            let v = req(&paths, json!({"op": "compact", "name": name})).await?;
            if v["pre_tokens"].is_null() && v["post_tokens_estimate"].is_null() {
                println!("{} compaction completed; upstream reported no token reduction estimate",v["name"].as_str().unwrap_or(""));
            } else {
                println!("{} compacted: {} tokens before, about {} after (summary {} chars)", v["name"].as_str().unwrap_or(""),
                         v["pre_tokens"], v["post_tokens_estimate"], v["summary"].as_str().map(str::len).unwrap_or(0));
            }
            Ok(())
        }
        "attach" => {
            let name = positional(&args, 0).context("usage: kurisu attach <name>")?;
            let st = store::Store::open(&paths.db())?;
            let sid = st.resolve_req(&name)?;
            let session=st.get(&sid)?.context("no session")?;
            anyhow::ensure!(session.harness=="claude", "attach is available only for Claude's tmux UI; use ku send/history for {} ACP sessions",session.harness);
            let id = session.harness_session_id.context("that session has not started yet")?;
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
                let pid = server::call(&paths.socket(), json!({"op": "ping"})).await.ok().and_then(|p| p["pid"].as_i64());
                server::call(&paths.socket(), json!({"op": "shutdown"})).await?;
                // Wait until it has really exited, so a `ku web` started right after cannot
                // attach to the stopping server (and exit with it).
                for _ in 0..300 {
                    let gone = pid.is_none_or(|p| !std::path::Path::new(&format!("/proc/{p}")).exists() && std::process::Command::new("kill").args(["-0", &p.to_string()]).stderr(std::process::Stdio::null()).status().map(|s| !s.success()).unwrap_or(true));
                    if gone && tokio::net::UnixStream::connect(paths.socket()).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                println!("kurisu server stopped (its CLI sessions are closed; transcripts are kept)");
            } else {
                println!("kurisu server is not running");
            }
            Ok(())
        }
        _ => {
            println!("{}", include_str!("main.rs").lines().take_while(|l| l.starts_with("//!")).map(|l| l.trim_start_matches("//!").strip_prefix(' ').unwrap_or("")).collect::<Vec<_>>().join("\n"));
            println!("\nkurisu {}\n{}", source::version_line(), source::rebuild_note());
            Ok(())
        }
    }
}

async fn send(paths: &Paths, name: &str, text: &str) -> Result<()> {
    let v = req(paths, json!({"op": "send", "name": name, "text": text})).await?;
    if let Some(n) = v["notice"].as_str() {
        eprintln!("note: {n}");
    }
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

#[derive(Debug, PartialEq)]
enum WebCmd {
    Link,
    Serve { host: String, port: u16, legacy: bool },
}

/// `ku web` defaults to the running instance's `/web` link; `--serve` keeps the
/// old standalone server. `--host`/`--port` without `--serve` still mean serve:
/// a launcher that passes a port wants a server, never a link.
fn web_cmd(args: &[String]) -> Result<WebCmd> {
    let serve = args.iter().any(|a| a == "--serve");
    let host = flag(args, "--host");
    let port = flag(args, "--port");
    if !serve && host.is_none() && port.is_none() {
        return Ok(WebCmd::Link);
    }
    let port: u16 = port.map(|p| p.parse()).transpose().context("--port")?.unwrap_or(8787);
    Ok(WebCmd::Serve { host: host.unwrap_or_else(|| "127.0.0.1".into()), port, legacy: !serve })
}

#[derive(Debug, PartialEq)]
struct WebLink {
    url: String,
    listen: String,
    tailscale: bool,
    expires_in: u64,
}

/// Ask the running instance for a one-time `/web` link. Never starts a server,
/// and asks for the direct listener URL instead of Telegram's public one.
async fn request_web_link(paths: &Paths) -> Result<WebLink> {
    let sock = paths.socket();
    if tokio::net::UnixStream::connect(&sock).await.is_err() {
        bail!("the Kurisu server is not running (no socket at {}). Start it with `ku serve`, then run `ku web` again.", sock.display());
    }
    let offer = server::call(&sock, json!({"op": "remote_offer", "kind": "web"})).await?;
    if offer["enabled"] != json!(true) {
        let place = offer["where"].as_str().map(|w| format!(" It would listen on {w}.")).unwrap_or_default();
        bail!("the /web chat UI is off.{place} Turn it on with `ku web-link enable`, or send /web in Telegram.");
    }
    let v = server::call(&sock, json!({"op": "remote_link", "kind": "web", "direct": true})).await?;
    Ok(WebLink {
        url: v["url"].as_str().context("remote_link returned no URL")?.to_string(),
        listen: v["listen"].as_str().unwrap_or("").to_string(),
        tailscale: v["tailscale"] == json!(true),
        expires_in: v["expires_in"].as_u64().unwrap_or(remote::LINK_TTL.as_secs()),
    })
}

async fn web_link(paths: &Paths) -> Result<()> {
    let w = request_web_link(paths).await?;
    println!("Kurisu web chat: {}  (one-time link, expires in {} min)", w.url, w.expires_in / 60);
    if let Some(ts) = tailscale_hint(&w.url, &w.listen, w.tailscale) {
        println!("Tailscale address (phone): {ts}");
    }
    if open_url(&w.url) {
        println!("Opening it in your browser…");
    } else {
        println!("Open the URL above in a browser (no display or browser opener here).");
    }
    Ok(())
}

/// The phone-facing Tailscale URL, when it adds something: the same link on the
/// listener address. `ku web` already prints that address as the main URL
/// (direct links use the listener), so this returns `None` when they match
/// instead of printing the same URL twice.
fn tailscale_hint(url: &str, listen: &str, tailscale: bool) -> Option<String> {
    if !tailscale || listen.is_empty() {
        return None;
    }
    let rest = url.find("://")
        .and_then(|i| url[i + 3..].find('/').map(|j| &url[i + 3 + j..]))
        .unwrap_or("/");
    let ts = format!("http://{listen}{rest}");
    (ts != url).then_some(ts)
}

/// The opener command, from `$BROWSER` or the platform default. `None` means
/// headless: print the URL and do not try. Parameters keep tests display-free.
fn browser_argv(os: &str, browser: Option<&str>, has_display: bool) -> Option<Vec<String>> {
    if let Some(b) = browser.map(str::trim).filter(|b| !b.is_empty()) {
        let argv: Vec<String> = b.split_whitespace().map(String::from).collect();
        if !argv.is_empty() {
            return Some(argv);
        }
    }
    match os {
        "linux" if has_display => Some(vec!["xdg-open".into()]),
        "macos" => Some(vec!["open".into()]),
        _ => None,
    }
}

/// Put the URL at a `%s` placeholder when the command has one, else append it.
fn with_url(mut argv: Vec<String>, url: &str) -> Vec<String> {
    match argv.iter().position(|a| a.contains("%s")) {
        Some(i) => argv[i] = argv[i].replace("%s", url),
        None => argv.push(url.to_string()),
    }
    argv
}

/// Best effort: a missing browser or a headless box just prints the URL.
fn open_url(url: &str) -> bool {
    let browser = std::env::var("BROWSER").ok();
    let has_display = std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    let Some(argv) = browser_argv(std::env::consts::OS, browser.as_deref(), has_display) else { return false };
    let argv = with_url(argv, url);
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
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

fn ago(t: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let d = (now - t).max(0);
    if d < 90 { format!("{d}s") } else if d < 5400 { format!("{}m", d / 60) } else { format!("{}h", d / 3600) }
}

fn print_approvals(v: &Value) {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("  (no pending approvals)");
        return;
    }
    println!("  {:<8} {:<12} {:<9} {:<6} {:<14} {}", "ID", "SESSION", "STATUS", "AGE", "BY", "REQUEST");
    for a in rows {
        let pending = a["status"] == "pending";
        let left = a["expires_at"].as_i64().unwrap_or(0) - std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let by = if pending { format!("expires {}s", left.max(0)) } else { a["decided_by"].as_str().unwrap_or("").to_string() };
        let shown = if a["kind"] == "question" { a["preview"].as_str() } else { a["title"].as_str() };
        let title: String = shown.unwrap_or("").chars().take(90).collect();
        println!("  {:<8} {:<12} {:<9} {:<6} {:<14} {}", a["id"].as_str().unwrap_or(""), a["session_name"].as_str().unwrap_or(""),
                 a["status"].as_str().unwrap_or(""), ago(a["created_at"].as_i64().unwrap_or(0)), by, title);
    }
}

fn print_rules(v: &Value) {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("  (no rules: every permission prompt goes to you)");
        return;
    }
    println!("  {:<4} {:<6} {:<16} {:<20} {:<7} {}", "ID", "RULE", "APPLIES TO", "TOOL", "IN-CWD", "COMMAND PREFIX");
    for r in rows {
        let to = format!("{} {}", r["scope"].as_str().unwrap_or(""), r["target_name"].as_str().or(r["target"].as_str()).unwrap_or(""));
        println!("  {:<4} {:<6} {:<16} {:<20} {:<7} {}", r["id"].as_i64().unwrap_or(0), r["decision"].as_str().unwrap_or(""), to, r["tool"].as_str().unwrap_or(""),
                 if r["in_cwd"] == true { "yes" } else { "-" }, r["command_prefix"].as_str().unwrap_or("-"));
    }
}

fn print_limits(v: &Value) {
    let rl = &v["rate_limits"];
    if rl.is_null() {
        println!("  plan limits: unavailable ({})", v["note"].as_str().unwrap_or(""));
        return;
    }
    for (k, label) in [("five_hour", "5-hour"), ("seven_day", "7-day")] {
        let w = &rl[k];
        if let Some(p) = w["used_percentage"].as_f64() {
            let left = w["resets_at"].as_i64().map(|t| t - std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
            let reset = left.map(|s| format!(", resets in {}h{:02}m", s.max(0) / 3600, s.max(0) % 3600 / 60)).unwrap_or_default();
            println!("  {label} limit: {p:.0}% used{reset}");
        }
    }
    println!("  (from Claude Code's status line, captured {} ago)\n", ago(rl["captured_at"].as_i64().unwrap_or(0)));
}

fn print_usage(v: &Value) {
    print_limits(v);
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
            if s["lifecycle"] != "active" { cut(&s["lifecycle"], 8) } else if s["status"] == "awaiting_approval" { "APPROVAL".into() } else { cut(&s["status"], 8) },
            cut(&s["description"], 30), cut(&s["note"], 40)
        );
    }
}

async fn status(paths: &Paths) -> Result<()> {
    println!("Kurisu {}\n  source: {} commit {} (rebuild: ku source)\n", source::version_line(), source::REPO, source::COMMIT);
    println!("Host CLIs");
    let browser_line = {
        let running = if tokio::net::UnixStream::connect(paths.socket()).await.is_ok() {
            server::call(&paths.socket(), json!({"op": "browser_status"})).await.ok().map(|v| v["running"] == json!(true))
        } else {
            None
        };
        browser::status_line(&paths.home, running)
    };
    for c in [hostcheck::claude().await, hostcheck::codex().await, hostcheck::opencode().await] {
        println!("  {c}");
    }
    println!("  {browser_line}");
    println!("\nKurisu server ({})", paths.home.display());
    if tokio::net::UnixStream::connect(paths.socket()).await.is_ok() {
        let p = server::call(&paths.socket(), json!({"op": "ping"})).await?;
        println!("  running: pid {}  rss {} kB", p["pid"], p["rss_kb"]);
        if p["build"].is_object() { println!("{}", version::format(&p["build"])); }
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
        "opencode" => {
            let model = std::env::var("KURISU_OPENCODE_SMOKE_MODEL").context("set KURISU_OPENCODE_SMOKE_MODEL to an explicitly verified free model")?;
            let d = opencode::OpenCodeDriver::spawn().await?;
            let id = d.new_session(cwd.clone()).await?;
            d.configure(&id, &model, None, None).await?;
            let r1 = ask(&d,&id,q1).await?;
            d.close(&id).await?;
            drop(d);
            let d = opencode::OpenCodeDriver::spawn().await?;
            d.load_session(&id,cwd).await?;
            d.configure(&id,&model,None,None).await?;
            let r2 = ask(&d,&id,q2).await?;
            println!("reply1={r1:?} reply2={r2:?}");
            anyhow::ensure!(r1.trim()=="PONG" && r2.contains("PONG"),"smoke failed");
        }
        h => bail!("unknown harness {h}"),
    }
    println!("SMOKE OK ({harness})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn args(rest: &[&str]) -> Vec<String> {
        ["ku", "web"].iter().chain(rest).map(|s| s.to_string()).collect()
    }

    #[test]
    fn web_cmd_flags_imply_serving() {
        // Bare `ku web` still links.
        assert_eq!(web_cmd(&args(&[])).unwrap(), WebCmd::Link);
        // Explicit serve keeps its old defaults.
        assert_eq!(web_cmd(&args(&["--serve"])).unwrap(),
                   WebCmd::Serve { host: "127.0.0.1".into(), port: 8787, legacy: false });
        // A port or host without --serve is the legacy serving form.
        assert_eq!(web_cmd(&args(&["--port", "9000"])).unwrap(),
                   WebCmd::Serve { host: "127.0.0.1".into(), port: 9000, legacy: true });
        assert_eq!(web_cmd(&args(&["--host", "0.0.0.0"])).unwrap(),
                   WebCmd::Serve { host: "0.0.0.0".into(), port: 8787, legacy: true });
        assert_eq!(web_cmd(&args(&["--host", "0.0.0.0", "--port", "9000"])).unwrap(),
                   WebCmd::Serve { host: "0.0.0.0".into(), port: 9000, legacy: true });
        // --serve is authoritative even when flags come before it.
        assert_eq!(web_cmd(&args(&["--port", "9000", "--serve"])).unwrap(),
                   WebCmd::Serve { host: "127.0.0.1".into(), port: 9000, legacy: false });
        assert_eq!(web_cmd(&args(&["--serve", "--host", "0.0.0.0", "--port", "9000"])).unwrap(),
                   WebCmd::Serve { host: "0.0.0.0".into(), port: 9000, legacy: false });
    }

    #[tokio::test]
    async fn legacy_port_form_serves_the_http_api() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().join("home") };
        // `ku web --port N` must route to serving, not to link mode.
        let WebCmd::Serve { host, port, legacy } = web_cmd(&args(&["--port", "0"])).unwrap() else {
            panic!("--port must imply serving");
        };
        assert!(legacy);
        let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await.unwrap();
        let bound = listener.local_addr().unwrap();
        let token = "test-token-0123456789";
        let app = crate::web::app(paths, bound, token.into());
        let server = tokio::spawn(async move { let _ = axum::serve(listener, app).await; });
        let head = http_head(bound, &format!("/api/v1/stream?token={token}")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.to_lowercase().contains("content-type: text/event-stream"), "{head}");
        let denied = http_head(bound, "/api/v1/status").await;
        assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
        server.abort();
    }

    async fn http_head(bound: std::net::SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(bound).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: {bound}\r\nAccept: text/event-stream\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = [0u8; 2048];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), s.read(&mut buf)).await
            .expect("the API did not answer").unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    #[test]
    fn browser_opener_respects_browser_and_headless() {
        assert_eq!(browser_argv("linux", Some("firefox --new-tab"), false), Some(vec!["firefox".into(), "--new-tab".into()]));
        assert_eq!(browser_argv("linux", None, true), Some(vec!["xdg-open".into()]));
        assert_eq!(browser_argv("linux", None, false), None, "headless: print the URL");
        assert_eq!(browser_argv("macos", None, false), Some(vec!["open".into()]));
        assert_eq!(browser_argv("linux", Some("   "), true), Some(vec!["xdg-open".into()]));
        assert_eq!(with_url(vec!["firefox".to_string()], "http://x/y"), vec!["firefox".to_string(), "http://x/y".to_string()]);
        assert_eq!(with_url(vec!["browser".to_string(), "%s".to_string()], "http://x/y"), vec!["browser".to_string(), "http://x/y".to_string()]);
    }

    type Reply = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

    /// A Unix-socket stand-in for the server: records every op and replies from
    /// `reply`. No TCP listener, so no real web port is bound.
    async fn fake_socket(paths: &Paths, reply: Reply) -> (Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        std::fs::create_dir_all(&paths.home).unwrap();
        let listener = crate::paths::bind_private(&paths.socket()).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let h = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let seen = seen2.clone();
                let reply = reply.clone();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    let Ok(Some(line)) = lines.next_line().await else { return };
                    let req: Value = serde_json::from_str(&line).unwrap();
                    seen.lock().unwrap().push(req.clone());
                    let mut out = serde_json::to_vec(&json!({"ok": reply(&req)})).unwrap();
                    out.push(b'\n');
                    let _ = w.write_all(&out).await;
                });
            }
        });
        (seen, h)
    }

    #[test]
    fn tailscale_line_is_skipped_when_it_repeats_the_main_url() {
        let direct = "http://100.118.224.35:8791/_web/r/tok";
        // Direct links already use the listener address: no second line.
        assert_eq!(tailscale_hint(direct, "100.118.224.35:8791", true), None);
        assert_eq!(tailscale_hint(direct, "100.118.224.35:8791", false), None);
        // A different main URL still gets the Tailscale version.
        assert_eq!(tailscale_hint("http://127.0.0.1:8791/_web/r/tok", "100.118.224.35:8791", true),
                   Some("http://100.118.224.35:8791/_web/r/tok".into()));
        assert_eq!(tailscale_hint("http://box.tail.ts.net:8791/r/abc", "100.118.224.35:8791", true),
                   Some("http://100.118.224.35:8791/r/abc".into()));
    }

    #[tokio::test]
    async fn web_link_asks_for_the_direct_url() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().join("home") };
        let reply: Reply = Arc::new(|req: &Value| match req["op"].as_str() {
            Some("remote_offer") => json!({"enabled": true}),
            Some("remote_link") => json!({"url": "http://100.118.224.35:8791/_web/r/tok",
                                          "listen": "100.118.224.35:8791", "tailscale": true, "expires_in": 300}),
            _ => json!({}),
        });
        let (seen, h) = fake_socket(&paths, reply).await;
        let link = request_web_link(&paths).await.unwrap();
        assert_eq!(link.url, "http://100.118.224.35:8791/_web/r/tok");
        assert_eq!(link.listen, "100.118.224.35:8791");
        assert!(link.tailscale);
        let seen = seen.lock().unwrap();
        let req = seen.iter().find(|r| r["op"] == "remote_link").expect("remote_link op");
        assert_eq!(req["kind"], "web");
        assert_eq!(req["direct"], json!(true), "the CLI link must not use the public tunnel URL");
        h.abort();
    }

    #[tokio::test]
    async fn web_without_a_server_says_how_to_start_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().join("home") };
        let err = request_web_link(&paths).await.unwrap_err().to_string();
        assert!(err.contains("server is not running"), "{err}");
        assert!(err.contains("ku serve"), "{err}");
        assert!(!paths.socket().exists(), "ku web must not silently start a server");
    }

    #[tokio::test]
    async fn web_link_explains_when_the_web_ui_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().join("home") };
        let reply: Reply = Arc::new(|req: &Value| match req["op"].as_str() {
            Some("remote_offer") => json!({"enabled": false, "where": "100.118.224.35:8797 (Tailscale)"}),
            _ => json!({}),
        });
        let (_, h) = fake_socket(&paths, reply).await;
        let err = request_web_link(&paths).await.unwrap_err().to_string();
        assert!(err.contains("/web chat UI is off"), "{err}");
        assert!(err.contains("ku web-link enable"), "{err}");
        assert!(err.contains("8797"), "{err}");
        h.abort();
    }
}
