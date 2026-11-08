//! `ku web`: serves the embedded web UI (Svelte build in `web/dist`) and the
//! versioned HTTP API (`/api/v1`, documented in docs/api.md). Like the CLI, it
//! is a client of the Kurisu server socket.
//!
//! Security: binds 127.0.0.1 by default. Every API call needs the token
//! (`Authorization: Bearer <token>` or `x-kurisu-token`; the event stream also
//! accepts `?token=` for EventSource clients). On a loopback bind, requests
//! whose Host header is not the bound address are refused (DNS rebinding).
//! Cross-origin clients (a Tauri app, a UI served elsewhere) are allowed only
//! for the origins listed in `KURISU_WEB_CORS_ORIGINS` (comma-separated).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use include_dir::{include_dir, Dir};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::paths::Paths;

static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web/dist");

/// The service-worker template. Its cache name carries `__BUILD__`, replaced
/// when served with the hash of the built main bundle (see `sw_version`).
const SW_TEMPLATE: &str = include_str!("../web/dist/sw.js");

#[derive(Clone)]
struct Ctx {
    paths: Paths,
    token: Arc<String>,
    allowed_hosts: Arc<Vec<String>>,
    cors: Arc<Vec<String>>,
    events: broadcast::Sender<(String, Value)>,
    snapshot: Arc<RwLock<Value>>, // last sessions list + sequence numbers
    listener: Option<Arc<std::net::TcpListener>>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn host_ok(ctx: &Ctx, headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    ctx.allowed_hosts.is_empty() || ctx.allowed_hosts.iter().any(|h| h == host)
}

fn token_of<'a>(headers: &'a HeaderMap, query_token: Option<&'a str>) -> &'a str {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()) {
        if let Some(t) = v.strip_prefix("Bearer ") {
            return t;
        }
    }
    headers.get("x-kurisu-token").and_then(|h| h.to_str().ok()).or(query_token).unwrap_or("")
}

fn check(ctx: &Ctx, headers: &HeaderMap, query_token: Option<&str>) -> Result<(), Response> {
    if !host_ok(ctx, headers) {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": "unexpected Host header"}))).into_response());
    }
    if !ct_eq(token_of(headers, query_token), &ctx.token) {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or wrong token"}))).into_response());
    }
    Ok(())
}

