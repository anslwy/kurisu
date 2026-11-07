//! MCP stdio server exposing the Lead's peer-session tools. Each tool call is
//! forwarded to the running kurisud over its unix socket. Hand-rolled MCP
//! (initialize, tools/list, tools/call) keeps the binary small; `rmcp` is the
//! option if more of the protocol is needed.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub fn tools() -> Value {
    let s = |props: Value, req: &[&str]| json!({"type": "object", "properties": props, "required": req});
    json!([
        {"name": "session_list", "description": "List all sessions (peers) with harness, folder and status.",
         "inputSchema": s(json!({}), &[])},
        {"name": "session_create", "description": "Create a peer session (claude or codex) in an absolute folder.",
         "inputSchema": s(json!({"name": {"type": "string"}, "harness": {"type": "string", "enum": ["claude", "codex"]},
                                 "cwd": {"type": "string"}}), &["name", "harness", "cwd"])},
        {"name": "session_send", "description": "Send a message to a peer session and wait for its reply.",
         "inputSchema": s(json!({"name": {"type": "string"}, "text": {"type": "string"}}), &["name", "text"])},
        {"name": "session_history", "description": "Read a session's messages after a sequence number.",
         "inputSchema": s(json!({"name": {"type": "string"}, "after": {"type": "integer"}}), &["name"])},
        {"name": "session_status", "description": "Status of one session.",
         "inputSchema": s(json!({"name": {"type": "string"}}), &["name"])}
    ])
}

/// Map an MCP tool call to a kurisud control request.
pub fn to_request(tool: &str, args: &Value, caller: &str) -> Option<Value> {
    let mut req = args.clone();
    let op = match tool {
        "session_list" => "list",
        "session_create" => "create",
        "session_send" => {
            req["from"] = json!(format!("peer:{caller}"));
            "send"
        }
        "session_history" => "history",
        "session_status" => "status",
        _ => return None,
    };
    req["op"] = json!(op);
    Some(req)
}

pub async fn serve_stdio(sock: PathBuf) -> Result<()> {
    let caller = std::env::var("KURISU_SESSION").unwrap_or_else(|_| "lead".into());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no reply
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => json!({"protocolVersion": msg.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                                   "capabilities": {"tools": {}}, "serverInfo": {"name": "kurisu-sessions", "version": env!("CARGO_PKG_VERSION")}}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match to_request(name, &args, &caller) {
                    None => (format!("unknown tool {name}"), true),
                    Some(req) => match crate::call(&sock, req).await {
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
    fn tools_map_to_requests_and_tag_peer_sender() {
        assert_eq!(tools().as_array().unwrap().len(), 5);
        let r = to_request("session_send", &json!({"name": "dev", "text": "hi"}), "lead").unwrap();
        assert_eq!(r["op"], "send");
        assert_eq!(r["from"], "peer:lead");
        assert_eq!(to_request("session_list", &json!({}), "lead").unwrap()["op"], "list");
        assert!(to_request("rm_rf", &json!({}), "lead").is_none());
    }
}
