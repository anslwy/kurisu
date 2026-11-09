//! Antigravity owns authentication, conversations and its model catalogue;
//! Kurisu speaks only ACP and asks the CLI which models it really serves.
use agent_client_protocol::v1::schema::SessionId;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use crate::acp::AcpDriver;

/// The ids `agy models` reported on 1.5.3 (13 ids), used only when both the
/// CLI and the cache are unavailable. Never add an id the CLI did not report:
/// each entry here is evidence, not a guess.
pub const FALLBACK_MODELS: &[(&str, &str)] = &[
    ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
    ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
    ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
    ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
    ("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
    ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
    ("claude-opus-5-5-low", "Claude Opus 5.5 (Low)"),
    ("claude-opus-5-5-medium", "Claude Opus 5.5 (Medium)"),
    ("claude-opus-5-5-high", "Claude Opus 5.5 (High)"),
    ("claude-sonnet-5-5-low", "Claude Sonnet 5.5 (Low)"),
    ("claude-sonnet-5-5-medium", "Claude Sonnet 5.5 (Medium)"),
    ("claude-sonnet-5-5-high", "Claude Sonnet 5.5 (High)"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

/// The bare family id used when Kurisu says "the CLI's default".
pub const DEFAULT_MODEL_ID: &str = "gemini-3.8-flash";

/// A catalogue is only reused while the CLI login it came from is unchanged;
/// this TTL caps staleness when the token file itself does not move.
pub const MODEL_CACHE_TTL_SECS: i64 = 300;

/// `agy models` takes ~1.7 s; 5 s keeps a hung CLI from stalling the picker.
pub const MODEL_FETCH_TIMEOUT_SECS: u64 = 5;

pub fn bin() -> String {
    std::env::var("KURISU_AGY_BIN").unwrap_or_else(|_| "agy".into())
}

/// Parse `agy models` output: `id \t Display Name` lines after a progress
/// line. Anything without a tab, with a blank column or a space in the id is
/// ignored, so junk and the "Fetching available models..." header never
/// become a model id.
pub fn parse_models(text: &str) -> Vec<(String, String)> {
    let mut models: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let Some((id, label)) = line.split_once('\t') else { continue };
        let (id, label) = (id.trim(), label.trim());
        if id.is_empty() || label.is_empty() || id.chars().any(char::is_whitespace) { continue; }
        if seen.insert(id.to_string()) { models.push((id.to_string(), label.to_string())); }
    }
    models
}

/// Bounded synchronous `agy models`. Errors are expected (missing binary,
/// logged out, timeout, empty output) and are handled by the caller.
pub fn fetch_models(bin: &str, timeout: Duration) -> Result<Vec<(String, String)>> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(bin)
        .arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("could not run `{bin} models`"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? { break status; }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{bin} models` timed out after {}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader.join().unwrap_or_default();
    if !status.success() { bail!("`{bin} models` exited with {status}"); }
    let models = parse_models(&String::from_utf8_lossy(&bytes));
    if models.is_empty() { bail!("`{bin} models` returned no usable model ids"); }
    Ok(models)
}

/// A catalogue fetched from `agy models`, with the login it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCache {
    pub fetched_at: i64,
    pub token: String,
    pub models: Vec<(String, String)>,
}

impl ModelCache {
    /// Reusable while the login fingerprint matches and the TTL has not run
    /// out. A clock jump backwards also forces a refresh.
    pub fn fresh(&self, token: &str, now: i64) -> bool {
        !self.models.is_empty() && self.token == token
            && (0..MODEL_CACHE_TTL_SECS).contains(&(now - self.fetched_at))
    }
}

/// The agy login file; a re-login rewrites it, which must drop the cache at
/// once rather than waiting out the TTL.
fn token_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".gemini/antigravity-cli/antigravity-oauth-token")
}

/// `mtime.size` of the login file, or `absent` when it cannot be read. Only
/// equality matters, so two different tokens never share a fingerprint.
pub fn token_fingerprint_at(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => format!("{}.{}", m.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()).unwrap_or(0), m.len()),
        Err(_) => "absent".into(),
    }
}

pub fn token_fingerprint() -> String {
    token_fingerprint_at(&token_path())
}

fn cache_path() -> PathBuf {
    crate::paths::Paths::from_env().home.join("cache/agy-models.json")
}

fn read_cache_from(path: &Path) -> Option<ModelCache> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_cache_to(path: &Path, cache: &ModelCache) {
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    if let Ok(text) = serde_json::to_string(cache) { let _ = std::fs::write(path, text); }
}

pub fn fallback_models() -> Vec<(String, String)> {
    FALLBACK_MODELS.iter().map(|(id, label)| (id.to_string(), label.to_string())).collect()
}

/// Pick the catalogue to use and the cache to persist, if any. Pure so the
/// precedence is testable: fresh cache > live CLI > last good cache > static.
pub fn choose_catalogue(cached: Option<ModelCache>, token: &str, now: i64,
                        fetched: Result<Vec<(String, String)>>) -> (Vec<(String, String)>, Option<ModelCache>) {
    if let Some(c) = &cached {
        if c.fresh(token, now) { return (c.models.clone(), None); }
    }
    match fetched {
        Ok(models) if !models.is_empty() => (models.clone(), Some(ModelCache { fetched_at: now, token: token.to_string(), models })),
        _ => match cached.filter(|c| !c.models.is_empty()) {
            Some(c) => (c.models, None),
            None => (fallback_models(), None),
        },
    }
}

/// The agy model catalogue, straight from the CLI. Cached for 5 minutes and
/// keyed on the login file; a failed fetch falls back to the last good cache,
/// then to the ids 1.5.3 reported. Blocking by design: callers are request
/// handlers and store validation, and the fetch is bounded.
pub fn catalogue() -> Vec<(String, String)> {
    let now = chrono::Utc::now().timestamp();
    let token = token_fingerprint();
    let path = cache_path();
    let cached = read_cache_from(&path);
    if let Some(c) = &cached {
        if c.fresh(&token, now) { return c.models.clone(); }
    }
    let fetched = fetch_models(&bin(), Duration::from_secs(MODEL_FETCH_TIMEOUT_SECS));
    let (models, write) = choose_catalogue(cached, &token, now, fetched);
    if let Some(c) = write { write_cache_to(&path, &c); }
    models
}

/// Families whose bare id the CLI accepts, with the level it uses when no
/// level is sent.
const FAMILIES: &[(&str, &str)] = &[("gemini-3.8-flash", "high")];

/// Suffixes in CLI model names map to ACP's separate thought_level option,
/// but only for known families (gemini-3.8-flash): other ids are sent
/// verbatim, as before. A suffixed id is only accepted when `agy models`
/// listed it; a bare family id is accepted when one of its suffixed ids is
/// listed (agy resolves it to the family).
pub fn model_options(model: &str, effort: Option<&str>) -> Result<(String, String)> {
    model_options_with(model, effort, &catalogue())
}

pub fn model_options_with(model: &str, effort: Option<&str>, catalogue: &[(String, String)]) -> Result<(String, String)> {
    let model = if model == "default" { DEFAULT_MODEL_ID } else { model };
    let listed = catalogue.iter().any(|(id, _)| id == model);
    // A bare family id is acceptable when agy lists its suffixed members.
    let listed_base = catalogue.iter().any(|(id, _)| id.strip_prefix(model)
        .is_some_and(|rest| matches!(rest, "-high" | "-medium" | "-low")));
    if !listed && !listed_base && model != DEFAULT_MODEL_ID {
        let ids: Vec<&str> = catalogue.iter().map(|(id, _)| id.as_str()).collect();
        bail!("unknown agy model {model:?}; supported ids: {}", ids.join(", "));
    }
    // `gemini-3.8-flash-low` -> id `gemini-3.8-flash`, thought `low`; the bare
    // family id -> the CLI's own default level. Everything else is sent
    // verbatim with the default level, exactly as before.
    let family = FAMILIES.iter().find(|(base, _)| {
        model == *base || (listed && model.strip_prefix(base).is_some_and(|r| r.starts_with('-')))
    });
    let (id, suffix) = match family {
        Some(&(base, default)) => (base, model.strip_prefix(base).and_then(|r| r.strip_prefix('-')).unwrap_or(default)),
        None => (model, "high"),
    };
    let thought = effort.filter(|e| *e != "default").unwrap_or(suffix);
    if !["high", "medium", "low"].contains(&thought) {
        bail!("agy thought_level must be high|medium|low");
    }
    Ok((id.to_string(), thought.to_string()))
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

/// The argv for `agy` in ACP mode. Kurisu runs it headless, and headless agy
/// cannot prompt a human: it auto-denies any tool that needs a prompt and the
/// turn ends with no text. `auto` is the one mode whose contract is "routine
/// actions run", so it opts into agy's own bypass; `default` and `plan` must
/// keep asking - and keep failing as before - rather than silently widening.
pub fn acp_args(mode: Option<&str>) -> Vec<&'static str> {
    let mut args = vec!["acp", "--disable-telemetry"];
    if mode == Some("auto") {
        args.push("--dangerously-skip-permissions");
    }
    args
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

    /// Exactly what `agy models` printed on this account after the re-login
    /// (13 ids, argon first), plus the CLI's progress line.
    const MODELS_FIXTURE: &str = "Fetching available models...\n\
gemini-4-argon-medium\tGemini 4 Argon (Medium)\n\
gemini-4-argon-high\tGemini 4 Argon (High)\n\
gemini-4-argon-low\tGemini 4 Argon (Low)\n\
gemini-3.8-flash-high\tGemini 3.8 Flash (High)\n\
gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)\n\
gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
claude-opus-5-5-low\tClaude Opus 5.5 (Low)\n\
claude-opus-5-5-medium\tClaude Opus 5.5 (Medium)\n\
claude-opus-5-5-high\tClaude Opus 5.5 (High)\n\
claude-sonnet-5-5-low\tClaude Sonnet 5.5 (Low)\n\
claude-sonnet-5-5-medium\tClaude Sonnet 5.5 (Medium)\n\
claude-sonnet-5-5-high\tClaude Sonnet 5.5 (High)\n\
gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n";

    fn fixture_catalogue() -> Vec<(String, String)> { parse_models(MODELS_FIXTURE) }

    #[test]
    fn parses_the_clis_real_model_list_and_skips_junk() {
        let models = fixture_catalogue();
        let ids: Vec<&str> = models.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids.len(), 13, "{ids:?}");
        assert_eq!(ids[0], "gemini-4-argon-medium");
        assert_eq!(models[0].1, "Gemini 4 Argon (Medium)");
        assert!(ids.contains(&"gpt-oss-120b-medium") && ids.contains(&"claude-opus-5-5-high"));
        // The old hardcoded ids are gone from this account and must not reappear.
        assert!(!ids.contains(&"claude-sonnet-4-6") && !ids.contains(&"claude-opus-4-6-thinking"));
        // Junk never becomes an id.
        assert!(parse_models("Fetching available models...\nnot a model\nnope\t\n\tLabel\n").is_empty());
        let dupes = parse_models("a-1\tA\na-1\tA again\n");
        assert_eq!(dupes.len(), 1);
    }

    #[test]
    fn catalogue_prefers_a_fresh_cache_then_the_cli_then_the_last_good_list() {
        let live = fixture_catalogue();
        let cached = ModelCache { fetched_at: 1_000, token: "t1".into(), models: live.clone() };
        // Fresh cache: no fetch is needed, nothing to rewrite.
        let (models, write) = choose_catalogue(Some(cached.clone()), "t1", 1_100, Err(anyhow::anyhow!("no cli")));
        assert_eq!(models, live);
        assert!(write.is_none());
        // Stale cache: a good fetch wins and is persisted.
        let (models, write) = choose_catalogue(Some(cached.clone()), "t1", 1_000 + MODEL_CACHE_TTL_SECS + 1, Ok(live.clone()));
        assert_eq!(models, live);
        assert_eq!(write.unwrap().fetched_at, 1_000 + MODEL_CACHE_TTL_SECS + 1);
        // A failed fetch keeps the last good list, even when stale.
        let (models, write) = choose_catalogue(Some(cached.clone()), "t1", 99_999, Err(anyhow::anyhow!("timeout")));
        assert_eq!(models, live);
        assert!(write.is_none());
        // Nothing cached and no CLI: the static evidence list, never nothing.
        let (models, write) = choose_catalogue(None, "t1", 1_000, Err(anyhow::anyhow!("missing")));
        assert_eq!(models.len(), 13);
        assert!(write.is_none());
        // Empty CLI output must not wipe a good cache.
        let (models, _) = choose_catalogue(Some(cached), "t1", 99_999, Ok(vec![]));
        assert_eq!(models.len(), 13);
    }

    #[test]
    fn relogin_invalidates_the_cache_immediately() {
        let old = vec![("gemini-3.8-flash-high".to_string(), "Gemini 3.8 Flash (High)".to_string())];
        let cached = ModelCache { fetched_at: 1_000, token: "old-login.100".into(), models: old };
        // Same second, new login: the token fingerprint alone forces the refetch.
        let (models, write) = choose_catalogue(Some(cached.clone()), "new-login.120", 1_000, Ok(fixture_catalogue()));
        assert_eq!(models.len(), 13);
        assert!(models.iter().any(|(id, _)| id == "gemini-4-argon-high"));
        assert_eq!(write.unwrap().token, "new-login.120");
        // A clock jump backwards also refetches instead of trusting the cache.
        let (_, write) = choose_catalogue(Some(cached), "old-login.100", 10, Ok(fixture_catalogue()));
        assert!(write.is_some());
    }

    #[test]
    fn token_fingerprint_changes_when_the_login_file_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("antigravity-oauth-token");
        assert_eq!(token_fingerprint_at(&path), "absent");
        std::fs::write(&path, "token-one").unwrap();
        let first = token_fingerprint_at(&path);
        std::fs::write(&path, "token-two-longer").unwrap();
        assert_ne!(first, token_fingerprint_at(&path));
        assert!(!first.is_empty() && first != "absent");
    }

    #[test]
    fn models_map_suffix_and_explicit_effort() {
        let cat = fixture_catalogue();
        let opts = |m: &str, e: Option<&str>| model_options_with(m, e, &cat).unwrap();
        assert_eq!(opts("default", None), ("gemini-3.8-flash".to_string(), "high".to_string()));
        assert_eq!(opts("gemini-3.8-flash-low", None), ("gemini-3.8-flash".to_string(), "low".to_string()));
        assert_eq!(opts("gemini-3.8-flash-medium", Some("high")), ("gemini-3.8-flash".to_string(), "high".to_string()));
        // Argon now exists; it is listed and its bare family id resolves too.
        assert_eq!(opts("gemini-4-argon-high", None), ("gemini-4-argon-high".to_string(), "high".to_string()));
        assert_eq!(opts("gemini-4-argon", None), ("gemini-4-argon".to_string(), "high".to_string()));
        assert_eq!(opts("gemini-4-argon", Some("low")), ("gemini-4-argon".to_string(), "low".to_string()));
        assert_eq!(opts("claude-opus-5-5-medium", None), ("claude-opus-5-5-medium".to_string(), "high".to_string()));
        assert_eq!(opts("gpt-oss-120b-medium", None), ("gpt-oss-120b-medium".to_string(), "high".to_string()));
        for (id, _) in &cat { assert!(model_options_with(id, None, &cat).is_ok(), "{id} rejected"); }
        assert!(model_options_with("default", Some("max"), &cat).is_err());
    }

    #[test]
    fn unknown_models_are_rejected_with_the_supported_ids() {
        let cat = fixture_catalogue();
        // Ids this account no longer serves must fail in Kurisu with an
        // actionable message, never as a raw ACP -32602.
        for id in ["claude-sonnet-4-6", "claude-opus-4-6-thinking", "gemini-4-argon-ultra", "bogus"] {
            let error = model_options_with(id, None, &cat).unwrap_err().to_string();
            assert!(error.contains("supported ids:"), "{error}");
            assert!(error.contains("gemini-4-argon-medium"), "{error}");
            assert!(error.contains("gemini-3.8-flash-medium"), "{error}");
        }
    }

    #[test]
    fn fetch_models_parses_the_cli_and_bounds_a_hung_one() {
        use std::os::unix::fs::PermissionsExt;
        let stub = |body: &str| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("agy");
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path.to_string_lossy().into_owned())
        };
        let body = format!("#!/bin/sh\nprintf '{}'\n", MODELS_FIXTURE.replace('\t', "\\t").replace('\n', "\\n"));
        let (_d, good) = stub(&body);
        assert_eq!(fetch_models(&good, Duration::from_secs(5)).unwrap().len(), 13);
        let (_d, empty) = stub("#!/bin/sh\necho 'Fetching available models...'\n");
        assert!(fetch_models(&empty, Duration::from_secs(5)).is_err());
        let (_d, broken) = stub("#!/bin/sh\nexit 3\n");
        assert!(fetch_models(&broken, Duration::from_secs(5)).is_err());
        let (_d, slow) = stub("#!/bin/sh\nsleep 5\n");
        let start = std::time::Instant::now();
        assert!(fetch_models(&slow, Duration::from_millis(200)).is_err());
        assert!(start.elapsed() < Duration::from_secs(2), "took {:?}", start.elapsed());
        assert!(fetch_models("/nonexistent/kurisu-agy", Duration::from_secs(1)).is_err());
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

    #[test]
    fn auto_mode_gets_headless_skip_permissions_and_others_do_not() {
        let auto = acp_args(Some("auto"));
        assert_eq!(auto[..2], ["acp", "--disable-telemetry"]);
        assert!(auto.contains(&"--dangerously-skip-permissions"), "{auto:?}");
        for mode in [None, Some("default"), Some("plan"), Some("acceptEdits")] {
            let args = acp_args(mode);
            assert!(!args.contains(&"--dangerously-skip-permissions"),
                "{mode:?} must keep asking for permission, not run wide open: {args:?}");
        }
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
