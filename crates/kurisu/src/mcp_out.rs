//! `ku mcp serve`: Kurisu as an OUTWARD MCP server.
//!
//! `mcp.rs` is the stdio server Kurisu injects into every session it starts (bare
//! `ku mcp`); this is the other direction: a streamable-HTTP MCP endpoint that an
//! app on another device (Vicuna, an editor, a second agent) calls over Tailscale,
//! shaped like any remote MCP server: a URL plus a token. `ku mcp serve` runs the
//! listener in the foreground and talks to the daemon over the same 0600 unix
//! socket as `ku` itself.
//!
//!   ku mcp serve [--host IP] [--port N]   default 100.118.224.35:8793 (Tailscale)
//!   ku mcp token                          create if missing, print
//!   ku mcp url [--host IP] [--port N]     http://<host>:<port>/mcp/<token>
//!
//! The daemon can host the same router in-process (the remote Hub's "mcp" kind),
//! which is what the Telegram `/mcp_server` card enables: `<KURISU_HOME>/mcp-server.json`
//! holds `{enabled, host, port}`, a start with it enabled re-arms the listener, and
//! `ku mcp serve` in the foreground keeps working unchanged.
//!
//!   POST /mcp/<token>    JSON-RPC 2.0, answered as application/json
//!   POST /mcp            same, with `Authorization: Bearer <token>`
//!   GET  /mcp[...]       405: no server-to-client SSE stream (POST only)
//!
//! Bind: an explicit IP address only. A hostname or an unspecified address
//! (0.0.0.0, ::) is refused, so the listener never lands on every interface by
//! accident. Auth is the token file `<KURISU_HOME>/mcp-server-token` (0600), in the
//! URL for clients that cannot set headers or as a bearer header. On a loopback
//! bind a non-loopback Host header is refused (DNS rebinding).
//!
//! Safety. Tools, deliberately minimal: `sessions`, `send`, `read`, `peers`.
//! An outward call has no caller session, so `send` is recorded as the OWNER's own
//! chat message (like text typed in Telegram): the session treats it as the user
//! and runs a normal turn. Nothing else is reachable: no session create/stop, no
//! lifecycle, harness, model or MCP-server changes, and no approvals. The tool set
//! is a whitelist in `to_request`.
//!
//! Blocking: a real MCP client gives up at 120 s, so no call blocks longer than
//! about 55 s. `send` queues and returns a seq at once; `read` polls for new
//! messages, with `wait_secs` capped at 60 and every call cut off at 55 s.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SERVER_NAME: &str = "kurisu-mcp";
/// This box's Tailscale address: the outward server is for another device.
pub const DEFAULT_HOST: &str = "100.118.224.35";
pub const DEFAULT_PORT: u16 = 8793;
pub const TOKEN_FILE: &str = "mcp-server-token";
/// The stored switch, the way `terminal.json` / `browser.json` hold theirs.
pub const CONFIG_FILE: &str = "mcp-server.json";
/// The client-side hard limit is 120 s; stay well inside it.
pub const MAX_WAIT_SECS: u64 = 60;
/// A tool call is answered (or failed) before this, whatever the client sends.
const CALL_BUDGET: Duration = Duration::from_secs(55);
/// How often `read` asks the daemon for new messages while it waits.
const POLL: Duration = Duration::from_millis(400);
/// Per-message text cap in `read` results, so one huge reply cannot blow up a client.
const READ_MAX_CHARS: i64 = 6000;

#[derive(Clone)]
struct Ctx {
    home: PathBuf,
    token: Arc<String>,
    allowed_hosts: Arc<Vec<String>>,
}

impl Ctx {
    fn socket(&self) -> PathBuf {
        crate::paths::Paths { home: self.home.clone() }.socket()
    }
}

// ------------------------------------------------------------------ token

pub fn token_path(home: &Path) -> PathBuf {
    home.join(TOKEN_FILE)
}

/// The token, generated (0600) on first start; a hand-edited token is kept.
pub fn ensure_token(home: &Path) -> Result<String> {
    let path = token_path(home);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let token = raw.trim().to_string();
        // A short or non-hex token is a mistake; replace it rather than serve it.
        if token.len() >= 32 && token.chars().all(|c| c.is_ascii_hexdigit()) {
            tighten(&path);
            return Ok(token);
        }
    }
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    crate::telegram::write_private(&path, token.as_bytes())?;
    Ok(token)
}

