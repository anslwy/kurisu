//! kurisu-mcp: the MCP server Kurisu injects into EVERY session it starts.
//!
//! It replaces a hand-maintained roster document with live data from the
//! store: a new or compacted session calls `peers_list` / `whoami` to discover
//! who exists and who it is. Identity comes from `KURISU_SESSION`, set per
//! session at launch, and every call is tagged with it, so messages are
//! attributed to their sender. Calls are forwarded to the Kurisu server over
//! its 0600 unix socket.
//!
//! The protocol is hand-rolled (initialize, tools/list, tools/call) to keep the
//! binary small; `rmcp` is the upgrade path.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const SERVER_NAME: &str = "kurisu-mcp";

pub fn tools() -> Value {
    let s = |props: Value, req: &[&str]| json!({"type": "object", "properties": props, "required": req});
    let str_ = |d: &str| json!({"type": "string", "description": d});
    let int_ = |d: &str| json!({"type": "integer", "description": d});
    json!([
        {"name": "whoami", "description": "Your own session UUID, name, role, description, model and folder.",
         "inputSchema": s(json!({}), &[])},
        {"name": "peers_list", "description": "Every session in this Kurisu team: UUID, name, role, description, harness, model, folder, status, its own status note and last activity. Use this instead of any roster document.",
         "inputSchema": s(json!({}), &[])},
        {"name": "peer_get", "description": "One session's details.", "inputSchema": s(json!({"id": str_("session UUID or name")}), &["id"])},
        {"name": "session_create", "description": "Lead only: create a worker peer session. Defaults: Claude with model claude-haiku-5-5, its own workspace folder. harness \"codex\" runs it on Codex; \"opencode\" uses the installed OpenCode ACP server (Codex/OpenCode use their configured model unless given).",
         "inputSchema": s(json!({"name": str_("lowercase slug, e.g. worker-1"), "description": str_("what this worker is for"),
                                 "role": str_("worker (default)"), "harness": str_("claude (default), codex or opencode (ACP)"),
                                 "permission_mode": str_("auto (default when supported) | default | acceptEdits | plan; OpenCode: default or plan"),
                                 "model": str_("Claude: defaults to claude-haiku-5-5; Codex/OpenCode: configured default"), "cwd": str_("absolute folder")}), &["name", "description"])},
        {"name": "session_send", "description": "Send a message to a peer. The peer sees it as coming from you. By default it waits for the reply; with wait:false it queues the message and returns at once, and when the peer's turn ends Kurisu sends you a [Kurisu callback] message (done, error, stopped or interrupted by a restart, with its reply, truncated); it is queued if you are busy. notify:false skips the callback. A turn cut off by a Kurisu restart is resumed automatically.",
         "inputSchema": s(json!({"to": str_("peer UUID or name"), "msg": str_("the message"),
                                 "wait": json!({"type": "boolean", "description": "wait for the reply (default true); false queues the message and returns at once"}),
                                 "notify": json!({"type": "boolean", "description": "with wait:false: send me a callback when the peer's turn ends (default true)"})}), &["to", "msg"])},
        {"name": "session_history", "description": "Read any session's history as Kurisu stores it (works for every harness): user messages, agent replies, peer messages, errors, compaction summaries and, with tools:true, tool calls with input/output. Newest page first; page back with before=<first_seq>, forward with after=<last_seq>; search finds text anywhere in the history.",
         "inputSchema": s(json!({"id": str_("session UUID or name"), "limit": int_("messages per page (default 30, max 200)"),
                                 "before": int_("only messages older than this seq (page backwards)"),
                                 "after": int_("only messages newer than this seq (page forwards)"),
                                 "search": str_("case-insensitive text to find"),
                                 "roles": str_("comma list: user, agent, peer, error, compaction, approval, tool"),
                                 "tools": json!({"type": "boolean", "description": "include tool calls (default false)"}),
                                 "max_chars": int_("shorten each message to this many characters")}), &["id"])},
        {"name": "session_handoff", "description": "Use when asked to look at another session and continue from it: who it is, its folder, its latest compaction summary and its newest messages and tool calls (within a budget), from Kurisu's own history. Its CLI context is not transferred; read this, then continue the work.",
         "inputSchema": s(json!({"id": str_("session UUID or name"), "max_chars": int_("size budget (default 24000)")}), &["id"])},
        {"name": "session_status", "description": "Status of one session.", "inputSchema": s(json!({"id": str_("session UUID or name")}), &["id"])},
        {"name": "session_update", "description": "Modify a session you created (or one created by a session you created, and so on): task, model, effort, description, role, name, lifecycle (active|paused|archived). Model/effort apply from its next turn. browser (on|off) turns the Live Browser (lb) on or off for it (off by default; also allowed on your own id); turn it on only when the task clearly needs a browser or the user asks. harness (claude|codex|opencode|agy, with optional model) switches it to another harness (your own id is allowed: the switch then happens right after your reply): it is archived and replaced by a new session with the same name and folder, which reads the old history via session_handoff; change harness on its own. You may also update yourself (your own id or name; after a harness switch your old id and the live same-name replacement are both you): task, status, description, model, effort, applied from your next turn; not your own lifecycle, role or name. Other non-ancestors are refused.",
         "inputSchema": s(json!({"id": str_("target session UUID or name"), "task": str_("new current task (shown as its status note)"),
                                 "model": str_("e.g. claude-haiku-5-5"), "effort": str_("Claude/Codex levels; agy: high|medium|low; OpenCode: model-specific ACP effort value or default"),
                                 "permission_mode": str_("auto (routine actions run, risky ones ask the user) | default (ask for most tools) | acceptEdits | plan; applies from its next start; OpenCode: default (upstream Build permissions) or plan"),
                                 "harness": str_("claude|codex|opencode|agy: switch harness (see description)"),
                                 "description": str_("new description"), "role": str_("worker"), "name": str_("new name (the UUID never changes)"),
                                 "lifecycle": str_("active|paused|archived")}), &["id"])},
        {"name": "set_my_description", "description": "Keep your own description current (what you are for).",
         "inputSchema": s(json!({"text": str_("one or two sentences")}), &["text"])},
        {"name": "set_my_status", "description": "Set your own short status / current task, visible to peers.",
         "inputSchema": s(json!({"text": str_("e.g. 'writing tests for X'")}), &["text"])},
        {"name": "board_post", "description": "Post to the team's shared board (standup notes, decisions, blockers).",
         "inputSchema": s(json!({"text": str_("the note")}), &["text"])},
        {"name": "wake_at", "description": "Schedule a message to wake yourself (default) or a session you created at a future time; it arrives as a normal message (queued if that session is busy). Use for reminders, follow-ups and daily jobs. Survives restarts; a wake missed while Kurisu was down is delivered once at startup, marked late.",
         "inputSchema": s(json!({"session": str_("'self' (default) or a session you created (UUID or name)"),
                                 "at": str_("when: 'in 30m', 'in 2h', '18:30' (next time that clock time comes), '2026-11-08 06:00' (local) or RFC 3339; optional with repeat"),
                                 "repeat": str_("optional 5-field cron, local time, e.g. '0 6 * * *' daily at 06:00, '30 9 * * 1-5' weekdays 09:30"),
                                 "message": str_("what the woken session should do; write it so it makes sense on its own later")}), &["message"])},
        {"name": "wake_list", "description": "Pending wakes for you and the sessions you created (all=true: also recent fired, failed and cancelled ones).",
         "inputSchema": s(json!({"session": str_("only this session (UUID, name or 'self')"),
                                 "all": json!({"type": "boolean", "description": "include past wakes"})}), &[])},
        {"name": "wake_cancel", "description": "Cancel a pending wake (yours, for you, or for a session you created).",
         "inputSchema": s(json!({"id": int_("the wake id from wake_at or wake_list")}), &["id"])},
        {"name": "transcribe", "description": "Transcribe an audio file (e.g. a voice message, .ogg/.mp3/.wav) to text with the user's local speech-to-text. Optional: works only if the user set it up (ku stt setup); otherwise it says so. Voice attachments sent to you are usually transcribed already.",
         "inputSchema": s(json!({"path": str_("audio file path (absolute, or relative to your folder)")}), &["path"])},
        // Keep this line short: OpenCode's code-mode catalogue is budget-limited and
        // lists the shortest entries per namespace, so a long description made
        // send_file invisible whenever lb was attached (9 of 21 tools shown).
        {"name": "send_file", "description": "Send a file to the user's paired chat.", "inputSchema": s(json!({"path": str_("file"), "caption": str_("caption")}), &["path"])},
        {"name": "version", "description": "Build identity and uptime of the running Kurisu server (queried over its socket, not the installed CLI).", "inputSchema": s(json!({}), &[])},
        {"name": "usage", "description": "Plan / rate-limit usage, the same report as Telegram's /usage: installed CLIs only (Claude: status-line plan limits; Codex: official app-server account quotas, used % and reset times; OpenCode: provider quota unavailable through ACP; unavailable when login or quota data is missing), plus the local cost estimate if claude-monitor is installed. Returns JSON with a one-line summary.", "inputSchema": s(json!({}), &[])},
        {"name": "ask_user", "description": "Ask the user a question and wait for the answer, which comes back as this tool's result (status answered + answer, or dismissed). Use it instead of asking in your reply when you need a decision to continue, above all when there are choices: the user taps one on their phone (or types/speaks a free-text answer). Workers created by another session get told to ask their creator instead.",
         "inputSchema": s(json!({"question": str_("the question"), "choices": json!({"type": "array", "items": {"type": "string"}, "description": "short option labels (optional; free text is always allowed)"}),
                                 "multi_select": json!({"type": "boolean", "description": "allow several choices"}), "header": str_("optional short label, e.g. Fruit")}), &["question"])},
        {"name": "board_read", "description": "Read the newest board posts.", "inputSchema": s(json!({"tail": int_("how many (default 20)")}), &[])},
        {"name": "telegram_history", "description": "Recent messages in your owner's Telegram DM with Kurisu, as Kurisu stored them (each page is oldest first; newest page by default). Use it to catch what the user sent while you were asleep, mid-turn, or when a reply failed to send: each message has its direction (user|agent), time, text, an attachment marker (photo/voice/file, plus the voice transcript when speech-to-text ran) and, for user messages, answered (false = no agent reply was ever sent for it; pair with unanswered:true). Limits: Telegram's Bot API cannot fetch arbitrary past messages, so this only covers what Kurisu itself saw in the owner's DM, from when its channel recorded it. Nothing older, nothing from groups or channels, and no other chat can be read.",
         "inputSchema": s(json!({"chat": str_("optional: the owner's chat id, or 'dm' (the default); other chats are refused"),
                                 "limit": int_("messages per page (default 20, max 200)"),
                                 "before": int_("only messages older than this seq (page backwards)"),
                                 "search": str_("case-insensitive text to find in the message or its attachment name"),
                                 "unanswered": json!({"type": "boolean", "description": "only user messages that never got an agent reply"})}), &[])}
    ])
}

