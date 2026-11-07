//! `ku import omnigent`: move sessions from Omnigent into Kurisu.
//!
//! Reads only a snapshot of Omnigent's `chat.db` (SQLite online backup from a
//! read-only connection; the live database is never written and Omnigent keeps
//! running). For each selected session it creates a Kurisu session (same name,
//! folder, model, effort, harness, parent), imports the full history (messages,
//! tool calls with their output, errors, compaction summaries) with the original
//! times, and for Claude sessions copies the Claude transcript under a NEW session
//! id, which Kurisu resumes: the live context comes along, and Omnigent and Kurisu
//! never write the same transcript file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};

use crate::paths::Paths;

const SOURCE: &str = "omnigent";
const MAX_TOOL_CHARS: usize = 8000;

struct Opts {
    all: bool,
    sessions: Vec<String>,
    roster: PathBuf,
    db: PathBuf,
    dry_run: bool,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Clone)]
struct Conv {
    id: String, // 32 hex
    title: String,
    parent: Option<String>,
    archived: bool,
    model: Option<String>,
    effort: Option<String>,
    agent: String,
    workspace: Option<String>,
    claude_session: Option<String>,
}

#[derive(Debug)]
struct Plan {
    conv: Conv,
    name: String,
    harness: Option<&'static str>,
    role: &'static str,
    cwd: Option<String>,
    owns: String,
    transcript: Option<PathBuf>,
    items: usize,
    status: String,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn dashed(h: &str) -> String {
    uuid::Uuid::parse_str(h).map(|u| u.to_string()).unwrap_or_else(|_| h.to_string())
}

/// A Kurisu session name from a title: "Telegram Bridge Dev" -> "telegram-bridge-dev".
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').chars().take(48).collect::<String>().trim_end_matches('-').to_string();
    if out.is_empty() { "imported".into() } else { out }
}

/// Claude Code's project folder name for a working directory.
pub fn claude_project_dir(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn harness_of(agent: &str) -> Option<&'static str> {
    match agent {
        a if a.starts_with("claude") => Some("claude"),
        a if a.starts_with("codex") => Some("codex"),
        _ => None,
    }
}

/// Roster rows `| **Name** | `hexid` | `dir` ... | owns |` from Omni Lead's AGENTS.md.
pub fn parse_roster(text: &str) -> Vec<(String, String, String, String)> {
    let mut out = vec![];
    for line in text.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 5 {
            continue;
        }
        let name = cells[1].trim_matches('*').trim();
        let id = cells[2].trim_matches('`').trim();
        if id.len() != 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let dir = cells[3].split('`').nth(1).unwrap_or("").to_string();
        out.push((name.to_string(), id.to_lowercase(), dir, cells[4].to_string()));
    }
    out
}

/// Snapshot the live chat.db (online backup, read-only source).
fn snapshot(db: &Path, dest_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir)?;
    let dest = dest_dir.join(format!("omnigent-chat-{}.db", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let src = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("open {} read-only", db.display()))?;
    let mut dst = Connection::open(&dest)?;
    {
        let b = rusqlite::backup::Backup::new(&src, &mut dst)?;
        b.run_to_completion(256, std::time::Duration::from_millis(5), None)?;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).ok();
    Ok(dest)
}

fn load_convs(c: &Connection) -> Result<Vec<Conv>> {
    let mut st = c.prepare(
        "SELECT c.id, c.title, c.parent_conversation_id, c.archived, c.session_overrides, m.external_session_id, m.workspace, a.name \
         FROM conversations c LEFT JOIN omnigent_conversation_metadata m ON m.id = c.id LEFT JOIN agents a ON a.id = c.agent_id")?;
    let rows = st.query_map([], |r| {
        let id: Vec<u8> = r.get(0)?;
        let parent: Option<Vec<u8>> = r.get(2)?;
        let ov: Option<String> = r.get(4)?;
        let ext: Option<String> = match r.get::<_, rusqlite::types::Value>(5)? {
            rusqlite::types::Value::Blob(b) if b.len() == 16 => Some(dashed(&hex(&b))),
            rusqlite::types::Value::Text(t) if !t.is_empty() => Some(t),
            _ => None,
        };
        Ok((hex(&id), r.get::<_, Option<String>>(1)?.unwrap_or_default(), parent.map(|p| hex(&p)), r.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0,
            ov, ext, r.get::<_, Option<String>>(6)?, r.get::<_, Option<String>>(7)?.unwrap_or_default()))
    })?;
    let mut out = vec![];
    for row in rows {
        let (id, title, parent, archived, ov, ext, workspace, agent) = row?;
        let ov: Value = ov.as_deref().and_then(|o| serde_json::from_str(o).ok()).unwrap_or(json!({}));
        let model = ov["model_override"].as_str().or(ov["reported_model"].as_str()).map(String::from);
        let claude_session = ext.filter(|e| uuid::Uuid::parse_str(e).is_ok());
        out.push(Conv { id, title, parent, archived, model, effort: ov["reasoning_effort"].as_str().map(String::from), agent, workspace, claude_session });
    }
    Ok(out)
}

