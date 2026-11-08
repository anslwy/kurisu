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
        {"name": "session_create", "description": "Lead only: create a worker peer session. Defaults: Claude with model claude-haiku-5-5, its own workspace folder. harness \"codex\" runs it on Codex (model: Codex's configured default unless given).",
         "inputSchema": s(json!({"name": str_("lowercase slug, e.g. worker-1"), "description": str_("what this worker is for"),
                                 "role": str_("worker (default)"), "harness": str_("claude (default) or codex"),
                                 "permission_mode": str_("auto (default when supported) | default | acceptEdits | plan"),
                                 "model": str_("Claude: defaults to claude-haiku-5-5; Codex: its configured default"), "cwd": str_("absolute folder")}), &["name", "description"])},
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
        {"name": "session_update", "description": "Modify a session you created (or one created by a session you created, and so on): task, model, effort, description, role, name, lifecycle (active|paused|archived). Model/effort apply from its next turn. harness (claude|codex, with optional model) switches it to another harness (your own id is allowed: the switch then happens right after your reply): it is archived and replaced by a new session with the same name and folder, which reads the old history via session_handoff; change harness on its own. You may also update yourself (your own id or name; after a harness switch your old id and the live same-name replacement are both you): task, status, description, model, effort, applied from your next turn; not your own lifecycle, role or name. Other non-ancestors are refused.",
         "inputSchema": s(json!({"id": str_("target session UUID or name"), "task": str_("new current task (shown as its status note)"),
                                 "model": str_("e.g. claude-haiku-5-5"), "effort": str_("low|medium|high|xhigh|max"),
                                 "permission_mode": str_("auto (routine actions run, risky ones ask the user) | default (ask for most tools) | acceptEdits | plan; applies from its next start"),
                                 "harness": str_("claude|codex: switch harness (see description)"),
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
        {"name": "board_read", "description": "Read the newest board posts.", "inputSchema": s(json!({"tail": int_("how many (default 20)")}), &[])}
    ])
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
        "board_post" => json!({"op": "board_post", "text": a("text")}),
        "board_read" => json!({"op": "board_read", "tail": a("tail")}),
        _ => return None,
    };
    let mut req = req;
    req["caller"] = json!(caller);
    Some(req)
}

pub async fn serve_stdio(sock: PathBuf) -> Result<()> {
    let caller = std::env::var("KURISU_SESSION").unwrap_or_default();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no reply
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => json!({"protocolVersion": msg.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                                   "capabilities": {"tools": {}}, "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")}}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match to_request(name, &args, &caller) {
                    None => (format!("unknown tool {name}"), true),
                    Some(req) => match crate::server::call(&sock, req).await {
                        Ok(v) => (serde_json::to_string_pretty(&v)?, false),
                        Err(e) => (e.to_string(), true),
                    },
                };
                json!({"content": [{"type": "text", "text": text}], "isError": is_error})
            }
            "ping" => json!({}),
            _ => {
                let err = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}});
                out.write_all(format!("{err}\n").as_bytes()).await?;
                out.flush().await?;
                continue;
            }
        };
        out.write_all(format!("{}\n", json!({"jsonrpc": "2.0", "id": id, "result": result})).as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_cover_the_roster_api_and_tag_the_caller() {
        let names: Vec<String> = tools().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        for n in ["whoami", "peers_list", "peer_get", "session_create", "session_send", "session_history", "session_handoff",
                  "session_status", "session_update", "set_my_description", "set_my_status", "board_post", "board_read",
                  "wake_at", "wake_list", "wake_cancel", "transcribe"] {
            assert!(names.contains(&n.to_string()), "{n}");
            assert!(to_request(n, &json!({}), "w1").is_some(), "{n}");
        }
        let r = to_request("session_send", &json!({"to": "dev", "msg": "hi"}), "lead").unwrap();
        assert_eq!((r["op"].as_str(), r["name"].as_str(), r["caller"].as_str()), (Some("send"), Some("dev"), Some("lead")));
        // Waiting is the default; wait:false uses the server's fire-and-forget path.
        assert_eq!(r["wait"], json!(true));
        let r = to_request("session_send", &json!({"to": "dev", "msg": "hi", "wait": false}), "lead").unwrap();
        assert_eq!(r["wait"], json!(false));
        // A session can only describe itself.
        let r = to_request("set_my_status", &json!({"text": "x", "name": "someone-else"}), "w1").unwrap();
        assert_eq!(r["name"], "w1");
        assert_eq!(to_request("whoami", &json!({}), "w1").unwrap()["name"], "w1");
        let r = to_request("session_update", &json!({"id": "w1", "effort": "low"}), "lead").unwrap();
        assert_eq!((r["op"].as_str(), r["name"].as_str()), (Some("update"), Some("w1")));
        assert_eq!(r["fields"], json!({"effort": "low"}));
        assert!(to_request("rm_rf", &json!({}), "lead").is_none());
    }
}