/// API calls never start the server: after `ku shutdown`, `ku web` exits too.
async fn call(ctx: &Ctx, req: Value) -> Response {
    let r = crate::server::call(&ctx.paths.socket(), req).await;
    match r {
        Ok(v) => Json(json!({"ok": v})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": format!("{e:#}")}))).into_response(),
    }
}

macro_rules! guard {
    ($ctx:expr, $h:expr) => {
        if let Err(r) = check(&$ctx, &$h, None) {
            return r;
        }
    };
}


#[derive(Deserialize)]
struct TerminalQ { session: Option<String>, dir: Option<String> }
async fn terminal(State(ctx): State<Ctx>, mut h: HeaderMap, Query(q): Query<TerminalQ>, upgrade: axum::extract::ws::WebSocketUpgrade) -> Response {
    // Reuse the owner's token in a subprotocol, never the URL. Only negotiate
    // the fixed protocol, so credentials are not reflected in the response.
    if let Some(token) = h.get("sec-websocket-protocol").and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').map(str::trim).nth(1)).and_then(|v| HeaderValue::from_str(v).ok()) {
        h.insert("x-kurisu-token", token);
    }
    guard!(ctx, h);
    if !crate::remote::same_origin(&h) { return StatusCode::FORBIDDEN.into_response(); }
    let mut dir = q.dir;
    if dir.is_none() {
        if let Some(id) = q.session {
            match crate::server::call(&ctx.paths.socket(), json!({"op":"status", "name":id})).await {
                Ok(s) => dir = s["cwd"].as_str().map(String::from),
                Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
            }
        }
    }
    if dir.as_ref().is_some_and(|d| !std::path::Path::new(d).is_dir()) {
        return (StatusCode::BAD_REQUEST, "Terminal directory does not exist").into_response();
    }
    let shell = match crate::remote::spawn_shell(&ctx.paths.home, dir.as_deref()) {
        Ok(s) => Arc::new(s), Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    upgrade.protocols(["kurisu"]).on_upgrade(move |sock| crate::remote::pump_owned(sock, shell, None))
}

#[derive(Deserialize)]
struct WorkspaceQ { #[serde(default)] path: String, #[serde(default)] hidden: bool }
async fn workspace(State(ctx): State<Ctx>, h: HeaderMap, AxPath((id, action)): AxPath<(String, String)>, Query(q): Query<WorkspaceQ>) -> Response {
    guard!(ctx, h);
    match crate::server::call(&ctx.paths.socket(), json!({"op":"status", "name":id})).await {
        Ok(s) => match s["cwd"].as_str() {
            Some(cwd) => crate::workspace::response(cwd.to_owned(), action, q.path, q.hidden).await,
            None => (StatusCode::BAD_REQUEST, Json(json!({"error":"session has no workspace"}))).into_response(),
        },
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error":e.to_string()}))).into_response(),
    }
}

// ------------------------------------------------------------------ API v1

async fn status(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    guard!(ctx, h);
    let (c, x, o, a) = tokio::join!(crate::hostcheck::claude(), crate::hostcheck::codex(), crate::hostcheck::opencode(), crate::hostcheck::agy());
    let clis = json!([c.to_string(), x.to_string(), o.to_string(), a.to_string()]);
    // Structured too, so clients need not parse the text (GET /status/clis adds stt).
    let cli_status = json!([c.to_json(), x.to_json(), o.to_json(), a.to_json()]);
    let ping = crate::server::call(&ctx.paths.socket(), json!({"op": "ping"})).await.unwrap_or(Value::Null);
    Json(json!({"ok": {"api": "v1", "version": env!("CARGO_PKG_VERSION"), "clis": clis, "cli_status": cli_status, "server": ping, "home": ctx.paths.home}}))
        .into_response()
}

#[derive(Deserialize)]
struct ClisQ {
    fresh: Option<bool>,
}
async fn status_clis(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<ClisQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "clis", "fresh": q.fresh.unwrap_or(false)})).await
}

