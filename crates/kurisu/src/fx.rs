//! fx (a coding-agent CLI) over ACP. fx owns its logins and providers; Kurisu
//! picks the provider and model per session through `FX_PROVIDER`/`FX_MODEL` on
//! the session's own `fx acp` process, and the permission mode through ACP.
//!
//! Kurisu model ids for fx are `provider/model` (`codex/gpt-6-luna`,
//! `ollama/deepseek-v4.1-flash:cloud`), a bare model for fx's active provider,
//! or `default` for whatever fx itself has saved.
use crate::acp::AcpDriver;
use agent_client_protocol::v1::schema::SessionId;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

/// Providers fx ships with; others come from `providers` in ~/.fx/settings.json.
pub const BUILTIN_PROVIDERS: &[&str] = &["gateway", "codex", "grok"];

pub fn bin() -> String {
    crate::harness::resolve("fx")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| std::env::var("KURISU_FX_BIN").unwrap_or_else(|_| "fx".into()))
}

fn settings_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".fx/settings.json"))
}

fn settings() -> Value {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

/// Provider names fx will accept: the built-ins, the configured ones, and
/// `ollama` (Kurisu adds its entry on first use, see `ensure_ollama`).
fn known_providers(settings: &Value) -> Vec<String> {
    let mut out: Vec<String> = BUILTIN_PROVIDERS.iter().map(|s| s.to_string()).collect();
    out.extend(settings["providers"].as_object().into_iter().flat_map(|o| o.keys().cloned()));
    if !out.iter().any(|p| p == "ollama") {
        out.push("ollama".into());
    }
    out
}

/// `(provider, model)` for a Kurisu fx model id. A prefix that is not a known
/// provider stays part of the model (gateway ids look like `anthropic/x`).
pub fn split_model_with(model: &str, providers: &[String]) -> (Option<String>, Option<String>) {
    if model == "default" || model.is_empty() {
        return (None, None);
    }
    match model.split_once('/') {
        Some((p, m)) if !m.is_empty() && providers.iter().any(|x| x == p) => (Some(p.into()), Some(m.into())),
        _ => (None, Some(model.into())),
    }
}

pub fn split_model(model: &str) -> (Option<String>, Option<String>) {
    split_model_with(model, &known_providers(&settings()))
}

/// The `ollama` provider entry fx needs (an OpenAI-compatible endpoint, no key).
fn ollama_entry() -> Value {
    // OLLAMA_HOST is often a bind address (0.0.0.0); connect over loopback then.
    let host = std::env::var("OLLAMA_HOST").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "127.0.0.1:11434".into()).replace("0.0.0.0", "127.0.0.1");
    let host = if host.starts_with("http://") || host.starts_with("https://") { host } else { format!("http://{host}") };
    json!({"base_url": format!("{}/v1", host.trim_end_matches('/')), "protocol": "openai-chat-completions", "auth": {"type": "none"}})
}

/// Add `providers.ollama` to fx's settings when it is missing, keeping every
/// other key. Returns whether it wrote. An existing entry is never touched.
pub fn ensure_ollama_at(path: &std::path::Path) -> Result<bool> {
    let mut v: Value = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let obj = v.as_object_mut().with_context(|| format!("{} is not a JSON object", path.display()))?;
    let providers = obj.entry("providers").or_insert_with(|| json!({}));
    let providers = providers.as_object_mut().context("fx settings: providers is not an object")?;
    if providers.contains_key("ollama") {
        return Ok(false);
    }
    providers.insert("ollama".into(), ollama_entry());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.kurisu-tmp");
    std::fs::write(&tmp, serde_json::to_string(&v)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Extra environment for a session's `fx acp`: its provider and model.
pub fn session_env(model: &str) -> Result<Vec<(String, String)>> {
    let (provider, model) = split_model(model);
    let mut env = vec![];
    if let Some(p) = provider {
        if p == "ollama" {
            if let Some(path) = settings_path() {
                ensure_ollama_at(&path)?;
            }
        }
        env.push(("FX_PROVIDER".to_string(), p));
    }
    if let Some(m) = model {
        env.push(("FX_MODEL".to_string(), m));
    }
    Ok(env)
}

/// Kurisu's permission modes as fx session modes. fx has no plan mode.
pub fn mode_id(mode: Option<&str>) -> Result<&'static str> {
    Ok(match mode.unwrap_or("default") {
        "default" | "acceptEdits" => "ask",
        "auto" => "auto",
        "bypassPermissions" => "full-access",
        other => bail!("fx permission mode {other:?} unsupported; use default|auto|acceptEdits|bypassPermissions"),
    })
}

/// Provider and model come from the process environment; only effort (when
/// fx advertises it for this model) and the permission mode go over ACP.
pub async fn configure(driver: &AcpDriver, id: &SessionId, effort: Option<&str>, mode: Option<&str>) -> Result<()> {
    let has_effort = driver.configs.lock().await.get(&id.to_string()).and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|c| c["id"] == "effort"));
    if let Some(e) = effort.filter(|e| *e != "default" && has_effort) {
        driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"effort","value":e})).await?;
    }
    let result = driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"mode","value":mode_id(mode)?})).await?;
    driver.configs.lock().await.insert(id.to_string(), result["configOptions"].clone());
    Ok(())
}

