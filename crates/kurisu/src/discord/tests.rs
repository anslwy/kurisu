use super::{rest::Rest, Config};
use crate::channels::{
    dispatch::{Dispatcher, Incoming},
    state::State,
    stream::Relay,
    text::{self, Chunk},
    Address,
};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State as HttpState},
    http::StatusCode,
    response::Response,
    routing::any,
    Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, String, Value, std::time::Instant)>>,
    messages: Mutex<HashMap<String, String>>,
    channels: Mutex<HashMap<String, Value>>,
    history: Mutex<HashMap<String, Vec<Value>>>,
    mode: AtomicUsize,
    count: AtomicUsize,
}
struct Mock {
    api: String,
    wire: Arc<Rest>,
    data: Arc<Fake>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn endpoint(HttpState(s): HttpState<Arc<Fake>>, request: Request) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query=request.uri().query().unwrap_or("").to_string();
    let body = to_bytes(request.into_body(), 30 * 1024 * 1024)
        .await
        .unwrap();
    let payload: Value =
        serde_json::from_slice(&body).unwrap_or_else(|_| json!(String::from_utf8_lossy(&body)));
    s.calls.lock().unwrap().push((
        method.clone(),
        path.clone(),
        payload.clone(),
        std::time::Instant::now(),
    ));
    let (status, result) = if path == "/download" {
        return Response::builder()
            .status(200)
            .body(Body::from("uploaded bytes"))
            .unwrap();
    } else if path == "/users/@me" {
        (200, json!({"id":"app","bot":true}))
    } else if path.starts_with("/applications/") {
        (200, payload.clone())
    } else if path.ends_with("/sendMessage") {
        (200, json!({"ok":true,"result":{"message_id":99}}))
    } else if path.contains("/callback") {
        (204, json!({}))
    } else if method=="GET" && path.ends_with("/messages") {
        if s.mode.load(Ordering::SeqCst)==6 {(500,json!({}))}else{
            let destination=path.split('/').nth(2).unwrap();
            let before=query.split('&').find_map(|s|s.strip_prefix("before=")).and_then(|s|s.parse::<u64>().ok()).unwrap_or(u64::MAX);
            let mut rows=s.history.lock().unwrap().get(destination).cloned().unwrap_or_default();
            rows.retain(|v|v["id"].as_str().unwrap().parse::<u64>().unwrap()<before);
            rows.sort_by_key(|v|std::cmp::Reverse(v["id"].as_str().unwrap().parse::<u64>().unwrap()));
            rows.truncate(100);(200,json!(rows))
        }
    } else if method=="GET" && path.starts_with("/channels/") && !path.contains("/messages") {
        let id=path.rsplit('/').next().unwrap();
        (200,s.channels.lock().unwrap().get(id).cloned().unwrap_or(json!({"id":id,"guild_id":"guild","type":0})))
    } else if method=="POST" && path.ends_with("/threads") {
        if s.mode.load(Ordering::SeqCst)==4 {(200,json!({}))}else{
            let id=format!("private-{}",s.count.fetch_add(1,Ordering::SeqCst));
            let parent=path.split('/').nth(2).unwrap();
            let channel=json!({"id":id,"guild_id":"guild","parent_id":parent,"type":12,"thread_metadata":{"invitable":false}});
            s.channels.lock().unwrap().insert(id,channel.clone());(200,channel)
        }
    } else if method=="PUT" && path.contains("/thread-members/") {
        if s.mode.load(Ordering::SeqCst)==5 {(403,json!({"message":"invite forbidden"}))}else{(204,json!({}))}
    } else if method == "POST" && path.ends_with("/messages") {
        let mode = s.mode.load(Ordering::SeqCst);
        if mode == 1 {
            (
                403,
                json!({"code":50007,"message":"Cannot send messages to this user"}),
            )
        } else if mode == 2 {
            s.mode.store(0, Ordering::SeqCst);
            (429, json!({"retry_after":0.05,"global":true}))
        } else if mode == 3 {
            (200, json!({}))
        } else {
            let id = (s.count.fetch_add(1, Ordering::SeqCst) + 1).to_string();
            s.messages.lock().unwrap().insert(
                id.clone(),
                payload["content"].as_str().unwrap_or("attachment").into(),
            );
            (200, json!({"id":id}))
        }
    } else if method == "PATCH" {
        if s.mode.load(Ordering::SeqCst) == 7 {
            (500, json!({"message":"simulated progress edit failure"}))
        } else {
            let id = path.rsplit('/').next().unwrap().to_string();
            if let Some(content) = payload["content"].as_str() {
                s.messages
                    .lock()
                    .unwrap()
                    .insert(id.clone(), content.into());
            }
            (200, json!({"id":id}))
        }
    } else {
        (200, json!({}))
    };
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .header("content-type", "application/json")
        .body(Body::from(result.to_string()))
        .unwrap()
}
async fn mock(interval: Duration) -> Mock {
    let data = Arc::new(Fake::default());
    let app = Router::new()
        .fallback(any(endpoint))
        .with_state(data.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut wire = Rest::new("fake-token-NEVER-LOG-ME".into(), api.clone()).unwrap();
    wire.interval = interval;
    Mock {
        api,
        wire: Arc::new(wire),
        data,
        task,
    }
}
async fn server() -> (tempfile::TempDir, Arc<crate::server::Server>, String) {
    let dir = tempfile::tempdir().unwrap();
    let server = Arc::new(
        crate::server::Server::new(crate::paths::Paths {
            home: dir.path().join("home"),
        })
        .unwrap(),
    );
    server.test_no_turns();
    let s = server
        .test_call(json!({"op":"create","name":"alice","harness":"claude","role":"worker"}))
        .await
        .unwrap();
    let sid = s["id"].as_str().unwrap().to_string();
    server.test_catalogue(json!({"clis":[{"name":"claude","installed":true},{"name":"codex","installed":false},{"name":"opencode","installed":false},{"name":"agy","installed":false}]})).await;
    (dir, server, sid)
}
fn incoming(id: &str, text: &str) -> Incoming {
    Incoming {
        id: id.into(),
        user: "user".into(),
        destination: "dm".into(),
        guild: None,
        text: text.into(),
        attachments: vec![],
        action: None,
    }
}
fn address() -> Address {
    Address {
        channel: "discord".into(),
        destination: "dm".into(),
        person: "alice".into(),
    }
}
fn pair(server: &crate::server::Server, sid: &str) {
    let code = server
        .channels
        .mint("alice", sid, Some("user"), 300)
        .unwrap();
    assert_eq!(
        server.channels.redeem(&code, "user", "dm").unwrap(),
        "alice"
    );
}

#[test]
fn pairing_is_one_use_expires_and_binds_an_explicit_identity() {
    let dir = tempfile::tempdir().unwrap();
    let state = State::open(dir.path()).unwrap();
    let code = state
        .mint("alice", "same-existing-session", Some("123"), 300)
        .unwrap();
    assert!(state.redeem(&code, "attacker", "dm").is_err());
    assert!(state.person("discord", "attacker").unwrap().is_none());
    state.redeem(&code, "123", "dm").unwrap();
    assert!(state.redeem(&code, "123", "dm").is_err());
    assert_eq!(
        state.session("alice").unwrap().as_deref(),
        Some("same-existing-session")
    );
    let expired = state.mint("bob", "other", None, -1).unwrap();
    assert!(state.redeem(&expired, "456", "other-dm").is_err());
    drop(state);
    let state = State::open(dir.path()).unwrap();
    assert_eq!(
        state.person("discord", "123").unwrap().as_deref(),
        Some("alice")
    );
}

#[test]
fn legacy_telegram_owner_scopes_import_and_link_without_modifying_old_files() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = json!({"token":"not-a-real-token","chat_id":"42","enabled":true});
    std::fs::create_dir(dir.path().join("telegram")).unwrap();
    let legacy =
        json!({"session_id":"existing-lead","scopes":{"chat:-3:5":"topic-session"},"offset":77});
    std::fs::write(dir.path().join("telegram.json"), cfg.to_string()).unwrap();
    std::fs::write(dir.path().join("telegram/state.json"), legacy.to_string()).unwrap();
    let state = State::open(dir.path()).unwrap();
    assert_eq!(
        state.person("telegram", "42").unwrap().as_deref(),
        Some("owner")
    );
    assert_eq!(
        state.scope("chat:-3:5").unwrap().as_deref(),
        Some("topic-session")
    );
    let code = state.mint("owner", "existing-lead", None, 300).unwrap();
    state.redeem(&code, "discord-owner", "dm").unwrap();
    state.select("owner", "new-session").unwrap();
    state.import_telegram(dir.path()).unwrap();
    assert_eq!(
        state.session("owner").unwrap().as_deref(),
        Some("new-session")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("telegram/state.json")).unwrap(),
        legacy.to_string()
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(dir.path().join("channels.sqlite3"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn chunking_keeps_urls_code_and_unicode_whole_with_lossless_oversize_attachment() {
    let absurd = format!("https://example.com/oauth?state={}", "x".repeat(12000));
    let source = format!("before {absurd} after");
    let chunks = text::discord(&source);
    assert!(chunks.contains(&Chunk::File(absurd.clone())));
    let code = format!("`{} words inside code`", "a".repeat(1500));
    let chunks = text::discord(&format!("{} {code} after", "prose ".repeat(200)));
    assert!(chunks
        .iter()
        .any(|c| matches!(c,Chunk::Text(t) if t.contains(&code))));
    let chunks = text::discord(&"😀 ".repeat(1500));
    assert!(chunks
        .iter()
        .all(|c| matches!(c,Chunk::Text(t) if t.encode_utf16().count()<=2000)));
    assert_eq!(
        text::split_text(&"z".repeat(5000), 3900),
        vec!["z".repeat(5000)]
    );
    let link = "[a link label with spaces](https://example.com/a(path))";
    assert!(text::discord(&format!("{} {link}", "a ".repeat(995)))
        .iter()
        .any(|c| matches!(c,Chunk::Text(t) if t.contains(link))));
}

#[test]
fn streamed_tail_and_final_row_preserve_message_identity_across_restart() {
    let mut relay = Relay::default();
    relay.delta("The person is still allowed ", Some("m1"));
    assert_eq!(
        Relay::visible(&relay.segments[0]),
        "The person is still allowed"
    );
    relay = serde_json::from_value(serde_json::to_value(relay).unwrap()).unwrap();
    relay.delta("in.", Some("m1"));
    relay.boundary();
    relay.row(10, "The person is still allowed in.");
    assert_eq!(relay.segments.len(), 1);
    assert_eq!(relay.segments[0].text, "The person is still allowed in.");
    relay.delta("Found it.", Some("m2"));
    relay.boundary();
    relay.row(11, "Found it.");
    assert_eq!(relay.segments.len(), 2);
    relay.row(11, "Found it.");
    assert_eq!(relay.segments.len(), 2);
}

#[test]
fn config_and_errors_redact_bot_credentials() {
    let cfg = Config {
        token: "fake-token-NEVER-LOG-ME".into(),
        guild: "123".into(),
        enabled: true,
    };
    assert!(!format!("{cfg:?}").contains(&cfg.token));
    let wire = Rest::new(cfg.token.clone(), "http://127.0.0.1:1".into()).unwrap();
    assert_eq!(wire.safe(&cfg.token), "[REDACTED]");
    let commands = super::gateway::commands();
    assert_eq!(commands.as_array().unwrap().len(), 13);
    let names: Vec<&str> = commands
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(names.contains(&"web") && names.contains(&"terminal"));
    assert!(commands
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["contexts"] == json!([0,1])));
}

#[tokio::test]
async fn private_token_file_setup_writes_0600_without_exposing_the_credential() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("private-token");
    let secret = format!("test-generated-{}", uuid::Uuid::new_v4().simple());
    crate::telegram::write_private(&input, secret.as_bytes()).unwrap();
    let paths = crate::paths::Paths {
        home: dir.path().join("isolated-home"),
    };
    let args = vec![
        "ku".into(),
        "discord".into(),
        "on".into(),
        "--token-file".into(),
        input.to_string_lossy().into_owned(),
        "--guild".into(),
        "123".into(),
    ];
    super::cli(&paths, &args).await.unwrap();
    let cfg = super::load(&paths.home).unwrap();
    assert!(cfg.token == secret, "private token input was not preserved");
    assert!(
        !format!("{cfg:?}").contains(&secret),
        "Debug output must redact credentials"
    );
    assert!(!Rest::new(secret.clone(), "http://127.0.0.1:1".into())
        .unwrap()
        .safe(&format!("rejected {secret}"))
        .contains(&secret));
    assert_eq!(
        std::fs::metadata(paths.home.join("discord.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        super::token_input(&args, None).is_err(),
        "non-private token files must be refused"
    );
}

#[tokio::test]
async fn outbound_text_edits_and_files_cannot_echo_the_transport_credential() {
    use crate::channels::Channel as _;
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, _) = server().await;
    let engine = Dispatcher::new(server, mock.wire.clone());
    let marker = "fake-token-NEVER-LOG-ME";
    engine
        .publish(&address(), "redaction", &format!("reply {marker}"))
        .await
        .unwrap();
    engine
        .publish(&address(), "redaction", &format!("edited reply {marker}"))
        .await
        .unwrap();
    assert!(mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|call| !call.2.to_string().contains(marker)));
    assert!(mock
        .data
        .messages
        .lock()
        .unwrap()
        .values()
        .all(|text| text.contains("[REDACTED]")));
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("private.txt");
    crate::telegram::write_private(&file, marker.as_bytes()).unwrap();
    let count = mock.data.calls.lock().unwrap().len();
    assert!(mock
        .wire
        .file(&address(), &file, "caption", "file-test")
        .await
        .is_err());
    assert_eq!(
        mock.data.calls.lock().unwrap().len(),
        count,
        "credential-bearing file must not reach the transport"
    );
}

#[tokio::test]
async fn unpaired_and_bot_webhook_guild_messages_do_not_reach_the_server() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, _) = server().await;
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    assert!(engine.accept(incoming("bad", "hello")).await.is_err());
    assert!(server.channels.jobs().unwrap().is_empty());
    for extra in [
        json!({"author":{"id":"user","bot":true}}),
        json!({"webhook_id":"hook"}),
        json!({"guild_id":"guild"}),
    ] {
        let mut event =
            json!({"id":"1","author":{"id":"user"},"channel_id":"dm","content":"ignore me"});
        for (key, value) in extra.as_object().unwrap() {
            event[key] = value.clone();
        }
        super::gateway::dispatch(&engine, &mock.wire, "guild", "MESSAGE_CREATE", event)
            .await
            .unwrap();
    }
    assert!(server.channels.jobs().unwrap().is_empty());
    assert!(mock.data.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn inbound_replay_and_server_post_are_deduplicated_durably() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    assert!(engine.accept(incoming("unique", "hello")).await.unwrap());
    assert!(!engine.accept(incoming("unique", "hello")).await.unwrap());
    engine.step().await.unwrap();
    let request = json!({"op":"send","name":sid,"text":"hello","wait":false,"channel_event":"discord:unique"});
    let duplicate = server.test_call(request.clone()).await.unwrap();
    assert_eq!(duplicate["duplicate"], true);
    let rows = server
        .test_call(json!({"op":"history","name":sid,"after":0}))
        .await
        .unwrap();
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .filter(|r| r["role"] == "user")
            .count(),
        1
    );
    let state = State::open(&server.home()).unwrap();
    assert_eq!(state.jobs().unwrap().len(), 1);
    assert_eq!(state.jobs().unwrap()[0]["status"], "posted");
    let restarted = Arc::new(
        crate::server::Server::new(crate::paths::Paths {
            home: server.home(),
        })
        .unwrap(),
    );
    restarted.test_no_turns();
    let recovered = restarted.test_call(request).await.unwrap();
    assert_eq!(recovered["seq"], duplicate["seq"]);
    assert_eq!(
        restarted
            .test_call(json!({"op":"history","name":sid,"after":0}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["role"] == "user")
            .count(),
        1
    );
}

