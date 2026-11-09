//! P3 acceptance tests use a real in-process HTTP MCP/authorization server, no provider.
use super::*;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

#[derive(Default)]
struct FakeAs {
    base: String,
    probes: std::sync::Mutex<Vec<String>>,
    forms: std::sync::Mutex<Vec<(String, String)>>,
    refresh_status: AtomicU16,
    reject_rotated: AtomicBool,
    accept_old: AtomicBool,
    omit_refresh: AtomicBool,
    pause_refresh: AtomicBool,
    entered: Notify,
    release: Notify,
}

struct Provider {
    state: Arc<FakeAs>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn provider() -> Provider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let state = Arc::new(FakeAs {
        base: format!("http://{}", listener.local_addr().unwrap()),
        refresh_status: AtomicU16::new(200),
        ..Default::default()
    });
    let app = Router::new()
        .route("/mcp", get(|State(s): State<Arc<FakeAs>>, headers: HeaderMap| async move {
            let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            s.probes.lock().unwrap().push(auth.clone());
            let status = if (auth == "Bearer rotated-access" && !s.reject_rotated.load(Ordering::SeqCst))
                || (auth == "Bearer old-access" && s.accept_old.load(Ordering::SeqCst)) {
                StatusCode::OK
            } else { StatusCode::UNAUTHORIZED };
            (status, [("www-authenticate", format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"", s.base))], "")
        }))
        .route("/.well-known/oauth-protected-resource", get(|State(s): State<Arc<FakeAs>>| async move {
            Json(json!({"resource": format!("{}/mcp", s.base), "authorization_servers": [s.base]}))
        }))
        .route("/.well-known/oauth-authorization-server", get(|State(s): State<Arc<FakeAs>>| async move {
            Json(json!({"issuer": s.base, "authorization_endpoint": format!("{}/authorize", s.base),
                "token_endpoint": format!("{}/token", s.base), "registration_endpoint": format!("{}/register", s.base),
                "code_challenge_methods_supported": ["S256"]}))
        }))
        .route("/register", post(|| async { Json(json!({"client_id": "p3-client"})) }))
        .route("/token", post(|State(s): State<Arc<FakeAs>>, headers: HeaderMap, body: String| async move {
            s.forms.lock().unwrap().push((headers["content-type"].to_str().unwrap().to_string(), body.clone()));
            if body.contains("grant_type=authorization_code") {
                return (StatusCode::OK, Json(json!({"access_token": "old-access", "refresh_token": "old-refresh", "expires_in": 3600, "token_type": "Bearer"})));
            }
            if s.pause_refresh.load(Ordering::SeqCst) {
                s.entered.notify_one();
                s.release.notified().await;
            }
            let status = StatusCode::from_u16(s.refresh_status.load(Ordering::SeqCst)).unwrap();
            let mut value = if status.is_success() {
                json!({"access_token": "rotated-access", "expires_in": 7200, "token_type": "Bearer"})
            } else {
                // Provider errors can echo secrets; they must not reach chat/logs.
                json!({"error": "invalid_grant", "error_description": "old-refresh p3-client-secret"})
            };
            if status.is_success() && !s.omit_refresh.load(Ordering::SeqCst) {
                value["refresh_token"] = json!("rotated-refresh");
            }
            (status, Json(value))
        })).with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Provider { state, task }
}

async fn h(s: &Arc<Server>, request: Value) -> Value {
    Box::pin(s.handle(request)).await.unwrap()
}