#[derive(Deserialize)]
struct ListQ {
    all: Option<bool>,
}
async fn sessions(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<ListQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "peers", "include_archived": q.all.unwrap_or(true)})).await
}
async fn create(State(ctx): State<Ctx>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "create", "name": b["name"], "description": b.get("description"), "model": b.get("model"), "role": "worker",
                      "cwd": b.get("cwd"), "harness": b.get("harness"), "permission_mode": b.get("permission_mode")})).await
}
async fn one(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "status", "name": id})).await
}
#[derive(Deserialize)]
struct HistQ {
    after: Option<i64>,
    tail: Option<i64>,
}
async fn messages(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Query(q): Query<HistQ>) -> Response {
    guard!(ctx, h);
    match q.tail {
        Some(t) => call(&ctx, json!({"op": "history", "name": id, "tail": t})).await,
        None => call(&ctx, json!({"op": "history", "name": id, "after": q.after.unwrap_or(0)})).await,
    }
}
#[derive(Deserialize)]
struct PageQ {
    before: Option<i64>,
    after: Option<i64>,
    limit: Option<i64>,
    search: Option<String>,
    roles: Option<String>,
    tools: Option<bool>,
    max_chars: Option<i64>,
}
async fn history(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Query(q): Query<PageQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "history_page", "name": id, "before": q.before, "after": q.after, "limit": q.limit, "search": q.search,
                      "roles": q.roles, "tools": q.tools, "max_chars": q.max_chars})).await
}
#[derive(Deserialize)]
struct HandoffQ {
    max_chars: Option<i64>,
}
async fn handoff(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Query(q): Query<HandoffQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "handoff", "name": id, "max_chars": q.max_chars})).await
}
#[derive(Deserialize)]
struct AllMsgQ {
    after: Option<i64>,
    limit: Option<i64>,
}
async fn all_messages(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<AllMsgQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "messages_after", "after": q.after.unwrap_or(0), "limit": q.limit.unwrap_or(200)})).await
}
#[derive(Deserialize)]
struct ModelsQ {
    harness: Option<String>,
    model: Option<String>,
}
async fn models(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<ModelsQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "models", "harness": q.harness,"model":q.model})).await
}
#[derive(Deserialize)]
struct LeadQ {
    create: Option<bool>,
}
async fn lead_get(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<LeadQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "lead_get", "create": q.create.unwrap_or(false)})).await
}
async fn send(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "send", "name": id, "text": b.get("text"), "wait": b.get("wait"), "mode": b.get("mode"),
                      "files": b.get("files"), "images": b.get("images")})).await
}
async fn mcp_list(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "mcp_list", "name": id})).await
}
async fn mcp_set(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "mcp_set", "name": id, "server": b.get("name"), "config": b.get("config"), "enabled": b.get("enabled")})).await
}
async fn mcp_remove(State(ctx): State<Ctx>, h: HeaderMap, AxPath((id, server)): AxPath<(String, String)>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "mcp_remove", "name": id, "server": server})).await
}
async fn usage(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "usage"})).await
}
async fn compact(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "compact", "name": id})).await
}
async fn upload(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "upload", "name": id, "filename": b.get("name"), "data_b64": b.get("data_b64")})).await
}
async fn events_of(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "audit", "name": id})).await
}
async fn update(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    let mut fields = serde_json::Map::new();
    for f in ["task", "description", "model", "effort", "permission_mode", "role", "lifecycle", "name", "harness"] {
        if let Some(v) = b.get(f).and_then(Value::as_str) {
            fields.insert(f.into(), json!(v));
        }
    }
    call(&ctx, json!({"op": "update", "name": id, "fields": fields})).await
}
async fn stop(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "stop", "name": id})).await
}
async fn lead(State(ctx): State<Ctx>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "lead", "text": b["text"], "model": b.get("model")})).await
}
async fn audit(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "audit"})).await
}
#[derive(Deserialize)]
struct ApprovalsQ {
    all: Option<bool>,
    session: Option<String>,
}
async fn approvals(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<ApprovalsQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "approvals", "all": q.all.unwrap_or(false), "name": q.session})).await
}
async fn approve(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, body: Option<Json<Value>>) -> Response {
    guard!(ctx, h);
    let b = body.map(|Json(b)| b).unwrap_or_default();
    call(&ctx, json!({"op": "approve", "id": id, "always": b.get("always").and_then(Value::as_bool).unwrap_or(false), "reason": b.get("reason")})).await
}
async fn deny(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, body: Option<Json<Value>>) -> Response {
    guard!(ctx, h);
    let b = body.map(|Json(b)| b).unwrap_or_default();
    call(&ctx, json!({"op": "deny", "id": id, "reason": b.get("reason")})).await
}
async fn answer(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "answer", "id": id, "answers": b.get("answers")})).await
}
async fn rules(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "rules"})).await
}
async fn rule_add(State(ctx): State<Ctx>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "rule_add", "scope": b.get("scope"), "target": b.get("target"), "tool": b.get("tool"),
                      "in_cwd": b.get("in_cwd"), "command_prefix": b.get("command_prefix"), "decision": b.get("decision")})).await
}
async fn rule_remove(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<i64>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "rule_remove", "id": id})).await
}

#[derive(Deserialize)]
struct WakesQ {
    session: Option<String>,
    all: Option<bool>,
}
async fn wakes(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<WakesQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "wakes", "name": q.session, "all": q.all.unwrap_or(false)})).await
}
async fn wake_add(State(ctx): State<Ctx>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "wake_add", "name": b.get("session"), "at": b.get("at"), "repeat": b.get("repeat"), "message": b.get("message")})).await
}
async fn wake_cancel(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<i64>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "wake_cancel", "id": id})).await
}

