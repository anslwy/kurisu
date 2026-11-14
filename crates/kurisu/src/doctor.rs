//! Offline diagnostics. Never starts a server, installs a browser, or migrates data.
use std::{path::Path, time::Duration};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use crate::{browser, paths::Paths, remote, server, store::Store, version, web};

pub const PROTOCOL: u64 = 1;
const RESTART: &str = "the running server is older than this binary; restart it with ku shutdown && ku serve";

pub fn unknown_op(error: &str) -> bool {
    error.strip_prefix("unknown op ").is_some_and(|op| !op.is_empty() && !op.chars().any(char::is_whitespace))
}
pub fn older_binary_hint(error: &str, daemon: &Value) -> String {
    let check = build_check(&version::info(), daemon);
    format!("{error}; {RESTART}; binary SHA={}; daemon SHA={}", check.expected.as_str().unwrap_or("unknown"), check.actual.as_str().unwrap_or("unknown"))
}

pub fn bundle_check(bundle: &str, bundle_sha: &str, current_bundle: &str, daemon: &Value) -> Check {
    compare("WEB BUNDLE", json!({"bundle":current_bundle,"git_sha":daemon["git_sha"]}), json!({"bundle":bundle,"git_sha":bundle_sha}))
}

pub async fn web_chat_line(paths: &Paths) -> String {
    let mut check = match remote::load_web(&paths.home) {
        Some(cfg) => probe_listener(cfg, true).await.remove(0),
        None => compare("/WEB CHAT", json!({"enabled":false}), json!({"enabled":false})),
    };
    check.check = "/WEB CHAT".into();
    check.line()
}