#[tokio::test]
async fn blocked_dm_is_an_error_and_cannot_become_success_on_replay() {
    let mock = mock(Duration::ZERO).await;
    mock.data.mode.store(1, Ordering::SeqCst);
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    let error = engine
        .publish(&address(), "blocked", "reply")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("403") && error.contains("50007"));
    let receipt = server
        .channels
        .prepare("dm:blocked:0", &address(), "reply")
        .unwrap();
    assert_eq!(receipt["status"], "failed");
    assert!(receipt["message"].is_null());
    assert!(engine
        .publish(&address(), "blocked", "reply")
        .await
        .is_err());
    assert_eq!(mock.data.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lost_receipt_stays_uncertain_and_does_not_resend() {
    let mock = mock(Duration::ZERO).await;
    mock.data.mode.store(3, Ordering::SeqCst);
    let (_dir, server, _) = server().await;
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    assert!(engine
        .publish(&address(), "uncertain", "reply")
        .await
        .is_err());
    assert!(engine
        .publish(&address(), "uncertain", "reply")
        .await
        .unwrap_err()
        .to_string()
        .contains("uncertain"));
    assert_eq!(mock.data.calls.lock().unwrap().len(), 1);
    let reopened = State::open(&server.home()).unwrap();
    assert_eq!(
        reopened
            .prepare("dm:uncertain:0", &address(), "reply")
            .unwrap()["status"],
        "uncertain"
    );
}

#[tokio::test]
async fn rate_limit_retry_after_and_coalesced_edits_keep_the_latest_text() {
    let mock = mock(Duration::from_millis(1100)).await;
    mock.data.mode.store(2, Ordering::SeqCst);
    let (_dir, server, _) = server().await;
    let engine = Dispatcher::new(server, mock.wire.clone());
    engine
        .publish(&address(), "stream", "first words")
        .await
        .unwrap();
    // Each burst collapses into one latest state before transport publication.
    for i in 0..5 {
        let mut text = String::new();
        for j in 0..40 {
            text.push_str(&format!("{i}:{j} "));
        }
        engine.publish(&address(), "stream", &text).await.unwrap();
    }
    let calls = mock.data.calls.lock().unwrap();
    let edits: Vec<_> = calls.iter().filter(|c| c.0 == "PATCH").collect();
    assert_eq!(edits.len(), 5);
    for pair in edits.windows(2) {
        assert!(pair[1].3.duration_since(pair[0].3) >= Duration::from_millis(1000));
    }
    let first = &calls[0];
    let second = &calls[1];
    assert!(second.3.duration_since(first.3) >= Duration::from_millis(50));
    assert_eq!(mock.data.messages.lock().unwrap().len(), 1);
    assert!(mock.data.messages.lock().unwrap()["1"].contains("4:39"));
}

#[tokio::test]
async fn files_upload_through_the_session_and_send_file_uses_the_originating_person() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    let mut input = incoming("attachment", "inspect this");
    input.attachments = vec![
        json!({"filename":"../../report.txt","url":format!("{}/download",mock.api),"size":14}),
    ];
    engine.accept(input).await.unwrap();
    engine.step().await.unwrap();
    let payload = server.channels.jobs().unwrap()[0]["payload"].clone();
    let path = payload["attachments"][0]["path"].as_str().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "uploaded bytes");
    assert!(std::path::Path::new(path).starts_with(server.home().join("workspaces")));
    *server.discord.inner.lock().await = Some(super::Running {
        tasks: vec![],
        engine: engine.clone(),
        _lease:super::lease::BotLease::acquire(&_dir.path().join("bot-registry"),"fake-files-bot",&server.home()).unwrap(),
    });
    let result = server
        .test_call(json!({"op":"send_file","caller":sid,"path":path,"caption":"report"}))
        .await
        .unwrap();
    assert_eq!(result["ok"], true);
    assert!(result["message_id"].is_string());
    mock.data.mode.store(1, Ordering::SeqCst);
    assert!(server
        .test_call(json!({"op":"send_file","caller":sid,"path":path}))
        .await
        .is_err());
    server.discord.stop().await;
}

