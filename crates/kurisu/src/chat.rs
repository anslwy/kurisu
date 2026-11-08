//! `/web`: the chat-style UI behind a one-time `/web` Telegram link.
//!
//! Auth is entirely `remote.rs`'s: the link is single use, opening it shows a code,
//! the owner approves in Telegram and that browser gets the `ku_web` cookie. Every
//! handler below refuses without an approved cookie and then talks to the server
//! over its socket *as the user* — the same channel `ku` and `ku web` use, so
//! there is no second token and no second approval path.
//!
//!   GET  /_web/                          the app (cookie required)
//!   GET  /_web/api/sessions               sessions, plus the one to open by default
//!   GET  /_web/api/sessions/{id}/messages history (after / before / limit, tools included)
//!   POST /_web/api/sessions/{id}/send     {"text": "..."} — queued, never waits for the turn
//!   POST /_web/api/sessions/{id}/interrupt cancel the running turn server-side
//!   GET  /_web/stream                     SSE: hello, sessions, message, delta, tool, turn, resync, closed
//!
//! `/stream` here carries the same events as `ku web`'s `/api/v1/stream`
//! (`hello` / `sessions` / `message` / `resync`), driven by the same `changes`
//! op, because this listener lives inside the server process and `ku web` may
//! not be running at all.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::remote::Web;

const APP: &str = include_str!("../web/dist/chat.html");

pub fn router(web: Web) -> Router {
    Router::new()
        .route("/_web", get(|| async { axum::response::Redirect::permanent("/_web/") }))
        // Pairing lives in remote.rs, where every kind shares it: the one-time
        // link, and the state the wait page polls until Telegram says yes.
        .route("/_web/r/{token}", get(crate::remote::open_link))
        .route("/_web/state", get(crate::remote::state))
        .route("/_web/", get(app))
        // The Vite bundle for chat.html, built with base "./" so it lands here.
        .route("/_web/assets/{*path}", get(asset))
        .route("/_web/api/build", get(build_status))
        .route("/_web/api/sessions", get(sessions))
        .route("/_web/api/sessions/{id}/messages", get(messages))
        .route("/_web/api/sessions/{id}/send", post(send))
        .route("/_web/api/sessions/{id}/interrupt", post(interrupt))
        .route("/_web/terminal", get(terminal))
        .route("/_web/api/sessions/{id}/workspace/{action}", get(workspace))
        .route("/_web/stream", get(stream))
        .with_state(web)
}

/// The approved browser behind this request, or `None` (no/bad cookie, closed, denied).
async fn viewer(web: &Web, h: &HeaderMap) -> Option<std::sync::Arc<crate::server::Server>> {
    let server = web.server.upgrade()?;
    let cookie = crate::remote::cookie_of(h, "web")?;
    server.remote.touch_kind(&cookie, "web").await.then_some(server)
}

#[derive(Deserialize)]
struct TerminalQ { session: Option<String>, dir: Option<String> }
async fn terminal(State(web): State<Web>, h: HeaderMap, Query(q): Query<TerminalQ>, upgrade: axum::extract::ws::WebSocketUpgrade) -> Response {
    let Some(server) = viewer(&web, &h).await else { return refused() };
    if !crate::remote::same_origin(&h) { return StatusCode::FORBIDDEN.into_response(); }
    let mut dir = q.dir;
    if dir.is_none() {
        if let Some(id) = q.session {
            match call(&server, json!({"op":"status", "name":id})).await {
                Ok(s) => dir = s["cwd"].as_str().map(String::from),
                Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
            }
        }
    }
    if dir.as_ref().is_some_and(|d| !std::path::Path::new(d).is_dir()) {
        return (StatusCode::BAD_REQUEST, "Terminal directory does not exist").into_response();
    }
    let shell = match crate::remote::spawn_shell(&server.home(), dir.as_deref()) {
        Ok(s) => std::sync::Arc::new(s), Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let Some(cookie) = crate::remote::cookie_of(&h, "web") else { return refused() };
    upgrade.on_upgrade(move |sock| crate::remote::pump_owned(sock, shell, Some((server, cookie))))
}

#[derive(Deserialize)]
struct WorkspaceQ { #[serde(default)] path: String, #[serde(default)] hidden: bool }
async fn workspace(State(web): State<Web>, h: HeaderMap, AxPath((id, action)): AxPath<(String, String)>, Query(q): Query<WorkspaceQ>) -> Response {
    let Some(server) = viewer(&web, &h).await else { return refused() };
    match call(&server, json!({"op":"status", "name":id})).await {
        Ok(s) => match s["cwd"].as_str() {
            Some(cwd) => crate::workspace::response(cwd.to_owned(), action, q.path, q.hidden).await,
            None => (StatusCode::BAD_REQUEST, Json(json!({"error":"session has no workspace"}))).into_response(),
        },
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error":e}))).into_response(),
    }
}

fn refused() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "not paired: open a fresh /web link in Telegram"}))).into_response()
}

