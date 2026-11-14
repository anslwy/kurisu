//! Locate a harness CLI even when PATH is narrow.
//!
//! Official installers put `opencode` in `~/.opencode/bin` and only add it to PATH
//! from shell rc files, so a daemon started by a deploy, nohup or systemd never
//! sees it. Resolution order, cheapest first:
//!
//! 1. `KURISU_<NAME>_BIN` (an explicit override is used as given);
//! 2. PATH — today's behaviour, no subprocess;
//! 3. well-known install dirs under HOME, plus the two usual system prefixes;
//! 4. once per process: `$SHELL -lc 'command -v <bin>'`.
//!
//! Candidates from 3 and 4 must actually run `--version` before they are used, so
//! a stale shim cannot win. Hits are cached for the life of the process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Harnesses Kurisu can drive; the deploy script asserts every one resolves.
pub const HARNESSES: [&str; 6] = ["claude", "codex", "opencode", "agy", "fx", "pi"];
/// Harnesses whose absence is not a deploy or doctor failure.
pub const OPTIONAL: [&str; 2] = ["fx", "pi"];

/// Every required harness resolved (optional ones may be missing).
pub fn missing_required(all: &[(&'static str, Option<PathBuf>)]) -> Vec<&'static str> {
    all.iter().filter(|(n, p)| p.is_none() && !OPTIONAL.contains(n)).map(|(n, _)| *n).collect()
}

/// The absolute path of `bin`, or `None` when no candidate exists and runs.
pub fn resolve(bin: &str) -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    // An explicit override is cheap to honour and may change under tests, so it
    // never goes through the cache.
    if let Some(configured) = override_env(bin) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        return locate(bin, Some(&configured), &path, home.as_deref());
    }
    if let Some(hit) = cache.lock().unwrap().get(bin) {
        return Some(hit.clone());
    }
    let found = if bin.contains('/') {
        executable(Path::new(bin)).then(|| PathBuf::from(bin))
    } else {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let path = std::env::var_os("PATH").unwrap_or_default();
        locate(bin, None, &path, home.as_deref()).or_else(|| login_shell(bin))
    };
    if let Some(p) = &found {
        cache.lock().unwrap().insert(bin.to_string(), p.clone());
    }
    found
}

/// Every harness with its resolved absolute path (for `ku harness` and doctor).
pub fn all() -> Vec<(&'static str, Option<PathBuf>)> {
    HARNESSES.iter().map(|h| (*h, resolve(h))).collect()
}

/// Pure lookup: override, PATH, well-known dirs. Never runs a login shell, so
/// tests can prove the fallback with an empty PATH and a fake HOME.
pub fn locate(
    bin: &str,
    configured: Option<&str>,
    path: &std::ffi::OsStr,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(want) = configured {
        let p = PathBuf::from(want);
        if want.contains('/') {
            return executable(&p).then_some(p);
        }
        if let Some(hit) = in_path(want, path) {
            return Some(hit);
        }
    }
    let name = configured.unwrap_or(bin);
    if let Some(hit) = in_path(name, path) {
        return Some(hit);
    }
    let home = home?;
    known_dirs(home)
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| executable(p) && runs(p))
}

/// `KURISU_OPENCODE_BIN`, `KURISU_CLAUDE_BIN`, `KURISU_AGY_BIN`, `KURISU_CODEX_BIN`, `KURISU_FX_BIN`.
fn override_env(bin: &str) -> Option<String> {
    std::env::var(format!("KURISU_{}_BIN", bin.to_ascii_uppercase()))
        .ok()
        .filter(|v| !v.is_empty())
}

fn in_path(bin: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|d| d.join(bin))
        .find(|p| executable(p))
}

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