#[tokio::test]
async fn global_dm_commands_and_cross_identity_selection_use_existing_sessions() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    super::gateway::register(&mock.wire).await.unwrap();
    assert!(
        mock.data
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.1 == "/users/@me")
            .all(|c| c.2 == json!("")),
        "identity GET must have no request body"
    );
    assert!(mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.0 == "GET" && c.1 == "/users/@me"));
    assert!(!mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.1 == "/oauth2/applications/@me"));
    assert!(mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.1 == "/applications/app/commands" && c.0 == "PUT"));
    for (i, cmd) in ["/id", "/list", "/status", "/help", "/new"]
        .iter()
        .enumerate()
    {
        engine
            .accept(incoming(&format!("command{i}"), cmd))
            .await
            .unwrap();
        engine.step().await.unwrap();
    }
    let new = server.channels.session("alice").unwrap().unwrap();
    assert_ne!(new, sid);
    engine
        .accept(incoming("use", &format!("/use {}", &sid[..10])))
        .await
        .unwrap();
    engine.step().await.unwrap();
    assert_eq!(server.channels.session("alice").unwrap().unwrap(), sid);
    let rows = server
        .test_call(json!({"op":"history","name":sid,"after":0}))
        .await
        .unwrap();
    assert!(rows.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn real_fake_gateway_delivers_text_and_final_reply_without_a_bot_token() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let fake = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(
            json!({"op":10,"d":{"heartbeat_interval":500}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let identify = ws.next().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(identify.to_text().unwrap()).unwrap();
        assert_eq!(v["op"], 2);
        assert_eq!(v["d"]["intents"], 37377);
        ws.send(Message::Text(json!({"op":0,"t":"MESSAGE_CREATE","s":1,"d":{"id":"gateway-1","author":{"id":"user"},"channel_id":"dm","content":"hello via gateway","attachments":[]}}).to_string().into())).await.unwrap();
        while let Some(Ok(frame)) = ws.next().await {
            if frame.is_text() {
                ws.send(Message::Text(json!({"op":11}).to_string().into()))
                    .await
                    .unwrap();
            }
        }
    });
    let worker = tokio::spawn(engine.clone().run());
    let gateway = tokio::spawn(super::gateway::run(
        engine.clone(),
        mock.wire.clone(),
        "fake-token-NEVER-LOG-ME".into(),
        "guild".into(),
        Some(url),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server
                .test_store()
                .lock()
                .await
                .history(&sid, 0, 100)
                .unwrap()
                .iter()
                .any(|m| m.text == "hello via gateway")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    server
        .test_store()
        .lock()
        .await
        .append(&sid, "agent", "reply through the existing session")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if mock
                .data
                .messages
                .lock()
                .unwrap()
                .values()
                .any(|v| v == "reply through the existing session")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    gateway.abort();
    fake.abort();
    engine.stop().await;
}

#[test]
fn gateway_proxy_selection_prefers_https_proxy_then_lowercase_then_all_proxy() {
    let lookup=|name:&str| [("HTTPS_PROXY","http://upper:1"),("https_proxy","http://lower:2"),("ALL_PROXY","http://all:3")]
        .into_iter().find(|(key,_)|*key==name).map(|(_,value)|value.to_string());
    assert_eq!(super::gateway::gateway_proxy_from(lookup).as_deref(),Some("http://upper:1"));
    assert_eq!(super::gateway::gateway_proxy_from(|name|(name=="https_proxy").then(||"  http://lower:2  ".into())).as_deref(),Some("http://lower:2"));
    assert_eq!(super::gateway::gateway_proxy_from(|name|(name=="all_proxy").then(||"http://all:4".into())).as_deref(),Some("http://all:4"));
    assert!(super::gateway::gateway_proxy_from(|_|None).is_none());
}

#[tokio::test]
async fn gateway_websocket_tunnels_through_http_connect_proxy_and_round_trips_a_frame() {
    use futures_util::{SinkExt,StreamExt};
    use tokio::io::{AsyncReadExt,AsyncWriteExt};
    use tokio_tungstenite::tungstenite::Message;

    // Local plain-text gateway fixture, reached only through the CONNECT proxy.
    let fixture=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target=fixture.local_addr().unwrap();
    let url=super::gateway::gateway_url(&format!("ws://{target}"),true).unwrap();
    assert!(super::gateway::gateway_url(&format!("ws://example.com:{target}"),true).is_err());
    let fixture_task=tokio::spawn(async move {
        let (socket,_)=fixture.accept().await.unwrap();
        let mut ws=tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(Ok(frame))=ws.next().await {
            if let Message::Text(text)=frame {ws.send(Message::Text(text)).await.unwrap();}
        }
    });

    let proxy=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr=proxy.local_addr().unwrap();
    let proxy_task=tokio::spawn(async move {
        let (mut client,_)=proxy.accept().await.unwrap();
        let mut head=Vec::new();
        loop {
            let mut byte=[0u8;1];
            assert_ne!(client.read(&mut byte).await.unwrap(),0,"proxy client closed during CONNECT");
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {break;}
        }
        let head=String::from_utf8(head).unwrap();
        assert!(head.starts_with(&format!("CONNECT {target} HTTP/1.1\r\n")),"unexpected CONNECT request: {head}");
        let mut upstream=tokio::net::TcpStream::connect(target).await.unwrap();
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
        tokio::io::copy_bidirectional(&mut client,&mut upstream).await.unwrap();
    });

    let mut socket=tokio::time::timeout(Duration::from_secs(5),super::gateway::dial_gateway_through(&url,&format!("http://{proxy_addr}")))
        .await.expect("tunnelled dial timed out").expect("tunnelled dial failed");
    socket.send(Message::Text("proxied frame".into())).await.unwrap();
    let echoed=tokio::time::timeout(Duration::from_secs(5),socket.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(echoed.to_text().unwrap(),"proxied frame");
    drop(socket);
    tokio::time::timeout(Duration::from_secs(5),proxy_task).await.unwrap().unwrap();
    fixture_task.abort();
}

#[tokio::test]
async fn new_then_immediate_text_binds_when_dispatched_and_send_now_releases_only_text() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, old) = server().await;
    pair(&server, &old);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine
        .accept(incoming("new-command", "/new"))
        .await
        .unwrap();
    engine
        .accept(incoming("first-new-input", "new session input"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    let new = server.channels.session("alice").unwrap().unwrap();
    assert_ne!(new, old);
    assert!(server
        .test_call(json!({"op":"history","name":new,"after":0}))
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["text"] == "new session input"));
    assert!(server
        .test_call(json!({"op":"history","name":old,"after":0}))
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    engine
        .accept(incoming("queued-text", "another message"))
        .await
        .unwrap();
    engine
        .accept(incoming("send-now-command", "/send_now"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    engine.step().await.unwrap();
    assert!(server
        .test_call(json!({"op":"history","name":new,"after":0}))
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["text"] == "another message"));
    engine.stop().await;
}

#[tokio::test]
async fn failed_command_reply_is_a_failed_inbox_job_and_loud_health_state() {
    let mock = mock(Duration::ZERO).await;
    mock.data.mode.store(1, Ordering::SeqCst);
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine
        .accept(incoming("failed-command", "/id"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    let health = server.channels.health().unwrap();
    assert_eq!(health["failed_jobs"], 1);
    assert_eq!(health["failed_deliveries"], 1);
    assert!(server.channels.jobs().unwrap().is_empty());
    assert!(mock.data.calls.lock().unwrap().len() >= 2); // failed reply + failed fallback
}

#[tokio::test]
async fn pairing_in_dm_is_idempotent_under_duplicate_gateway_delivery() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    let code = server
        .channels
        .mint("alice", &sid, Some("user"), 300)
        .unwrap();
    let input = incoming("pair-start", &format!("/start {code}"));
    assert!(engine.accept(input.clone()).await.unwrap());
    assert!(!engine.accept(input).await.unwrap());
    engine.step().await.unwrap();
    assert_eq!(
        server
            .channels
            .person("discord", "user")
            .unwrap()
            .as_deref(),
        Some("alice")
    );
    assert!(server.channels.jobs().unwrap().is_empty());
}

#[tokio::test]
async fn local_pairing_before_bot_installation_never_contacts_discord() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    let pair=server.test_call(json!({"op":"discord_pair","person":"before-install","session":sid,"user":"future-user"})).await.unwrap();
    assert!(pair["code"].as_str().is_some_and(|code| !code.is_empty()));
    assert!(mock.data.calls.lock().unwrap().is_empty());
    assert!(server
        .channels
        .person("discord", "future-user")
        .unwrap()
        .is_none());
    // Simulate the first DM only after installation: the stored local code is sufficient.
    let input = Incoming {
        id: "post-install-start".into(),
        user: "future-user".into(),
        destination: "future-dm".into(),
        guild: None,
        text: format!("/start {}", pair["code"].as_str().unwrap()),
        attachments: vec![],
        action: None,
    };
    assert!(engine.accept(input).await.unwrap());
    assert_eq!(
        server
            .channels
            .person("discord", "future-user")
            .unwrap()
            .as_deref(),
        Some("before-install")
    );
    assert!(mock.data.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn model_components_share_catalogue_effort_and_model_update_operations() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine
        .accept(incoming("pick-model", "/model"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    for index in 0..3 {
        let action = mock
            .data
            .calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|c| {
                c.2["components"][0]["components"][0]["options"][0]["value"]
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap();
        let mut input = incoming(&format!("model-step-{index}"), "");
        input.action = Some(action);
        engine.accept(input).await.unwrap();
        engine.step().await.unwrap();
    }
    assert!(mock
        .data
        .messages
        .lock()
        .unwrap()
        .values()
        .any(|m| m.contains("Model and effort updated")));
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 0);
}

#[tokio::test]
async fn paused_stream_tail_edits_one_bubble_and_final_history_does_not_replay_it() {
    let mock = mock(Duration::from_millis(1100)).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine
        .accept(incoming("stream-input", "check the logs"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    engine.step().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let head = "The reply arrives in pieces and the person is still allowed ";
    for piece in head.split_inclusive(' ') {
        server.test_event(
            json!({"type":"delta","session":sid,"kind":"text","message_id":"m1","text":piece}),
        );
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if mock
                .data
                .messages
                .lock()
                .unwrap()
                .values()
                .any(|m| m == head.trim())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(2400)).await;
    server.test_event(
        json!({"type":"delta","session":sid,"kind":"text","message_id":"m1","text":"in."}),
    );
    server.test_event(json!({"type":"tool","session":sid}));
    let full = format!("{head}in.");
    server
        .test_store()
        .lock()
        .await
        .append(&sid, "agent", &full)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if server.channels.jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        mock.data
            .messages
            .lock()
            .unwrap()
            .values()
            .filter(|m| m.starts_with("The reply"))
            .collect::<Vec<_>>(),
        vec![&full]
    );
    assert!(mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.0 == "PATCH"));
    let source = mock.data.calls.lock().unwrap();
    let creates = source
        .iter()
        .filter(|c| c.0 == "POST" && c.1.ends_with("/messages"))
        .count();
    assert_eq!(
        creates, 2,
        "one ack and one assistant bubble, despite many deltas"
    );
    drop(source);
    engine.stop().await;
}

#[tokio::test]
async fn linked_telegram_identity_keeps_raw_payloads_budget_and_origin_file_routing() {
    use std::sync::atomic::AtomicUsize;
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    crate::telegram::save_config(
        &server.home(),
        &crate::telegram::Config {
            token: "FAKE".into(),
            chat_id: "42".into(),
            owner_user_id: None,
            new_session_cwd: None,
            enabled: true,
        },
    )
    .unwrap();
    server.channels.import_telegram(&server.home()).unwrap();
    server.channels.select("owner", &sid).unwrap();
    let code = server
        .channels
        .mint("owner", &sid, Some("user"), 300)
        .unwrap();
    server.channels.redeem(&code, "user", "dm").unwrap();
    let bridge = crate::telegram::Bridge {
        server: server.clone(),
        cfg: crate::telegram::load_config(&server.home()).unwrap(),
        home: server.home(),
        api: mock.api.clone(),
        http: reqwest::Client::new(),
        state: Mutex::new(json!({})),
        relayed: Mutex::default(),
        last_bubble: Mutex::default(),
        net: Mutex::default(),
        act: Mutex::default(),
        jobs: Mutex::default(),
        queued: AtomicUsize::new(0),
        job_notify: tokio::sync::Notify::new(),
        compactions: Mutex::default(),
        question_lock: tokio::sync::Mutex::new(()),
        mcp_cache: Mutex::default(),
        codex_model: Mutex::new((0.0, None)),
        bot_info: Mutex::new(json!({})),
        skills_cache: Mutex::default(),
        stt_probe: Mutex::new((0.0, None)),
    };
    let ctx = bridge.dm();
    assert_eq!(bridge.current_session(&ctx).as_deref(), Some(sid.as_str()));
    let other = server
        .test_call(
            json!({"op":"create","name":"other-person-session","harness":"claude","role":"worker"}),
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.channels.select("owner", &other).unwrap();
    assert_eq!(
        bridge.current_session(&ctx).as_deref(),
        Some(other.as_str())
    );
    bridge.select_session(&ctx, &sid);
    assert_eq!(
        server.channels.session("owner").unwrap().as_deref(),
        Some(sid.as_str())
    );
    let mut owner_address=address();owner_address.person="owner".into();
    server.channels.route(&sid, &owner_address).unwrap();
    bridge
        .post_message(
            &sid,
            &[json!({"type":"input_text","text":"Telegram origin"})],
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        server.channels.destination(&sid).unwrap().unwrap().channel,
        "telegram"
    );
    let raw = format!("**raw Markdown** <b>raw HTML</b> {}", "word ".repeat(900));
    bridge.send_text(&ctx, &ctx.chat_id, &raw).await.unwrap();
    let calls = mock.data.calls.lock().unwrap();
    let texts: Vec<_> = calls
        .iter()
        .filter(|c| c.1.ends_with("/sendMessage"))
        .collect();
    assert_eq!(texts.len(), 2);
    assert!(texts[0].2["text"]
        .as_str()
        .unwrap()
        .starts_with("**raw Markdown** <b>raw HTML</b>"));
    assert!(texts.iter().all(|c| c.2.get("parse_mode").is_none()
        && c.2["text"].as_str().unwrap().chars().count() <= 3900));
}

#[tokio::test]
async fn approval_buttons_bind_person_and_edit_the_original_message() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let approval = server
        .test_store()
        .lock()
        .await
        .approval_create(
            &sid,
            "card-r1",
            "Bash",
            "List workspace",
            &json!({"command":"ls -la"}),
            "/workspace",
            crate::channels::now() + 300,
        )
        .unwrap();
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine.sync_prompts(&address(), &sid).await.unwrap();
    let calls = mock.data.calls.lock().unwrap();
    let card = calls
        .iter()
        .find(|c| c.0 == "PATCH" && c.2["components"][0]["components"][0]["label"] == "Approve")
        .unwrap();
    let path = card.1.clone();
    let action = card.2["components"][0]["components"][0]["custom_id"]
        .as_str()
        .unwrap()
        .strip_prefix("action:")
        .unwrap()
        .to_string();
    drop(calls);
    assert!(server.channels.peek_action("intruder", &action).is_err());
    let mut input = incoming("permission-click", "");
    input.action = Some(action.clone());
    engine.accept(input).await.unwrap();
    engine.step().await.unwrap();
    let approvals = server
        .test_call(json!({"op":"approvals","name":sid,"all":true}))
        .await
        .unwrap();
    assert_eq!(approvals[0]["id"], approval.id);
    assert_eq!(approvals[0]["status"], "approved");
    assert!(mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.0 == "PATCH"
            && c.1 == path
            && c.2["components"] == json!([])
            && c.2["content"].as_str().unwrap_or("").contains("approved")));
    assert_eq!(
        mock.data
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == "POST" && c.1.ends_with("/messages"))
            .count(),
        1
    );
    assert!(server.channels.peek_action("alice", &action).is_err());
}

#[tokio::test]
async fn prompt_cards_recover_and_external_denial_removes_buttons() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let a = server
        .test_store()
        .lock()
        .await
        .approval_create(
            &sid,
            "card-r2",
            "Write",
            "Write file",
            &json!({"path":"test"}),
            "/workspace",
            crate::channels::now() + 300,
        )
        .unwrap();
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine.sync_prompts(&address(), &sid).await.unwrap();
    drop(engine);
    server
        .test_call(json!({"op":"deny","id":a.id}))
        .await
        .unwrap();
    let recovered = Dispatcher::new(server.clone(), mock.wire.clone());
    recovered.step().await.unwrap();
    assert_eq!(
        mock.data
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == "POST" && c.1.ends_with("/messages"))
            .count(),
        1
    );
    assert!(mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.2["components"] == json!([])
            && c.2["content"].as_str().unwrap_or("").contains("denied")));
}

#[tokio::test]
async fn queue_release_button_forces_only_its_persons_pending_input() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    server
        .test_store()
        .lock()
        .await
        .set_status(&sid, "working")
        .unwrap();
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine.accept(incoming("held-text", "hello")).await.unwrap();
    engine.step().await.unwrap();
    let action = {
        let calls = mock.data.calls.lock().unwrap();
        calls
            .iter()
            .find_map(|c| {
                (c.2["components"][0]["components"][0]["label"] == "Release now").then(|| {
                    c.2["components"][0]["components"][0]["custom_id"]
                        .as_str()
                        .unwrap()[7..]
                        .to_string()
                })
            })
            .unwrap()
    };
    assert!(!server.channels.jobs().unwrap()[0]["force"]
        .as_bool()
        .unwrap());
    let mut click = incoming("release-click", "");
    click.action = Some(action);
    engine.accept(click).await.unwrap();
    engine.step().await.unwrap();
    assert!(server.channels.jobs().unwrap()[0]["force"]
        .as_bool()
        .unwrap());
    assert!(mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.2["components"] == json!([])
            && c.2["content"].as_str().unwrap_or("").contains("Released 1")));
    engine.stop().await;
}

#[tokio::test]
async fn question_buttons_answer_single_choice_and_reject_stale_wrong_session() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let question = json!({"questions":[{"question":"Which color?","header":"Color","options":[{"label":"Red"},{"label":"Blue"}],"multiSelect":false}]});
    server
        .test_store()
        .lock()
        .await
        .approval_create(
            &sid,
            "question-r",
            "AskUserQuestion",
            "Choose color",
            &question,
            "/workspace",
            crate::channels::now() + 300,
        )
        .unwrap();
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine.sync_prompts(&address(), &sid).await.unwrap();
    let action = {
        let calls = mock.data.calls.lock().unwrap();
        calls
            .iter()
            .find_map(|c| {
                (c.2["components"][0]["components"][0]["label"] == "Red").then(|| {
                    c.2["components"][0]["components"][0]["custom_id"]
                        .as_str()
                        .unwrap()[7..]
                        .to_string()
                })
            })
            .unwrap()
    };
    let mut click = incoming("question-click", "");
    click.action = Some(action);
    engine.accept(click).await.unwrap();
    engine.step().await.unwrap();
    let approvals = server
        .test_call(json!({"op":"approvals","name":sid,"all":true}))
        .await
        .unwrap();
    assert_eq!(approvals[0]["status"], "approved");
    assert_eq!(approvals[0]["answer"]["Which color?"], "Red");
}