/// `fx models --json` ids for `provider` (fx's active one when None).
fn fetch_models(provider: Option<&str>) -> Result<Vec<String>> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(bin());
    cmd.args(["models", "--json"]).stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped());
    if let Some(p) = provider {
        cmd.env("FX_PROVIDER", p);
    }
    let mut child = cmd.spawn().context("run `fx models`")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`fx models` timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output()?;
    let v: Value = serde_json::from_slice(&out.stdout).context("parse `fx models --json`")?;
    Ok(v["ids"].as_array().into_iter().flatten().filter_map(|i| i.as_str().map(String::from)).collect())
}

/// Picker entries: fx's default, the active provider's models from `fx models`,
/// and each configured provider's saved model. Any `provider/model` is accepted.
pub fn catalogue() -> Vec<(String, String)> {
    let s = settings();
    let active = s["provider"].as_str().unwrap_or("gateway").to_string();
    let mut out = vec![("default".to_string(), format!("fx default ({active})"))];
    for id in fetch_models(None).unwrap_or_default() {
        out.push((format!("{active}/{id}"), format!("{id} ({active})")));
    }
    for (p, m) in s["models"].as_object().into_iter().flatten() {
        if let Some(m) = m.as_str() {
            let id = format!("{p}/{m}");
            if !out.iter().any(|(x, _)| *x == id) {
                out.push((id, format!("{m} ({p}, saved in fx)")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers() -> Vec<String> {
        known_providers(&json!({"providers": {"local": {}}}))
    }

    #[test]
    fn model_ids_split_only_on_known_providers() {
        let p = providers();
        assert_eq!(split_model_with("codex/gpt-6-luna", &p), (Some("codex".into()), Some("gpt-6-luna".into())));
        assert_eq!(split_model_with("ollama/deepseek-v4.1-flash:cloud", &p), (Some("ollama".into()), Some("deepseek-v4.1-flash:cloud".into())));
        assert_eq!(split_model_with("local/m", &p), (Some("local".into()), Some("m".into())));
        assert_eq!(split_model_with("gateway/anthropic/claude-sonnet-5.5", &p), (Some("gateway".into()), Some("anthropic/claude-sonnet-5.5".into())));
        assert_eq!(split_model_with("anthropic/claude-sonnet-5.5", &p), (None, Some("anthropic/claude-sonnet-5.5".into())));
        assert_eq!(split_model_with("gpt-6-luna", &p), (None, Some("gpt-6-luna".into())));
        assert_eq!(split_model_with("default", &p), (None, None));
    }

    #[test]
    fn modes_map_to_fx_session_modes() {
        assert_eq!(mode_id(None).unwrap(), "ask");
        assert_eq!(mode_id(Some("default")).unwrap(), "ask");
        assert_eq!(mode_id(Some("auto")).unwrap(), "auto");
        assert_eq!(mode_id(Some("bypassPermissions")).unwrap(), "full-access");
        assert!(mode_id(Some("plan")).is_err());
    }

    #[test]
    fn ollama_entry_is_added_once_and_keeps_other_settings() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join(".fx/settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"provider":"codex","models":{"codex":"gpt-6-luna"}}"#).unwrap();
        assert!(ensure_ollama_at(&path).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["provider"], "codex");
        assert_eq!(v["models"]["codex"], "gpt-6-luna");
        assert_eq!(v["providers"]["ollama"]["protocol"], "openai-chat-completions");
        assert_eq!(v["providers"]["ollama"]["auth"]["type"], "none");
        std::fs::write(&path, r#"{"providers":{"ollama":{"base_url":"http://h:1/v1"}}}"#).unwrap();
        assert!(!ensure_ollama_at(&path).unwrap());
        assert!(std::fs::read_to_string(&path).unwrap().contains("http://h:1/v1"));
        let fresh = d.path().join("none/settings.json");
        assert!(ensure_ollama_at(&fresh).unwrap());
    }
}