fn item_counts(c: &Connection, id: &str) -> Result<usize> {
    let b = uuid::Uuid::parse_str(id)?.as_bytes().to_vec();
    Ok(c.query_row("SELECT COUNT(*) FROM conversation_items WHERE conversation_id=?1 AND type IN (1,2,5,6)", params![b], |r| r.get::<_, i64>(0))? as usize)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| match b["type"].as_str() {
            Some("input_image") => Some("[image]".to_string()),
            Some("input_file") => Some(format!("[file {}]", b["filename"].as_str().unwrap_or(""))),
            _ => b["text"].as_str().map(String::from),
        }).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn clip(s: &str) -> Value {
    if s.chars().count() <= MAX_TOOL_CHARS {
        return json!(s);
    }
    let cut: String = s.chars().take(MAX_TOOL_CHARS).collect();
    json!(format!("{cut}… [{} more characters]", s.chars().count() - MAX_TOOL_CHARS))
}

/// Omnigent items as Kurisu messages: user/agent text, tool calls with output, errors,
/// compaction summaries. Reasoning, runtime events and slash commands are left out.
fn messages(c: &Connection, id: &str) -> Result<Vec<Value>> {
    let b = uuid::Uuid::parse_str(id)?.as_bytes().to_vec();
    let mut st = c.prepare("SELECT type, data, created_at FROM conversation_items WHERE conversation_id=?1 ORDER BY position")?;
    let rows = st.query_map(params![b], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, Option<i64>>(2)?.unwrap_or(0))))?;
    let mut out: Vec<Value> = vec![];
    let mut calls: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let (ty, data, at) = row?;
        let Ok(d) = serde_json::from_str::<Value>(&data) else { continue };
        match ty {
            1 => {
                let text = text_of(&d["content"]);
                if text.trim().is_empty() {
                    continue;
                }
                let role = if d["role"] == "assistant" { "agent" } else { "user" };
                out.push(json!({"role": role, "text": text, "at": at}));
            }
            2 => {
                let name = d["name"].as_str().unwrap_or("tool").to_string();
                let args = d["arguments"].as_str().and_then(|a| serde_json::from_str::<Value>(a).ok()).unwrap_or_else(|| d["arguments"].clone());
                let call_id = d["call_id"].as_str().unwrap_or("").to_string();
                let title = claude_cli_acp::permission_title(&name, &args);
                let input = clip(&args.to_string());
                let input = serde_json::from_str::<Value>(input.as_str().unwrap_or("")).unwrap_or(input);
                calls.insert(call_id.clone(), out.len());
                out.push(json!({"role": "tool", "text": title, "at": at,
                                "data": {"tool_call_id": call_id, "tool": name, "status": "running", "input": input}}));
            }
            3 => {
                let call_id = d["call_id"].as_str().unwrap_or("");
                if let Some(i) = calls.get(call_id) {
                    let output = match &d["output"] {
                        Value::String(s) => clip(s),
                        other => clip(&other.to_string()),
                    };
                    out[*i]["data"]["status"] = json!("completed");
                    out[*i]["data"]["output"] = output;
                }
            }
            5 => {
                let msg = d["message"].as_str().unwrap_or("error");
                out.push(json!({"role": "error", "text": format!("(Omnigent) {msg}"), "at": at}));
            }
            6 => {
                if let Some(s) = d["summary"].as_str().filter(|s| !s.trim().is_empty()) {
                    out.push(json!({"role": "compaction", "text": s, "at": at}));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn claude_home() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude"))
}

fn find_transcript(uuid: &str) -> Option<PathBuf> {
    let projects = claude_home().join("projects");
    for e in std::fs::read_dir(projects).ok()?.flatten() {
        let p = e.path().join(format!("{uuid}.jsonl"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Copy a Claude transcript to a new session id in `cwd`'s project folder (the id
/// inside the file is rewritten too). Returns the new id and path.
fn copy_transcript(src: &Path, old: &str, cwd: &str) -> Result<(String, PathBuf)> {
    let new = uuid::Uuid::new_v4().to_string();
    let dir = claude_home().join("projects").join(claude_project_dir(cwd));
    std::fs::create_dir_all(&dir)?;
    let text = std::fs::read_to_string(src).with_context(|| format!("read {}", src.display()))?;
    let dest = dir.join(format!("{new}.jsonl"));
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&dest)?;
    f.write_all(text.replace(old, &new).as_bytes())?;
    Ok((new, dest))
}

fn parse_opts(args: &[String], home: &Path) -> Result<Opts> {
    let omni = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut o = Opts { all: false, sessions: vec![], roster: omni.join("omni-lead/AGENTS.md"), db: omni.join(".omnigent/chat.db"),
                       dry_run: false, model: None, effort: None };
    let _ = home;
    let mut i = 3;
    while i < args.len() {
        let v = || args.get(i + 1).cloned().ok_or_else(|| anyhow!("{} needs a value", args[i]));
        match args[i].as_str() {
            "--all" => o.all = true,
            "--dry-run" => o.dry_run = true,
            "--session" => {
                o.sessions.push(v()?.replace('-', "").to_lowercase());
                i += 1;
            }
            "--roster" => {
                o.roster = PathBuf::from(v()?);
                i += 1;
            }
            "--db" => {
                o.db = PathBuf::from(v()?);
                i += 1;
            }
            "--model" => {
                o.model = Some(v()?);
                i += 1;
            }
            "--effort" => {
                o.effort = Some(v()?);
                i += 1;
            }
            other => bail!("unknown option {other}"),
        }
        i += 1;
    }
    Ok(o)
}

pub async fn cli(paths: &Paths, args: &[String]) -> Result<()> {
    if args.get(2).map(String::as_str) != Some("omnigent") {
        bail!("usage: kurisu import omnigent [--session ID]... [--all] [--roster AGENTS.md] [--db chat.db] [--model M] [--effort E] [--dry-run]");
    }
    let o = parse_opts(args, &paths.home)?;
    let snap = snapshot(&o.db, &paths.home.join("import"))?;
    println!("snapshot: {} (the live database was only read)", snap.display());
    let c = Connection::open_with_flags(&snap, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let convs = load_convs(&c)?;
    let by_id: HashMap<String, Conv> = convs.iter().map(|x| (x.id.clone(), x.clone())).collect();
    let omni_home = o.roster.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default();
    let roster = std::fs::read_to_string(&o.roster).map(|t| parse_roster(&t)).unwrap_or_default();
    // Selection: explicit ids, else everything (--all), else the roster.
    let mut selected: Vec<(String, Option<(String, String, String)>)> = vec![];
    if !o.sessions.is_empty() {
        for s in &o.sessions {
            let id = by_id.keys().find(|k| k.starts_with(s.as_str())).cloned().ok_or_else(|| anyhow!("no Omnigent session {s}"))?;
            let r = roster.iter().find(|r| r.1 == id).map(|r| (r.0.clone(), r.2.clone(), r.3.clone()));
            selected.push((id, r));
        }
    } else if o.all {
        for x in convs.iter().filter(|x| !x.archived) {
            let r = roster.iter().find(|r| r.1 == x.id).map(|r| (r.0.clone(), r.2.clone(), r.3.clone()));
            selected.push((x.id.clone(), r));
        }
    } else {
        if roster.is_empty() {
            bail!("no roster found at {} (use --session ID or --all)", o.roster.display());
        }
        for r in &roster {
            selected.push((r.1.clone(), Some((r.0.clone(), r.2.clone(), r.3.clone()))));
        }
    }
    // The Lead goes first so the others can name it as their parent.
    let is_lead = |c: &Conv, r: &Option<(String, String, String)>| r.as_ref().is_some_and(|r| r.2.to_lowercase().contains("lead/orchestrator")) || c.title == "Omni Lead";
    selected.sort_by_key(|(id, r)| !by_id.get(id).is_some_and(|c| is_lead(c, r)));

    let mut plans = vec![];
    for (id, r) in &selected {
        let Some(conv) = by_id.get(id).cloned() else {
            println!("skip {id}: not in Omnigent's database");
            continue;
        };
        let harness = harness_of(&conv.agent);
        let cwd = conv.workspace.clone().filter(|w| w.starts_with('/'))
            .or_else(|| r.as_ref().map(|r| omni_home.join(&r.1).to_string_lossy().into_owned()).filter(|_| r.as_ref().is_some_and(|r| !r.1.is_empty())));
        let transcript = if harness == Some("claude") { conv.claude_session.as_deref().and_then(find_transcript) } else { None };
        let mut name = slug(r.as_ref().map(|r| r.0.as_str()).unwrap_or(&conv.title));
        let role = if is_lead(&conv, r) { "lead" } else { "worker" };
        let look = crate::req(paths, json!({"op": "import_lookup", "source": SOURCE, "source_id": conv.id, "new_name": name})).await?;
        let status = if let Some(s) = look["session"].as_object() {
            format!("already imported as {} ({})", s["name"].as_str().unwrap_or(""), &s["id"].as_str().unwrap_or("")[..8])
        } else if harness.is_none() {
            format!("skip: harness '{}' has no Kurisu driver (claude and codex only)", conv.agent)
        } else if role == "lead" && look["lead"].is_object() {
            format!("skip: Kurisu already has a Lead ({}); the others become its workers", look["lead"]["name"].as_str().unwrap_or("?"))
        } else {
            if look["name_taken"] == json!(true) {
                name = format!("{name}-omni");
            }
            "import".into()
        };
        let items = item_counts(&c, &conv.id)?;
        plans.push(Plan { conv, name, harness, role, cwd, owns: r.as_ref().map(|r| r.2.clone()).unwrap_or_default(), transcript, items, status });
    }

    println!("\n{:<24} {:<9} {:<7} {:<22} {:<7} {:>6}  {:<10} folder / plan", "session", "omnigent", "harness", "model / effort", "role", "items", "context");
    for p in &plans {
        let model = o.model.clone().or(p.conv.model.clone()).unwrap_or_else(|| "default".into());
        let effort = o.effort.clone().or(p.conv.effort.clone()).unwrap_or_else(|| "-".into());
        let ctx = match (&p.harness, &p.transcript) {
            (Some("claude"), Some(_)) => "resume",
            (Some("claude"), None) => "history",
            (Some("codex"), _) => "history",
            _ => "-",
        };
        println!("{:<24} {:<9} {:<7} {:<22} {:<7} {:>6}  {:<10} {}", p.name, &p.conv.id[..8], p.harness.unwrap_or("-"), format!("{model} / {effort}"),
                 p.role, p.items, ctx, p.cwd.as_deref().unwrap_or("(Kurisu workspace)"));
        println!("{:<24} {}", "", p.status);
    }
    if o.dry_run {
        drop(c);
        remove_snapshot(&snap);
        println!("\ndry run: nothing was changed. Claude sessions marked 'resume' get a copy of their transcript under a new id (Kurisu resumes the copy; Omnigent keeps its own).");
        return Ok(());
    }

    // Parent: the Kurisu Lead (an existing one, or the imported Omni Lead).
    let mut lead_id: Option<String> = crate::req(paths, json!({"op": "import_lookup", "source": SOURCE, "source_id": "-"})).await?["lead"]["id"].as_str().map(String::from);
    let mut imported: HashMap<String, String> = HashMap::new();
    for p in plans.iter().filter(|p| p.status == "import") {
        let parent = if p.role == "lead" {
            None
        } else {
            p.conv.parent.as_ref().and_then(|x| imported.get(x).cloned()).or(lead_id.clone())
        };
        let mut req = json!({"op": "import_session", "source": SOURCE, "source_id": p.conv.id, "title": p.conv.title, "new_name": p.name,
                             "harness": p.harness, "role": p.role, "cwd": p.cwd, "created_by": parent,
                             "model": o.model.clone().or(p.conv.model.clone()), "effort": o.effort.clone().or(p.conv.effort.clone()),
                             "description": if p.owns.is_empty() { format!("Imported from Omnigent: {}", p.conv.title) } else { format!("{} (imported from Omnigent: {})", p.owns, p.conv.title) },
                             "messages": messages(&c, &p.conv.id)?});
        let mut copied: Option<PathBuf> = None;
        if let (Some(t), Some(old), Some(cwd)) = (&p.transcript, &p.conv.claude_session, &p.cwd) {
            let (new, path) = copy_transcript(t, old, cwd)?;
            req["harness_session_id"] = json!(new);
            copied = Some(path);
        }
        match crate::req(paths, req).await {
            Ok(r) => {
                println!("imported {} -> {} ({}): {} messages{}", p.conv.title, r["name"].as_str().unwrap_or(""), &r["id"].as_str().unwrap_or("")[..8],
                         r["messages"], if copied.is_some() { ", resumes a copy of its Claude transcript" } else { "" });
                if let Some(id) = r["id"].as_str() {
                    imported.insert(p.conv.id.clone(), id.to_string());
                    if p.role == "lead" {
                        lead_id = Some(id.to_string());
                    }
                }
            }
            Err(e) => {
                if let Some(path) = copied {
                    let _ = std::fs::remove_file(path);
                }
                println!("FAILED {}: {e:#}", p.conv.title);
            }
        }
    }
    drop(c);
    remove_snapshot(&snap);
    Ok(())
}

fn remove_snapshot(snap: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", snap.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roster_and_project_dirs() {
        assert_eq!(slug("Telegram Bridge Dev"), "telegram-bridge-dev");
        assert_eq!(slug("Cojudge Dev (old, omni-lead cwd)"), "cojudge-dev-old-omni-lead-cwd");
        assert_eq!(slug("  ✨ "), "imported");
        assert_eq!(claude_project_dir("/run/media/deck/SDCARD/distrobox/omni/sales-cojudge"), "-run-media-deck-SDCARD-distrobox-omni-sales-cojudge");
        let r = parse_roster("| Name | ID | Working dir | Owns |\n|---|---|---|---|\n| **Omni Lead** | `bdb75c402e544dbb9fbe86226a9ee3f1` | `omni-lead` | this session: lead/orchestrator |\n| **IT Admin** | `942bac90f6174640a7dc84e24b17b682` | `omni-lead` (child of Omni Lead) | host |");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], ("Omni Lead".into(), "bdb75c402e544dbb9fbe86226a9ee3f1".into(), "omni-lead".into(), "this session: lead/orchestrator".into()));
        assert_eq!(r[1].2, "omni-lead");
    }

    #[test]
    fn items_become_kurisu_messages() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE conversation_items (conversation_id BLOB, position INT, type INT, data TEXT, created_at INT);").unwrap();
        let id = "94b68d608de44ca8acb24219608d1a3d";
        let b = uuid::Uuid::parse_str(id).unwrap().as_bytes().to_vec();
        let rows = [(1, r#"{"role":"user","content":[{"type":"input_text","text":"hi"}]}"#),
                    (4, r#"{"summary":[],"content":[{"type":"reasoning_text","text":"thinking"}]}"#),
                    (2, r#"{"name":"Bash","arguments":"{\"command\":\"ls\"}","call_id":"t1"}"#),
                    (3, r#"{"call_id":"t1","output":"a.txt"}"#),
                    (1, r#"{"role":"assistant","content":[{"type":"output_text","text":"done"}]}"#),
                    (5, r#"{"message":"boom"}"#),
                    (6, r#"{"summary":"SUMMARY"}"#),
                    (8, r#"{"event_type":"x"}"#)];
        for (i, (t, d)) in rows.iter().enumerate() {
            c.execute("INSERT INTO conversation_items VALUES (?1,?2,?3,?4,?5)", params![b, i as i64, t, d, 1000 + i as i64]).unwrap();
        }
        let m = messages(&c, id).unwrap();
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "tool", "agent", "error", "compaction"]);
        assert_eq!(m[1]["text"], "Bash: ls");
        assert_eq!((m[1]["data"]["status"].as_str(), m[1]["data"]["output"].as_str(), m[1]["data"]["input"]["command"].as_str()), (Some("completed"), Some("a.txt"), Some("ls")));
        assert_eq!(m[0]["at"], 1000);
    }

    #[test]
    fn transcript_copies_get_a_new_id() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir.path());
        let old = "d31bd8be-67c9-4341-a865-0efa07f48d5c";
        let src = dir.path().join("src.jsonl");
        std::fs::write(&src, format!("{{\"sessionId\":\"{old}\",\"cwd\":\"/w\"}}\n")).unwrap();
        let (new, path) = copy_transcript(&src, old, "/w/x").unwrap();
        assert_ne!(new, old);
        assert!(path.starts_with(dir.path().join("projects/-w-x")));
        let t = std::fs::read_to_string(&path).unwrap();
        assert!(t.contains(&new) && !t.contains(old));
        assert!(std::fs::read_to_string(&src).unwrap().contains(old), "the original is untouched");
    }
}