/// Tools only the lead session gets (others never see them; the server refuses them too).
pub const LEAD_ONLY: &[&str] = &["ask_user"];

/// The tool list one session sees: the lead-only tools are left out for everyone else.
pub fn tools_for(lead: bool) -> Value {
    let all = tools().as_array().cloned().unwrap_or_default();
    json!(all.into_iter().filter(|t| lead || !LEAD_ONLY.contains(&t["name"].as_str().unwrap_or(""))).collect::<Vec<_>>())
}

/// Whether the calling session is the lead (asked live, so a role change applies at once).
async fn caller_is_lead(sock: &std::path::Path, caller: &str) -> bool {
    !caller.is_empty() && crate::server::call(sock, json!({"op": "status", "name": caller, "caller": caller})).await
        .is_ok_and(|s| s["role"] == "lead")
}

/// Map an MCP tool call to a server request, tagged with the caller's identity.
pub fn to_request(tool: &str, args: &Value, caller: &str) -> Option<Value> {
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let req = match tool {
        "whoami" => json!({"op": "status", "name": caller}),
        "peers_list" => json!({"op": "peers"}),
        "session_update" => {
            let mut fields = args.as_object().cloned().unwrap_or_default();
            fields.remove("id");
            json!({"op": "update", "name": a("id"), "fields": fields})
        }
        "peer_get" | "session_status" => json!({"op": "status", "name": a("id")}),
        "session_create" => json!({"op": "create", "name": a("name"), "description": a("description"),
                                   "role": a("role"), "model": a("model"), "cwd": a("cwd"), "harness": a("harness"),
                                   "permission_mode": a("permission_mode")}),
        "session_send" => json!({"op": "send", "name": a("to"), "text": a("msg"),
                                 "wait": args.get("wait").and_then(Value::as_bool).unwrap_or(true),
                                 "notify": args.get("notify").and_then(Value::as_bool).unwrap_or(true)}),
        "session_history" => {
            let mut r = args.as_object().cloned().unwrap_or_default();
            r.remove("id");
            if let Some(t) = r.remove("tail") {
                r.entry("limit").or_insert(t);
            }
            let mut r = Value::Object(r);
            r["op"] = json!("history_page");
            r["name"] = a("id");
            r
        }
        "session_handoff" => json!({"op": "handoff", "name": a("id"), "max_chars": a("max_chars")}),
        "set_my_description" => json!({"op": "set_description", "name": caller, "text": a("text")}),
        "set_my_status" => json!({"op": "set_note", "name": caller, "text": a("text")}),
        "wake_at" => {
            let target = match args.get("session").and_then(Value::as_str) {
                None | Some("") | Some("self") | Some("me") => json!(caller),
                Some(x) => json!(x),
            };
            json!({"op": "wake_add", "name": target, "at": a("at"), "repeat": a("repeat"), "message": a("message")})
        }
        "wake_list" => {
            let target = match args.get("session").and_then(Value::as_str) {
                None | Some("") => Value::Null,
                Some("self") | Some("me") => json!(caller),
                Some(x) => json!(x),
            };
            json!({"op": "wakes", "name": target, "all": args.get("all").map(|v| v.as_bool() == Some(true) || v.as_str() == Some("true")).unwrap_or(false)})
        }
        "wake_cancel" => json!({"op": "wake_cancel", "id": a("id")}),
        "transcribe" => json!({"op": "transcribe", "path": a("path")}),
        "send_file" => json!({"op": "send_file", "path": a("path"), "caption": a("caption")}),
        "board_post" => json!({"op": "board_post", "text": a("text")}),
        "board_read" => json!({"op": "board_read", "tail": a("tail")}),
        "ask_user" => json!({"op": "ask_user", "question": a("question"), "choices": a("choices"), "multi_select": a("multi_select"), "header": a("header")}),
        "telegram_history" => json!({"op": "telegram_history", "chat": a("chat"), "limit": a("limit"),
                                     "before": a("before"), "search": a("search"), "unanswered": a("unanswered")}),
        "version" => json!({"op": "version"}),
        "usage" => json!({"op": "usage_report"}),
        _ => return None,
    };
    let mut req = req;
    req["caller"] = json!(caller);
    Some(req)
}