async fn stt_status(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "stt_status"})).await
}
async fn transcribe(State(ctx): State<Ctx>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "transcribe", "path": b.get("path")})).await
}

#[derive(Deserialize)]
struct StreamQ {
    token: Option<String>,
}
/// Server-sent events: `hello` (snapshot), then `sessions`, `message`, `audit`, `approvals`.
async fn stream(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<StreamQ>) -> Response {
    if let Err(r) = check(&ctx, &h, q.token.as_deref()) {
        return r;
    }
    let hello = ctx.snapshot.read().await.clone();
    let first = tokio_stream::once(Ok::<_, Infallible>(Event::default().event("hello").json_data(hello).unwrap_or_default()));
    let rest = BroadcastStream::new(ctx.events.subscribe()).filter_map(|r| match r {
        Ok((kind, data)) => Some(Ok(Event::default().event(kind).json_data(data).unwrap_or_default())),
        // A slow client missed events: tell it to resync instead of silently diverging.
        Err(_) => Some(Ok(Event::default().event("resync").data("{}"))),
    });
    let stopping=ctx.stopping.clone();
    Sse::new(first.chain(rest).take_while(move |_| !stopping.load(std::sync::atomic::Ordering::SeqCst))).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}

/// One poller per `ku web` process turns server state into stream events.
/// When the server goes away (`ku shutdown`), `ku web` exits instead of
/// starting a new server behind the user's back.
async fn poller(ctx: Ctx) {
    let (mut msg_seq, mut audit_seq) = (-1i64, -1i64);
    let mut last_sessions = String::new();
    let mut last_approvals = String::new();
    let mut misses = 0;
    loop {
        if crate::server::connect_retry(&ctx.paths.socket(), Duration::from_millis(250)).await.is_err() {
            misses += 1;
            // A deploy repoints the socket: ride out a few seconds, and only exit when
            // the server has really gone away (ku shutdown), not on a handover blip.
            if misses >= 20 {
                eprintln!("kurisu server stopped (ku shutdown?); ku web is exiting. Run `ku web` to start both again.");
                std::process::exit(0);
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
            continue;
        }
        misses = 0;
        let r = async {
            let sock = ctx.paths.socket();
            let ch = crate::server::call(&sock, json!({"op": "changes", "msg_after": msg_seq, "audit_after": audit_seq})).await?;
            let list = crate::server::call(&sock, json!({"op": "peers", "include_archived": true})).await?;
            anyhow::Ok((ch, list))
        }
        .await;
        if let Ok((ch, list)) = r {
            for m in ch["messages"].as_array().into_iter().flatten() {
                let _ = ctx.events.send(("message".into(), m.clone()));
            }
            for a in ch["audit"].as_array().into_iter().flatten() {
                let _ = ctx.events.send(("audit".into(), a.clone()));
            }
            msg_seq = ch["msg_seq"].as_i64().unwrap_or(msg_seq);
            audit_seq = ch["audit_seq"].as_i64().unwrap_or(audit_seq);
            let ser = list.to_string();
            if ser != last_sessions {
                let _ = ctx.events.send(("sessions".into(), list.clone()));
                last_sessions = ser;
            }
            let pending = ch["approvals"].clone();
            let ser = pending.to_string();
            if ser != last_approvals {
                let _ = ctx.events.send(("approvals".into(), pending.clone()));
                last_approvals = ser;
            }
            *ctx.snapshot.write().await = json!({"sessions": list, "msg_seq": msg_seq, "audit_seq": audit_seq, "approvals": pending});
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

// ------------------------------------------------------------------ static

/// One embedded file from `web/dist`, for other listeners (the `/web` app in
/// chat.rs) that serve the same Svelte build under a different prefix.
pub(crate) fn dist_asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    let file = DIST.get_file(path)?;
    Some((content_type(path), file.contents()))
}

/// Hashed JavaScript/CSS referenced by the embedded standalone entry point.
pub(crate) fn asset_set(html: &str) -> Vec<String> {
    html.split(['"', '\'', '<', '>', ' ', '\n']).map(|s|s.trim_start_matches(['.','/']))
        .filter(|s|s.starts_with("assets/") && (s.ends_with(".js") || s.ends_with(".css")))
        .map(String::from).collect::<std::collections::BTreeSet<_>>().into_iter().collect()
}
pub(crate) fn build_stamp() -> Value {
    let index=DIST.get_file("index.html").and_then(|f|f.contents_utf8()).unwrap_or_default();
    json!({"build":crate::version::info(),"assets":asset_set(index)})
}
async fn frontend_handoff(State(ctx): State<Ctx>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    guard!(ctx,h);
    let Some(receiver)=body["receiver"].as_str() else { return StatusCode::BAD_REQUEST.into_response() };
    let Some(listener)=ctx.listener.as_ref() else { return StatusCode::CONFLICT.into_response() };
    use std::os::fd::AsRawFd;
    match crate::fdpass::send(std::path::Path::new(receiver),listener.as_raw_fd()) {
        Ok(())=>Json(json!({"exported":true})).into_response(),
        Err(e)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e.to_string()}))).into_response(),
    }
}
async fn frontend_build() -> Response {
    let mut stamp=build_stamp();
    stamp["pid"]=json!(std::process::id());
    let mut response=Json(stamp).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL,HeaderValue::from_static("no-store"));
    response
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The cache version for the service worker: the hash Vite gave the main
/// bundle. A rebuilt UI therefore installs a new worker, which drops the
/// previous shell cache (the old index and its assets). `"dev"` only if the
/// embedded index.html is not a Vite build.
pub(crate) fn sw_version(index_html: &str) -> String { bundle_version(index_html, "main") }