/// Repair the mode of a token file that lost its 0600 (a hand-made copy).
fn tighten(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// First 8 characters then an ellipsis: enough to recognise the token, never
/// enough to use it. The status card only ever carries this form.
pub fn mask(token: &str) -> String {
    format!("{}…", token.chars().take(8).collect::<String>())
}

/// The URL with the token masked, for the status card.
pub fn masked_url(addr: SocketAddr, token: Option<&str>) -> String {
    match token {
        Some(t) if !t.is_empty() => format!("http://{addr}/mcp/{}", mask(t)),
        _ => format!("http://{addr}/mcp/<token created on enable>"),
    }
}

/// The mode of the token file as `0600`, or None when there is no file yet.
pub fn token_mode(home: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(token_path(home)).ok()?;
    Some(format!("{:04o}", meta.permissions().mode() & 0o777))
}

// ------------------------------------------------------------------ switch

/// The persisted switch, next to the token. The listener itself is owned by the
/// daemon's remote Hub (`ku mcp serve` in the foreground stays a separate command).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub enabled: bool,
    /// The address to listen on, kept between restarts so the URL stays stable.
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_host() -> String { DEFAULT_HOST.to_string() }
fn default_port() -> u16 { DEFAULT_PORT }

impl Default for Config {
    fn default() -> Self { Config { enabled: false, host: default_host(), port: default_port() } }
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join(CONFIG_FILE)
}

/// The stored switch; None when the file is missing or unreadable (treated as off).
pub fn load_config(home: &Path) -> Option<Config> {
    serde_json::from_slice(&std::fs::read(config_path(home)).ok()?).ok()
}

/// 0600 like the token: the file says where a listener sits, which is not public.
pub fn save_config(home: &Path, cfg: &Config) -> Result<()> {
    crate::telegram::write_private(&config_path(home), &serde_json::to_vec_pretty(cfg)?)
}

/// The configured address, refusing an unspecified or hostname host as `--host` does.
pub fn socket_addr(cfg: &Config) -> Result<SocketAddr> {
    let (ip, port) = host_port(Some(&cfg.host), Some(&cfg.port.to_string()))?;
    Ok(SocketAddr::new(ip, port))
}

/// Turn the stored switch off; answers whether it had been on.
pub fn disable(home: &Path) -> Result<bool> {
    match load_config(home).filter(|c| c.enabled) {
        Some(mut c) => {
            c.enabled = false;
            save_config(home, &c)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

// ------------------------------------------------------------- status card

/// "1h 02m", "3d 4h", "5m 09s": the uptime line the owner reads in Telegram.
pub fn human_uptime(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60, secs % 60);
    if d > 0 { format!("{d}d {h}h") }
    else if h > 0 { format!("{h}h {m:02}m") }
    else if m > 0 { format!("{m}m {s:02}s") }
    else { format!("{s}s") }
}

/// The `/mcp_server` card text: plain lines, no markdown (Telegram shows it raw),
/// the token always masked. `st` is the `mcp_server_status` reply.
pub fn status_text(st: &Value) -> String {
    let listening = st["listening"] == json!(true);
    let mut lines = vec![
        "🔌 Outward MCP server".to_string(),
        String::new(),
        format!("state: {}", if listening { "running" } else { "stopped" }),
        format!("address: {}", st["listen"].as_str().filter(|s| !s.is_empty()).unwrap_or("unknown")),
    ];
    let token = match (st["token_path"].as_str(), st["token_mode"].as_str()) {
        (Some(path), Some(mode)) => format!("{path} ({mode})"),
        (Some(path), None) => format!("{path} (not created yet)"),
        _ => "unknown".into(),
    };
    lines.push(format!("token: {token}"));
    if listening {
        let pid = st["pid"].as_u64().unwrap_or(0);
        lines.push(format!("pid: {pid} (the daemon)"));
        if let Some(secs) = st["uptime_seconds"].as_u64() {
            lines.push(format!("uptime: {}", human_uptime(secs)));
        }
    }
    if let Some(url) = st["url"].as_str().filter(|u| !u.is_empty()) {
        lines.push(format!("url: {url}"));
    }
    if let Some(note) = st["note"].as_str().filter(|n| !n.trim().is_empty()) {
        lines.push(format!("note: {note}"));
    }
    lines.extend([
        String::new(),
        if listening {
            "The URL is masked. Show URL sends the full one in its own message.".into()
        } else {
            "Enable starts it inside the daemon; a restart with it enabled brings it back.".into()
        },
    ]);
    lines.join("\n")
}

/// Enable (when stopped) or Disable (when running), plus Show URL. `msv:` is
/// this card's callback prefix. `st` is the `mcp_server_status` reply.
pub fn keyboard(st: &Value) -> Value {
    let toggle = if st["listening"] == json!(true) {
        json!({"text": "⏹ Disable", "callback_data": "msv:off"})
    } else {
        json!({"text": "▶️ Enable", "callback_data": "msv:on"})
    };
    json!({"inline_keyboard": [[toggle, {"text": "🔗 Show URL", "callback_data": "msv:url"}]]})
}

// ------------------------------------------------------------------ auth

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "))
}

fn host_ok(allowed: &[String], h: &HeaderMap) -> bool {
    // No Host at all is not a browser (browsers always send one), so it is no rebinding risk.
    let Some(host) = h.get(header::HOST) else { return true };
    let host = host.to_str().unwrap_or("");
    allowed.is_empty() || allowed.iter().any(|a| a == host)
}

/// The token in the URL or in `Authorization: Bearer`; either one is enough.
fn authorize(ctx: &Ctx, h: &HeaderMap, path_token: Option<&str>) -> Result<(), Response> {
    if !host_ok(&ctx.allowed_hosts, h) {
        return Err(refused(StatusCode::FORBIDDEN, "unexpected Host header"));
    }
    let header_ok = bearer(h).is_some_and(|t| ct_eq(t, &ctx.token));
    let path_ok = path_token.is_some_and(|t| ct_eq(t, &ctx.token));
    if !header_ok && !path_ok {
        return Err(refused(StatusCode::UNAUTHORIZED, "missing or wrong token"));
    }
    Ok(())
}

fn refused(status: StatusCode, why: &str) -> Response {
    (status, Json(json!({"error": why}))).into_response()
}

// ------------------------------------------------------------------ tools

fn str_(d: &str) -> Value {
    json!({"type": "string", "description": d})
}

fn int_(d: &str) -> Value {
    json!({"type": "integer", "description": d})
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

/// The outward tool list. Deliberately small: read the team, talk to it.
pub fn tools() -> Value {
    json!([
        {"name": "sessions",
         "description": "Every Kurisu session: name, harness, status (and role, description, last activity). \
Names are what send and read take.",
         "inputSchema": schema(json!({}), &[])},
        {"name": "peers",
         "description": "Alias for sessions, for callers that think in peers: the same list of session names, harnesses and statuses.",
         "inputSchema": schema(json!({}), &[])},
        {"name": "send",
         "description": "Send a message to a session and return at once. It is recorded as the OWNER's own chat message \
(an outward call has no caller session), and the session runs a normal turn on it. Returns {seq}; it never waits for the reply, \
so read with that seq to collect it. No approvals, no lifecycle/harness/model changes.",
         "inputSchema": schema(json!({"to": str_("session name or UUID"),
                                       "text": str_("the message")}), &["to", "text"])},
        {"name": "read",
         "description": "Messages of a session newer than a seq (ascending). With wait_secs it polls up to that many seconds \
(capped at 60) for something new, so a caller can wait on the seq send returned; then it answers, possibly empty. \
Text per message is capped at 6000 characters.",
         "inputSchema": schema(json!({"session": str_("session name or UUID"),
                                       "after": int_("only messages newer than this seq (default 0: the whole start of history, so pass the seq send gave you)"),
                                       "limit": int_("max messages to return (default 50, max 200)"),
                                       "wait_secs": int_("poll up to this long for new messages (0 default, max 60)")}), &["session"])}
    ])
}

/// Numbers arrive as numbers or as strings from LLM clients; accept both.
fn num(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
}

/// Map an outward tool call to the one daemon request it makes.
///
/// Deliberately a whitelist: no create/stop/update/approval op can be reached from
/// here, and nothing carries a `caller`, so the daemon treats `send` as the user's.
pub fn to_request(tool: &str, args: &Value) -> Option<Value> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match tool {
        "sessions" | "peers" => Some(json!({"op": "peers"})),
        "send" => Some(json!({"op": "send", "name": s("to"), "text": s("text"), "wait": false, "notify": false})),
        "read" => Some(json!({"op": "history_page", "name": s("session"), "after": num(args, "after").unwrap_or(0).max(0),
                              "limit": num(args, "limit").unwrap_or(50).clamp(1, 200), "tools": false,
                              "max_chars": READ_MAX_CHARS})),
        _ => None,
    }
}

/// The compact rows `sessions` / `peers` answer with.
fn session_rows(peers: &Value) -> Value {
    json!(peers.as_array().into_iter().flatten().map(|p| json!({
        "name": p["name"],
        "harness": p["harness"],
        "status": p["status"],
        "role": p["role"],
        "description": p["description"],
        "last_activity": p["last_activity"],
    })).collect::<Vec<_>>())
}

fn compact(page: &Value) -> (Value, Option<i64>, bool) {
    let rows: Vec<Value> = page["messages"].as_array().into_iter().flatten().map(|m| json!({
        "seq": m["seq"], "role": m["role"], "text": m["text"], "at": m["at"],
    })).collect();
    (json!(rows), page["last_seq"].as_i64(), page["has_more"].as_bool().unwrap_or(false))
}

async fn call_daemon(ctx: &Ctx, req: Value) -> Result<Value, String> {
    crate::server::call(&ctx.socket(), req).await.map_err(|e| format!("{e:#}"))
}

/// One tool call. `read` polls; everything else is a single daemon request.
async fn call_tool(ctx: &Ctx, name: &str, args: &Value) -> Result<Value, String> {
    if name == "read" {
        return read(ctx, args).await;
    }
    let req = to_request(name, args).ok_or_else(|| format!("unknown tool {name}"))?;
    let v = call_daemon(ctx, req).await?;
    Ok(match name {
        "send" => json!({"seq": v["seq"], "session": v["name"], "queued": v["queued"] == true}),
        "sessions" | "peers" => json!({"sessions": session_rows(&v)}),
        _ => v,
    })
}

async fn read(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let session = args.get("session").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "missing session".to_string())?;
    let after = num(args, "after").unwrap_or(0).max(0);
    let wait = num(args, "wait_secs").unwrap_or(0).clamp(0, MAX_WAIT_SECS as i64) as u64;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(wait);
    loop {
        let mut req = to_request("read", args).unwrap_or(Value::Null);
        req["session"] = json!(session);
        req["after"] = json!(after);
        let page = call_daemon(ctx, req).await?;
        let (messages, last_seq, has_more) = compact(&page);
        let empty = messages.as_array().is_some_and(Vec::is_empty);
        if !empty || wait == 0 || Instant::now() >= deadline {
            return Ok(json!({
                "session": page["session"]["name"].as_str().unwrap_or(session),
                "after": after,
                "messages": messages,
                "last_seq": last_seq.or(if after > 0 { Some(after) } else { None }),
                "has_more": has_more,
                "waited_ms": started.elapsed().as_millis() as u64,
            }));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(POLL.min(left)).await;
    }
}

// -------------------------------------------------------------------- JSON-RPC

fn json_response(v: Value) -> Response {
    let mut r = Json(v).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn initialize_result(msg: &Value) -> Value {
    json!({
        "protocolVersion": msg.pointer("/params/protocolVersion").and_then(Value::as_str)
            .filter(|v| !v.is_empty()).unwrap_or("2025-06-18"),
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
        "instructions": "Kurisu's outward MCP endpoint: sessions, send, read, peers. \
An outward call has no caller session, so send is recorded as the OWNER's own chat message and the session answers it as the user. \
send returns a seq at once; read polls for the reply (wait_secs, max 60). \
No approvals, no session lifecycle, harness, model or MCP-server changes are reachable here."
    })
}

fn tool_result(v: &Value) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(v).unwrap_or_default()}],
        "structuredContent": v,
        "isError": false,
    })
}