async fn setup(p: &Provider, expires_at: i64) -> (tempfile::TempDir, Arc<Server>, String) {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(
        Server::new(Paths {
            home: dir.path().to_path_buf(),
        })
        .unwrap(),
    );
    let id = h(
        &s,
        json!({"op": "create", "name": "dev", "cwd": dir.path().join("work")}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h(&s, json!({"op": "mcp_set", "name": "dev", "server": "probe", "config": {
        "type": "http", "url": format!("{}/mcp", p.state.base), "headers": {"X-Workspace": "kept"}
    }})).await;
    s.paths.write_mcp_oauth_token(&id, "probe", &json!({
        "issuer": p.state.base, "token_endpoint": format!("{}/token", p.state.base),
        "client_id": "p3-client", "client_secret": "p3-client-secret", "scope": "tools:read",
        "access_token": "old-access", "refresh_token": "old-refresh", "expires_at": expires_at
    })).unwrap();
    (dir, s, id)
}

async fn assert_injected(s: &Arc<Server>, id: &str, access: &str) {
    let session = s.store.lock().await.get(id).unwrap().unwrap();
    s.claude_opts(&session).await.unwrap();
    let path = s.paths.session_dir(id).join("mcp.json");
    let claude: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        claude["mcpServers"]["probe"]["headers"]["Authorization"],
        format!("Bearer {access}")
    );
    assert_eq!(
        claude["mcpServers"]["probe"]["headers"]["X-Workspace"],
        "kept"
    );
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let codex = s.codex_extra(&session).await.unwrap();
    assert_eq!(
        codex["config"]["mcp_servers.probe"]["http_headers"]["Authorization"],
        format!("Bearer {access}")
    );
    assert!(codex["config"]["mcp_servers.probe"]
        .get("headers")
        .is_none());
    // OpenCode and agy share this exact ACP session/new payload.
    let acp = s.opencode_mcp(&session).await.unwrap();
    let probe = acp
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "probe")
        .unwrap();
    assert!(probe["headers"]
        .as_array()
        .unwrap()
        .contains(&json!({"name": "Authorization", "value": format!("Bearer {access}")})));
}

#[tokio::test]
async fn oauth_refresh_on_401_retries_once_and_rotates_token() {
    let p = provider().await;
    let (_dir, s, id) = setup(&p, now() + 3600).await;
    let before = s.toolset_state(&id).await.unwrap().0;
    s.store.lock().await.set_launch_fp(&id, &before).unwrap();
    s.live.lock().await.insert(id.clone());
    assert!(s.prepare_mcp_oauth(&id).await.is_empty());
    assert_eq!(
        *p.state.probes.lock().unwrap(),
        ["Bearer old-access", "Bearer rotated-access"]
    );
    let forms = p.state.forms.lock().unwrap().clone();
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0].0, crate::oauth::FORM_CONTENT_TYPE);
    let url = reqwest::Url::parse(&format!("http://form.invalid/?{}", forms[0].1)).unwrap();
    let fields: HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(fields["grant_type"], "refresh_token");
    assert_eq!(fields["refresh_token"], "old-refresh");
    assert_eq!(fields["client_id"], "p3-client");
    assert_eq!(fields["client_secret"], "p3-client-secret");
    assert_eq!(fields["resource"], format!("{}/mcp", p.state.base));
    let path = s.paths.mcp_oauth_token(&id, "probe");
    let token = crate::oauth::Token::load(&path).unwrap().unwrap();
    assert_eq!(token.access_token(), Some("rotated-access"));
    assert_eq!(token.0["refresh_token"], "rotated-refresh");
    assert_eq!(token.0["scope"], "tools:read");
    assert!(token.expires_at().unwrap() >= now() + 7100);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(!path.with_extension("tmp").exists());
    assert_ne!(s.toolset_state(&id).await.unwrap().0, before);
    let list = h(&s, json!({"op": "mcp_list", "name": "dev"})).await;
    assert_eq!(
        list["loaded"], false,
        "rotation invalidates an already running harness"
    );
    assert_eq!(list["servers"][0]["auth_state"], "authorized");
    assert_injected(&s, &id, "rotated-access").await;
}

#[tokio::test]
async fn oauth_valid_token_without_expiry_does_not_refresh() {
    let p = provider().await;
    p.state.accept_old.store(true, Ordering::SeqCst);
    let (_dir, s, id) = setup(&p, now() + 3600).await;
    let path = s.paths.mcp_oauth_token(&id, "probe");
    let mut token = crate::oauth::Token::load(&path).unwrap().unwrap();
    token.0.as_object_mut().unwrap().remove("expires_at");
    s.paths
        .write_mcp_oauth_token(&id, "probe", &token.0)
        .unwrap();
    let before = s.toolset_state(&id).await.unwrap().0;
    for _ in 0..2 {
        assert!(s.prepare_mcp_oauth(&id).await.is_empty());
    }
    assert_eq!(p.state.probes.lock().unwrap().len(), 2);
    assert!(p.state.forms.lock().unwrap().is_empty());
    assert_eq!(s.toolset_state(&id).await.unwrap().0, before);
    assert_injected(&s, &id, "old-access").await;
}