#[tokio::test]
async fn remote_commands_are_known_but_owner_scoped() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    engine.accept(incoming("member-web", "/web")).await.unwrap();
    engine.step().await.unwrap();
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 0);
    let msgs = mock.data.messages.lock().unwrap().clone();
    assert!(msgs
        .values()
        .any(|v| v.contains("/web is owner-only") && v.contains("locally paired owner")));
    assert!(!msgs
        .values()
        .any(|v| v.contains("Could not deliver reply")));
    assert!(
        super::gateway::COMMANDS.contains(&"web") && super::gateway::COMMANDS.contains(&"terminal")
    );
    let (_dir2, owner, owner_sid) = self::server().await;
    let code = owner
        .channels
        .mint("owner", &owner_sid, Some("user"), 300)
        .unwrap();
    owner.channels.redeem(&code, "user", "dm").unwrap();
    let engine = Dispatcher::new(owner.clone(), mock.wire.clone());
    engine
        .accept(incoming("owner-web-close", "/web close"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    assert!(mock
        .data
        .messages
        .lock()
        .unwrap()
        .values()
        .any(|v| v.contains("Closed 0 /web")));
}

#[tokio::test]
async fn guild_ingress_uses_author_and_independent_person_conversations() {
    let mock=mock(Duration::ZERO).await;
    let (_dir,server,sid)=server().await;
    pair(&server,&sid);
    let bob=server.test_call(json!({"op":"create","name":"bob","role":"worker","harness":"claude"})).await.unwrap();
    let bob=bob["id"].as_str().unwrap();
    let code=server.channels.mint("bob",bob,Some("other-user"),300).unwrap();
    server.channels.redeem(&code,"other-user","bob-dm").unwrap();
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    for (id,user) in [("guild-a","user"),("guild-b","other-user")] {
        super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",json!({"id":id,"guild_id":"guild","author":{"id":user},"channel_id":"text","content":format!("private input {user}")})).await.unwrap();
    }
    let jobs=server.channels.jobs().unwrap();
    assert_eq!(jobs.len(),2);
    assert_eq!(jobs[0]["person"],"alice");
    assert_eq!(jobs[1]["person"],"bob");
    assert_ne!(jobs[0]["session"],jobs[1]["session"]);
    assert_ne!(jobs[0]["session"],sid);
    assert_ne!(jobs[1]["session"],bob);
    assert_eq!(server.channels.session("alice").unwrap().as_deref(),Some(sid.as_str()));
    assert_eq!(server.channels.session("bob").unwrap().as_deref(),Some(bob));
    engine.step().await.unwrap();
    for job in jobs {
        let history=server.test_call(json!({"op":"history","name":job["session"],"after":0})).await.unwrap();
        let user=if job["person"]=="alice" {"user"}else{"other-user"};
        assert_eq!(history.as_array().unwrap().len(),1);
        assert_eq!(history[0]["text"],format!("private input {user}"));
    }
    engine.stop().await;
}

#[tokio::test]
async fn guild_allowlist_and_dm_only_pairing_reject_without_guild_notices() {
    let mock=mock(Duration::ZERO).await;
    let (_dir,server,sid)=server().await;
    pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",json!({"id":"outside","guild_id":"elsewhere","author":{"id":"user"},"channel_id":"text","content":"ignore"})).await.unwrap();
    let mut input=incoming("pair-public","/start fake-code");
    input.guild=Some("guild".into());input.destination="text".into();
    assert!(engine.accept(input).await.unwrap_err().to_string().contains("only in the bot DM"));
    assert!(server.channels.jobs().unwrap().is_empty());
    assert!(mock.data.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn guild_replies_and_files_stay_in_the_authors_private_thread() {
    let mock=mock(Duration::ZERO).await;
    let (_dir,server,sid)=server().await;pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    let event=json!({"id":"thread-first","guild_id":"guild","author":{"id":"user"},"channel_id":"parent","content":"hello"});
    super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",event.clone()).await.unwrap();
    let job=server.channels.jobs().unwrap().remove(0);
    let thread=job["destination"].as_str().unwrap().to_string();
    assert_ne!(thread,"parent");
    let to=Address{channel:"discord".into(),person:"alice".into(),destination:thread.clone()};
    engine.publish(&to,"reply","reply only to this person").await.unwrap();
    let file=server.home().join("proof.txt");std::fs::write(&file,"private file").unwrap();
    use crate::channels::Channel;
    mock.wire.file(&to,&file,"private attachment","file-test").await.unwrap();
    super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",event).await.unwrap();
    super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",json!({"id":"thread-next","guild_id":"guild","author":{"id":"user"},"channel_id":thread,"content":"second"})).await.unwrap();
    let jobs=server.channels.jobs().unwrap();assert_eq!(jobs.len(),2);
    assert_eq!(jobs[0]["destination"],jobs[1]["destination"]);
    assert_eq!(jobs[0]["session"],jobs[1]["session"]);
    let calls=mock.data.calls.lock().unwrap();
    let creates:Vec<_>=calls.iter().filter(|c|c.0=="POST" && c.1.ends_with("/threads")).collect();
    assert_eq!(creates.len(),1);assert_eq!(creates[0].2["type"],12);assert_eq!(creates[0].2["invitable"],false);
    assert!(calls.iter().filter(|c|c.0=="POST" && c.1.ends_with("/messages")).all(|c|c.1==format!("/channels/{thread}/messages")));
    assert!(calls.iter().any(|c|c.0=="PUT" && c.1==format!("/channels/{thread}/thread-members/user")));
    assert!(!calls.iter().any(|c|c.1.contains("/guilds/") || c.1.ends_with("/thread-members")));
}

#[tokio::test]
async fn threads_reject_other_people_and_public_or_changed_threads_without_posts() {
    let mock=mock(Duration::ZERO).await;
    let (_dir,server,sid)=server().await;pair(&server,&sid);
    let other=server.test_call(json!({"op":"create","name":"bob","role":"worker","harness":"claude"})).await.unwrap();
    let code=server.channels.mint("bob",other["id"].as_str().unwrap(),Some("other-user"),300).unwrap();server.channels.redeem(&code,"other-user","other-dm").unwrap();
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    let thread=super::threads::destination(&engine,&mock.wire,"guild","user","parent").await.unwrap();
    assert!(super::threads::destination(&engine,&mock.wire,"guild","other-user",&thread).await.is_err());
    mock.data.channels.lock().unwrap().insert("public".into(),json!({"id":"public","guild_id":"guild","parent_id":"parent","type":11}));
    assert!(super::threads::destination(&engine,&mock.wire,"guild","user","public").await.is_err());
    mock.data.channels.lock().unwrap().get_mut(&thread).unwrap()["thread_metadata"]["invitable"]=json!(true);
    assert!(super::threads::destination(&engine,&mock.wire,"guild","user",&thread).await.is_err());
    assert!(mock.data.calls.lock().unwrap().iter().all(|c|!c.1.ends_with("/messages")));
}

#[tokio::test]
async fn thread_invite_recovers_without_creating_another_thread() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());mock.data.mode.store(5,Ordering::SeqCst);
    assert!(super::threads::destination(&engine,&mock.wire,"guild","user","parent").await.is_err());
    let row=State::open(&server.home()).unwrap().thread_for("guild","parent","alice").unwrap().unwrap();
    assert_eq!(row["status"],"inviting");assert!(row["thread"].is_string());
    let restarted=Dispatcher::new(server.clone(),mock.wire.clone());mock.data.mode.store(0,Ordering::SeqCst);
    let thread=super::threads::destination(&restarted,&mock.wire,"guild","user","parent").await.unwrap();
    assert_eq!(row["thread"],thread);
    assert_eq!(mock.data.calls.lock().unwrap().iter().filter(|c|c.0=="POST" && c.1.ends_with("/threads")).count(),1);
}

#[tokio::test]
async fn uncertain_thread_creation_never_retries_a_create_or_posts_publicly() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());mock.data.mode.store(4,Ordering::SeqCst);
    assert!(super::threads::destination(&engine,&mock.wire,"guild","user","parent").await.is_err());
    mock.data.mode.store(0,Ordering::SeqCst);
    assert!(super::threads::destination(&engine,&mock.wire,"guild","user","parent").await.is_err());
    let calls=mock.data.calls.lock().unwrap();assert_eq!(calls.iter().filter(|c|c.0=="POST").count(),1);
    assert!(calls.iter().all(|c|!c.1.ends_with("/messages")));
}