#[derive(Debug, Serialize)]
pub struct Check {
    check: String,
    status: &'static str,
    expected: Value,
    actual: Value,
    detail: String,
}
impl Check {
    fn line(&self) -> String {
        format!("{:<18} {:<4} {}", self.check, self.status,
            if self.detail == "not running" || (self.expected.is_null() && self.actual.is_null()) { self.detail.clone() }
            else { format!("{} / {} {}", self.expected, self.actual, self.detail) })
    }
}
fn row(name: &str, status: &'static str, expected: Value, actual: Value, detail: impl Into<String>) -> Check {
    Check { check: name.into(), status, expected, actual, detail: detail.into() }
}
fn compare(name: &str, expected: Value, actual: Value) -> Check {
    let status = if expected == actual { "OK" } else { "FAIL" };
    row(name, status, expected, actual, "")
}
fn idle(name: &str) -> Check { row(name, "OK", Value::Null, Value::Null, "not running") }
async fn call(sock: &Path, op: &str) -> Result<Value> {
    // Doctor must answer about a dead server at once, not ride out a deploy's retry
    // window: a short connect budget keeps "nothing is listening" a real NotFound.
    tokio::time::timeout(Duration::from_secs(2), server::call_budget(sock, json!({"op":op,"protocol":PROTOCOL}), Duration::from_millis(300))).await?
}
fn absent(error: &anyhow::Error) -> bool {
    error.chain().filter_map(|e| e.downcast_ref::<std::io::Error>()).any(|e| matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused))
}
fn build_check(binary: &Value, daemon: &Value) -> Check {
    let mut c = compare("DAEMON vs BINARY", binary["git_sha"].clone(), daemon["git_sha"].clone());
    c.detail = format!("daemon dirty={} built={} started_at={}; binary dirty={} built={}", daemon["dirty"], daemon["built"], daemon["started_at"], binary["dirty"], binary["built"]);
    if c.status == "OK" && (daemon["dirty"] == true || binary["dirty"] == true) { c.status = "WARN"; }
    if binary["git_sha"].as_str().is_none_or(|s| s.is_empty() || s == "unknown") || daemon["git_sha"].as_str().is_none_or(|s| s.is_empty() || s == "unknown") {
        c.status = "WARN";
        c.detail.push_str("; build identity unavailable");
    }
    c
}
async fn daemon(paths: &Paths) -> (bool, Vec<Check>) {
    match call(&paths.socket(), "version").await {
        Err(e) if absent(&e) => (false, vec![idle("DAEMON vs BINARY"), idle("PROTOCOL")]),
        response => {
            let binary = version::info();
            let build = match response {
                Ok(v) => {
                    build_check(&binary, &v)
                }
                Err(e) => row("DAEMON vs BINARY", "FAIL", binary, Value::Null, e.to_string()),
            };
            let protocol = match call(&paths.socket(), "doctor").await {
                Ok(v) => compare("PROTOCOL", json!(PROTOCOL), v["protocol"].clone()),
                Err(e) => row("PROTOCOL", "FAIL", json!(PROTOCOL), Value::Null, e.to_string()),
            };
            (true, vec![build, protocol])
        }
    }
}
fn worker(index: &str, sw: &str) -> Check {
    let cache = sw.split("const CACHE").nth(1).and_then(|s| s.split_once('=').map(|(_, s)| s.trim_start()))
        .and_then(|s| s.chars().next().filter(|q| *q == '\'' || *q == '"').map(|q| (s, q)))
        .and_then(|(s,q)| s.get(1..)?.split(q).next()).unwrap_or("missing cache name");
    let hash = web::sw_version(index);
    let mut c = compare("SERVICE WORKER", json!(format!("kurisu-shell-{hash}")), json!(cache));
    if hash == "dev" && c.status == "OK" { c.status = "WARN"; c.detail = "index has no hashed main bundle".into(); }
    c
}
async fn fetch(addr: std::net::SocketAddr, path: &str) -> Result<String> {
    // No proxies, redirects, DNS or remote endpoints: only the configured local listener.
    let client = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(2)).build()?;
    Ok(client.get(format!("http://{addr}{path}")).send().await?.error_for_status()?.text().await?)
}
async fn listener(paths: &Paths, running: bool) -> Vec<Check> {
    let mut checks = standalone_listener(paths, running).await;
    if paths.home.join("web.json").exists() {
        if let Some(cfg) = remote::load_web(&paths.home) {
            let mut paired = probe_listener(cfg, running).await;
            paired[0].check = "/WEB CHAT".into();
            checks.push(paired.remove(0));
        }
    }
    if paths.home.join("web.json").exists() { checks.push(standalone_build(paths).await); }
    checks
}
fn frontend_build_check(expected: &Value, actual: &Value) -> Check {
    let current=crate::web_process::same_build(expected,actual);
    let mut check=row("/WEB BUILD",if current { "OK" } else { "FAIL" },
        json!({"git_sha":expected["build"]["git_sha"],"built":expected["build"]["built"]}),
        json!({"git_sha":actual["build"]["git_sha"],"built":actual["build"]["built"]}),
        format!("pid={} assets={}",actual["pid"],if expected["assets"]==actual["assets"] { "match" } else { "STALE" }));
    if !current { check.detail.push_str("; stale frontend: recycle ku web with the current binary"); }
    check
}
async fn standalone_build(paths: &Paths) -> Check {
    let cfg=std::fs::read(paths.home.join("web.json")).ok().and_then(|b|serde_json::from_slice::<remote::Config>(&b).ok());
    let Some(cfg)=cfg.filter(|c|c.enabled) else { return idle("/WEB BUILD") };
    let addr=match cfg.listen.parse() { Ok(a)=>a,Err(e)=>return row("/WEB BUILD","FAIL",Value::Null,Value::Null,format!("invalid listener: {e}")) };
    let expected=call(&paths.socket(),"doctor").await.ok().and_then(|v|v.get("web_build").cloned()).unwrap_or_else(web::build_stamp);
    match crate::web_process::fetch_stamp(addr).await {
        Ok(actual)=>frontend_build_check(&expected,&actual),
        Err(e)=> {
            let assets=crate::web_process::fetch_assets(addr).await.ok();
            let status=if assets.as_ref().is_some_and(|a|json!(a)!=expected["assets"]) { "FAIL" } else { "WARN" };
            row("/WEB BUILD",status,expected["build"]["git_sha"].clone(),Value::Null,format!("frontend build unverified; restart ku web: {e}"))
        }
    }
}
async fn standalone_listener(paths: &Paths, running: bool) -> Vec<Check> {
    // The standalone UI records its bound address in web.json. Paired /web
    // uses weblink.json; it has no service worker of its own.
    let metadata = paths.home.join("web.json");
    let cfg = if metadata.exists() {
        match std::fs::read(&metadata).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice::<remote::Config>(&b)?)) {
            Ok(c) => Some(c),
            Err(e) => return vec![row("/WEB LISTENER", "FAIL", json!("valid web.json"), json!(e.to_string()), ""), idle("SERVICE WORKER")],
        }
    } else {
        let file = remote::web_config_path(&paths.home);
        if file.exists() && remote::load_web(&paths.home).is_none() {
            return vec![row("/WEB LISTENER", "FAIL", json!("valid weblink.json"), Value::Null, "cannot read configuration"), idle("SERVICE WORKER")];
        }
        remote::load_web(&paths.home)
    };
    let Some(cfg) = cfg else { return vec![idle("/WEB LISTENER"), idle("SERVICE WORKER")]; };
    probe_listener(cfg, running).await
}
async fn probe_listener(cfg: remote::Config, running: bool) -> Vec<Check> {
    let addr = match cfg.listen.parse::<std::net::SocketAddr>() {
        Ok(mut a) => { if a.ip().is_unspecified() { a.set_ip(if a.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() }); } a },
        Err(e) => return vec![row("/WEB LISTENER", "FAIL", json!(cfg.listen), Value::Null, e.to_string()), idle("SERVICE WORKER")],
    };
    let connected = matches!(tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr)).await, Ok(Ok(_)));
    let mut check = compare("/WEB LISTENER", json!({"enabled": cfg.enabled, "listen":addr.to_string()}), json!({"enabled":connected,"listen":addr.to_string()}));
    if !running && !connected { check.status = "OK"; check.detail = "not running".into(); }
    let sw = if connected {
        match (fetch(addr, "/index.html").await, fetch(addr, "/sw.js").await) {
            (Ok(index), Ok(sw)) => worker(&index, &sw),
            // The paired chat listener does not expose the standalone service worker.
            (a,b) => row("SERVICE WORKER", "WARN", json!("served index.html and sw.js"), Value::Null,
                format!("worker not exposed by listener: index={}, sw={}", a.err().map(|e|e.to_string()).unwrap_or_default(), b.err().map(|e|e.to_string()).unwrap_or_default())),
        }
    } else { idle("SERVICE WORKER") };
    vec![check, sw]
}
async fn executable_version(path: &Path) -> Option<String> {
    let output = tokio::time::timeout(Duration::from_secs(2), tokio::process::Command::new(path).arg("--version").kill_on_drop(true).output()).await.ok()?.ok()?;
    if !output.status.success() { return None; }
    String::from_utf8_lossy(&output.stdout).split_whitespace().nth(1).map(String::from)
}
async fn live_browser(paths: &Paths, running: bool) -> Check {
    let configured = browser::load(&paths.home);
    let bin = configured.map(|c| c.lb.into()).or_else(browser::find_lb);
    let Some(bin) = bin else { return idle("LIVE BROWSER"); };
    let checkout = paths.home.join("lb").join(format!("src-{}", browser::LB_PINNED_COMMIT));
    let tag = std::process::Command::new("git").arg("-C").arg(checkout).args(["describe","--tags","--exact-match",browser::LB_PINNED_COMMIT]).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_else(|| browser::LB_PINNED.into());
    let actual = executable_version(&bin).await.unwrap_or_else(|| "unavailable".into());
    let mut c = compare("LIVE BROWSER", json!(tag.trim_start_matches('v')), json!(actual.trim_start_matches('v')));
    c.detail = format!("pin {}; installed {}", browser::LB_PINNED_COMMIT, bin.display());
    if running {
        if let Ok(reply) = call(&paths.socket(), "doctor").await {
            if let Some(pid) = reply["lb_pid"].as_u64() {
                // Linux keeps the running inode accessible even after an install replaces it.
                match executable_version(Path::new(&format!("/proc/{pid}/exe"))).await {
                    Some(v) => {
                        if v.trim_start_matches('v') != tag.trim_start_matches('v') { c.status = "FAIL"; }
                        c.actual = json!({"installed":actual,"running":v});
                        c.expected = json!({"installed":tag.trim_start_matches('v'),"running":tag.trim_start_matches('v')});
                    }
                    None => { if c.status == "OK" { c.status = "WARN"; } c.detail.push_str("; running lb version unavailable"); }
                }
            }
        }
    }
    c
}
fn data(paths: &Paths) -> Check {
    if !paths.db().exists() { return idle("DATA"); }
    match Store::check_read_only(&paths.db()) {
        Ok(()) => row("DATA", "OK", json!("readable UUID database"), json!("readable UUID database"), ""),
        Err(e) => row("DATA", "FAIL", json!("readable UUID database"), json!(e.to_string()), ""),
    }
}
/// The outward MCP server: OK only when the switch is on AND the daemon really
/// holds the port; OFF when disabled; FAIL when enabled but nothing listens or
/// when someone else owns the port.
async fn mcp_server(paths: &Paths, running: bool) -> Check {
    let Some(cfg) = crate::mcp_out::load_config(&paths.home) else {
        return row("MCP SERVER", "OFF", Value::Null, Value::Null, "off; send /mcp_server in Telegram to enable it");
    };
    let listen = format!("{}:{}", cfg.host, cfg.port);
    if !cfg.enabled {
        return row("MCP SERVER", "OFF", Value::Null, Value::Null, format!("off ({listen}); send /mcp_server in Telegram to enable it"));
    }
    let st = if running { call(&paths.socket(), "mcp_server_status").await.ok() } else { None };
    let listening = st.as_ref().is_some_and(|v| v["listening"] == json!(true));
    if listening {
        let pid = st.as_ref().and_then(|v| v["pid"].as_u64()).unwrap_or(0);
        return row("MCP SERVER", "OK", json!({"enabled":true,"listen":listen}), json!({"enabled":true,"listening":true}),
                   format!("daemon pid {pid} holds it"));
    }
    // Never call someone else's listener OK, and never kill it either.
    let detail = match crate::remote::port_owner(cfg.port) {
        Some(owner) => format!("port {} is held by {owner}, not this daemon; stop it or pick another port", cfg.port),
        None if !running => "enabled but the daemon is not running, so nothing listens".to_string(),
        None => st.as_ref().and_then(|v| v["note"].as_str()).filter(|n| !n.trim().is_empty()).map(String::from)
            .unwrap_or_else(|| "enabled but not listening; send /mcp_server and press Enable".to_string()),
    };
    row("MCP SERVER", "FAIL", json!({"enabled":true,"listen":listen}), json!({"enabled":true,"listening":false}), detail)
}
fn harnesses() -> Check {
    let all = crate::harness::all();
    let missing = crate::harness::missing_required(&all);
    let actual: serde_json::Map<String, Value> =
        all.iter().map(|(n, p)| (n.to_string(), json!(p.as_ref().map(|p| p.to_string_lossy().into_owned())))).collect();
    let mut c = row(
        "HARNESSES",
        if missing.is_empty() { "OK" } else { "FAIL" },
        json!("absolute path for claude, codex, opencode and agy (fx and pi optional)"),
        json!(actual),
        "",
    );
    if !missing.is_empty() {
        c.detail = format!("missing: {}", missing.join(", "));
    }
    c
}
async fn diagnose(paths: &Paths) -> Vec<Check> {
    let (running, mut checks) = daemon(paths).await;
    checks.extend(listener(paths, running).await);
    checks.push(live_browser(paths, running).await);
    checks.push(mcp_server(paths, running).await);
    checks.push(harnesses()); checks.push(data(paths));
    checks
}
pub async fn cli(paths: &Paths, as_json: bool) -> Result<()> {
    let checks = diagnose(paths).await;
    if as_json { println!("{}", serde_json::to_string_pretty(&checks)?); }
    else { for c in &checks { println!("{}", c.line()); } }
    if checks.iter().any(|c| c.status == "FAIL") { std::process::exit(1); }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn standalone_frontend_stale_build_is_not_ok() {
        let expected=serde_json::json!({"build":{"git_sha":"new","built":"2"},"assets":["main-new.js"]});
        let stale=serde_json::json!({"build":{"git_sha":"old","built":"1"},"assets":["main-old.js"]});
        assert_eq!(super::frontend_build_check(&expected,&stale).status,"FAIL");
        assert_eq!(super::frontend_build_check(&expected,&expected).status,"OK");
        let mut same_commit=expected.clone(); same_commit["build"]["built"]=serde_json::json!("1");
        assert_eq!(super::frontend_build_check(&expected,&same_commit).status,"FAIL");
    }

    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    struct TestHome { paths: Paths, _data: tempfile::TempDir }
    impl std::ops::Deref for TestHome {
        type Target = Paths;
        fn deref(&self) -> &Paths { &self.paths }
    }
    impl Drop for TestHome {
        fn drop(&mut self) { let _ = std::fs::remove_file(&self.paths.home); }
    }
    fn home() -> TestHome {
        let base = std::env::var_os("KURISU_TEST_TMPDIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let data = tempfile::tempdir_in(base).unwrap();
        // Keep the isolated KURISU_HOME/socket short; all file contents live on
        // the test filesystem rather than a potentially quota-limited /tmp.
        let home = std::path::PathBuf::from(format!("/tmp/kud-{}", uuid::Uuid::new_v4().simple()));
        std::os::unix::fs::symlink(data.path(), &home).unwrap();
        TestHome { paths: Paths { home }, _data: data }
    }
    async fn fixture(paths: &Paths, replies: Vec<Value>) -> tokio::task::JoinHandle<()> {
        let socket = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        tokio::spawn(async move {
            for reply in replies {
                let (mut stream, _) = socket.accept().await.unwrap();
                let mut request = String::new();
                BufReader::new(&mut stream).read_line(&mut request).await.unwrap();
                let request: Value = serde_json::from_str(&request).unwrap();
                assert!(matches!(request["op"].as_str(), Some("version" | "doctor" | "remote_offer")));
                stream.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
            }
        })
    }
    #[test]
    fn deployment_assertion_reports_failure_without_rollback() {
        use std::os::unix::fs::PermissionsExt;
        let paths = home();
        let bin = paths.home.join("ku-fixture");
        let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/doctor-after-deploy.sh");
        for code in [0, 1] {
            std::fs::write(&bin, format!("#!/bin/sh\necho 'CHECK {}'\nexit {code}\n", if code == 0 { "OK" } else { "FAIL" })).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
            let output = std::process::Command::new("sh").args(["-c", "set -e; trap '[ $? -eq 0 ] || echo ROLLBACK' EXIT; . \"$1\"; doctor_after_deploy; echo STILL_DEPLOYED", "fixture"]).arg(&helper)
                .env("LIVE_BIN", &bin).env("HOME_DIR", &paths.home).output().unwrap();
            let out = String::from_utf8_lossy(&output.stdout);
            let err = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success() && out.contains("STILL_DEPLOYED") && !out.contains("ROLLBACK"));
            assert_eq!(err.contains("ERROR: doctor assertion FAIL"), code == 1);
        }
    }
    #[test]
    fn clean_build_identity() {
        let binary = json!({"git_sha":"abc","dirty":false});
        assert_eq!(build_check(&binary, &binary).status, "OK");
        assert_eq!(build_check(&binary, &json!({"git_sha":"old"})).status, "FAIL");
        assert_eq!(build_check(&binary, &json!({"git_sha":"abc","dirty":true})).status, "WARN");
    }
    #[tokio::test]
    async fn installed_browser_version_fixture() {
        use std::os::unix::fs::PermissionsExt;
        let paths = home();
        let bin = paths.home.join("lb");
        for (version, status) in [(browser::LB_PINNED.trim_start_matches('v'), "OK"), ("0.0.1", "FAIL")] {
            std::fs::write(&bin, format!("#!/bin/sh\nprintf 'lb {version}\\n'\n")).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(browser::config_path(&paths.home), json!({"enabled":true,"lb":bin,"port":0,"passcode":"fixture","listen":"127.0.0.1:0"}).to_string()).unwrap();
            assert_eq!(live_browser(&paths, false).await.status, status);
        }
    }
    #[tokio::test]
    async fn socket_build_agreement_and_protocol() {
        for sha in [env!("KURISU_BUILD_COMMIT"), "older-build"] {
            let paths = home();
            let task = fixture(&paths, vec![json!({"ok":{"git_sha":sha}}), json!({"ok":{"protocol":PROTOCOL}})]).await;
            let (running, rows) = daemon(&paths).await;
            assert!(running);
            assert_eq!(rows[0].status, if sha != env!("KURISU_BUILD_COMMIT") { "FAIL" } else if version::info()["dirty"] == true { "WARN" } else { "OK" });
            assert_eq!(rows[1].status, "OK");
            task.await.unwrap();
            drop(paths);
        }
    }
    #[tokio::test]
    async fn mcp_server_row_is_off_when_disabled_and_fail_when_enabled_but_silent() {
        let paths = home();
        let c = super::mcp_server(&paths, false).await;
        assert_eq!(c.status, "OFF");
        assert!(c.detail.contains("/mcp_server"), "{}", c.detail);
        // Enabled but nothing can be listening: FAIL, never OK.
        let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
        crate::mcp_out::save_config(&paths.home,
            &crate::mcp_out::Config { enabled: true, host: "127.0.0.1".into(), port }).unwrap();
        let c = super::mcp_server(&paths, false).await;
        assert_eq!(c.status, "FAIL");
        assert!(c.detail.contains("daemon is not running"), "{}", c.detail);
        // Disabled again: OFF, not FAIL.
        crate::mcp_out::save_config(&paths.home,
            &crate::mcp_out::Config { enabled: false, host: "127.0.0.1".into(), port }).unwrap();
        let c = super::mcp_server(&paths, false).await;
        assert_eq!(c.status, "OFF");
        assert!(c.detail.contains(&port.to_string()), "{}", c.detail);
    }

    #[tokio::test]
    async fn unknown_ops_explain_restart() {
        let paths = home();
        let task = fixture(&paths, vec![json!({"ok":version::info()}), json!({"error":"unknown op doctor"}), json!({"ok":{"git_sha":"daemon-fixture-sha"}}), json!({"error":"unknown op remote_offer"}), json!({"ok":{"git_sha":"daemon-fixture-sha"}})]).await;
        let (_, rows) = daemon(&paths).await;
        assert_eq!(rows[1].status, "FAIL");
        assert!(rows[1].detail.contains(RESTART));
        assert!(rows[1].detail.contains("daemon SHA=daemon-fixture-sha"));
        assert!(rows[1].detail.contains(env!("KURISU_BUILD_COMMIT")));
        let error = server::call(&paths.socket(), json!({"op":"remote_offer"})).await.unwrap_err().to_string();
        assert!(error.contains(RESTART) && error.contains("daemon-fixture-sha") && error.contains(env!("KURISU_BUILD_COMMIT")));
        assert!(!unknown_op("unknown op doctor failed"));
        task.await.unwrap();
        drop(paths);
    }
    #[tokio::test]
    async fn absent_daemon_is_clean() {
        let paths = home();
        let (running, rows) = daemon(&paths).await;
        assert!(!running);
        assert!(rows.iter().all(|r| r.status == "OK" && r.detail == "not running"));
        let rows = listener(&paths, false).await;
        assert!(rows.iter().all(|r| r.status == "OK"));
        assert_eq!(data(&paths).status, "OK");
        drop(paths);
    }
    async fn http_fixture(index: &'static str, sw: &'static str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            // The connection probe precedes the two HTTP fetches.
            let _ = listener.accept().await.unwrap();
            for body in [index, sw] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).await.unwrap();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
        });
        (addr, task)
    }
    #[tokio::test]
    async fn http_cache_and_listener_checks() {
        for (sw, status) in [("const CACHE = \"kurisu-shell-abc\";", "OK"), ("const CACHE = \"kurisu-shell-v1\";", "FAIL")] {
            let paths = home();
            let (addr, task) = http_fixture("<script src=\"./assets/main-abc.js\"></script>", sw).await;
            std::fs::write(remote::web_config_path(&paths.home), json!({"enabled":true,"listen":addr.to_string()}).to_string()).unwrap();
            let rows = listener(&paths, true).await;
            assert_eq!(rows[0].status, "OK");
            assert_eq!(rows[1].status, status);
            task.await.unwrap();
            let rows = listener(&paths, true).await;
            assert_eq!(rows[0].status, "FAIL");
            let rows = listener(&paths, false).await;
            assert_eq!(rows[0].status, "OK");
            std::fs::write(remote::web_config_path(&paths.home), json!({"enabled":false,"listen":addr.to_string()}).to_string()).unwrap();
            assert_eq!(listener(&paths, true).await[0].status, "OK");
            drop(paths);
        }
    }
    #[tokio::test]
    async fn web_enable_disable_and_reload_match_doctor_without_restart() {
        let paths = home();
        let server = std::sync::Arc::new(crate::server::Server::new(paths.paths.clone()).unwrap());
        let socket = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        let rpc = tokio::spawn({
            let server = server.clone();
            async move {
                loop {
                    let (mut stream, _) = socket.accept().await.unwrap();
                    let mut line = String::new();
                    BufReader::new(&mut stream).read_line(&mut line).await.unwrap();
                    if line.is_empty() { continue; } // CLI's running-server probe
                    let request = serde_json::from_str(&line).unwrap();
                    let response = match server.handle_pub(request).await {
                        Ok(v) => json!({"ok":v}), Err(e) => json!({"error":e.to_string()}),
                    };
                    stream.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                }
            }
        });
        let reserve = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = reserve.local_addr().unwrap();
        drop(reserve);
        // Match a home with both standalone discovery and paired /web config.
        std::fs::write(paths.home.join("web.json"), json!({"enabled":false,"listen":"127.0.0.1:0"}).to_string()).unwrap();
        async fn web_check(paths: &Paths) -> Check {
            diagnose(paths).await.into_iter().find(|c| c.check == "/WEB CHAT").unwrap()
        }
        let args = |action: &str| vec!["ku".into(), "web-link".into(), action.into(), "--listen".into(), addr.to_string()];
        crate::remote::web_cli(&paths, &args("enable")).await.unwrap();
        assert_eq!(web_check(&paths).await.status, "OK");
        assert!(reqwest::get(format!("http://{addr}/_web/")).await.unwrap().status().is_success());
        crate::remote::web_cli(&paths, &args("disable")).await.unwrap();
        assert_eq!(web_check(&paths).await.status, "OK");
        assert!(tokio::net::TcpStream::connect(addr).await.is_err());
        crate::remote::web_cli(&paths, &args("enable")).await.unwrap();
        assert_eq!(web_check(&paths).await.status, "OK");
        // A genuinely stale daemon must still fail; diagnostics must not repair it.
        server.remote.stop().await;
        tokio::task::yield_now().await;
        assert_eq!(web_check(&paths).await.status, "FAIL");
        // The same re-read used at daemon startup restores the persisted listener.
        let restarted = std::sync::Arc::new(crate::server::Server::new(paths.paths.clone()).unwrap());
        restarted.remote.reload_web(&restarted).await.unwrap();
        assert_eq!(web_check(&paths).await.status, "OK");
        restarted.remote.reload_web(&restarted).await.unwrap(); // idempotent
        restarted.remote.stop().await;
        rpc.abort(); let _ = rpc.await;
    }

    #[test]
    fn read_only_data_guard() {
        let paths = home();
        let db = rusqlite::Connection::open(paths.db()).unwrap();
        db.execute_batch("CREATE TABLE sessions(name TEXT PRIMARY KEY)").unwrap();
        drop(db);
        let before = std::fs::read(paths.db()).unwrap();
        let check = data(&paths);
        assert_eq!(check.status, "FAIL");
        assert!(check.actual.as_str().unwrap().contains("pre-UUID"));
        assert_eq!(before, std::fs::read(paths.db()).unwrap());
        drop(paths);
    }
}