/// Where official installers put things, most specific first. Node versions are
/// newest-first so an old nvm tree cannot shadow the current one.
fn known_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        ".opencode/bin",
        ".local/bin",
        ".pi/agent/bin",
        ".cargo/bin",
        ".bun/bin",
        ".npm-global/bin",
        ".deno/bin",
    ]
    .iter()
    .map(|d| home.join(d))
    .collect();
    if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut nodes: Vec<(Vec<u64>, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let version: Vec<u64> = name
                    .trim_start_matches('v')
                    .split('.')
                    .filter_map(|p| p.parse().ok())
                    .collect();
                let bin = e.path().join("bin");
                bin.is_dir().then_some((version, bin))
            })
            .collect();
        nodes.sort();
        nodes.reverse();
        dirs.extend(nodes.into_iter().map(|(_, bin)| bin));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// Does `p --version` exit 0? A stale shim that cannot start loses the race.
fn runs(p: &Path) -> bool {
    let Ok(mut child) = std::process::Command::new(p)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The login shell's view of PATH, asked once and cached (rc files included).
fn login_shell(bin: &str) -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(bin) {
        return hit.clone();
    }
    let found = probe_login_shell(bin);
    cache.lock().unwrap().insert(bin.to_string(), found.clone());
    found
}

fn probe_login_shell(bin: &str) -> Option<PathBuf> {
    if !bin
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    {
        return None;
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let mut child = std::process::Command::new(shell)
        .args(["-lc", &format!("command -v {bin}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                let path = PathBuf::from(out.lines().rev().find(|l| !l.trim().is_empty())?.trim());
                return (executable(&path) && runs(&path)).then_some(path);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A script that answers `--version`; `broken` writes one that never runs.
    fn cli(path: &Path, broken: bool) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = if broken {
            "#!/bin/sh\nexit 3\n"
        } else {
            "#!/bin/sh\necho 9.9.9\n"
        };
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn empty_path_finds_the_official_opencode_install() {
        let home = tempfile::tempdir().unwrap();
        let install = home.path().join(".opencode/bin/opencode");
        cli(&install, false);
        assert_eq!(
            locate(
                "opencode",
                None,
                std::ffi::OsStr::new(""),
                Some(home.path())
            ),
            Some(install)
        );
    }

    #[test]
    fn path_wins_over_the_fallback_dir() {
        let home = tempfile::tempdir().unwrap();
        let fallback = home.path().join(".opencode/bin/opencode");
        cli(&fallback, false);
        let tools = home.path().join("tools");
        let on_path = tools.join("opencode");
        cli(&on_path, false);
        assert_eq!(
            locate("opencode", None, tools.as_os_str(), Some(home.path())),
            Some(on_path)
        );
    }

    #[test]
    fn newest_nvm_node_bin_is_tried_in_order() {
        let home = tempfile::tempdir().unwrap();
        for (v, name) in [("v18.1.0", "old"), ("v22.4.0", "new")] {
            cli(
                &home
                    .path()
                    .join(".nvm/versions/node")
                    .join(v)
                    .join("bin")
                    .join(name),
                false,
            );
        }
        let dirs = known_dirs(home.path());
        let first_node = dirs
            .iter()
            .position(|d| d.ends_with("bin") && d.to_string_lossy().contains(".nvm"))
            .unwrap();
        assert!(dirs[first_node].to_string_lossy().contains("v22.4.0"));
    }

    #[test]
    fn a_candidate_that_cannot_run_loses_to_nothing() {
        let home = tempfile::tempdir().unwrap();
        cli(&home.path().join(".local/bin/opencode"), true);
        assert_eq!(
            locate(
                "opencode",
                None,
                std::ffi::OsStr::new("/nonexistent"),
                Some(home.path())
            ),
            None
        );
    }

    #[test]
    fn explicit_override_is_used_as_given_and_never_falls_back() {
        let home = tempfile::tempdir().unwrap();
        cli(&home.path().join(".local/bin/opencode"), false);
        assert_eq!(
            locate(
                "opencode",
                Some("/nonexistent-explicit"),
                std::ffi::OsStr::new(""),
                Some(home.path())
            ),
            None
        );
        let custom = home.path().join("custom/opencode");
        cli(&custom, false);
        assert_eq!(
            locate(
                "opencode",
                custom.to_str(),
                std::ffi::OsStr::new(""),
                Some(home.path())
            ),
            Some(custom)
        );
    }
}