#[tokio::test]
async fn guild_interactions_ack_before_thread_creation_and_use_member_identity() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    super::gateway::dispatch(&engine,&mock.wire,"guild","INTERACTION_CREATE",json!({"id":"guild-command","token":"fake-interaction","guild_id":"guild","member":{"user":{"id":"user"}},"channel_id":"parent","type":2,"data":{"name":"id"}})).await.unwrap();
    let jobs=server.channels.jobs().unwrap();assert_eq!(jobs.len(),1);assert_eq!(jobs[0]["person"],"alice");
    let calls=mock.data.calls.lock().unwrap();assert!(calls[0].1.ends_with("/callback"));assert_eq!(calls[0].2["type"],4);assert_eq!(calls[0].2["data"]["flags"],64);
}

#[test]
fn gateway_receipt_and_resume_sequence_commit_atomically_across_restart() {
    let dir=tempfile::tempdir().unwrap();let state=State::open(dir.path()).unwrap();
    let checkpoint=json!({"session_id":"ready-session","url":"wss://gateway.discord.gg","seq":42});
    let event=json!({"id":"9001","channel_id":"dm","author":{"id":"user"},"content":"recover once"});
    assert!(state.gateway_record("app:guild",&checkpoint,"MESSAGE_CREATE",&event).unwrap());
    assert!(!state.gateway_record("app:guild",&checkpoint,"MESSAGE_CREATE",&event).unwrap());
    assert!(state.gateway_record("app:guild",&json!({"seq":43}),"MESSAGE_CREATE",&json!({"id":"9002"})).is_err());
    drop(state);let state=State::open(dir.path()).unwrap();
    assert_eq!(state.gateway_checkpoint("app:guild").unwrap().unwrap(),checkpoint);
    assert_eq!(state.gateway_pending("app:guild").unwrap().len(),1);
    state.gateway_done("app:guild","message:9001","done",None).unwrap();
    assert!(state.gateway_pending("app:guild").unwrap().is_empty());
}

#[tokio::test]
async fn backfill_pages_survive_restart_and_merge_live_duplicates_oldest_first() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());let account="app:guild";
    server.channels.gateway_watch(account,"dm",None,"1000").unwrap();server.channels.backfill_begin(account).unwrap();
    let history:Vec<_>=(1001..=1215).map(|id|json!({"id":id.to_string(),"channel_id":"dm","author":{"id":"user"},"content":format!("missed {id}")})).collect();
    mock.data.history.lock().unwrap().insert("dm".into(),history);
    let live=json!({"id":"1216","channel_id":"dm","author":{"id":"user"},"content":"new live message"});
    server.channels.gateway_record(account,&json!({"seq":2}),"MESSAGE_CREATE",&live).unwrap();
    super::recovery::step(&engine,&mock.wire,"guild",account).await.unwrap();
    assert!(server.channels.jobs().unwrap().is_empty());
    let restarted=State::open(&server.home()).unwrap();let progress=restarted.backfills(account).unwrap();assert_eq!(progress.len(),1);assert_eq!(progress[0]["before"],"1116");
    super::recovery::step(&engine,&mock.wire,"guild",account).await.unwrap();assert!(server.channels.jobs().unwrap().is_empty());
    super::recovery::step(&engine,&mock.wire,"guild",account).await.unwrap();
    super::recovery::step(&engine,&mock.wire,"guild",account).await.unwrap();
    let jobs=server.channels.jobs().unwrap();assert_eq!(jobs.len(),216);
    for (i,job) in jobs.iter().enumerate(){assert_eq!(job["id"],format!("message:{}",1001+i));}
    assert!(restarted.backfills(account).unwrap().is_empty());
    server.channels.backfill_begin(account).unwrap();super::recovery::step(&engine,&mock.wire,"guild",account).await.unwrap();
    assert_eq!(server.channels.jobs().unwrap().len(),216);
}

#[tokio::test]
async fn failed_history_request_keeps_the_recovery_cursor_for_retry() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    server.channels.gateway_watch("app:guild","dm",None,"1000").unwrap();server.channels.backfill_begin("app:guild").unwrap();
    mock.data.mode.store(6,Ordering::SeqCst);
    assert!(super::recovery::step(&engine,&mock.wire,"guild","app:guild").await.is_err());
    assert_eq!(server.channels.backfills("app:guild").unwrap().len(),1);
    mock.data.mode.store(0,Ordering::SeqCst);
    mock.data.history.lock().unwrap().insert("dm".into(),vec![json!({"id":"1001","channel_id":"dm","author":{"id":"user"},"content":"retained"})]);
    super::recovery::step(&engine,&mock.wire,"guild","app:guild").await.unwrap();
    assert_eq!(server.channels.jobs().unwrap().len(),1);assert!(server.channels.backfills("app:guild").unwrap().is_empty());
}

#[tokio::test]
async fn gateway_resumes_on_advertised_url_and_replayed_events_reach_one_user_row() {
    use futures_util::{SinkExt,StreamExt};use tokio_tungstenite::tungstenite::Message;
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    let first=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("ws://{}",first.local_addr().unwrap());
    let resumed=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let resume_url=format!("ws://{}",resumed.local_addr().unwrap());
    let fake=tokio::spawn(async move {
        let (socket,_)=first.accept().await.unwrap();let mut ws=tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(json!({"op":10,"d":{"heartbeat_interval":10000}}).to_string().into())).await.unwrap();
        let auth:Value=serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();assert_eq!(auth["op"],2);
        ws.send(Message::Text(json!({"op":0,"t":"READY","s":1,"d":{"session_id":"resume-this","resume_gateway_url":resume_url}}).to_string().into())).await.unwrap();
        let event=json!({"id":"9001","channel_id":"dm","author":{"id":"user"},"content":"first"});
        ws.send(Message::Text(json!({"op":0,"t":"MESSAGE_CREATE","s":2,"d":event}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"op":7}).to_string().into())).await.unwrap();drop(ws);
        let (socket,_)=resumed.accept().await.unwrap();let mut ws=tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(json!({"op":10,"d":{"heartbeat_interval":10000}}).to_string().into())).await.unwrap();
        let auth:Value=serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();assert_eq!(auth["op"],6);assert_eq!(auth["d"]["session_id"],"resume-this");assert_eq!(auth["d"]["seq"],2);
        ws.send(Message::Text(json!({"op":0,"t":"MESSAGE_CREATE","s":3,"d":event}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"op":0,"t":"MESSAGE_CREATE","s":4,"d":{"id":"9002","channel_id":"dm","author":{"id":"user"},"content":"missed"}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"op":0,"t":"RESUMED","s":5,"d":{}}).to_string().into())).await.unwrap();
        while let Some(Ok(frame))=ws.next().await {if frame.is_text(){let _=ws.send(Message::Text(json!({"op":11}).to_string().into())).await;}}
    });
    let worker=tokio::spawn(engine.clone().run());let gateway=tokio::spawn(super::gateway::run(engine.clone(),mock.wire.clone(),"fake-token-NEVER-LOG-ME".into(),"guild".into(),Some(url)));
    tokio::time::timeout(Duration::from_secs(8),async {
        loop {
            // The no-turn fixture never finishes an agent turn; release its second queued input.
            server.channels.force_conversation(&address()).unwrap();
            let history=server.test_call(json!({"op":"history","name":sid,"after":0})).await.unwrap();
            if history.as_array().unwrap().iter().filter(|r|r["role"]=="user").count()==2 {
                assert_eq!(history.as_array().unwrap().iter().filter(|r|r["text"]=="first").count(),1);
                assert_eq!(history.as_array().unwrap().iter().filter(|r|r["text"]=="missed").count(),1);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    assert_eq!(server.channels.gateway_checkpoint("app:guild").unwrap().unwrap()["seq"],5);
    worker.abort();gateway.abort();fake.abort();engine.stop().await;
}

#[tokio::test]
async fn gateway_invalid_session_reidentifies_and_fatal_intents_stop_without_retry() {
    use futures_util::{SinkExt,StreamExt};use tokio_tungstenite::tungstenite::{Message,protocol::{CloseFrame,frame::coding::CloseCode}};
    let mock=mock(Duration::ZERO).await;let (_dir,server,_)=server().await;let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("ws://{}",listener.local_addr().unwrap());let fake_url=url.clone();
    let fake=tokio::spawn(async move {
        for round in 0..2 {
            let (socket,_)=listener.accept().await.unwrap();let mut ws=tokio_tungstenite::accept_async(socket).await.unwrap();
            ws.send(Message::Text(json!({"op":10,"d":{"heartbeat_interval":10000}}).to_string().into())).await.unwrap();
            let auth:Value=serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();assert_eq!(auth["op"],2);
            if round==0 {
                ws.send(Message::Text(json!({"op":0,"t":"READY","s":1,"d":{"session_id":"invalidated","resume_gateway_url":fake_url}}).to_string().into())).await.unwrap();
                ws.send(Message::Text(json!({"op":9,"d":false}).to_string().into())).await.unwrap();
            }else {ws.send(Message::Close(Some(CloseFrame{code:CloseCode::Library(4014),reason:"disallowed intent".into()}))).await.unwrap();}
        }
    });
    let error=tokio::time::timeout(Duration::from_secs(5),super::gateway::run(engine,mock.wire.clone(),"fake-token-NEVER-LOG-ME".into(),"guild".into(),Some(url))).await.unwrap().unwrap_err();
    assert!(error.to_string().contains("fatal close 4014"));assert!(!error.to_string().contains("fake-token"));assert!(server.channels.gateway_checkpoint("app:guild").unwrap().is_none());fake.await.unwrap();
}

async fn pair_bob(server:&Arc<crate::server::Server>)->String {
    let bob=server.test_call(json!({"op":"create","name":"bob","role":"worker","harness":"claude"})).await.unwrap()["id"].as_str().unwrap().to_string();
    let code=server.channels.mint("bob",&bob,Some("other-user"),300).unwrap();server.channels.redeem(&code,"other-user","bob-dm").unwrap();bob
}

#[test]
fn pairing_and_outbox_receipts_cannot_cross_person_ownership() {
    let dir=tempfile::tempdir().unwrap();let state=State::open(dir.path()).unwrap();
    let code=state.mint("alice","alice-session",Some("user"),300).unwrap();state.redeem(&code,"user","dm").unwrap();
    assert!(state.mint("bob","alice-session",Some("other-user"),300).is_err());
    assert!(state.select("bob","alice-session").is_err());
    let mut bob=address();bob.person="bob".into();bob.destination="bob-dm".into();
    state.prepare("same-key",&address(),"alice secret").unwrap();
    assert!(state.prepare("same-key",&bob,"bob message").is_err());
    assert!(state.route("alice-session",&bob).is_err());
    drop(state);let state=State::open(dir.path()).unwrap();assert_eq!(state.session_owner("alice-session").unwrap().as_deref(),Some("alice"));
}

#[tokio::test]
async fn person_lists_and_direct_operations_cannot_read_or_change_another_session() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);let bob=pair_bob(&server).await;
    server.test_store().lock().await.append(&bob,"agent","BOB_PRIVATE_HISTORY").unwrap();
    let peers=server.handle_person_pub("alice".into(),json!({"op":"peers"})).await.unwrap();
    assert_eq!(peers.as_array().unwrap().len(),1);assert_eq!(peers[0]["id"],sid);assert!(!peers.to_string().contains("BOB_PRIVATE_HISTORY"));
    for operation in ["status","history","history_page","handoff","send","stop","interrupt","update","upload","approvals","usage","audit","changes"] {
        let request=json!({"op":operation,"name":bob,"text":"attack","fields":{"model":"stolen"},"filename":"attack.txt","data_b64":"eA=="});
        assert!(server.handle_person_pub("alice".into(),request).await.unwrap_err().to_string().contains("permission denied"),"{operation}");
    }
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    engine.accept(incoming("private-list","/list")).await.unwrap();engine.step().await.unwrap();
    let text=mock.data.messages.lock().unwrap().values().cloned().collect::<Vec<_>>().join("\n");assert!(text.contains(&sid));assert!(!text.contains(&bob));assert!(!text.contains("BOB_PRIVATE_HISTORY"));
    engine.accept(incoming("foreign-use",&format!("/use {}",&bob[..10]))).await.unwrap();engine.step().await.unwrap();
    assert_eq!(server.channels.session("alice").unwrap().as_deref(),Some(sid.as_str()));
    let history=server.test_call(json!({"op":"history","name":bob,"after":0})).await.unwrap();assert_eq!(history.as_array().unwrap().len(),1);assert_eq!(history[0]["text"],"BOB_PRIVATE_HISTORY");
}

#[tokio::test]
async fn paired_mcp_callers_and_their_children_inherit_person_limits() {
    let (_dir,server,sid)=server().await;pair(&server,&sid);let bob=pair_bob(&server).await;
    let bob_cwd=server.test_call(json!({"op":"status","name":bob})).await.unwrap()["cwd"].as_str().unwrap().to_string();
    let child=server.test_call(json!({"op":"create","caller":sid,"name":"alice-child","cwd":bob_cwd,"harness":"claude"})).await.unwrap();
    let child_id=child["id"].as_str().unwrap();assert_eq!(server.channels.session_owner(child_id).unwrap().as_deref(),Some("alice"));
    assert!(child["cwd"].as_str().unwrap().contains("/discord-people/alice/"));assert_ne!(child["cwd"],bob_cwd);
    let peers=server.test_call(json!({"op":"peers","caller":child_id})).await.unwrap();assert_eq!(peers.as_array().unwrap().len(),2);assert!(!peers.to_string().contains(&bob));
    for operation in ["history","status","handoff","stop"] {
        assert!(server.test_call(json!({"op":operation,"caller":child_id,"name":bob})).await.is_err());
    }
    for operation in ["board_read","messages_after","telegram_history","rules","usage_report","discord_pair","shutdown"] {
        assert!(server.test_call(json!({"op":operation,"caller":child_id})).await.is_err(),"{operation}");
    }
    let own=server.test_call(json!({"op":"status","caller":child_id,"name":sid})).await.unwrap();assert_eq!(own["id"],sid);
}

#[tokio::test]
async fn stale_or_forged_model_and_approval_actions_cannot_control_another_person() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);let bob=pair_bob(&server).await;
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    let action=server.channels.action("alice",&json!({"kind":"apply","session":bob,"harness":"claude","model":"forbidden-model","destination":"dm"})).unwrap();
    let mut click=incoming("forged-model","");click.action=Some(action);engine.accept(click).await.unwrap();engine.step().await.unwrap();
    let snap=server.test_call(json!({"op":"status","name":bob})).await.unwrap();assert_ne!(snap["model"],"forbidden-model");
    let approval=server.test_store().lock().await.approval_create(&bob,"bob-request","Bash","private approval",&json!({"command":"private command"}),"/workspace",crate::channels::now()+300).unwrap();
    for operation in ["approve","deny","answer"] {
        assert!(server.handle_person_pub("alice".into(),json!({"op":operation,"id":approval.id,"answers":{"0":"yes"}})).await.is_err());
    }
    assert_eq!(server.test_call(json!({"op":"approvals","name":bob,"all":true})).await.unwrap()[0]["status"],"pending");
}

