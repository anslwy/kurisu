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
        {"name": "whoami", "description": "Your own session id (name), role, description, model and folder.",
         "inputSchema": s(json!({}), &[])},
        {"name": "peers_list", "description": "Every session in this Kurisu team: id, role, description, harness, model, folder, status, its own status note and last activity. Use this instead of any roster document.",
         "inputSchema": s(json!({}), &[])},
        {"name": "peer_get", "description": "One session's details.", "inputSchema": s(json!({"id": str_("session id (name)")}), &["id"])},
        {"name": "session_create", "description": "Lead only: create a worker peer session. Defaults: Claude, model claude-haiku-5-5, its own workspace folder.",
         "inputSchema": s(json!({"name": str_("lowercase slug, e.g. worker-1"), "description": str_("what this worker is for"),
                                 "role": str_("worker (default)"), "model": str_("defaults to claude-haiku-5-5"), "cwd": str_("absolute folder")}), &["name", "description"])},
        {"name": "session_send", "description": "Send a message to a peer and wait for its reply. The peer sees it as coming from you.",
         "inputSchema": s(json!({"to": str_("peer id (name)"), "msg": str_("the message")}), &["to", "msg"])},
        {"name": "session_history", "description": "A session's recent messages (user, agent, peer:<id>).",
         "inputSchema": s(json!({"id": str_("session id"), "tail": int_("how many (default 20)"), "offset": int_("skip the newest N")}), &["id"])},
        {"name": "session_status", "description": "Status of one session.", "inputSchema": s(json!({"id": str_("session id")}), &["id"])},
        {"name": "session_update", "description": "Modify a session you created (or one created by a session you created, and so on): task, model, effort, description, role, name, lifecycle (active|paused|archived). Model/effort apply from its next turn. Non-ancestors are refused.",
         "inputSchema": s(json!({"id": str_("target session id"), "task": str_("new current task (shown as its status note)"),
                                 "model": str_("e.g. claude-haiku-5-5"), "effort": str_("low|medium|high|xhigh|max"),
                                 "description": str_("new description"), "role": str_("worker"), "name": str_("new id"),
                                 "lifecycle": str_("active|paused|archived")}), &["id"])},
        {"name": "set_my_description", "description": "Keep your own description current (what you are for).",
         "inputSchema": s(json!({"text": str_("one or two sentences")}), &["text"])},
        {"name": "set_my_status", "description": "Set your own short status / current task, visible to peers.",
         "inputSchema": s(json!({"text": str_("e.g. 'writing tests for X'")}), &["text"])},
        {"name": "board_post", "description": "Post to the team's shared board (standup notes, decisions, blockers).",
         "inputSchema": s(json!({"text": str_("the note")}), &["text"])},
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
                                   "role": a("role"), "model": a("model"), "cwd": a("cwd")}),
        "session_send" => json!({"op": "send", "name": a("to"), "text": a("msg")}),
        "session_history" => json!({"op": "history", "name": a("id"), "tail": a("tail"), "offset": a("offset")}),
        "set_my_description" => json!({"op": "set_description", "name": caller, "text": a("text")}),
        "set_my_status" => json!({"op": "set_note", "name": caller, "text": a("text")}),
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
        for n in ["whoami", "peers_list", "peer_get", "session_create", "session_send", "session_history",
                  "session_status", "session_update", "set_my_description", "set_my_status", "board_post", "board_read"] {
            assert!(names.contains(&n.to_string()), "{n}");
            assert!(to_request(n, &json!({}), "w1").is_some(), "{n}");
        }
        let r = to_request("session_send", &json!({"to": "dev", "msg": "hi"}), "lead").unwrap();
        assert_eq!((r["op"].as_str(), r["name"].as_str(), r["caller"].as_str()), (Some("send"), Some("dev"), Some("lead")));
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
