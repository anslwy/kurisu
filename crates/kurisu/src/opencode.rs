//! Installed OpenCode catalogue and shared ACP driver.
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::{path::{Path, PathBuf}, process::Stdio, time::Duration};
use tokio::process::Command;
pub use crate::acp::AcpDriver as OpenCodeDriver;

/// The installed OpenCode binary (tests point this at a fixture).
pub fn bin() -> String {
    std::env::var("KURISU_OPENCODE_BIN").unwrap_or_else(|_| "opencode".into())
}

/// The model ids the installed CLI actually offers (`opencode models`, one per
/// line). Providers and models added through `opencode.json` or `opencode auth`
/// are read by OpenCode itself, so they need no Kurisu-side list.
pub async fn list_models() -> Result<Vec<String>> {
    let bin = bin();
    let timeout = std::env::var("KURISU_OPENCODE_MODELS_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let out = tokio::time::timeout(
        Duration::from_secs(timeout),
        Command::new(&bin)
            .arg("models")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("`{bin} models` timed out after {timeout}s"))?
    .with_context(|| format!("run `{bin} models`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("no error output").trim();
        bail!("`{bin} models` failed ({}): {detail}", out.status);
    }
    let mut ids: Vec<String> = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let id = line.trim();
        // The CLI prints `provider/model` ids; skip blank lines and prose.
        if id.is_empty() || id.contains(char::is_whitespace) || !id.contains('/') {
            continue;
        }
        if !ids.iter().any(|x| x == id) {
            ids.push(id.to_string());
        }
    }
    if ids.is_empty() {
        bail!("`{bin} models` printed no model ids");
    }
    Ok(ids)
}

/// Where OpenCode keeps its user config (KURISU_OPENCODE_CONFIG overrides it).
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KURISU_OPENCODE_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("opencode").join("opencode.json"))
}

/// Models declared in `opencode.json`, plus its configured default. Used only
/// when `opencode models` cannot run: this is still real provider data, never
/// a guessed id.
pub fn config_models(path: &Path) -> Result<(Vec<String>, Option<String>)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let mut ids: Vec<String> = vec![];
    for (provider, cfg) in v["provider"].as_object().into_iter().flatten() {
        for model in cfg["models"].as_object().into_iter().flat_map(|m| m.keys()) {
            let id = format!("{provider}/{model}");
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let default = v["model"]
        .as_str()
        .filter(|m| ids.iter().any(|id| id == m))
        .map(String::from);
    Ok((ids, default))
}