#[tokio::test]
async fn oauth_harness_switch_copies_private_tokens_and_url_change_clears_them() {
    let p = provider().await;
    let (_dir, s, id) = setup(&p, now() + 3600).await;
    let old = s.store.lock().await.get(&id).unwrap().unwrap();
    let result = s
        .do_reharness(&id, &old, "codex", None, false, false)
        .await
        .unwrap();
    let new = result["new"]["id"].as_str().unwrap();
    let path = s.paths.mcp_oauth_token(new, "probe");
    assert_eq!(
        crate::oauth::Token::load(&path).unwrap().unwrap().0["refresh_token"],
        "old-refresh"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_injected(&s, new, "old-access").await;
    h(
        &s,
        json!({"op": "mcp_set", "name": "dev", "server": "probe", "config": {
            "type": "http", "url": "https://different.example/mcp"
        }}),
    )
    .await;
    assert!(
        !path.exists(),
        "old bearer credentials must not reach a different resource"
    );
    let rows = s.runtime_mcp_rows(new).await.unwrap();
    assert!(rows[0].1.get("headers").is_none());
}

#[tokio::test]
async fn oauth_refresh_before_expiry_preserves_unrotated_refresh_token() {
    let p = provider().await;
    p.state.omit_refresh.store(true, Ordering::SeqCst);
    let (_dir, s, id) = setup(&p, now() + 30).await;
    assert!(s.prepare_mcp_oauth(&id).await.is_empty());
    assert_eq!(*p.state.probes.lock().unwrap(), ["Bearer rotated-access"]);
    assert_eq!(p.state.forms.lock().unwrap().len(), 1);
    assert_eq!(
        crate::oauth::Token::load(&s.paths.mcp_oauth_token(&id, "probe"))
            .unwrap()
            .unwrap()
            .0["refresh_token"],
        "old-refresh"
    );
}

#[tokio::test]
async fn oauth_refresh_failure_transitions_to_needs_auth_without_crash() {
    for (status, reject_retry) in [(400, false), (401, false), (200, true)] {
        let p = provider().await;
        p.state.refresh_status.store(status, Ordering::SeqCst);
        p.state.reject_rotated.store(reject_retry, Ordering::SeqCst);
        let (_dir, s, id) = setup(&p, now() + 3600).await;
        let notices = s.prepare_mcp_oauth(&id).await;
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("needs authorization"));
        assert!(!notices[0].contains("old-refresh"));
        assert_eq!(
            p.state.forms.lock().unwrap().len(),
            1,
            "never loop refreshes"
        );
        assert_eq!(
            p.state.probes.lock().unwrap().len(),
            if reject_retry { 2 } else { 1 }
        );
        let list = h(&s, json!({"op": "mcp_list", "name": "dev"})).await;
        assert_eq!(list["servers"][0]["auth_state"], "needs-auth");
        s.live.lock().await.insert(id.clone());
        let fp = s.toolset_state(&id).await.unwrap().0;
        s.store.lock().await.set_launch_fp(&id, &fp).unwrap();
        let list = h(&s, json!({"op": "mcp_list", "name": "dev"})).await;
        assert_eq!(
            list["servers"][0]["loaded"], false,
            "an omitted server is never reported loaded"
        );
        let session = s.store.lock().await.get(&id).unwrap().unwrap();
        s.claude_opts(&session).await.unwrap();
        assert!(s.codex_extra(&session).await.unwrap()["config"]
            .get("mcp_servers.probe")
            .is_none());
        let acp = s.opencode_mcp(&session).await.unwrap();
        assert_eq!(
            acp.as_array().unwrap().len(),
            1,
            "the built-in MCP remains available"
        );
        assert!(
            s.prepare_mcp_oauth(&id).await.is_empty(),
            "invalid credentials are not retried next turn"
        );
    }
}

#[tokio::test]
async fn oauth_transient_refresh_failure_retries_next_turn() {
    let p = provider().await;
    p.state.refresh_status.store(503, Ordering::SeqCst);
    let (_dir, s, id) = setup(&p, now() - 10).await;
    assert!(s.prepare_mcp_oauth(&id).await[0].contains("temporarily unavailable"));
    let token = crate::oauth::Token::load(&s.paths.mcp_oauth_token(&id, "probe"))
        .unwrap()
        .unwrap();
    assert_eq!(token.0["refresh_token"], "old-refresh");
    assert!(!s.runtime_mcp_rows(&id).await.unwrap()[0].2);
    p.state.refresh_status.store(200, Ordering::SeqCst);
    assert!(s.prepare_mcp_oauth(&id).await.is_empty());
    assert_injected(&s, &id, "rotated-access").await;
}