pub async fn serve_stdio(sock: PathBuf) -> Result<()> {
    let caller = std::env::var("KURISU_SESSION").unwrap_or_default();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let out = std::sync::Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no reply
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => json!({"protocolVersion": msg.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                                   "capabilities": {"tools": {}}, "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")}}),
            "tools/list" => json!({"tools": tools_for(caller_is_lead(&sock, &caller).await)}),
            "tools/call" => {
                // Answered from its own task: ask_user can wait minutes for the user, and
                // the client may ping or call other tools meanwhile.
                let (sock, caller, out) = (sock.clone(), caller.clone(), out.clone());
                tokio::spawn(async move {
                    let result = call_tool(&sock, &msg, &caller, &out).await;
                    let _ = write_line(&out, &json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
                });
                continue;
            }
            "ping" => json!({}),
            _ => {
                write_line(&out, &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})).await?;
                continue;
            }
        };
        write_line(&out, &json!({"jsonrpc": "2.0", "id": id, "result": result})).await?;
    }
    Ok(())
}

type Out = std::sync::Arc<tokio::sync::Mutex<tokio::io::Stdout>>;

async fn write_line(out: &Out, v: &Value) -> Result<()> {
    let mut o = out.lock().await;
    o.write_all(format!("{v}\n").as_bytes()).await?;
    o.flush().await?;
    Ok(())
}

