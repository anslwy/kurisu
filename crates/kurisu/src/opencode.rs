//! Installed OpenCode catalogue and shared ACP driver.
pub use crate::acp::AcpDriver as OpenCodeDriver;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

/// The installed OpenCode binary (tests point this at a fixture).
pub fn find_bin() -> Option<PathBuf> {
    resolve_bin(
        std::env::var("KURISU_OPENCODE_BIN").ok().as_deref(),
        &std::env::var_os("PATH").unwrap_or_default(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
}
fn resolve_bin(
    configured: Option<&str>,
    path: &std::ffi::OsStr,
    home: Option<&Path>,
) -> Option<PathBuf> {
    fn executable(p: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    }
    let locate = |name: &str| -> Option<PathBuf> {
        if Path::new(name).is_absolute() {
            return executable(Path::new(name)).then(|| PathBuf::from(name));
        }
        std::env::split_paths(path)
            .map(|d| d.join(name))
            .find(|p| executable(p))
    };
    if let Some(bin) = configured {
        return locate(bin);
    }
    locate("opencode").or_else(|| {
        home.and_then(|home| {
            [
                ".opencode/bin/opencode",
                ".local/bin/opencode",
                ".bun/bin/opencode",
                ".npm-global/bin/opencode",
            ]
            .into_iter()
            .map(|p| home.join(p))
            .find(|p| executable(p))
        })
    })
}
pub fn bin() -> String {
    find_bin()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            std::env::var("KURISU_OPENCODE_BIN").unwrap_or_else(|_| "opencode".into())
        })
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
        let detail = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no error output")
            .trim();
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
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
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

#[cfg(test)]
mod discovery_tests {
    use super::*;
    fn fixture(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[test]
    fn fallback_finds_user_install_without_mutating_home_or_path() {
        let d = tempfile::tempdir().unwrap();
        let install = d.path().join(".opencode/bin/opencode");
        fixture(&install);
        assert_eq!(
            resolve_bin(None, std::ffi::OsStr::new("/nonexistent"), Some(d.path())),
            Some(install)
        );
    }
    #[test]
    fn explicit_override_and_path_precede_fallback() {
        let d = tempfile::tempdir().unwrap();
        let fallback = d.path().join(".opencode/bin/opencode");
        fixture(&fallback);
        let bin = d.path().join("tools/opencode");
        fixture(&bin);
        let custom = d.path().join("custom");
        fixture(&custom);
        let path = bin.parent().unwrap().to_path_buf();
        assert_eq!(
            resolve_bin(None, path.as_os_str(), Some(d.path())),
            Some(bin)
        );
        assert_eq!(
            resolve_bin(custom.to_str(), path.as_os_str(), Some(d.path())),
            Some(custom)
        );
        assert!(
            resolve_bin(Some("/nonexistent-explicit-opencode"), path.as_os_str(), Some(d.path()))
                .is_none()
        );
        std::fs::set_permissions(&fallback, {
            use std::os::unix::fs::PermissionsExt;
            std::fs::Permissions::from_mode(0o600)
        })
        .unwrap();
        assert!(resolve_bin(None, std::ffi::OsStr::new("/nonexistent"), Some(d.path())).is_none());
    }
}