pub(crate) fn bundle_version(index_html: &str, entry: &str) -> String {
    index_html
        .split(&format!("assets/{entry}-"))
        .nth(1)
        .and_then(|s| s.split(['.', '"', '/', '?']).next())
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .unwrap_or("dev")
        .to_string()
}

/// The worker with its cache name filled in, so a deploy is never served from
/// the previous build's cached shell.
fn sw_js() -> String {
    let index = DIST.get_file("index.html").map(|f| String::from_utf8_lossy(f.contents())).unwrap_or_default();
    SW_TEMPLATE.replace("__BUILD__", &sw_version(&index))
}

/// Hashed assets never change under their name, so they can be kept for ever;
/// the shell, the worker and the manifest must be revalidated on every load.
fn cache_control(name: &str) -> &'static str {
    if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" }
}

async fn static_file(State(ctx): State<Ctx>, h: HeaderMap, req: Request) -> Response {
    if !host_ok(&ctx, &h) {
        return (StatusCode::FORBIDDEN, "unexpected Host header").into_response();
    }
    let path = req.uri().path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown API route (see docs/api.md)"}))).into_response();
    }
    let (body, name) = match DIST.get_file(path).filter(|_| !path.is_empty()) {
        Some(_) if path == "sw.js" => (sw_js().into_bytes(), path),
        Some(f) => (f.contents().to_vec(), path),
        None => match DIST.get_file("index.html") {
            Some(f) => (f.contents().to_vec(), "index.html"), // SPA fallback
            None => return (StatusCode::NOT_FOUND, "web UI not built").into_response(),
        },
    };
    (
        [
            (header::CONTENT_TYPE, content_type(name)),
            (header::CACHE_CONTROL, cache_control(name)),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

async fn cors(State(ctx): State<Ctx>, req: Request, next: Next) -> Response {
    let origin = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok()).map(String::from);
    let allowed = origin.as_ref().filter(|o| ctx.cors.iter().any(|a| a == *o)).cloned();
    if req.method() == Method::OPTIONS {
        let mut r = StatusCode::NO_CONTENT.into_response();
        if let Some(o) = allowed {
            let hm = r.headers_mut();
            hm.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_str(&o).unwrap());
            hm.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type, x-kurisu-token"));
            hm.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"));
            hm.insert(header::VARY, HeaderValue::from_static("Origin"));
        }
        return r;
    }
    let mut r = next.run(req).await;
    if let Some(o) = allowed {
        r.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_str(&o).unwrap());
        r.headers_mut().insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    r
}

