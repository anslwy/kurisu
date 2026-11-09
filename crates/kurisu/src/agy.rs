//! Antigravity owns authentication and conversations; Kurisu speaks only ACP.
use agent_client_protocol::v1::schema::SessionId;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;
use crate::acp::AcpDriver;

pub const MODELS: &[&str] = &[
    "gemini-3.8-flash", "gemini-3.8-flash-high", "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low", "claude-sonnet-4-6", "claude-opus-4-6-thinking",
    "gpt-oss-120b-medium",
];
pub fn bin() -> String {
    std::env::var("KURISU_AGY_BIN").unwrap_or_else(|_| "agy".into())
}

/// Families whose `-high|-medium|-low` suffix is a thought_level, with the
/// level the CLI itself uses when no level is sent.
const FAMILIES: &[(&str, &str)] = &[("gemini-3.8-flash", "high")];

/// Suffixes in CLI model names map to ACP's separate thought_level option.
/// An explicit Kurisu effort overrides the suffix; unspecified effort falls
/// back to the family's CLI default (high for 3.8 Flash). Other model ids are
/// sent verbatim with a high thought level, matching the CLI default.
pub fn model_options<'a>(model: &'a str, effort: Option<&'a str>) -> Result<(&'a str, &'a str)> {
    let model = if model == "default" { "gemini-3.8-flash" } else { model };
    if !MODELS.contains(&model) {
        bail!("unknown agy model {model:?}; valid ids: {}", MODELS.join(", "));
    }
    let (id, default): (&'a str, &'a str) = match FAMILIES.iter()
        .find(|(base, _)| model == *base || model.starts_with(&format!("{base}-"))) {
        Some((base, level)) => (base, level),
        None => (model, "high"),
    };
    let suffix = model.strip_prefix(id).and_then(|r| r.strip_prefix('-'))
        .filter(|level| !level.is_empty()).unwrap_or(default);
    let thought = effort.filter(|e| *e != "default").unwrap_or(suffix);
    if !["high", "medium", "low"].contains(&thought) {
        bail!("agy thought_level must be high|medium|low");
    }
    Ok((id, thought))
}

/// Parse the tab-separated `agy -p /quota` output:
/// `bucket \t label \t <pct>% \t <RFC3339 reset>`. The CLI reports the share
/// REMAINING; Kurisu stores `used_percentage` (100 - remaining) so /usage can
/// render agy exactly like Claude and Codex. Lines that do not match are
/// ignored; None means the CLI printed nothing usable.
pub fn parse_quota(text: &str, now: i64) -> Option<Value> {
    let mut buckets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cols.len() < 3 || cols[0].is_empty() || cols[1].is_empty() { continue; }
        let Some(pct) = cols[2].strip_suffix('%').unwrap_or(cols[2]).trim().parse::<f64>().ok()
            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p)) else { continue };
        if !seen.insert((cols[0].to_string(), cols[1].to_string())) { continue; }
        let resets_at = cols.get(3)
            .and_then(|r| chrono::DateTime::parse_from_rfc3339(r).ok())
            .map(|d| d.timestamp());
        buckets.push(json!({"bucket": cols[0], "label": cols[1],
            "used_percentage": 100.0 - pct, "resets_at": resets_at}));
    }
    (!buckets.is_empty()).then(|| json!({"source": "agy_quota", "confidence": "official",
        "captured_at": now, "buckets": buckets}))
}