/// The Svelte bundle is hashed, so it is safe to cache for ever.
async fn asset(AxPath(path): AxPath<String>) -> Response {
    let Some((kind, bytes)) = crate::web::dist_asset(&format!("assets/{path}")) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        bytes,
    )
        .into_response()
}

fn json_page(v: Value) -> Response {
    secure(Json(v).into_response())
}

fn secure(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    h.insert("content-security-policy",
             "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'"
                 .parse().unwrap());
    r
}

// ------------------------------------------------------------------ API

async fn call(server: &crate::server::Server, req: Value) -> Result<Value, String> {
    crate::server::call(&server.socket(), req).await.map_err(|e| format!("{e:#}"))
}

fn app_html() -> String {
    let hash = crate::web::bundle_version(APP, "chat");
    let sha = env!("KURISU_BUILD_COMMIT");
    APP.replace("</head>", &format!("<meta name=\"ku-bundle\" content=\"{hash}\"><meta name=\"ku-git-sha\" content=\"{sha}\"></head>"))
}
#[derive(Deserialize)]
struct BuildQ { bundle: String, git_sha: String }
async fn build_status(State(web): State<Web>, h: HeaderMap, Query(q): Query<BuildQ>) -> Response {
    let Some(server) = viewer(&web, &h).await else { return refused() };
    let check = crate::doctor::bundle_check(&q.bundle, &q.git_sha, &crate::web::bundle_version(APP, "chat"), &server.version_info());
    secure(Json(check).into_response())
}

async fn app(State(web): State<Web>, h: HeaderMap) -> Response {
    match viewer(&web, &h).await {
        Some(_) => secure(Html(app_html()).into_response()),
        None => secure(Html("<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
<body style=\"font:16px system-ui;background:#fbf7fd;color:#3e3249;padding:24px\"><h2>Not paired</h2>\
<p>This link was used, denied or has expired. Send <code>/web</code> in Telegram for a new one.</p></body>").into_response()),
    }
}

#[derive(Deserialize)]
struct ListQ {
    archived: Option<bool>,
}