fn router(ctx: Ctx) -> Router {
    let v1 = Router::new()
        .route("/terminal", get(terminal))
        .route("/sessions/{id}/workspace/{action}", get(workspace))
        .route("/status", get(status))
        .route("/status/clis", get(status_clis))
        .route("/sessions", get(sessions).post(create))
        .route("/sessions/{id}", get(one))
        .route("/sessions/{id}/messages", get(messages).post(send))
        .route("/sessions/{id}/events", get(events_of))
        .route("/sessions/{id}/history", get(history))
        .route("/sessions/{id}/handoff", get(handoff))
        .route("/sessions/{id}/files", post(upload))
        .route("/sessions/{id}/compact", post(compact))
        .route("/sessions/{id}/mcp", get(mcp_list).post(mcp_set))
        .route("/sessions/{id}/mcp/{server}/delete", post(mcp_remove))
        .route("/sessions/{id}/update", post(update))
        .route("/sessions/{id}/stop", post(stop))
        .route("/lead", post(lead).get(lead_get))
        .route("/messages", get(all_messages))
        .route("/models", get(models))
        .route("/audit", get(audit))
        .route("/usage", get(usage))
        .route("/approvals", get(approvals))
        .route("/approvals/{id}/approve", post(approve))
        .route("/approvals/{id}/deny", post(deny))
        .route("/approvals/{id}/answer", post(answer))
        .route("/rules", get(rules).post(rule_add))
        .route("/wakes", get(wakes).post(wake_add))
        .route("/stt", get(stt_status))
        .route("/transcribe", post(transcribe))
        .route("/wakes/{id}/cancel", post(wake_cancel))
        .route("/rules/{id}/delete", post(rule_remove))
        .route("/stream", get(stream));
    // Attachments travel as base64 JSON: allow bodies up to ~35 MB (25 MB decoded).
    let v1 = v1.layer(axum::extract::DefaultBodyLimit::max(36 * 1024 * 1024));
    Router::new()
        .route("/_ku/web-build", get(frontend_build))
        .route("/_ku/web-handoff", post(frontend_handoff))
        .nest("/api/v1", v1)
        .fallback(static_file)
        .layer(middleware::from_fn_with_state(ctx.clone(), cors))
        .with_state(ctx)
}

pub async fn serve(paths: Paths, host: &str, port: u16) -> Result<()> {
    let addr: SocketAddr = format!("{host}:{port}").parse().context("--host/--port")?;
    let loopback = addr.ip().is_loopback();
    if !loopback {
        eprintln!("WARNING: binding {addr} exposes your agents to the network; anyone with the token controls them.");
    }
    crate::ensure_server(&paths).await?;
    let listener = match std::env::var("KURISU_STANDALONE_WEB_FD") {
        Ok(value)=> {
            use std::os::fd::FromRawFd;
            std::env::remove_var("KURISU_STANDALONE_WEB_FD");
            let fd:i32=value.parse()?;
            let inherited=unsafe { std::net::TcpListener::from_raw_fd(fd) };
            if inherited.local_addr()?!=addr { anyhow::bail!("inherited standalone /web listener address mismatch"); }
            inherited.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(inherited)?
        }
        Err(_)=>tokio::net::TcpListener::bind(addr).await.with_context(|| format!("bind {addr}"))?,
    };
    let retained=listener.into_std()?;
    let listener=tokio::net::TcpListener::from_std(retained.try_clone()?)?;
    let bound = listener.local_addr()?;
    // Discovery only: doctor reads the actual bound address, including --port 0.
    // Leave this enabled on an abnormal exit so doctor can detect a dead listener.
    std::fs::create_dir_all(&paths.home)?;
    let discovery = crate::remote::Config { enabled: true, listen: bound.to_string(), public_url: None };
    crate::telegram::write_private(&paths.home.join("web.json"), &serde_json::to_vec_pretty(&discovery)?)?;
    let token = std::env::var("KURISU_WEB_TOKEN")
        .ok()
        .filter(|t| t.len() >= 16)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let mut ctx = ctx_for(paths.clone(), bound, token.clone());
    ctx.listener=Some(Arc::new(retained));
    let stopping=ctx.stopping.clone();
    let shutdown_events=ctx.events.clone();
    tokio::spawn(poller(ctx.clone()));
    let shown = if bound.ip().is_loopback() { format!("127.0.0.1:{}", bound.port()) } else { bound.to_string() };
    println!("Kurisu web UI (KURISU_HOME={})", paths.home.display());
    println!("  open: http://{shown}/#token={token}");
    println!("  API:  http://{shown}/api/v1  (docs/api.md)");
    println!("  Ctrl-C to stop the web UI (sessions keep running; `ku shutdown` stops them)");
    let served = axum::serve(listener, router(ctx))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            stopping.store(true,std::sync::atomic::Ordering::SeqCst);
            let _=shutdown_events.send(("shutdown".into(),Value::Null));
        })
        .await;
    // Do not overwrite another standalone listener's newer discovery record.
    let metadata = paths.home.join("web.json");
    if std::fs::read(&metadata).ok().and_then(|b| serde_json::from_slice::<crate::remote::Config>(&b).ok()).is_some_and(|c| c.listen == bound.to_string()) {
        let stopped = crate::remote::Config { enabled: false, ..discovery };
        crate::telegram::write_private(&metadata, &serde_json::to_vec_pretty(&stopped)?)?;
    }
    served?;
    Ok(())
}

