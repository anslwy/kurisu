//! Detect the user's own CLIs without touching credentials.
//!
//! Login state comes only from the CLIs' own status commands
//! (`claude auth status --json`, `codex login status`). Of their output we keep
//! only the login boolean and the method; account details are never printed or
//! stored.

use std::fmt;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

pub struct CliStatus {
    pub name: &'static str,
    pub path: Option<String>,
    pub version: Option<String>,
    pub logged_in: Option<bool>,
    pub method: Option<String>,
}

impl fmt::Display for CliStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            None => write!(f, "{:<7} not found on PATH", self.name),
            Some(p) => write!(
                f,
                "{:<7} {}  version {}  login: {}",
                self.name,
                p,
                self.version.as_deref().unwrap_or("?"),
                match (self.logged_in, &self.method) {
                    (Some(true), Some(m)) => format!("yes ({m})"),
                    (Some(true), None) => "yes".into(),
                    (Some(false), _) => "NO — run the CLI's login yourself".into(),
                    (None, _) => "unknown".into(),
                }
            ),
        }
    }
}

pub fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

async fn output(bin: &str, args: &[&str]) -> Option<String> {
    let fut = Command::new(bin).args(args).stdin(std::process::Stdio::null()).output();
    let out = tokio::time::timeout(Duration::from_secs(20), fut).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    Some(text)
}

/// Keep only `loggedIn` and `authMethod` from `claude auth status --json`.
pub fn parse_claude_auth(text: &str) -> (Option<bool>, Option<String>) {
    let start = text.find('{');
    let v: Option<Value> = start.and_then(|i| serde_json::from_str(&text[i..]).ok());
    match v {
        Some(v) => (
            v.get("loggedIn").and_then(Value::as_bool),
            v.get("authMethod").and_then(Value::as_str).map(String::from),
        ),
        None => (None, None),
    }
}

/// `codex login status` prints e.g. "Logged in using ChatGPT" / "Not logged in".
pub fn parse_codex_login(text: &str) -> (Option<bool>, Option<String>) {
    for line in text.lines().filter(|l| !l.contains("WARNING")) {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Logged in using ") {
            return (Some(true), Some(rest.trim().to_string()));
        }
        if l.starts_with("Logged in") {
            return (Some(true), None);
        }
        if l.starts_with("Not logged in") {
            return (Some(false), None);
        }
    }
    (None, None)
}

fn first_version(text: &str) -> Option<String> {
    text.lines().filter(|l| !l.contains("WARNING")).find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string())
}

pub async fn claude() -> CliStatus {
    let bin = std::env::var("KURISU_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let path = which(&bin).or_else(|| std::path::Path::new(&bin).is_file().then(|| bin.clone()));
    let Some(p) = path.clone() else { return CliStatus { name: "claude", path, version: None, logged_in: None, method: None } };
    let version = output(&p, &["--version"]).await.as_deref().and_then(first_version);
    let (logged_in, method) = output(&p, &["auth", "status", "--json"]).await.as_deref().map(parse_claude_auth).unwrap_or((None, None));
    CliStatus { name: "claude", path, version, logged_in, method }
}

pub async fn codex() -> CliStatus {
    let path = which("codex");
    let Some(p) = path.clone() else { return CliStatus { name: "codex", path, version: None, logged_in: None, method: None } };
    let version = output(&p, &["--version"]).await.as_deref().and_then(first_version);
    let (logged_in, method) = output(&p, &["login", "status"]).await.as_deref().map(parse_codex_login).unwrap_or((None, None));
    CliStatus { name: "codex", path, version, logged_in, method }
}

impl CliStatus {
    /// For the API: installed, path, version and (if the CLI reports it) logged_in and method.
    pub fn to_json(&self) -> Value {
        serde_json::json!({"name": self.name, "installed": self.path.is_some(), "path": self.path, "version": self.version,
                           "logged_in": self.logged_in, "login_method": self.method})
    }
}

/// The optional `stt` CLI: installed, and set up (`stt status` exits 0). No login.
pub async fn stt() -> Value {
    let Some(p) = which("stt") else {
        return serde_json::json!({"name": "stt", "installed": false, "path": null, "version": null, "set_up": false, "daemon": null});
    };
    let fut = Command::new(&p).arg("status").stdin(std::process::Stdio::null()).output();
    let out = tokio::time::timeout(Duration::from_secs(10), fut).await.ok().and_then(|r| r.ok());
    let set_up = out.as_ref().is_some_and(|o| o.status.success());
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    // "set up: model=small int8 ..." -> the model is the closest thing to a version.
    let model = text.lines().find_map(|l| l.strip_prefix("set up: ")).and_then(|r| r.split_whitespace().find_map(|w| w.strip_prefix("model=")))
        .map(|m| format!("model {m}"));
    let daemon = if text.contains("daemon: running") { "running" } else { "stopped" };
    serde_json::json!({"name": "stt", "installed": true, "path": p, "version": model, "set_up": set_up, "daemon": daemon})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_auth_keeps_only_login_and_method() {
        let t = r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "someone@example.com", "orgName": "x"}"#;
        assert_eq!(parse_claude_auth(t), (Some(true), Some("claude.ai".into())));
        assert_eq!(parse_claude_auth(r#"{"loggedIn": false}"#), (Some(false), None));
        assert_eq!(parse_claude_auth("garbage"), (None, None));
        let s = CliStatus { name: "claude", path: Some("/x/claude".into()), version: Some("2.1".into()), logged_in: Some(true), method: Some("claude.ai".into()) };
        assert!(!s.to_string().contains("example.com"));
    }

    #[test]
    fn codex_login_lines() {
        assert_eq!(parse_codex_login("WARNING: x\nLogged in using ChatGPT\n"), (Some(true), Some("ChatGPT".into())));
        assert_eq!(parse_codex_login("Not logged in"), (Some(false), None));
        assert_eq!(parse_codex_login(""), (None, None));
    }
}

/// OpenCode auth is provider-specific; ACP reports auth_required when needed.
pub async fn opencode() -> CliStatus {
    let path = which("opencode");
    let version = match &path { Some(p) => output(p, &["--version"]).await.as_deref().and_then(first_version), None=>None };
    CliStatus { name:"opencode", path, version, logged_in:None, method:None }
}