async fn sessions(State(web): State<Web>, h: HeaderMap, Query(q): Query<ListQ>) -> Response {
    let Some(server) = viewer(&web, &h).await else { return refused() };
    match call(&server, json!({"op": "peers", "include_archived": true})).await {
        Ok(list) => {
            let all = list.as_array().cloned().unwrap_or_default();
            let shown = visible(&all, q.archived.unwrap_or(false));
            json_page(json!({"sessions": shown, "default": pick_default(&all), "archived": q.archived.unwrap_or(false)}))
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct MsgQ {
    after: Option<i64>,
    before: Option<i64>,
    limit: Option<i64>,
}

async fn messages(State(web): State<Web>, h: HeaderMap, AxPath(id): AxPath<String>, Query(q): Query<MsgQ>) -> Response {
    let Some(server) = viewer(&web, &h).await else { return refused() };
    let limit = q.limit.unwrap_or(60).clamp(1, 200);
    let req = json!({"op": "history_page", "name": id, "after": q.after, "before": q.before, "limit": limit, "tools": true});
    match call(&server, req).await {
        Ok(v) => json_page(v),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

async fn send(State(web): State<Web>, h: HeaderMap, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    let Some(server) = viewer(&web, &h).await else { return refused() };
    let text = b["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "missing text"}))).into_response();
    }
    // wait:false: record the message and return; the turn streams over /_web/stream.
    match call(&server, json!({"op": "send", "name": id, "text": text, "wait": false, "mode": b["mode"]})).await {
        Ok(v) => json_page(v),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

async fn interrupt(State(web): State<Web>, h: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let Some(server) = viewer(&web, &h).await else { return refused() };
    match call(&server, json!({"op": "interrupt", "name": id})).await {
        Ok(v) => json_page(v),
        Err(e) => (StatusCode::CONFLICT, Json(json!({"error": e}))).into_response(),
    }
}

async fn stream(State(web): State<Web>, h: HeaderMap) -> Response {
    let Some(server) = viewer(&web, &h).await else { return refused() };
    let cookie = crate::remote::cookie_of(&h, "web").unwrap_or_default();
    let mut live = server.subscribe_events();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    let weak = web.server.clone();
    tokio::spawn(async move {
        // Keep the server alive while this connection is open; the loop below
        // still upgrades the Weak so a dropped server ends the stream cleanly.
        let _keep = server;
        let mut msg_seq = -1i64;
        // `changes` only returns messages when both cursors are set; we do not
        // forward audit rows, but the cursor has to be a real number.
        let mut audit_seq = 0i64;
        let mut last = String::new();
        let mut misses = 0u32;
        loop {
            let Some(srv) = weak.upgrade() else { break };
            if !srv.remote.touch_kind(&cookie, "web").await {
                let _ = tx.send(Ok(Event::default().event("closed").data("{}"))).await;
                break;
            }
            let r = async {
                let ch = crate::server::call(&srv.socket(), json!({"op": "changes", "msg_after": msg_seq, "audit_after": audit_seq})).await?;
                let list = crate::server::call(&srv.socket(), json!({"op": "peers", "include_archived": true})).await?;
                anyhow::Ok((ch, list))
            }.await;
            match r {
                Ok((ch, list)) => {
                    misses = 0;
                    let first = msg_seq < 0;
                    if first {
                        let hello = json!({"sessions": list, "default": pick_default(list.as_array().map(Vec::as_slice).unwrap_or(&[]))});
                        if tx.send(Ok(Event::default().event("hello").json_data(hello).unwrap_or_default())).await.is_err() {
                            break;
                        }
                        last = list.to_string();
                    } else {
                        for m in ch["messages"].as_array().into_iter().flatten() {
                            if tx.send(Ok(Event::default().event("message").json_data(m.clone()).unwrap_or_default())).await.is_err() {
                                return;
                            }
                        }
                        let ser = list.to_string();
                        if ser != last {
                            last = ser;
                            if tx.send(Ok(Event::default().event("sessions").json_data(list).unwrap_or_default())).await.is_err() {
                                return;
                            }
                        }
                    }
                    msg_seq = ch["msg_seq"].as_i64().unwrap_or(msg_seq);
                    audit_seq = ch["audit_seq"].as_i64().unwrap_or(audit_seq);
                }
                Err(_) => {
                    // The server went away: tell the page so it can say so instead of lying.
                    misses += 1;
                    if misses >= 3 {
                        let _ = tx.send(Ok(Event::default().event("resync").data("{\"server\":\"down\"}"))).await;
                        break;
                    }
                }
            }
            tokio::select! {
                // Live turn output: assistant text deltas, tool start/update,
                // and turn state. These never wait for the history poll below.
                ev = live.recv() => match ev {
                    Ok(v) => {
                        let name = match v.get("type").and_then(Value::as_str) {
                            Some("turn") => "turn",
                            Some("tool") => "tool",
                            Some("delivery") => "delivery",
                            _ => "delta",
                        };
                        if tx.send(Ok(Event::default().event(name).json_data(v).unwrap_or_default())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // The connection fell behind. Let EventSource reconnect and
                        // replay history from a fresh hello rather than leaving a hole.
                        let _ = tx.send(Ok(Event::default().event("resync").data("{\"reason\":\"lagged\"}"))).await;
                        break;
                    }
                    Err(_) => break,
                },
                _ = tokio::time::sleep(Duration::from_millis(700)) => {}
                _ = tx.closed() => break,
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}

// ------------------------------------------------------------------ selection

/// The sessions the sidebar shows. Archived ones stay reachable, just not by default.
pub fn visible(list: &[Value], archived: bool) -> Vec<Value> {
    list.iter().filter(|s| archived || s["lifecycle"] != json!("archived")).cloned().collect()
}

fn newer(a: &Value, b: &Value) -> bool {
    let at = |s: &Value| s["last"]["at"].as_i64().or_else(|| s["last_activity"].as_i64()).unwrap_or(0);
    at(a) > at(b)
}

/// Which session opens first: the most recently active live one, else any live
/// one, else the most recently active archived one (so a solo archived session
/// is still reachable). Never picks nothing when the list is not empty.
pub fn pick_default(list: &[Value]) -> Option<String> {
    let live: Vec<&Value> = list.iter().filter(|s| s["lifecycle"] != json!("archived")).collect();
    let pick = |xs: &[&Value]| xs.iter().copied().reduce(|a, b| if newer(b, a) { b } else { a }).and_then(|s| s["name"].as_str().map(String::from));
    pick(&live).or_else(|| pick(&list.iter().collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_routes_require_paired_browser() {
        let web = Web { server: std::sync::Weak::new(), kind: "web" };
        for action in ["list-dir", "read-file", "git-status", "git-diff"] {
            let response = workspace(State(web.clone()), HeaderMap::new(), AxPath(("session".into(), action.into())), Query(WorkspaceQ { path: String::new(), hidden: false })).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    fn s(name: &str, lifecycle: &str, last_at: i64) -> Value {
        json!({"name": name, "lifecycle": lifecycle, "last": {"at": last_at}})
    }

    #[test]
    fn served_bundle_provenance_detects_stale_pages() {
        let html = app_html();
        let bundle = crate::web::bundle_version(APP, "chat");
        assert!(html.contains(&format!("name=\"ku-bundle\" content=\"{bundle}\"")));
        assert!(html.contains(env!("KURISU_BUILD_COMMIT")));
        let current = crate::version::info();
        let same = crate::doctor::bundle_check(&bundle, env!("KURISU_BUILD_COMMIT"), &bundle, &current);
        assert_eq!(serde_json::to_value(same).unwrap()["status"], "OK");
        for (hash, sha) in [("old-bundle", env!("KURISU_BUILD_COMMIT")), (bundle.as_str(), "old-git-sha")] {
            let stale = crate::doctor::bundle_check(hash, sha, &bundle, &current);
            assert_eq!(serde_json::to_value(stale).unwrap()["status"], "FAIL");
        }
    }
    #[test]
    fn app_is_the_svelte_build_and_its_bundle_is_embedded() {
        assert!(APP.contains("id=\"app\""), "the Svelte mount point is in the page");
        assert!(!APP.contains("<style>"), "CSS comes from the Vite bundle, not inline");
        // chat.html is served at /_web/, so its assets must be relative and exist.
        let js = APP
            .split("./assets/")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("chat.html references ./assets/<hash>.js");
        assert!(crate::web::dist_asset(&format!("assets/{js}")).is_some(), "assets/{js} is embedded");
    }

    #[test]
    fn selection_prefers_the_newest_live_session() {
        let list = vec![s("old", "active", 10), s("new", "active", 30), s("arch", "archived", 99)];
        assert_eq!(pick_default(&list), Some("new".into()));
        assert_eq!(visible(&list, false).len(), 2);
        assert_eq!(visible(&list, true).len(), 3);
    }

    #[test]
    fn selection_falls_back_to_archived_and_handles_empty() {
        assert_eq!(pick_default(&[]), None);
        let only_archived = vec![s("gone", "archived", 5)];
        assert_eq!(pick_default(&only_archived), Some("gone".into()));
        // A live session with no timestamps still wins over an archived one.
        let mixed = vec![json!({"name": "live", "lifecycle": "active"}), s("gone", "archived", 999)];
        assert_eq!(pick_default(&mixed), Some("live".into()));
        // Missing timestamps must not panic and still pick something.
        let untimed = vec![json!({"name": "a", "lifecycle": "active"}), json!({"name": "b", "lifecycle": "paused"})];
        assert!(pick_default(&untimed).is_some());
    }
}