fn ctx_for(paths: Paths, bound: SocketAddr, token: String) -> Ctx {
    let allowed = if bound.ip().is_loopback() {
        vec![format!("127.0.0.1:{}", bound.port()), format!("localhost:{}", bound.port()), format!("[::1]:{}", bound.port())]
    } else {
        vec![]
    };
    let cors_list: Vec<String> = std::env::var("KURISU_WEB_CORS_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let (tx, _) = broadcast::channel(512);
    Ctx {
        paths,
        token: Arc::new(token),
        allowed_hosts: Arc::new(allowed),
        cors: Arc::new(cors_list),
        events: tx,
        snapshot: Arc::new(RwLock::new(json!({"sessions": [], "msg_seq": 0, "audit_seq": 0}))),
        listener: None,
        stopping: Arc::default(),
    }
}

/// The router for a bound address, without the poller: test-only helper for
/// proving the serving path (not the polling path) opens the API.
#[cfg(test)]
pub(crate) fn app(paths: Paths, bound: SocketAddr, token: String) -> Router {
    router(ctx_for(paths, bound, token))
}

#[cfg(test)]
mod tests {
    #[test]
    fn standalone_asset_set_tracks_hashed_chunks_and_deduplicates() {
        assert_eq!(super::asset_set(r#"<script src="/assets/main-new.js"></script><link href='./assets/Splitter-next.js'><script src="/assets/main-new.js"></script>"#),
            vec!["assets/Splitter-next.js".to_string(),"assets/main-new.js".to_string()]);
    }

    use super::*;

    fn ctx() -> Ctx {
        let (tx, _) = broadcast::channel(4);
        Ctx {
            paths: Paths { home: std::env::temp_dir() },
            token: Arc::new("t0k3n-abcdefghijkl".into()),
            allowed_hosts: Arc::new(vec!["127.0.0.1:8787".into()]),
            cors: Arc::new(vec![]),
            events: tx,
            snapshot: Arc::new(RwLock::new(json!({}))),
            listener: None,
            stopping: Arc::default(),
        }
    }

    #[test]
    fn token_and_host_checks() {
        let c = ctx();
        let mut h = HeaderMap::new();
        h.insert("host", "127.0.0.1:8787".parse().unwrap());
        assert!(check(&c, &h, None).is_err(), "no token");
        assert!(check(&c, &h, Some("t0k3n-abcdefghijkl")).is_ok(), "query token (stream)");
        h.insert("authorization", "Bearer wrong-token-xxxxxxx".parse().unwrap());
        assert!(check(&c, &h, None).is_err(), "wrong token");
        h.insert("authorization", "Bearer t0k3n-abcdefghijkl".parse().unwrap());
        assert!(check(&c, &h, None).is_ok(), "bearer");
        h.remove("authorization");
        h.insert("x-kurisu-token", "t0k3n-abcdefghijkl".parse().unwrap());
        assert!(check(&c, &h, None).is_ok(), "x-kurisu-token");
        h.insert("host", "evil.example:8787".parse().unwrap());
        assert!(check(&c, &h, None).is_err(), "rebinding guard");
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
    }

    #[tokio::test]
    async fn workspace_routes_require_owner_auth() {
        for action in ["list-dir", "read-file", "git-status", "git-diff"] {
            let mut headers = HeaderMap::new();
            headers.insert("host", "127.0.0.1:8787".parse().unwrap());
            let response = workspace(State(ctx()), headers, AxPath(("session".into(), action.into())), Query(WorkspaceQ { path: String::new(), hidden: false })).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[test]
    fn embedded_ui_is_present() {
        let index = DIST.get_file("index.html").expect("web/dist/index.html is embedded");
        let text = std::str::from_utf8(index.contents()).unwrap();
        assert!(text.contains("manifest.webmanifest"), "PWA manifest linked");
        assert!(DIST.get_file("sw.js").is_some(), "service worker embedded");
        assert_eq!(content_type("assets/x.js"), "text/javascript; charset=utf-8");
    }

    #[test]
    fn service_worker_cache_name_follows_the_built_bundle() {
        let a = sw_version(r#"<script type="module" crossorigin src="./assets/main-C2k1sOet.js"></script>"#);
        assert_eq!(a, "C2k1sOet");
        let b = sw_version(r#"<script src="./assets/main-abc12345.js"></script>"#);
        assert_ne!(a, b, "a rebuilt bundle must get a new cache name");
        assert_eq!(sw_version("<p>not a vite build</p>"), "dev");

        // The served worker is the template with that name filled in; a stale
        // placeholder would keep every deploy on the first shell forever.
        let sw = sw_js();
        let built = sw_version(std::str::from_utf8(DIST.get_file("index.html").unwrap().contents()).unwrap());
        assert!(sw.contains(&format!("kurisu-shell-{built}")), "cache name is versioned: {sw}");
        assert!(!sw.contains("__BUILD__"), "no placeholder is served");
        let template = std::str::from_utf8(DIST.get_file("sw.js").unwrap().contents()).unwrap();
        assert!(template.contains("__BUILD__"), "the embedded template keeps the placeholder");
    }

    #[tokio::test]
    async fn shell_and_sw_are_no_cache_while_hashed_assets_are_immutable() {
        use axum::body::Body;
        let c = ctx();
        let mut h = HeaderMap::new();
        h.insert("host", "127.0.0.1:8787".parse().unwrap());
        let index = std::str::from_utf8(DIST.get_file("index.html").unwrap().contents()).unwrap();
        let asset = index.split("\"./").find_map(|s| s.strip_prefix("assets/main-")).and_then(|s| s.split('"').next())
            .map(|rest| format!("/assets/main-{rest}")).expect("index.html references the main bundle");
        for (path, cache) in [("/index.html", "no-cache"), ("/sw.js", "no-cache"), (asset.as_str(), "public, max-age=31536000, immutable")] {
            let req = Request::builder().uri(path).body(Body::empty()).unwrap();
            let r = static_file(State(c.clone()), h.clone(), req).await;
            assert_eq!(r.status(), StatusCode::OK, "{path}");
            assert_eq!(r.headers()[header::CACHE_CONTROL], cache, "{path}");
            if path == "/sw.js" {
                let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
                let text = String::from_utf8(body.to_vec()).unwrap();
                assert!(text.contains("kurisu-shell-"), "versioned cache name served: {text}");
                assert!(!text.contains("__BUILD__"), "placeholder substituted");
            }
        }
    }
}