#[tokio::test]
async fn owned_thread_sessions_cannot_move_to_another_thread_or_dm() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    for (id,parent) in [("scope-1","parent-one"),("scope-2","parent-two")] {
        super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",json!({"id":id,"guild_id":"guild","author":{"id":"user"},"channel_id":parent,"content":"/id"})).await.unwrap();
    }
    let jobs=server.channels.jobs().unwrap();let first=Address{channel:"discord".into(),person:"alice".into(),destination:jobs[0]["destination"].as_str().unwrap().into()};
    let second=Address{destination:jobs[1]["destination"].as_str().unwrap().into(),..first.clone()};let first_id=jobs[0]["session"].as_str().unwrap();let second_id=jobs[1]["session"].as_str().unwrap();
    assert!(!server.channels.selectable(&first,second_id).unwrap());assert!(!server.channels.selectable(&address(),first_id).unwrap());assert!(server.channels.route(first_id,&second).is_err());
    engine.step().await.unwrap();
    super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",json!({"id":"cross-use","guild_id":"guild","author":{"id":"user"},"channel_id":first.destination,"content":format!("/use {second_id}")})).await.unwrap();engine.step().await.unwrap();
    assert_eq!(server.channels.conversation(&first).unwrap().as_deref(),Some(first_id));
    assert_eq!(server.channels.session("alice").unwrap().as_deref(),Some(sid.as_str()));
    assert!(mock.data.calls.lock().unwrap().iter().filter(|c|c.0=="POST"&&c.1.ends_with("/messages")).all(|c|c.1==format!("/channels/{}/messages",first.destination)||c.1==format!("/channels/{}/messages",second.destination)));
}

#[tokio::test]
async fn private_files_never_fall_back_to_the_telegram_owner_or_another_dm() {
    let (_dir,server,sid)=server().await;pair(&server,&sid);let bob=pair_bob(&server).await;
    crate::telegram::save_config(&server.home(),&crate::telegram::Config{token:"FAKE".into(),chat_id:"42".into(),owner_user_id:None,new_session_cwd:None,enabled:true}).unwrap();
    let snap=server.test_call(json!({"op":"status","name":sid})).await.unwrap();let cwd=std::path::PathBuf::from(snap["cwd"].as_str().unwrap());std::fs::create_dir_all(&cwd).unwrap();std::fs::write(cwd.join("private.txt"),"private file").unwrap();
    let error=server.test_call(json!({"op":"send_file","caller":sid,"path":"private.txt"})).await.unwrap_err().to_string();assert!(error.contains("No private Discord route"));
    let bob_address=Address{channel:"discord".into(),person:"bob".into(),destination:"bob-dm".into()};assert!(server.channels.route(&sid,&bob_address).is_err());
    let bob_cwd=server.test_call(json!({"op":"status","name":bob})).await.unwrap()["cwd"].as_str().unwrap().to_string();std::fs::create_dir_all(&bob_cwd).unwrap();let bob_file=std::path::Path::new(&bob_cwd).join("secret.txt");std::fs::write(&bob_file,"BOB_FILE").unwrap();
    assert!(server.test_call(json!({"op":"send_file","caller":sid,"path":bob_file})).await.unwrap_err().to_string().contains("permission denied"));
}

#[test]
fn legacy_ambiguous_sessions_fail_closed_and_replacements_keep_their_owner() {
    let dir=tempfile::tempdir().unwrap();let state=State::open(dir.path()).unwrap();drop(state);
    let db=rusqlite::Connection::open(dir.path().join("channels.sqlite3")).unwrap();db.execute_batch("INSERT INTO people VALUES('alice','shared'),('bob','shared');").unwrap();drop(db);
    let state=State::open(dir.path()).unwrap();assert!(state.ambiguous_session("shared").unwrap());assert!(state.session_owner("shared").unwrap().is_none());assert!(state.selectable(&address(),"shared").unwrap()==false);
    state.select("alice","owned").unwrap();state.replaced("owned","replacement").unwrap();assert_eq!(state.session_owner("replacement").unwrap().as_deref(),Some("alice"));assert!(state.select("bob","replacement").is_err());
}

#[tokio::test]
async fn history_backfill_cannot_rebind_a_private_thread_to_another_author() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;pair(&server,&sid);pair_bob(&server).await;
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    let thread=super::threads::destination(&engine,&mock.wire,"guild","user","parent").await.unwrap();
    server.channels.gateway_watch("app:guild",&thread,Some("guild"),"1000").unwrap();server.channels.backfill_begin("app:guild").unwrap();
    mock.data.history.lock().unwrap().insert(thread.clone(),vec![
        json!({"id":"1001","channel_id":thread,"author":{"id":"other-user"},"content":"FOREIGN_PRIVATE_REPLAY"}),
        json!({"id":"1002","channel_id":thread,"author":{"id":"user"},"content":"/id"}),
    ]);
    super::recovery::step(&engine,&mock.wire,"guild","app:guild").await.unwrap();
    let jobs=server.channels.jobs().unwrap();assert_eq!(jobs.len(),1);assert_eq!(jobs[0]["id"],"message:1002");assert_eq!(jobs[0]["person"],"alice");
    assert_eq!(server.channels.health().unwrap()["rejected_gateway_events"],1);
    engine.step().await.unwrap();
    assert!(mock.data.calls.lock().unwrap().iter().filter(|c|c.0=="POST"&&c.1.ends_with("/messages")).all(|c|c.1==format!("/channels/{thread}/messages")));
    assert!(!mock.data.messages.lock().unwrap().values().any(|v|v.contains("FOREIGN_PRIVATE_REPLAY")));
}

#[tokio::test]
async fn unpaired_text_and_roster_commands_only_receive_the_pairing_notice() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;
    let secret="ROSTER_SECRET_MUST_NOT_LEAK";
    server.test_store().lock().await.append(&sid,"agent",secret).unwrap();
    let before=server.test_call(json!({"op":"list"})).await.unwrap();
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    for (i,text) in ["who are you","/list","/status","/new","/model","/use alice","/help"].iter().enumerate() {
        super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",json!({"id":format!("unpaired-{i}"),"channel_id":"unknown-dm","author":{"id":"unknown"},"content":text})).await.unwrap();
        engine.step().await.unwrap();
    }
    assert!(server.channels.jobs().unwrap().is_empty());
    assert!(server.channels.person("discord","unknown").unwrap().is_none());
    assert_eq!(server.test_call(json!({"op":"list"})).await.unwrap(),before);
    let history=server.test_call(json!({"op":"history","name":sid,"after":0})).await.unwrap();
    assert_eq!(history.as_array().unwrap().len(),1);assert_eq!(history[0]["text"],secret);
    let messages=mock.data.messages.lock().unwrap();assert_eq!(messages.len(),7);
    assert!(messages.values().all(|s|s==crate::channels::dispatch::PAIRING_NOTICE));
    assert!(mock.data.calls.lock().unwrap().iter().all(|c|c.0=="POST"&&c.1=="/channels/unknown-dm/messages"));
}

#[tokio::test]
async fn unpaired_gateway_events_are_rejected_before_queue_and_cannot_run_after_pairing() {
    use futures_util::{SinkExt,StreamExt};use tokio_tungstenite::tungstenite::Message;
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;
    let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("ws://{}",listener.local_addr().unwrap());let resume_url=url.clone();
    let fake=tokio::spawn(async move {
        let (socket,_)=listener.accept().await.unwrap();let mut ws=tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(json!({"op":10,"d":{"heartbeat_interval":10000}}).to_string().into())).await.unwrap();
        let _=ws.next().await;
        ws.send(Message::Text(json!({"op":0,"t":"READY","s":1,"d":{"session_id":"test-session","resume_gateway_url":resume_url}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"op":0,"t":"MESSAGE_CREATE","s":2,"d":{"id":"unpaired-text","channel_id":"dm","author":{"id":"user"},"content":"who are you"}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"op":0,"t":"INTERACTION_CREATE","s":3,"d":{"id":"unpaired-list","token":"fake-interaction","channel_id":"dm","user":{"id":"user"},"type":2,"data":{"name":"list"}}}).to_string().into())).await.unwrap();
        while let Some(Ok(frame))=ws.next().await {if frame.is_text(){let _=ws.send(Message::Text(json!({"op":11}).to_string().into())).await;}}
    });
    let gateway=tokio::spawn(super::gateway::run(engine.clone(),mock.wire.clone(),"fake-token-NEVER-LOG-ME".into(),"guild".into(),Some(url)));
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {if mock.data.calls.lock().unwrap().iter().any(|c|c.1.contains("/callback")){break;}tokio::time::sleep(Duration::from_millis(10)).await;}
    }).await.unwrap();
    // Authorize the sender later. The earlier rejected slash must never become an allowed job.
    pair(&server,&sid);
    super::recovery::step(&engine,&mock.wire,"guild","app:guild").await.unwrap();engine.step().await.unwrap();
    assert!(server.channels.jobs().unwrap().is_empty());
    assert!(server.test_call(json!({"op":"history","name":sid,"after":0})).await.unwrap().as_array().unwrap().is_empty());
    let calls=mock.data.calls.lock().unwrap();let callbacks:Vec<_>=calls.iter().filter(|c|c.1.contains("/callback")).collect();
    assert_eq!(callbacks.len(),1);assert_eq!(callbacks[0].2["type"],4);assert_eq!(callbacks[0].2["data"]["flags"],64);assert_eq!(callbacks[0].2["data"]["content"],crate::channels::dispatch::PAIRING_NOTICE);
    let notices:Vec<_>=calls.iter().filter(|c|c.1.ends_with("/messages")).collect();
    assert_eq!(notices.len(),1);assert_eq!(notices[0].2["content"],crate::channels::dispatch::PAIRING_NOTICE);drop(calls);
    assert_eq!(server.channels.health().unwrap()["rejected_gateway_events"],2);
    gateway.abort();fake.abort();engine.stop().await;
}