/// One tools/call. While it runs, a client that sent a progressToken gets a progress
/// notification every 20 s, so clients that reset their timeout on progress keep waiting.
async fn call_tool(sock: &std::path::Path, msg: &Value, caller: &str, out: &Out) -> Value {
    let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
    let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
    let token = msg.pointer("/params/_meta/progressToken").cloned();
    let (text, is_error) = match to_request(name, &args, caller) {
        None => (format!("unknown tool {name}"), true),
        Some(req) => {
            let call = crate::server::call(sock, req);
            tokio::pin!(call);
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + std::time::Duration::from_secs(20), std::time::Duration::from_secs(20));
            let mut n = 0;
            let res = loop {
                tokio::select! {
                    r = &mut call => break r,
                    _ = tick.tick(), if token.is_some() => {
                        n += 1;
                        let _ = write_line(out, &json!({"jsonrpc": "2.0", "method": "notifications/progress",
                            "params": {"progressToken": token, "progress": n, "message": "waiting for the user"}})).await;
                    }
                }
            };
            match res {
                Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
                Err(e) => (e.to_string(), true),
            }
        }
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_cover_the_roster_api_and_tag_the_caller() {
        let names: Vec<String> = tools().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        for n in ["whoami", "peers_list", "peer_get", "session_create", "session_send", "session_history", "session_handoff",
                  "session_status", "session_update", "set_my_description", "set_my_status", "board_post", "board_read", "ask_user",
                  "wake_at", "wake_list", "wake_cancel", "transcribe", "send_file", "usage", "version", "telegram_history"] {
            assert!(names.contains(&n.to_string()), "{n}");
            assert!(to_request(n, &json!({}), "w1").is_some(), "{n}");
        }
        // telegram_history maps its filters and keeps the caller tag.
        let r = to_request("telegram_history", &json!({"limit": 5, "search": "deploy", "unanswered": true}), "w1").unwrap();
        assert_eq!((r["op"].as_str(), r["limit"].as_i64(), r["search"].as_str(), r["unanswered"].as_bool(), r["caller"].as_str()),
                   (Some("telegram_history"), Some(5), Some("deploy"), Some(true), Some("w1")));
        // The description must stay honest about what it cannot see.
        let d = tools().as_array().unwrap().iter().find(|t| t["name"] == "telegram_history").unwrap()["description"].as_str().unwrap().to_string();
        assert!(d.contains("Bot API cannot fetch") && d.contains("owner's DM"), "{d}");
        assert_eq!(to_request("version", &json!({"git_sha":"caller-sha"}), "w1").unwrap(), json!({"op":"version", "caller":"w1"}));
        let r = to_request("session_send", &json!({"to": "dev", "msg": "hi"}), "lead").unwrap();
        assert_eq!((r["op"].as_str(), r["name"].as_str(), r["caller"].as_str()), (Some("send"), Some("dev"), Some("lead")));
        // Waiting is the default; wait:false uses the server's fire-and-forget path.
        assert_eq!(r["wait"], json!(true));
        let r = to_request("session_send", &json!({"to": "dev", "msg": "hi", "wait": false}), "lead").unwrap();
        assert_eq!(r["wait"], json!(false));
        // A session can only describe itself.
        let lead: Vec<Value> = tools_for(true).as_array().unwrap().iter().map(|t| t["name"].clone()).collect();
        let other: Vec<Value> = tools_for(false).as_array().unwrap().iter().map(|t| t["name"].clone()).collect();
        assert!(lead.contains(&json!("ask_user")) && !other.contains(&json!("ask_user")), "ask_user is lead-only");
        assert_eq!(lead.len(), other.len() + 1);
        let r = to_request("ask_user", &json!({"question": "Which fruit?", "choices": ["Mango", "Kiwi"], "multi_select": true, "header": "Fruit"}), "w1").unwrap();
        assert_eq!(r, json!({"op": "ask_user", "question": "Which fruit?", "choices": ["Mango", "Kiwi"], "multi_select": true, "header": "Fruit", "caller": "w1"}));
        let r = to_request("set_my_status", &json!({"text": "x", "name": "someone-else"}), "w1").unwrap();
        assert_eq!(r["name"], "w1");
        assert_eq!(to_request("whoami", &json!({}), "w1").unwrap()["name"], "w1");
        let r = to_request("session_update", &json!({"id": "w1", "effort": "low"}), "lead").unwrap();
        assert_eq!((r["op"].as_str(), r["name"].as_str()), (Some("update"), Some("w1")));
        assert_eq!(r["fields"], json!({"effort": "low"}));
        assert!(to_request("rm_rf", &json!({}), "lead").is_none());
    }

    /// OpenCode's code-mode catalogue is budget-limited and lists the shortest
    /// entries per namespace. With lb attached it showed 9 of 21 kurisu-mcp tools;
    /// send_file's long description pushed it out and a weak model then invented
    /// delivery. Keep it among the shortest so it is always in the visible catalogue.
    #[test]
    fn send_file_is_short_enough_for_the_code_mode_catalogue() {
        fn cat_line_len(t: &Value) -> usize {
            let name = t["name"].as_str().unwrap();
            let mut s = format!("  - tools[\"kurisu-mcp\"].{name}(");
            let props = t["inputSchema"]["properties"].as_object();
            match props {
                None => s.push_str("): Promise<unknown> // "),
                Some(p) if p.is_empty() => s.push_str("): Promise<unknown> // "),
                Some(p) => {
                    s.push_str("{\n");
                    for (k, v) in p {
                        s.push_str(&format!("  /** {} */\n  {}: {},\n",
                            v["description"].as_str().unwrap_or(""), k, v["type"].as_str().unwrap_or("string")));
                    }
                    s.push_str("}): Promise<unknown> // ");
                }
            }
            s.push_str(t["description"].as_str().unwrap_or(""));
            s.chars().count()
        }
        let mut lens: Vec<(usize, String)> = tools().as_array().unwrap().iter()
            .map(|t| (cat_line_len(t), t["name"].as_str().unwrap().to_string())).collect();
        lens.sort();
        let rank = lens.iter().position(|(_, n)| n == "send_file").unwrap();
        assert!(rank < 9, "send_file must be among the 9 shortest tools the code-mode catalogue can show: {lens:?}");
        let len = lens[rank].0;
        assert!(len <= 170, "keep send_file's catalogue line short ({len} chars): {lens:?}");
    }
}
