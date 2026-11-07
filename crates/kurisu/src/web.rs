//! `ku web`: a small local web UI. It is a client of the Kurisu server socket
//! (like the CLI), served by axum on loopback with a random token.
//!
//! Security: binds 127.0.0.1 by default; every /api call needs the token
//! (`x-kurisu-token` header, taken from the URL fragment by the page, so it is
//! never sent in a request line or a Referer); requests whose Host header is
//! not the bound loopback address are refused (DNS-rebinding guard).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::paths::Paths;

const INDEX: &str = include_str!("web/index.html");

#[derive(Clone)]
struct Ctx {
    paths: Paths,
    token: Arc<String>,
    allowed_hosts: Arc<Vec<String>>,
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn check(ctx: &Ctx, headers: &HeaderMap) -> Result<(), Response> {
    let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("");
    if !ctx.allowed_hosts.is_empty() && !ctx.allowed_hosts.iter().any(|h| h == host) {
        return Err((StatusCode::FORBIDDEN, "unexpected Host header").into_response());
    }
    let tok = headers.get("x-kurisu-token").and_then(|h| h.to_str().ok()).unwrap_or("");
    if !ct_eq(tok, &ctx.token) {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or wrong token"}))).into_response());
    }
    Ok(())
}

async fn call(ctx: &Ctx, req: Value) -> Response {
    let r = async {
        crate::ensure_server(&ctx.paths).await?;
        crate::server::call(&ctx.paths.socket(), req).await
    }
    .await;
    match r {
        Ok(v) => Json(json!({"ok": v})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": format!("{e:#}")}))).into_response(),
    }
}

macro_rules! guard {
    ($ctx:expr, $h:expr) => {
        if let Err(r) = check(&$ctx, &$h) {
            return r;
        }
    };
}

async fn index(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    let host = h.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ctx.allowed_hosts.is_empty() && !ctx.allowed_hosts.iter().any(|a| a == host) {
        return (StatusCode::FORBIDDEN, "unexpected Host header").into_response();
    }
    ([("cache-control", "no-store"), ("x-frame-options", "DENY"), ("referrer-policy", "no-referrer")], Html(INDEX)).into_response()
}

async fn status(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    guard!(ctx, h);
    let hosts = json!([crate::hostcheck::claude().await.to_string(), crate::hostcheck::codex().await.to_string()]);
    let ping = async {
        crate::ensure_server(&ctx.paths).await?;
        crate::server::call(&ctx.paths.socket(), json!({"op": "ping"})).await
    }
    .await
    .unwrap_or(Value::Null);
    Json(json!({"ok": {"clis": hosts, "server": ping, "home": ctx.paths.home}})).into_response()
}

#[derive(Deserialize)]
struct ListQ {
    all: Option<bool>,
}

async fn sessions(State(ctx): State<Ctx>, h: HeaderMap, Query(q): Query<ListQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "peers", "include_archived": q.all.unwrap_or(false)})).await
}

#[derive(Deserialize)]
struct HistQ {
    after: Option<i64>,
}

async fn history(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Query(q): Query<HistQ>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "history", "name": id, "after": q.after.unwrap_or(0)})).await
}

async fn one(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "status", "name": id})).await
}

async fn send(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "send", "name": id, "text": b["text"]})).await
}

async fn create(State(ctx): State<Ctx>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "create", "name": b["name"], "description": b.get("description"), "model": b.get("model"), "role": "worker"})).await
}

async fn lead(State(ctx): State<Ctx>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "lead", "text": b["text"], "model": b.get("model")})).await
}

async fn update(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    guard!(ctx, h);
    let mut fields = serde_json::Map::new();
    for f in ["task", "description", "model", "effort", "role", "lifecycle", "name"] {
        if let Some(v) = b.get(f).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            fields.insert(f.into(), json!(v));
        }
    }
    call(&ctx, json!({"op": "update", "name": id, "fields": fields})).await
}

async fn stop(State(ctx): State<Ctx>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "stop", "name": id})).await
}

async fn audit(State(ctx): State<Ctx>, h: HeaderMap) -> Response {
    guard!(ctx, h);
    call(&ctx, json!({"op": "audit"})).await
}

pub fn router(paths: Paths, token: String, allowed_hosts: Vec<String>) -> Router {
    let ctx = Ctx { paths, token: Arc::new(token), allowed_hosts: Arc::new(allowed_hosts) };
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/sessions", get(sessions).post(create))
        .route("/api/sessions/{id}", get(one))
        .route("/api/sessions/{id}/history", get(history))
        .route("/api/sessions/{id}/send", post(send))
        .route("/api/sessions/{id}/update", post(update))
        .route("/api/sessions/{id}/stop", post(stop))
        .route("/api/lead", post(lead))
        .route("/api/audit", get(audit))
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
    let shown = if bound.ip().is_loopback() { format!("127.0.0.1:{}", bound.port()) } else { bound.to_string() };
    println!("Kurisu web UI (KURISU_HOME={})", paths.home.display());
    println!("  open: http://{shown}/#token={token}");
    println!("  Ctrl-C to stop the web UI (sessions keep running; `ku shutdown` stops them)");
    axum::serve(listener, router(paths, token, allowed))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_and_host_checks() {
        let ctx = Ctx {
            paths: Paths { home: std::env::temp_dir() },
            token: Arc::new("t0k3n-abcdefghijkl".into()),
            allowed_hosts: Arc::new(vec!["127.0.0.1:8787".into()]),
        };
        let mut h = HeaderMap::new();
        h.insert("host", "127.0.0.1:8787".parse().unwrap());
        assert!(check(&ctx, &h).is_err(), "no token");
        h.insert("x-kurisu-token", "wrong-token-xxxxxxx".parse().unwrap());
        assert!(check(&ctx, &h).is_err(), "wrong token");
        h.insert("x-kurisu-token", "t0k3n-abcdefghijkl".parse().unwrap());
        assert!(check(&ctx, &h).is_ok());
        h.insert("host", "evil.example:8787".parse().unwrap());
        assert!(check(&ctx, &h).is_err(), "rebinding guard");
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
        assert!(INDEX.contains("x-kurisu-token") && !INDEX.contains("http://"), "page uses the header, no external loads");
    }
}