#[tokio::test]
async fn a_second_home_cannot_start_an_inbox_worker_for_the_same_bot() {
    let mock=mock(Duration::ZERO).await;
    let (_a,a,_)=server().await;let (_b,b,b_sid)=server().await;pair(&b,&b_sid);
    let queued=Dispatcher::new(b.clone(),mock.wire.clone());queued.accept(incoming("queued-in-other-home","who are you")).await.unwrap();
    let dir=tempfile::tempdir().unwrap();let registry=dir.path().join("registry");
    let first=super::Channel::default();let second=super::Channel::default();
    let config=Config{token:"fake-token-NEVER-LOG-ME".into(),guild:"guild".into(),enabled:true};
    first.start(a.clone(),a.home(),config.clone(),mock.wire.clone(),Some("ws://127.0.0.1:9".into()),registry.clone()).await.unwrap();
    let other_bot=super::lease::BotLease::acquire(&registry,"another-bot",&b.home()).unwrap();
    let error=second.start(b.clone(),b.home(),config,mock.wire.clone(),Some("ws://127.0.0.1:9".into()),registry.clone()).await.unwrap_err();
    drop(other_bot);
    assert!(error.to_string().contains("already active"));assert_eq!(second.state().await,"stopped");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(b.channels.job_status("queued-in-other-home").unwrap().as_deref(),Some("pending"));
    assert!(b.test_call(json!({"op":"history","name":b_sid,"after":0})).await.unwrap().as_array().unwrap().is_empty());
    assert_eq!(mock.data.calls.lock().unwrap().iter().filter(|c|c.0=="PUT"&&c.1=="/applications/app/commands").count(),1);
    assert!(!std::fs::read_to_string(registry.join("app.lock")).unwrap().contains("fake-token"));
    first.stop().await;
    let lease=super::lease::BotLease::acquire(&registry,"app",&b.home()).unwrap();drop(lease);
    assert_eq!(b.channels.person("discord","user").unwrap().as_deref(),Some("alice"));
    assert_eq!(b.channels.session("alice").unwrap().as_deref(),Some(b_sid.as_str()));
}

#[tokio::test]
async fn paired_owner_text_and_roster_still_use_the_same_dm_and_session() {
    let mock=mock(Duration::ZERO).await;let (_dir,server,sid)=server().await;
    let code=server.channels.mint("owner",&sid,Some("user"),300).unwrap();server.channels.redeem(&code,"user","dm").unwrap();
    let before=server.channels.addresses().unwrap();let engine=Dispatcher::new(server.clone(),mock.wire.clone());
    super::gateway::dispatch(&engine,&mock.wire,"guild","MESSAGE_CREATE",json!({"id":"owner-text","channel_id":"dm","author":{"id":"user"},"content":"owner text"})).await.unwrap();engine.step().await.unwrap();
    super::gateway::dispatch(&engine,&mock.wire,"guild","INTERACTION_CREATE",json!({"id":"owner-list","token":"fake-interaction","channel_id":"dm","user":{"id":"user"},"type":2,"data":{"name":"list"}})).await.unwrap();engine.step().await.unwrap();
    assert_eq!(server.channels.addresses().unwrap(),before);assert_eq!(server.channels.session("owner").unwrap().as_deref(),Some(sid.as_str()));
    let history=server.test_call(json!({"op":"history","name":sid,"after":0})).await.unwrap();assert_eq!(history[0]["text"],"owner text");
    let calls=mock.data.calls.lock().unwrap();assert!(calls.iter().filter(|c|c.0=="POST"&&c.1.ends_with("/messages")).all(|c|c.1=="/channels/dm/messages"));
    assert!(mock.data.messages.lock().unwrap().values().any(|text|text.contains(&sid)));drop(calls);engine.stop().await;
}

#[test]
fn both_channels_emit_one_heartbeat_per_interval() {
    use crate::channels::dispatch::{ProgressLimit, ProgressTicker};
    let start = tokio::time::Instant::now();
    let mut tg = ProgressTicker::with_start(
        ProgressLimit::Chars(crate::telegram::tg::MAX_TG_CHARS),
        start,
    );
    let mut dc = ProgressTicker::with_start(ProgressLimit::Utf16(text::MAX_DISCORD_UTF16), start);
    let snap = json!({"activity":{"state":"running","tool":"Shell","title":"Shell: cargo test -p kurisu"}});
    assert!(tg.observe_snapshot(&snap));
    assert!(dc.observe_snapshot(&snap));

    for secs in [0u64, 2, 4] {
        let t = start + Duration::from_secs(secs);
        assert!(tg.poll_at(t, str::to_string).is_none());
        assert!(dc.poll_at(t, str::to_string).is_none());
    }

    let t5 = start + Duration::from_secs(5);
    let want5 = Some("⏳ Still working — running Shell: cargo test -p kurisu (5s)");
    assert_eq!(tg.poll_at(t5, str::to_string).as_deref(), want5);
    assert_eq!(dc.poll_at(t5, str::to_string).as_deref(), want5);

    // Many polls within the next 10s interval must emit nothing.
    for millis in [5_100u64, 5_250, 9_000, 14_900] {
        let t = start + Duration::from_millis(millis);
        assert!(tg.poll_at(t, str::to_string).is_none());
        assert!(dc.poll_at(t, str::to_string).is_none());
    }

    let t15 = start + Duration::from_secs(15);
    let want15 = Some("⏳ Still working — running Shell: cargo test -p kurisu (15s)");
    assert_eq!(tg.poll_at(t15, str::to_string).as_deref(), want15);
    assert_eq!(dc.poll_at(t15, str::to_string).as_deref(), want15);

    // Streaming assistant text resets the interval timer.
    let t20 = start + Duration::from_secs(20);
    tg.reset_at(t20);
    dc.reset_at(t20);
    assert!(tg.poll_at(start + Duration::from_secs(29), str::to_string).is_none());
    assert!(dc.poll_at(start + Duration::from_secs(29), str::to_string).is_none());
    assert!(tg.poll_at(start + Duration::from_secs(30), str::to_string).is_some());
    assert!(dc.poll_at(start + Duration::from_secs(30), str::to_string).is_some());
}

#[test]
fn progress_truncation_respects_each_channels_limit() {
    use crate::channels::dispatch::{format_progress, ProgressLimit};
    let tg_limit = crate::telegram::tg::MAX_TG_CHARS;
    let dc_limit = text::MAX_DISCORD_UTF16;

    let normal = format!("running Shell: {}", "x".repeat(crate::activity::MAX_TITLE_CHARS));
    let tg_normal = format_progress(&normal, 30, ProgressLimit::Chars(tg_limit));
    let dc_normal = format_progress(&normal, 30, ProgressLimit::Utf16(dc_limit));
    assert!(tg_normal.chars().count() <= tg_limit);
    assert!(dc_normal.encode_utf16().count() <= dc_limit);
    assert!(tg_normal.contains(&"x".repeat(crate::activity::MAX_TITLE_CHARS)));
    assert!(dc_normal.contains(&"x".repeat(crate::activity::MAX_TITLE_CHARS)));

    let huge_ascii = format!("running Shell: {}", "a".repeat(10_000));
    let tg_clamped = format_progress(&huge_ascii, 95, ProgressLimit::Chars(tg_limit));
    let dc_clamped = format_progress(&huge_ascii, 95, ProgressLimit::Utf16(dc_limit));
    assert_eq!(tg_clamped.chars().count(), tg_limit);
    assert!(tg_clamped.ends_with("… (95s)"));
    assert_eq!(dc_clamped.encode_utf16().count(), dc_limit);
    assert!(dc_clamped.ends_with("… (95s)"));
    assert_eq!(text::discord(&dc_clamped), vec![Chunk::Text(dc_clamped)]);

    // Astral emoji (2 UTF-16 code units per scalar) must stay within Discord's 2000 UTF-16 budget.
    let huge_emoji = format!("running {}", "🦀".repeat(1_500));
    let dc_emoji = format_progress(&huge_emoji, 120, ProgressLimit::Utf16(dc_limit));
    assert!(dc_emoji.encode_utf16().count() <= dc_limit);
    assert!(dc_emoji.ends_with("… (120s)"));
    assert_eq!(text::discord(&dc_emoji), vec![Chunk::Text(dc_emoji)]);
}

#[tokio::test]
async fn slow_turn_produces_progress_while_fast_turn_produces_none() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let fast_engine = Dispatcher::new(server.clone(), mock.wire.clone());

    // 1. Fast turn (<20s): produces "⏳ On it." and the answer, with zero progress edits.
    fast_engine
        .accept(incoming("fast-turn", "fast question"))
        .await
        .unwrap();
    fast_engine.step().await.unwrap();
    fast_engine.step().await.unwrap();
    server
        .test_store()
        .lock()
        .await
        .append(&sid, "agent", "Fast answer.")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.channels.jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    fast_engine.stop().await;
    assert!(!mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.2["content"]
            .as_str()
            .is_some_and(|t| t.contains("⏳ Still working"))));
    assert!(mock
        .data
        .messages
        .lock()
        .unwrap()
        .values()
        .any(|m| m == "Fast answer."));

    // 2. Slow turn: edits "⏳ On it." in place with redacted activity, then delivers final answer.
    let engine = Dispatcher::with_progress_cadence(
        server.clone(),
        mock.wire.clone(),
        Duration::from_millis(80),
        Duration::from_millis(80),
    );
    let before_posts = mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.0 == "POST" && c.1.ends_with("/messages"))
        .count();
    server
        .test_set_activity(
            &sid,
            "Shell",
            "Shell: cargo test -- token=fake-token-NEVER-LOG-ME",
            "running",
        )
        .await;
    engine
        .accept(incoming("slow-turn", "run slow tool"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    engine.step().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if mock
                .data
                .messages
                .lock()
                .unwrap()
                .values()
                .any(|m| m.starts_with("⏳ Still working — running Shell: cargo test"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let msgs = mock.data.messages.lock().unwrap().clone();
    assert!(msgs
        .values()
        .all(|m| !m.contains("fake-token-NEVER-LOG-ME")));
    assert!(msgs
        .values()
        .any(|m| m.contains("⏳ Still working — running Shell: cargo test -- token=[REDACTED]")));
    // Verify no extra POST was created for the progress ticker (it edited the ack message in place).
    let mid_posts = mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.0 == "POST" && c.1.ends_with("/messages"))
        .count();
    assert_eq!(mid_posts - before_posts, 1, "only the ack message was POSTed");

    server
        .test_store()
        .lock()
        .await
        .append(&sid, "agent", "Slow turn final answer.")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.channels.jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(mock
        .data
        .messages
        .lock()
        .unwrap()
        .values()
        .any(|m| m == "Slow turn final answer."));
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 0);
    engine.stop().await;
}

#[tokio::test]
async fn progress_edit_failure_never_fails_the_turn() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::with_progress_cadence(
        server.clone(),
        mock.wire.clone(),
        Duration::from_millis(80),
        Duration::from_millis(80),
    );

    // Force PATCH to fail with HTTP 500 when the progress ticker fires.
    mock.data.mode.store(7, Ordering::SeqCst);
    server
        .test_set_activity(&sid, "Shell", "Shell: cargo check", "running")
        .await;
    engine
        .accept(incoming("edit-fail-turn", "work through transient edit error"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    engine.step().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if mock
                .data
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.0 == "PATCH")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    // Restore normal transport and finish the turn: final answer must arrive intact.
    mock.data.mode.store(0, Ordering::SeqCst);
    server
        .test_store()
        .lock()
        .await
        .append(&sid, "agent", "Answer after failed progress edit.")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.channels.jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let msgs = mock.data.messages.lock().unwrap().clone();
    assert!(msgs
        .values()
        .any(|m| m == "Answer after failed progress edit."));
    assert!(!msgs
        .values()
        .any(|m| m.contains("Could not deliver reply")));
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 0);
    engine.stop().await;
}

