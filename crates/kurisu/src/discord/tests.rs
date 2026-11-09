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
        let id = path.rsplit('/').next().unwrap().to_string();
        if let Some(content) = payload["content"].as_str() {
            s.messages
                .lock()
                .unwrap()
                .insert(id.clone(), content.into());
        }
        (200, json!({"id":id}))
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
    server.channels.route(&sid, &address()).unwrap();
    bridge
        .post_message(
            &sid,
            &[json!({"type":"input_text","text":"Telegram origin"})],
            false,
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
    assert_eq!(server.channels.health().unwrap()["failed_jobs"], 1);
    assert!(mock
        .data
        .messages
        .lock()
        .unwrap()
        .values()
        .any(|v| v.contains("locally paired owner")));
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
    let calls=mock.data.calls.lock().unwrap();assert!(calls[0].1.ends_with("/callback"));assert_eq!(calls[0].2["type"],5);assert_eq!(calls[0].2["data"]["flags"],64);
}
