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

#[derive(Clone)]
struct Ctx {
    paths: Paths,
    token: Arc<String>,
    allowed_hosts: Arc<Vec<String>>,
    cors: Arc<Vec<String>>,
    events: broadcast::Sender<(String, Value)>,
    snapshot: Arc<RwLock<Value>>, // last sessions list + sequence numbers
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

// ------------------------------------------------------------------ API v1

async fn status(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    guard!(ctx, h);
    let clis = json!([crate::hostcheck::claude().await.to_string(), crate::hostcheck::codex().await.to_string()]);
    let ping = crate::server::call(&ctx.paths.socket(), json!({"op": "ping"})).await.unwrap_or(Value::Null);
    Json(json!({"ok": {"api": "v1", "version": env!("CARGO_PKG_VERSION"), "clis": clis, "server": ping, "home": ctx.paths.home}}))
        .into_response()
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
    call(&ctx, json!({"op": "create", "name": b["name"], "description": b.get("description"), "model": b.get("model"), "role": "worker"})).await
}
async fn one(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "status", "name": id})).await
}
#[derive(Deserialize)]
struct HistQ {
    after: Option<i64>,
}
async fn messages(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Query(q): Query<HistQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "history", "name": id, "after": q.after.unwrap_or(0)})).await
}
async fn send(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "send", "name": id, "text": b["text"], "wait": b.get("wait"), "mode": b.get("mode")})).await
}
async fn events_of(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "audit", "name": id})).await
}
async fn update(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    let mut fields = serde_json::Map::new();
    for f in ["task", "description", "model", "effort", "role", "lifecycle", "name"] {
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
    Sse::new(first.chain(rest)).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
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
        if tokio::net::UnixStream::connect(ctx.paths.socket()).await.is_err() {
            misses += 1;
            if misses >= 3 {
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

async fn static_file(State(ctx): State<Ctx>, h: HeaderMap, req: Request) -> Response {
    if !host_ok(&ctx, &h) {
        return (StatusCode::FORBIDDEN, "unexpected Host header").into_response();
    }
    let path = req.uri().path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown API route (see docs/api.md)"}))).into_response();
    }
    let (file, name) = match DIST.get_file(path).filter(|_| !path.is_empty()) {
        Some(f) => (f, path),
        None => match DIST.get_file("index.html") {
            Some(f) => (f, "index.html"), // SPA fallback
            None => return (StatusCode::NOT_FOUND, "web UI not built").into_response(),
        },
    };
    let cache = if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    (
        [
            (header::CONTENT_TYPE, content_type(name)),
            (header::CACHE_CONTROL, cache),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        file.contents(),
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
        .route("/status", get(status))
        .route("/sessions", get(sessions).post(create))
        .route("/sessions/{id}", get(one))
        .route("/sessions/{id}/messages", get(messages).post(send))
        .route("/sessions/{id}/events", get(events_of))
        .route("/sessions/{id}/update", post(update))
        .route("/sessions/{id}/stop", post(stop))
        .route("/lead", post(lead))
        .route("/audit", get(audit))
        .route("/approvals", get(approvals))
        .route("/approvals/{id}/approve", post(approve))
        .route("/approvals/{id}/deny", post(deny))
        .route("/rules", get(rules).post(rule_add))
        .route("/rules/{id}/delete", post(rule_remove))
        .route("/stream", get(stream));
    Router::new()
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
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("bind {addr}"))?;
    let bound = listener.local_addr()?;
    let token = std::env::var("KURISU_WEB_TOKEN")
        .ok()
        .filter(|t| t.len() >= 16)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let allowed = if loopback {
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
    let ctx = Ctx {
        paths: paths.clone(),
        token: Arc::new(token.clone()),
        allowed_hosts: Arc::new(allowed),
        cors: Arc::new(cors_list),
        events: tx,
        snapshot: Arc::new(RwLock::new(json!({"sessions": [], "msg_seq": 0, "audit_seq": 0}))),
    };
    tokio::spawn(poller(ctx.clone()));
    let shown = if bound.ip().is_loopback() { format!("127.0.0.1:{}", bound.port()) } else { bound.to_string() };
    println!("Kurisu web UI (KURISU_HOME={})", paths.home.display());
    println!("  open: http://{shown}/#token={token}");
    println!("  API:  http://{shown}/api/v1  (docs/api.md)");
    println!("  Ctrl-C to stop the web UI (sessions keep running; `ku shutdown` stops them)");
    axum::serve(listener, router(ctx))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn embedded_ui_is_present() {
        let index = DIST.get_file("index.html").expect("web/dist/index.html is embedded");
        let text = std::str::from_utf8(index.contents()).unwrap();
        assert!(text.contains("manifest.webmanifest"), "PWA manifest linked");
        assert!(DIST.get_file("sw.js").is_some(), "service worker embedded");
        assert_eq!(content_type("assets/x.js"), "text/javascript; charset=utf-8");
    }
}