#[tokio::test]
async fn oauth_refresh_cannot_resurrect_disabled_or_removed_credentials() {
    for op in ["mcp_set", "mcp_remove"] {
        let p = provider().await;
        p.state.pause_refresh.store(true, Ordering::SeqCst);
        let (_dir, s, id) = setup(&p, now() - 10).await;
        let worker = {
            let s = s.clone();
            let id = id.clone();
            tokio::spawn(async move { s.prepare_mcp_oauth(&id).await })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            p.state.entered.notified(),
        )
        .await
        .unwrap();
        h(
            &s,
            json!({"op": op, "name": "dev", "server": "probe", "enabled": false}),
        )
        .await;
        p.state.release.notify_one();
        worker.await.unwrap();
        assert!(!s.paths.mcp_oauth_token(&id, "probe").exists());
    }
}

#[tokio::test]
async fn oauth_tokens_and_headers_redacted_everywhere() {
    let p = provider().await;
    let (_dir, s, id) = setup(&p, now() - 10).await;
    assert!(s.prepare_mcp_oauth(&id).await.is_empty());
    let token = crate::oauth::Token::load(&s.paths.mcp_oauth_token(&id, "probe"))
        .unwrap()
        .unwrap();
    let list = h(&s, json!({"op": "mcp_list", "name": "dev"})).await;
    let audit = h(&s, json!({"op": "audit"})).await;
    let db = rusqlite::Connection::open(s.paths.db()).unwrap();
    let stored: String = db
        .query_row(
            "SELECT config FROM session_mcp WHERE session=?1 AND name='probe'",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    let diagnostics = format!("{token:?} {list} {audit} {stored}");
    for secret in [
        "old-access",
        "old-refresh",
        "rotated-access",
        "rotated-refresh",
        "p3-client-secret",
        "Bearer",
    ] {
        assert!(
            !diagnostics.contains(secret),
            "secret reached an observable surface"
        );
    }
    // A user-supplied header wins; stdio and disabled entries never receive OAuth headers.
    h(&s, json!({"op": "mcp_set", "name": "dev", "server": "probe", "config": {
        "type": "http", "url": format!("{}/mcp", p.state.base), "headers": {"authorization": "fixed-secret"}
    }})).await;
    let rows = s.runtime_mcp_rows(&id).await.unwrap();
    assert_eq!(rows[0].1["headers"]["authorization"], "fixed-secret");
    assert!(rows[0].1["headers"].get("Authorization").is_none());
}

#[tokio::test]
async fn oauth_e2e_fake_as_in_process() {
    let p = provider().await;
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(
        Server::new(Paths {
            home: dir.path().to_path_buf(),
        })
        .unwrap(),
    );
    let id = h(
        &s,
        json!({"op": "create", "name": "dev", "cwd": dir.path().join("work")}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let connect = h(
        &s,
        json!({"op": "mcp_connect", "session": "dev", "name": "probe",
        "url": format!("{}/mcp", p.state.base), "redirect_uri": "http://127.0.0.1/callback"}),
    )
    .await;
    let callback = h(&s, json!({"op": "mcp_callback", "caller": id,
        "url": format!("http://127.0.0.1/callback?code=p3-code&state={}", connect["state"].as_str().unwrap())})).await;
    assert_eq!(callback["auth_state"], "authorized");
    p.state.probes.lock().unwrap().clear();
    assert!(s.prepare_mcp_oauth(&id).await.is_empty());
    // Add a non-secret header to exercise identical serialization across all harnesses.
    let mut config = s.store.lock().await.mcp_list(&id).unwrap()[0].1.clone();
    config["headers"] = json!({"X-Workspace": "kept"});
    h(
        &s,
        json!({"op": "mcp_set", "name": "dev", "server": "probe", "config": config}),
    )
    .await;
    assert_injected(&s, &id, "rotated-access").await;
    h(
        &s,
        json!({"op": "mcp_remove", "name": "dev", "server": "probe"}),
    )
    .await;
    assert!(!s.paths.mcp_oauth_token(&id, "probe").exists());
    assert!(!s.paths.mcp_oauth_pending(&id, "probe").exists());
    assert!(s.runtime_mcp_rows(&id).await.unwrap().is_empty());
}