#[tokio::test]
async fn non_owner_reply_containing_terminal_delivers_text_plus_note_and_no_link() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());

    engine
        .accept(incoming("member-turn", "how do I open a shell?"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    engine.step().await.unwrap();
    server
        .test_store()
        .lock()
        .await
        .append(
            &sid,
            "agent",
            "Here is the setup summary. Use /terminal to open a shell.",
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.channels.jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let msgs = mock.data.messages.lock().unwrap().clone();
    assert!(msgs.values().any(|m| {
        m.contains("Here is the setup summary. Use /terminal to open a shell.")
            && m.contains("/terminal is owner-only")
    }));
    assert!(!msgs.values().any(|m| m.contains("One-time /terminal link")));
    assert!(!msgs.values().any(|m| m.contains("Could not deliver reply")));
    assert!(!mock
        .data
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.2.get("components").is_some()));
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 0);
    engine.stop().await;
}

#[tokio::test]
async fn owner_reply_containing_terminal_is_unchanged() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = self::server().await;
    let code = server.channels.mint("owner", &sid, Some("user"), 300).unwrap();
    server.channels.redeem(&code, "user", "dm").unwrap();
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());

    engine
        .accept(incoming("owner-turn", "how do I open a shell?"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    engine.step().await.unwrap();
    let reply = "Here is the setup summary. Use /terminal to open a shell.";
    server
        .test_store()
        .lock()
        .await
        .append(&sid, "agent", reply)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.channels.jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let msgs = mock.data.messages.lock().unwrap().clone();
    assert!(msgs.values().any(|m| m == reply));
    assert!(!msgs.values().any(|m| m.contains("owner-only")));
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 0);
    engine.stop().await;
}

#[tokio::test]
async fn owner_only_failure_never_produces_could_not_deliver_reply_fallback() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());

    // Member starts a normal turn ("⏳ On it.") and also invokes /terminal and /web mid-turn.
    engine
        .accept(incoming("member-normal-turn", "identify this frame"))
        .await
        .unwrap();
    engine.step().await.unwrap();
    engine.step().await.unwrap();
    engine
        .accept(incoming("member-slash-terminal", "/terminal"))
        .await
        .unwrap();
    engine
        .accept(incoming("member-slash-web", "/web"))
        .await
        .unwrap();
    engine.step().await.unwrap();

    server
        .test_store()
        .lock()
        .await
        .append(&sid, "agent", "Steel Ball Run — Gyro Zeppeli.")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.channels.jobs().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let msgs = mock.data.messages.lock().unwrap().clone();
    assert!(msgs
        .values()
        .any(|m| m == "Steel Ball Run — Gyro Zeppeli."));
    assert!(msgs
        .values()
        .any(|m| m.contains("/terminal is owner-only")));
    assert!(msgs.values().any(|m| m.contains("/web is owner-only")));
    assert!(!msgs
        .values()
        .any(|m| m.contains("Could not deliver reply")));
    assert!(!msgs.values().any(|m| m.contains("One-time /")));
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 0);
    engine.stop().await;
}

#[tokio::test]
async fn slash_new_session_progress_shows_tool_history_rows() {
    // Sessions created by the /new slash command (discord-interaction…) and harnesses
    // like agy that record tool calls only as `tool` history rows must still update
    // the "⏳ On it." ack with what the agent is doing.
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, old) = server().await;
    pair(&server, &old);
    let engine = Dispatcher::with_progress_cadence(
        server.clone(),
        mock.wire.clone(),
        Duration::from_millis(80),
        Duration::from_millis(80),
    );
    engine.accept(incoming("interaction:1", "/new")).await.unwrap();
    engine.step().await.unwrap();
    let sid = server.channels.session("alice").unwrap().unwrap();
    assert_ne!(sid, old);
    engine.accept(incoming("message:2", "search some news for hong kong")).await.unwrap();
    engine.step().await.unwrap();
    engine.step().await.unwrap();
    server
        .test_store()
        .lock()
        .await
        .append(&sid, "tool", "\"Hong Kong news latest\" on Web")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if mock.data.messages.lock().unwrap().values().any(|m| {
                m.starts_with("⏳ Still working — running \"Hong Kong news latest\" on Web")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("ack never edited: {:?}", mock.data.messages.lock().unwrap()));
    server.test_store().lock().await.append(&sid, "agent", "HK headlines.").unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !server.channels.jobs().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(mock.data.messages.lock().unwrap().values().any(|m| m == "HK headlines."));
    engine.stop().await;
}

#[test]
fn ticker_first_update_comes_early() {
    use crate::channels::dispatch::{ProgressLimit, ProgressTicker, PROGRESS_INITIAL_DELAY};
    assert!(PROGRESS_INITIAL_DELAY <= Duration::from_secs(5));
    let t0 = tokio::time::Instant::now();
    let mut t = ProgressTicker::with_start(ProgressLimit::Utf16(2000), t0);
    t.observe_tool_row("\"x\" on Web\nmore");
    let line = t.poll_at(t0 + Duration::from_secs(6), |a| a.to_string()).unwrap();
    assert!(line.contains("running \"x\" on Web"), "{line}");
}

/// ask_user is lead-only: these cards come from a lead session the person is paired to.
async fn lead_server() -> (tempfile::TempDir, Arc<crate::server::Server>, String) {
    let (dir, server, _) = server().await;
    let lead = server.test_call(json!({"op":"create","name":"boss","harness":"claude","role":"lead"})).await.unwrap();
    let sid = lead["id"].as_str().unwrap().to_string();
    (dir, server, sid)
}

/// Every custom_id on the newest card whose first row has `label`, by label.
fn card_buttons(mock: &Mock, label: &str) -> Vec<(String, String)> {
    let calls = mock.data.calls.lock().unwrap();
    calls.iter().rev().find_map(|c| {
        let rows = c.2["components"].as_array()?;
        let all: Vec<(String, String)> = rows.iter().flat_map(|r| r["components"].as_array().cloned().unwrap_or_default())
            .map(|b| (b["label"].as_str().unwrap().to_string(), b["custom_id"].as_str().unwrap()[7..].to_string())).collect();
        all.iter().any(|(l, _)| l == label).then_some(all)
    }).unwrap_or_default()
}

fn last_card_text(mock: &Mock) -> String {
    mock.data.calls.lock().unwrap().iter().rev()
        .find_map(|c| c.2["content"].as_str().filter(|t| t.contains("Question from")).map(String::from)).unwrap_or_default()
}

async fn tap(engine: &Arc<Dispatcher>, id: &str, action: &str) {
    let mut click = incoming(id, "");
    click.action = Some(action.into());
    engine.accept(click).await.unwrap();
    engine.step().await.unwrap();
}

async fn ask(server: &Arc<crate::server::Server>, sid: &str, q: Value) -> tokio::task::JoinHandle<Value> {
    let mut req = json!({"op":"ask_user","caller":sid,"question":"Which fruit?","header":"Fruit"});
    for (k, v) in q.as_object().unwrap() { req[k] = v.clone(); }
    let (s, task) = (server.clone(), tokio::spawn({ let s = server.clone(); async move { s.test_call(req).await.unwrap() } }));
    for _ in 0..200 {
        if !s.test_call(json!({"op":"approvals"})).await.unwrap().as_array().unwrap().is_empty() { break; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task
}

#[tokio::test]
async fn ask_user_card_single_choice_tap_and_dismiss() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = lead_server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    let task = ask(&server, &sid, json!({"choices":["Mango","Kiwi","Papaya"]})).await;
    engine.sync_prompts(&address(), &sid).await.unwrap();
    // One button per choice, plus Dismiss; the question is shown, not raw JSON.
    let buttons = card_buttons(&mock, "Kiwi");
    let labels: Vec<&str> = buttons.iter().map(|b| b.0.as_str()).collect();
    assert_eq!(labels, ["Mango", "Kiwi", "Papaya", "Dismiss"]);
    let text = last_card_text(&mock);
    assert!(text.contains("Which fruit?") && text.contains("Tap a choice") && !text.contains("\"questions\""), "{text}");
    tap(&engine, "tap-kiwi", &buttons[1].1).await;
    assert_eq!(task.await.unwrap(), json!({"status":"answered","answer":"Kiwi"}));
    // The card is edited to show the answer, with its buttons removed.
    let calls = mock.data.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.2["components"] == json!([]) && c.2["content"].as_str().unwrap_or("").contains("✔ Answered: Kiwi")));
    // Dismiss returns `dismissed` and the card says so.
    let task = ask(&server, &sid, json!({"choices":["Mango","Kiwi"]})).await;
    engine.sync_prompts(&address(), &sid).await.unwrap();
    let dismiss = card_buttons(&mock, "Dismiss").into_iter().find(|b| b.0 == "Dismiss").unwrap();
    tap(&engine, "tap-dismiss", &dismiss.1).await;
    assert_eq!(task.await.unwrap()["status"], "dismissed");
    let calls = mock.data.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.2["components"] == json!([]) && c.2["content"].as_str().unwrap_or("").contains("✖ Dismissed.")));
}

#[tokio::test]
async fn ask_user_card_multi_select_toggles_then_done() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = lead_server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    let task = ask(&server, &sid, json!({"choices":["Mango","Kiwi","Papaya"],"multi_select":true})).await;
    engine.sync_prompts(&address(), &sid).await.unwrap();
    let buttons = card_buttons(&mock, "Done");
    let labels: Vec<&str> = buttons.iter().map(|b| b.0.as_str()).collect();
    assert_eq!(labels, ["⬜ Mango", "⬜ Kiwi", "⬜ Papaya", "Done", "Dismiss"]);
    // Done with nothing picked is refused; the question stays open.
    tap(&engine, "done-empty", &buttons[3].1).await;
    assert!(!task.is_finished());
    tap(&engine, "t-mango", &buttons[0].1).await;
    tap(&engine, "t-papaya", &buttons[2].1).await;
    tap(&engine, "t-mango-off", &buttons[0].1).await;
    tap(&engine, "t-kiwi", &buttons[1].1).await;
    let now: Vec<String> = card_buttons(&mock, "Done").into_iter().map(|b| b.0).collect();
    assert_eq!(now, ["⬜ Mango", "☑ Kiwi", "☑ Papaya", "Done", "Dismiss"]);
    tap(&engine, "done", &buttons[3].1).await;
    assert_eq!(task.await.unwrap(), json!({"status":"answered","answer":"Kiwi, Papaya"}));
}

#[tokio::test]
async fn ask_user_card_free_text_reply_answers_it() {
    let mock = mock(Duration::ZERO).await;
    let (_dir, server, sid) = lead_server().await;
    pair(&server, &sid);
    let engine = Dispatcher::new(server.clone(), mock.wire.clone());
    let task = ask(&server, &sid, json!({"choices":["Mango","Kiwi"]})).await;
    engine.sync_prompts(&address(), &sid).await.unwrap();
    engine.accept(incoming("reply-1", "a ripe papaya")).await.unwrap();
    engine.step().await.unwrap();
    assert_eq!(task.await.unwrap(), json!({"status":"answered","answer":"a ripe papaya"}));
    assert!(last_card_text(&mock).contains("✔ Answered: a ripe papaya"));
    // With no question pending, a message goes to the session as usual.
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 0);
}