/// Best-effort bounded `agy -p /quota`. The CLI may be missing, logged out,
/// slow or broken: every one of those yields None so /usage still renders.
pub async fn quota_with(bin: &str, now: i64, timeout: Duration) -> Option<Value> {
    let fut = tokio::process::Command::new(bin)
        .args(["-p", "/quota"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, fut).await.ok()?.ok()?;
    if !out.status.success() { return None; }
    parse_quota(&String::from_utf8_lossy(&out.stdout), now)
}

/// The 20 s cap keeps /usage responsive; the call itself normally takes ~2 s.
pub async fn quota(bin: &str, now: i64) -> Option<Value> {
    quota_with(bin, now, Duration::from_secs(20)).await
}

pub async fn configure(driver: &AcpDriver, id: &SessionId, model: &str, effort: Option<&str>, mode: Option<&str>) -> Result<()> {
    let (model, thought) = model_options(model, effort)?;
    driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"model","value":model})).await?;
    driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"thought_level","value":thought})).await?;
    let mode = match mode.unwrap_or("default") {
        "default" | "auto" => "default",
        "acceptEdits" => "accept-edits",
        "plan" => "plan",
        other => bail!("agy permission mode {other:?} unsupported; use default|acceptEdits|plan"),
    };
    driver.rpc("session/set_mode", json!({"sessionId":id,"modeId":mode})).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{agent_text, Driver};
    use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};
    use tokio::sync::mpsc;

    async fn fixture(attempts: usize) -> AcpDriver {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/agy-acp.py");
        AcpDriver::spawn_for("agy", path.to_string_lossy().into(), vec![("AGY_FIXTURE_AUTH_ATTEMPTS".into(), attempts.to_string())]).await.unwrap()
    }

    #[test]
    fn models_map_suffix_and_explicit_effort() {
        assert_eq!(model_options("default", None).unwrap(), ("gemini-3.8-flash", "high"));
        assert_eq!(model_options("gemini-3.8-flash-low", None).unwrap(), ("gemini-3.8-flash", "low"));
        assert_eq!(model_options("gemini-3.8-flash-medium", Some("high")).unwrap(), ("gemini-3.8-flash", "high"));
        for id in MODELS { assert!(model_options(id, None).is_ok(), "{id} rejected"); }
        let error = model_options("bogus", None).unwrap_err().to_string();
        assert!(error.contains("valid ids:") && error.contains("claude-sonnet-4-6"));
        assert!(model_options("default", Some("max")).is_err());
    }

    #[test]
    fn invented_models_are_rejected_with_the_supported_ids() {
        // This account has no gemini-4-argon family; a stale id must fail in
        // Kurisu with an actionable message, never as a raw ACP -32602.
        assert!(!MODELS.iter().any(|id| id.contains("argon")));
        for id in ["gemini-4-argon", "gemini-4-argon-medium", "gemini-4-argon-high"] {
            let error = model_options(id, None).unwrap_err().to_string();
            assert!(error.contains("valid ids:"), "{error}");
            assert!(error.contains("gemini-3.8-flash-medium"), "{error}");
        }
    }

    const QUOTA_FIXTURE: &str = "Gemini Models\tWeekly Limit Remaining\t100%\t2026-11-16T10:52:36Z\n\
        Gemini Models\tFive Hour Limit Remaining\t87.5%\t2026-11-09T15:52:36Z\n\
        Claude and GPT models\tWeekly Limit Remaining\t100%\t2026-11-16T12:00:23Z\n\
        not a quota line\n";

    #[test]
    fn quota_tsv_parses_buckets_and_skips_junk() {
        let v = parse_quota(QUOTA_FIXTURE, 1_000).unwrap();
        let buckets = v["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[1]["bucket"], "Gemini Models");
        // The CLI prints remaining; Kurisu stores used.
        assert_eq!(buckets[1]["used_percentage"], 12.5);
        assert_eq!(buckets[2]["used_percentage"], 0.0);
        assert_eq!(buckets[1]["resets_at"], 1_794_239_556);
        assert!(parse_quota("no quotas here\n", 1_000).is_none());
    }

    #[tokio::test]
    async fn quota_runs_a_stub_command_and_fails_soft() {
        use std::os::unix::fs::PermissionsExt;
        let stub = |body: &str| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("agy");
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path.to_string_lossy().into_owned())
        };
        // printf keeps the tabs literal; no real agy, credentials or network.
        let (_d, good) = stub("#!/bin/sh\nprintf 'Gemini Models\\tWeekly Limit Remaining\\t100%%\\t2026-11-16T10:52:36Z\\n'");
        let v = quota_with(&good, 1_000, Duration::from_secs(5)).await.unwrap();
        assert_eq!(v["buckets"].as_array().unwrap().len(), 1);
        assert_eq!(v["buckets"][0]["used_percentage"], 0.0);
        let (_d, bad) = stub("#!/bin/sh\necho boom\nexit 3\n");
        assert!(quota_with(&bad, 1_000, Duration::from_secs(5)).await.is_none());
        let (_d, slow) = stub("#!/bin/sh\nsleep 5\n");
        let start = std::time::Instant::now();
        assert!(quota_with(&slow, 1_000, Duration::from_millis(200)).await.is_none());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(quota_with("/nonexistent/kurisu-agy", 1_000, Duration::from_secs(1)).await.is_none());
    }

    #[tokio::test]
    async fn warmup_retries_then_succeeds_and_keeps_stdin_open() {
        let driver = fixture(2).await;
        let response = driver.session(None, std::env::current_dir().unwrap(), json!([])).await.unwrap();
        assert_eq!(response["attempts"], 3);
        assert_eq!(response["sessionId"], "agy-conversation-123");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(driver.alive.load(Ordering::Acquire));
        let id = SessionId::new("agy-conversation-123");
        driver.configure(&id, "default", None, Some("acceptEdits")).await.unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let result = driver.prompt(&id, "ping", tx).await.unwrap();
        assert!(matches!(result.stop_reason, agent_client_protocol::v1::schema::StopReason::EndTurn));
        let mut updates = Vec::new();
        while let Some(update) = rx.recv().await { updates.push(update); }
        assert_eq!(agent_text(&updates), "PONG");
        assert_eq!(updates.len(), 4);
        assert_eq!(driver.usage.lock().await["agy-conversation-123"]["input_tokens"], 10);
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn permanent_auth_failure_is_bounded_and_actionable() {
        let driver = fixture(usize::MAX).await;
        let start = tokio::time::Instant::now();
        let error = driver.session_with_warmup("session/new", json!({"cwd":std::env::current_dir().unwrap(),"mcpServers":[]}), Duration::from_millis(600)).await.unwrap_err();
        assert!(error.to_string().contains("run `agy acp --login`"));
        assert!(start.elapsed() < Duration::from_secs(2));
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn resume_uses_persisted_conversation_id() {
        let driver = fixture(0).await;
        let id = driver.new_session(std::env::current_dir().unwrap()).await.unwrap();
        driver.shutdown().await;
        let resumed = fixture(0).await;
        resumed.load_session(&id, std::env::current_dir().unwrap()).await.unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        resumed.prompt(&id, "continue", tx).await.unwrap();
        assert!(rx.recv().await.is_some());
        resumed.shutdown().await;
    }

    #[tokio::test]
    async fn missing_binary_has_install_command() {
        let error = AcpDriver::spawn_for("agy", "/nonexistent/kurisu-agy".into(), vec![]).await.err().unwrap();
        assert!(error.to_string().contains("curl -fsSL https://antigravity.google/cli/install.sh | bash"));
    }

    #[tokio::test]
    async fn permissions_map_kurisu_option_to_agent_option() {
        use agent_client_protocol::v1::schema::{RequestPermissionOutcome, SelectedPermissionOutcome, PermissionOptionId};
        let driver = fixture(0).await;
        let id = driver.new_session(std::env::current_dir().unwrap()).await.unwrap();
        let (updates, _rx) = mpsc::channel(32);
        let (permissions, mut requests) = mpsc::channel(4);
        let prompt = driver.prompt_with_permissions(&id, "permission", updates, Some(permissions));
        let approve = async {
            let request = requests.recv().await.unwrap();
            let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(PermissionOptionId::new(claude_cli_acp::ALLOW_ONCE)));
            driver.respond_permission(&request.tool_call.tool_call_id.to_string(), &outcome).await.unwrap();
        };
        let (result, ()) = tokio::join!(prompt, approve);
        result.unwrap();
        driver.shutdown().await;
    }

    /// Safe live handshake: never reads credentials or consumes a prompt quota.
    #[tokio::test]
    async fn real_agy_acp_session_and_config() {
        let _env = crate::server::OPENCODE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if crate::hostcheck::which(&bin()).is_none() {
            eprintln!("SKIP: agy absent");
            return;
        }
        let driver = AcpDriver::spawn_for("agy", bin(), vec![]).await.unwrap();
        let session = driver.session(None, std::env::current_dir().unwrap(), json!([])).await;
        let value = match session {
            Ok(value) => value,
            Err(error) if error.to_string().starts_with("agy authentication required;") => {
                eprintln!("SKIP: agy not logged in: {error}");
                driver.shutdown().await;
                return;
            }
            Err(error) => panic!("agy ACP failed: {error:#}"),
        };
        let id = SessionId::new(value["sessionId"].as_str().unwrap().to_owned());
        driver.configure(&id, "default", None, Some("default")).await.unwrap();
        driver.close(&id).await.unwrap();
        driver.shutdown().await;
    }
}