fn tool_failure(why: &str) -> Value {
    json!({"content": [{"type": "text", "text": why}], "isError": true})
}

async fn handle(ctx: &Ctx, msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone(); // a notification (no id) needs no reply
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let reply = match method {
        "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": initialize_result(msg)}),
        "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/call" => {
            let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
            let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
            // The client's own timeout (120 s) is never reached: this one fires first.
            let result = match tokio::time::timeout(CALL_BUDGET, call_tool(ctx, name, &args)).await {
                Ok(Ok(v)) => tool_result(&v),
                Ok(Err(e)) => tool_failure(&e),
                Err(_) => tool_failure(&format!("tool call took longer than {} s and was stopped", CALL_BUDGET.as_secs())),
            };
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        }
        _ => rpc_error(id, -32601, &format!("method not found: {method}")),
    };
    Some(reply)
}

async fn dispatch(ctx: Ctx, body: Bytes) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return json_response(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    if let Value::Array(items) = msg {
        let mut out = Vec::new();
        for item in &items {
            if let Some(r) = handle(&ctx, item).await {
                out.push(r);
            }
        }
        return if out.is_empty() { StatusCode::ACCEPTED.into_response() } else { json_response(Value::Array(out)) };
    }
    match handle(&ctx, &msg).await {
        Some(r) => json_response(r),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn rpc_path(State(ctx): State<Ctx>, AxPath(token): AxPath<String>, h: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&ctx, &h, Some(&token)) {
        return r;
    }
    dispatch(ctx, body).await
}

async fn rpc_header(State(ctx): State<Ctx>, h: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = authorize(&ctx, &h, None) {
        return r;
    }
    dispatch(ctx, body).await
}

/// No server-to-client SSE stream: POST answers JSON, GET says so plainly.
fn no_stream_body() -> Response {
    refused(StatusCode::METHOD_NOT_ALLOWED, "POST JSON-RPC here; this endpoint does not offer GET/SSE streaming")
}

async fn no_stream_path(State(ctx): State<Ctx>, AxPath(token): AxPath<String>, h: HeaderMap) -> Response {
    match authorize(&ctx, &h, Some(&token)) {
        Ok(()) => no_stream_body(),
        Err(r) => r,
    }
}

async fn no_stream_header(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    match authorize(&ctx, &h, None) {
        Ok(()) => no_stream_body(),
        Err(r) => r,
    }
}

fn allowed_hosts(bound: SocketAddr) -> Vec<String> {
    if bound.ip().is_loopback() {
        vec![format!("127.0.0.1:{}", bound.port()), format!("localhost:{}", bound.port()), format!("[::1]:{}", bound.port())]
    } else {
        vec![]
    }
}

pub fn router(home: PathBuf, token: String, bound: SocketAddr) -> Router {
    let ctx = Ctx { home, token: Arc::new(token), allowed_hosts: Arc::new(allowed_hosts(bound)) };
    Router::new()
        .route("/mcp", post(rpc_header).get(no_stream_header))
        .route("/mcp/", post(rpc_header))
        .route("/mcp/{token}", post(rpc_path).get(no_stream_path))
        .with_state(ctx)
}

// ------------------------------------------------------------------ listener

/// `--host` / `--port` with their defaults. The host must be an explicit IP
/// address: no hostname, and never the unspecified 0.0.0.0 / ::.
pub fn host_port(host: Option<&str>, port: Option<&str>) -> Result<(IpAddr, u16)> {
    let raw = host.map(str::trim).unwrap_or(DEFAULT_HOST);
    let ip: IpAddr = raw.trim_matches(|c| c == '[' || c == ']').parse()
        .with_context(|| format!("--host takes an explicit IP address (e.g. {DEFAULT_HOST} or 127.0.0.1), not {raw:?}"))?;
    if ip.is_unspecified() {
        anyhow::bail!("refusing to bind {ip}: that listens on every interface; pass the explicit address (e.g. {DEFAULT_HOST})");
    }
    let port = match port {
        Some(p) => p.trim().parse::<u16>().context("--port takes a number, e.g. --port 8793")?,
        None => DEFAULT_PORT,
    };
    Ok((ip, port))
}

pub fn url_for(addr: SocketAddr, token: &str) -> String {
    format!("http://{addr}/mcp/{token}")
}

/// `ku mcp url`: the full URL, creating the token if needed.
pub fn url(home: &Path, host: Option<&str>, port: Option<&str>) -> Result<String> {
    let (ip, port) = host_port(host, port)?;
    Ok(url_for(SocketAddr::new(ip, port), &ensure_token(home)?))
}

/// `ku mcp serve`: run the listener in the foreground until Ctrl-C / SIGTERM.
pub async fn serve(paths: crate::paths::Paths, host: IpAddr, port: u16) -> Result<()> {
    let token = ensure_token(&paths.home)?;
    let listener = tokio::net::TcpListener::bind((host, port)).await
        .with_context(|| format!("bind {}", SocketAddr::new(host, port)))?;
    let addr = listener.local_addr()?;
    println!("Kurisu MCP server (KURISU_HOME={})", paths.home.display());
    println!("  url:   {}", url_for(addr, &token));
    println!("  token: {} (0600)", token_path(&paths.home).display());
    println!("  tools: sessions, send, read, peers (send acts as the owner's own message)");
    let app = router(paths.home.clone(), token, addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
            }
        })
        .await
        .context("mcp server listener")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tmp() -> String {
        std::env::var("KURISU_TEST_TMPDIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned())
    }

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("mcp-out-").tempdir_in(test_tmp()).unwrap()
    }

    /// An obviously fake, low-entropy token (secret scanners flag random-looking ones).
    fn fake_token() -> String {
        "0".repeat(32)
    }

    fn ctx(home: &Path) -> Ctx {
        Ctx {
            home: home.to_path_buf(),
            token: Arc::new(fake_token()),
            allowed_hosts: Arc::new(vec!["127.0.0.1:8793".into(), "localhost:8793".into()]),
        }
    }

    fn headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("host", "127.0.0.1:8793".parse().unwrap());
        h
    }

    #[test]
    fn token_is_generated_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = home();
        let token = ensure_token(dir.path()).unwrap();
        assert_eq!(token.len(), 64, "two uuid simple forms");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        let path = token_path(dir.path());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(ensure_token(dir.path()).unwrap(), token, "the same token is reused");
        // A hand-edited token is kept; a malformed one is replaced.
        std::fs::write(&path, "deadbeefdeadbeefdeadbeefdeadbeef\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(ensure_token(dir.path()).unwrap(), "deadbeefdeadbeefdeadbeefdeadbeef");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "mode repaired");
        std::fs::write(&path, "short\n").unwrap();
        assert_ne!(ensure_token(dir.path()).unwrap(), "short");
    }

    #[test]
    fn token_check_accepts_url_or_bearer_and_refuses_the_rest() {
        let dir = home();
        let c = ctx(dir.path());
        let good = fake_token();
        let good = good.as_str();
        assert!(authorize(&c, &headers(), None).is_err(), "no token at all");
        assert!(authorize(&c, &headers(), Some("wrong-token-wrong-token")).is_err(), "wrong path token");
        assert!(authorize(&c, &headers(), Some(good)).is_ok(), "plain URL token");
        let mut h = headers();
        h.insert(header::AUTHORIZATION, "Bearer wrong-token-wrong-token".parse().unwrap());
        assert!(authorize(&c, &h, None).is_err(), "wrong bearer");
        h.insert(header::AUTHORIZATION, format!("Bearer {good}").parse().unwrap());
        assert!(authorize(&c, &h, None).is_ok(), "bearer token");
        // A wrong Host is refused even with the right token (DNS rebinding).
        h.insert("host", "evil.example:8793".parse().unwrap());
        assert!(authorize(&c, &h, None).is_err());
        assert!(authorize(&c, &h, Some(good)).is_err());
        // A host-less request on a loopback bind is allowed (some clients omit it).
        let bare = HeaderMap::new();
        assert!(authorize(&c, &bare, Some(good)).is_ok());
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
    }

    #[test]
    fn tools_are_the_minimal_outward_set_and_map_to_owner_requests() {
        let names: Vec<String> = tools().as_array().unwrap().iter()
            .map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["sessions", "peers", "send", "read"]);
        for n in &names {
            assert!(to_request(n, &json!({})).is_some(), "{n}");
        }
        // send queues as the owner: wait:false, no notify, and no caller tag at all.
        let r = to_request("send", &json!({"to": "dev", "text": "hi"})).unwrap();
        assert_eq!(r, json!({"op": "send", "name": "dev", "text": "hi", "wait": false, "notify": false}));
        assert!(r.get("caller").is_none(), "an outward call has no caller: the owner sent it");
        // read pages forward from a seq, caps the limit and the text size.
        let r = to_request("read", &json!({"session": "dev", "after": "12", "limit": 9000})).unwrap();
        assert_eq!((r["op"].as_str(), r["limit"].as_i64(), r["after"].as_i64()), (Some("history_page"), Some(200), Some(12)));
        assert_eq!(r["tools"], json!(false));
        let r = to_request("read", &json!({"session": "dev"})).unwrap();
        assert_eq!((r["limit"].as_i64(), r["after"].as_i64()), (Some(50), Some(0)));
        // sessions and peers are the same read.
        assert_eq!(to_request("sessions", &json!({})).unwrap(), to_request("peers", &json!({})).unwrap());
        // Nothing that changes a session or answers an approval is reachable.
        for n in ["session_create", "session_update", "session_send", "session_status", "approve", "deny", "stop", "mcp_set",
                  "set_my_status", "wake_at", "send_file", "ask_user", "rm_rf", ""] {
            assert!(to_request(n, &json!({})).is_none(), "{n} must not be reachable outward");
        }
        // The owner semantics are documented where a client reads them.
        let send = tools().as_array().unwrap().iter().find(|t| t["name"] == "send").unwrap().clone();
        assert!(send["description"].as_str().unwrap().contains("OWNER"));
    }

    #[test]
    fn host_must_be_an_explicit_address() {
        let (ip, port) = host_port(None, None).unwrap();
        assert_eq!((ip.to_string().as_str(), port), (DEFAULT_HOST, DEFAULT_PORT));
        assert_eq!(host_port(Some("127.0.0.1"), Some("18793")).unwrap().1, 18793);
        assert!(host_port(Some("[::1]"), None).is_ok());
        for bad in ["0.0.0.0", "::", "[::]", "localhost", "example.com", ""] {
            assert!(host_port(Some(bad), None).is_err(), "{bad} must be refused");
        }
        assert!(host_port(None, Some("nope")).is_err());
        let dir = home();
        let u = url(dir.path(), Some("127.0.0.1"), Some("18793")).unwrap();
        assert_eq!(u, format!("http://127.0.0.1:18793/mcp/{}", ensure_token(dir.path()).unwrap()));
    }

    #[test]
    fn wait_is_capped_and_session_rows_are_compact() {
        assert_eq!(num(&json!({"wait_secs": "900"}), "wait_secs").unwrap().clamp(0, MAX_WAIT_SECS as i64), 60);
        let peers = json!([{"id": "s1", "name": "dev", "harness": "claude", "status": "working", "role": "worker",
                            "description": "d", "last_activity": 5, "cwd": "/x", "children": []}]);
        let rows = session_rows(&peers);
        assert_eq!(rows[0]["name"], "dev");
        assert_eq!((rows[0]["harness"].as_str(), rows[0]["status"].as_str()), (Some("claude"), Some("working")));
        assert!(rows[0].get("cwd").is_none(), "only the fields a client needs");
        assert_eq!(session_rows(&json!(null)), json!([]));
    }

    #[test]
    fn config_round_trips_and_the_switch_turns_off() {
        use std::os::unix::fs::PermissionsExt;
        let dir = home();
        // Missing file: nothing is enabled and the defaults are the Tailscale address.
        assert!(load_config(dir.path()).is_none());
        assert!(!Config::default().enabled);
        assert_eq!((Config::default().host.as_str(), Config::default().port), (DEFAULT_HOST, DEFAULT_PORT));
        assert!(!disable(dir.path()).unwrap(), "nothing was on");
        assert_eq!(socket_addr(&Config::default()).unwrap(), SocketAddr::new(DEFAULT_HOST.parse().unwrap(), DEFAULT_PORT));
        let cfg = Config { enabled: true, host: "127.0.0.1".into(), port: 18793 };
        save_config(dir.path(), &cfg).unwrap();
        assert_eq!(std::fs::metadata(config_path(dir.path())).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(load_config(dir.path()).unwrap(), cfg);
        assert_eq!(socket_addr(&cfg).unwrap().to_string(), "127.0.0.1:18793");
        assert!(disable(dir.path()).unwrap(), "it was on");
        assert_eq!(load_config(dir.path()).unwrap(), Config { enabled: false, ..cfg.clone() }, "host and port are kept");
        assert!(!disable(dir.path()).unwrap(), "turning it off twice changes nothing");
        // A host-edited file must not smuggle in 0.0.0.0 through the config.
        save_config(dir.path(), &Config { enabled: true, host: "0.0.0.0".into(), port: 8793 }).unwrap();
        assert!(socket_addr(&load_config(dir.path()).unwrap()).is_err());
    }

    #[test]
    fn status_text_is_plain_masked_and_says_state_and_address() {
        let token = fake_token();
        let addr: SocketAddr = "100.118.224.35:8793".parse().unwrap();
        let running = json!({
            "enabled": true, "listening": true, "listen": addr.to_string(),
            "token_path": "/srv/kurisu/mcp-server-token", "token_mode": "0600",
            "pid": 4242, "uptime_seconds": 3725,
            "url": masked_url(addr, Some(&token)), "note": "",
        });
        let text = status_text(&running);
        assert!(text.contains("state: running"), "{text}");
        assert!(text.contains("100.118.224.35:8793"), "{text}");
        assert!(text.contains("token: /srv/kurisu/mcp-server-token (0600)"), "{text}");
        assert!(text.contains("pid: 4242"), "{text}");
        assert!(text.contains("uptime: 1h 02m"), "{text}");
        // Masked: the first 8 characters, then an ellipsis. The token itself never appears.
        assert!(text.contains(&format!("/mcp/{}…", &token[..8])), "{text}");
        assert!(!text.contains(&token), "the status line must never carry the full token");
        assert!(!text.contains("**") && !text.contains('`'), "plain text: Telegram shows markdown raw");
        // Stopped: no pid/uptime, and the reason is shown.
        let stopped = json!({
            "enabled": false, "listening": false, "listen": "100.118.224.35:8793",
            "token_path": "/srv/kurisu/mcp-server-token", "token_mode": Value::Null,
            "url": "http://100.118.224.35:8793/mcp/<token created on enable>", "note": "port in use by pid 2407275 (ku)",
        });
        let text = status_text(&stopped);
        assert!(text.contains("state: stopped"), "{text}");
        assert!(!text.contains("pid:") && !text.contains("uptime:"), "{text}");
        assert!(text.contains("(not created yet)"), "{text}");
        assert!(text.contains("note: port in use by pid 2407275 (ku)"), "{text}");
    }

    #[test]
    fn keyboard_shows_exactly_one_toggle_for_the_state() {
        fn data(st: Value) -> Vec<String> {
            let kb = keyboard(&st);
            let rows = kb["inline_keyboard"].as_array().unwrap();
            rows.iter().flat_map(|r| r.as_array().unwrap().iter())
                .inspect(|b| assert!(b["text"].as_str().unwrap().len() <= 64))
                .map(|b| b["callback_data"].as_str().unwrap().to_string()).collect()
        }
        assert_eq!(data(json!({"listening": false})), vec!["msv:on", "msv:url"]);
        assert_eq!(data(json!({})), vec!["msv:on", "msv:url"], "unknown state offers Enable");
        assert_eq!(data(json!({"listening": true})), vec!["msv:off", "msv:url"]);
    }

    #[test]
    fn mask_uptime_and_url_helpers() {
        assert_eq!(mask("0123456789abcdef"), "01234567…");
        assert_eq!(mask("short"), "short…");
        let addr: SocketAddr = "127.0.0.1:8793".parse().unwrap();
        assert_eq!(masked_url(addr, Some("0123456789abcdef")), "http://127.0.0.1:8793/mcp/01234567…");
        assert!(masked_url(addr, None).contains("created on enable"));
        assert_eq!(human_uptime(0), "0s");
        assert_eq!(human_uptime(75), "1m 15s");
        assert_eq!(human_uptime(3725), "1h 02m");
        assert_eq!(human_uptime(90061), "1d 1h");
    }

    /// A stand-in daemon: answers each unix-socket line with one `{"ok": ...}` line.
    fn fake_daemon(socket: PathBuf) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            while let Ok((stream, _)) = listener.accept().await {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                let Ok(Some(line)) = lines.next_line().await else { continue };
                let req: Value = serde_json::from_str(&line).unwrap_or_default();
                let ok = match req["op"].as_str() {
                    Some("peers") => json!([
                        {"id": "s1", "name": "tester", "harness": "claude", "status": "idle", "role": "worker",
                         "description": "the test session", "last_activity": 7, "lifecycle": "active"},
                        {"id": "s2", "name": "other", "harness": "codex", "status": "stopped", "role": "worker",
                         "description": "", "last_activity": 3, "lifecycle": "active"}
                    ]),
                    Some("send") => json!({"id": "s1", "name": "tester", "seq": 41, "queued": false}),
                    Some("history_page") => json!({"session": {"name": req["name"]}, "messages": [], "last_seq": null}),
                    _ => json!({}),
                };
                let _ = w.write_all(format!("{}\n", json!({"ok": ok})).as_bytes()).await;
            }
        })
    }

    /// The acceptance shape, in-tree: a real HTTP client drives initialize ->
    /// tools/list -> tools/call over an ephemeral port against a throwaway home.
    #[tokio::test]
    async fn initialize_list_and_sessions_over_http_on_an_ephemeral_port() {
        let dir = home();
        let paths = crate::paths::Paths { home: dir.path().to_path_buf() };
        let daemon = fake_daemon(paths.socket());
        let token = fake_token();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(dir.path().to_path_buf(), token.clone(), addr);
        let served = tokio::spawn(async move { let _ = axum::serve(listener, app).await; });
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/mcp/{token}");
        let rpc = |id: i64, method: &str, params: Value| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});

        let init: Value = client.post(&url)
            .json(&rpc(1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {},
                                               "clientInfo": {"name": "test", "version": "1"}})))
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["capabilities"]["tools"].is_object());

        let list: Value = client.post(&url).json(&rpc(2, "tools/list", json!({})))
            .send().await.unwrap().json().await.unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter()
            .map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["sessions", "peers", "send", "read"]);

        let call: Value = client.post(&url)
            .json(&rpc(3, "tools/call", json!({"name": "sessions", "arguments": {}})))
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(call["result"]["isError"], false);
        let sessions = &call["result"]["structuredContent"]["sessions"];
        assert_eq!(sessions[0]["name"], "tester");
        assert_eq!(sessions[0]["harness"], "claude");
        assert_eq!(sessions[1]["status"], "stopped");

        // send answers a seq at once instead of waiting for the turn.
        let sent: Value = client.post(&url)
            .json(&rpc(4, "tools/call", json!({"name": "send", "arguments": {"to": "tester", "text": "hello"}})))
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(sent["result"]["structuredContent"]["seq"], 41);
        assert_eq!(sent["result"]["structuredContent"]["session"], "tester");

        // A notification gets 202 with no body; a bad token is 401; GET has no SSE.
        let resp = client.post(&url).json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send().await.unwrap();
        assert_eq!(resp.status(), 202);
        let resp = client.post(format!("http://{addr}/mcp/wrong-token-wrong-token-wrong"))
            .json(&rpc(5, "tools/list", json!({}))).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        let resp = client.post(format!("http://{addr}/mcp"))
            .header("authorization", format!("Bearer {token}"))
            .json(&rpc(6, "ping", json!({}))).send().await.unwrap();
        assert_eq!(resp.status(), 200, "bearer header works without a URL token");
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 405);

        served.abort();
        daemon.abort();
    }
}
