//! claude-cli-acp: ACP-shaped driver for the user's own interactive `claude` CLI.
//!
//! The real, unmodified `claude` binary runs inside a private tmux server, under
//! the user's own login. No Agent SDK and no `-p` headless mode are used, and no
//! credential is ever read. The approach mirrors Omnigent's `claude_native`
//! harness.
//!
//! * **start**: `claude --session-id <uuid> --settings <hooks.json>` in a tmux
//!   session whose working directory is the ACP `cwd`;
//! * **ready / done**: Claude Code hooks (`SessionStart`, `Stop`, `StopFailure`,
//!   `PostToolUse`, `Notification`) run `<hook_cmd> <events.jsonl>`, which
//!   appends the hook's stdin JSON to a per-session event file;
//! * **send**: a bracketed paste through `tmux load-buffer` + `paste-buffer -p`,
//!   then Enter;
//! * **reply**: `last_assistant_message` from the `Stop` hook. The transcript
//!   path from the hook is kept for history and as a fallback;
//! * **resume**: `claude --resume <uuid>` with the same hooks;
//! * **permissions**: a `PermissionRequest` hook (fired where the TUI would
//!   show its approval dialog) appends the request to the event file and
//!   waits for a decision file. The driver surfaces it as an ACP
//!   `session/request_permission` and writes the client's answer back with
//!   [`ClaudeCli::respond_permission`]. Unanswered requests are denied when
//!   they expire, so a turn never hangs on a hidden dialog.
//!
//! Hook events are translated to ACP `session/update`s (agent message chunks,
//! tool calls) and the turn ends with an ACP `StopReason`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, LoadSessionRequest, Meta, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallUpdate, ToolCallUpdateFields,
};
use anyhow::{anyhow, bail, Context, Result};

pub mod usage;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};

/// Static configuration for the driver.
#[derive(Clone, Debug)]
pub struct Config {
    /// The user's installed `claude` binary.
    pub claude_bin: String,
    /// Private tmux socket name (`tmux -L <name>`), so the user's own tmux is untouched.
    pub tmux_socket: String,
    /// Per-session state dir root (events.jsonl, hooks.json).
    pub state_dir: PathBuf,
    /// argv prefix for the hook command; the events file path is appended.
    pub hook_cmd: Vec<String>,
    /// Extra args, e.g. `--permission-mode`, `--model`, `--effort`.
    pub extra_args: Vec<String>,
    pub ready_timeout: Duration,
    pub turn_timeout: Duration,
    /// How long a permission request waits for an answer before it is denied.
    pub permission_timeout: Duration,
}

impl Config {
    pub fn new(state_dir: impl Into<PathBuf>, hook_cmd: Vec<String>) -> Self {
        Self {
            claude_bin: "claude".into(),
            tmux_socket: "kurisu-claude".into(),
            state_dir: state_dir.into(),
            hook_cmd,
            extra_args: vec![],
            ready_timeout: Duration::from_secs(60),
            turn_timeout: Duration::from_secs(30 * 60),
            permission_timeout: Duration::from_secs(10 * 60),
        }
    }
}

struct Live {
    cwd: PathBuf,
    events: PathBuf,
}

/// Per-session launch options (on top of `Config::extra_args`), e.g.
/// `--model`, `--mcp-config`, `--append-system-prompt-file`.
#[derive(Clone, Debug, Default)]
pub struct SessionOpts {
    pub args: Vec<String>,
}

pub struct ClaudeCli {
    cfg: Config,
    live: Mutex<HashMap<String, Live>>,
}

/// One appended hook event.
#[derive(Debug, Clone)]
pub struct HookEvent {
    pub name: String,
    pub raw: Value,
}

pub fn parse_events(text: &str) -> Vec<HookEvent> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|raw| HookEvent {
            name: raw.get("hook_event_name").and_then(Value::as_str).unwrap_or("").to_string(),
            raw,
        })
        .collect()
}

/// Translate one hook event into an ACP update (if it has one).
pub fn to_update(ev: &HookEvent) -> Option<SessionUpdate> {
    match ev.name.as_str() {
        "PostToolUse" => {
            let id = ev.raw.get("tool_use_id").and_then(Value::as_str).unwrap_or("tool").to_string();
            let name = ev.raw.get("tool_name").and_then(Value::as_str).unwrap_or("tool").to_string();
            Some(SessionUpdate::ToolCall(ToolCall::new(id, name)))
        }
        "Stop" => ev
            .raw
            .get("last_assistant_message")
            .and_then(Value::as_str)
            .map(|t| SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(t))))),
        _ => None,
    }
}

/// The hooks settings JSON passed with `--settings` (no other settings are changed).
/// `PermissionRequest` runs `<hook_cmd> <events> --permission <secs>`, which
/// blocks until a decision arrives (or denies after `secs`).
pub fn hooks_settings(hook_cmd: &[String], events: &Path, permission_secs: u64) -> Value {
    let mut argv: Vec<String> = hook_cmd.iter().map(|a| shell_quote(a)).collect();
    argv.push(shell_quote(&events.to_string_lossy()));
    let cmd = argv.join(" ");
    let entry = serde_json::json!([{ "hooks": [{ "type": "command", "command": cmd }] }]);
    let mut hooks = serde_json::Map::new();
    for name in ["SessionStart", "UserPromptSubmit", "Stop", "StopFailure", "PostToolUse", "Notification", "PreCompact"] {
        hooks.insert(name.into(), entry.clone());
    }
    let perm = format!("{cmd} --permission {permission_secs}");
    hooks.insert(
        "PermissionRequest".into(),
        serde_json::json!([{ "hooks": [{ "type": "command", "command": perm, "timeout": permission_secs + 30 }] }]),
    );
    serde_json::json!({ "hooks": hooks })
}

/// Option ids offered with every permission request.
pub const ALLOW_ONCE: &str = "allow_once";
pub const REJECT_ONCE: &str = "reject_once";

/// Build the ACP permission request for one `PermissionRequest` hook event.
/// The tool call id is the hook's request id (the key for `respond_permission`).
pub fn permission_request(session_id: &SessionId, ev: &HookEvent) -> Option<RequestPermissionRequest> {
    let rid = ev.raw.get("kurisu_request_id").and_then(Value::as_str)?;
    let tool = ev.raw.get("tool_name").and_then(Value::as_str).unwrap_or("tool");
    let input = ev.raw.get("tool_input").cloned().unwrap_or(Value::Null);
    let title = permission_title(tool, &input);
    let fields = ToolCallUpdateFields::new().title(title).name(tool.to_string()).raw_input(input);
    let mut meta = Meta::new();
    for k in ["cwd", "permission_suggestions", "expires_at"] {
        if let Some(v) = ev.raw.get(k) {
            meta.insert(k.into(), v.clone());
        }
    }
    Some(
        RequestPermissionRequest::new(
            session_id.clone(),
            ToolCallUpdate::new(rid.to_string(), fields),
            vec![
                PermissionOption::new(ALLOW_ONCE, "Allow", PermissionOptionKind::AllowOnce),
                PermissionOption::new(REJECT_ONCE, "Deny", PermissionOptionKind::RejectOnce),
            ],
        )
        .meta(meta),
    )
}

/// One-line summary of a tool call, e.g. `Bash: ls -la` or `Write: /x/y.txt`.
pub fn permission_title(tool: &str, input: &Value) -> String {
    let detail = ["command", "file_path", "path", "url", "pattern", "description"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .unwrap_or("");
    let detail: String = detail.chars().take(200).collect();
    if detail.is_empty() { tool.to_string() } else { format!("{tool}: {detail}") }
}

/// Heuristic for a modal dialog in the claude TUI: its key hints.
pub fn looks_like_dialog(screen: &str) -> bool {
    let tail: String = screen.lines().rev().filter(|l| !l.trim().is_empty()).take(6).collect::<Vec<_>>().join("\n");
    ["Esc to cancel", "Enter to confirm", "Enter to continue", "Enter to select", "to navigate"]
        .iter()
        .any(|k| tail.contains(k))
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-=:".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Text of the prompt (ACP content blocks -> plain text for the TUI).
pub fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl ClaudeCli {
    pub fn new(cfg: Config) -> Self {
        Self { cfg, live: Mutex::new(HashMap::new()) }
    }

    fn tmux(&self) -> Command {
        let mut c = Command::new("tmux");
        c.arg("-L").arg(&self.cfg.tmux_socket);
        c
    }

    fn tmux_name(id: &str) -> String {
        format!("claude-{}", &id[..8.min(id.len())])
    }

    async fn run(mut c: Command) -> Result<String> {
        let out = c.output().await?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn launch(&self, id: &str, cwd: &Path, resume: bool, opts: &SessionOpts) -> Result<()> {
        let dir = self.cfg.state_dir.join(id);
        tokio::fs::create_dir_all(&dir).await?;
        let events = dir.join("events.jsonl");
        let settings = dir.join("hooks.json");
        let hooks = hooks_settings(&self.cfg.hook_cmd, &events, self.cfg.permission_timeout.as_secs());
        tokio::fs::write(&settings, serde_json::to_vec(&hooks)?).await?;
        let offset = file_len(&events).await;
        let mut argv = vec![self.cfg.claude_bin.clone()];
        argv.push(if resume { "--resume".into() } else { "--session-id".into() });
        argv.push(id.to_string());
        argv.push("--settings".into());
        argv.push(settings.to_string_lossy().into_owned());
        argv.extend(self.cfg.extra_args.iter().cloned());
        argv.extend(opts.args.iter().cloned());
        let name = Self::tmux_name(id);
        let _ = Self::run({
            let mut c = self.tmux();
            c.args(["kill-session", "-t", &name]);
            c
        })
        .await;
        let mut c = self.tmux();
        c.args(["new-session", "-d", "-s", &name, "-x", "200", "-y", "50", "-c"]).arg(cwd);
        c.arg(argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
        Self::run(c).await.context("tmux new-session")?;
        if let Err(e) = self.wait_for(&events, offset, self.cfg.ready_timeout, |e| e.name == "SessionStart").await {
            let screen = self.capture(&name).await.unwrap_or_default();
            bail!("claude did not report SessionStart ({e}); screen:\n{}", screen.trim());
        }
        self.live.lock().await.insert(id.to_string(), Live { cwd: cwd.to_path_buf(), events });
        Ok(())
    }

    /// ACP `session/new`.
    pub async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse> {
        self.new_session_with(req, &SessionOpts::default()).await
    }

    pub async fn new_session_with(&self, req: NewSessionRequest, opts: &SessionOpts) -> Result<NewSessionResponse> {
        let id = uuid::Uuid::new_v4().to_string();
        self.launch(&id, &req.cwd, false, opts).await?;
        Ok(NewSessionResponse::new(SessionId::new(id)))
    }

    /// ACP `session/load` (here: resume the CLI's own transcript; no replay).
    pub async fn load_session(&self, req: LoadSessionRequest) -> Result<()> {
        self.load_session_with(req, &SessionOpts::default()).await
    }

    pub async fn load_session_with(&self, req: LoadSessionRequest, opts: &SessionOpts) -> Result<()> {
        let id = req.session_id.to_string();
        uuid::Uuid::parse_str(&id).map_err(|_| anyhow!("not a claude session id"))?;
        self.launch(&id, &req.cwd, true, opts).await
    }

    /// Stop the private tmux server (every CLI this driver started).
    pub async fn kill_all(&self) {
        self.live.lock().await.clear();
        let _ = Self::run({
            let mut c = self.tmux();
            c.arg("kill-server");
            c
        })
        .await;
    }

    /// Dismiss any modal dialog (setup prompts, menus) with Escape — never Enter,
    /// which could confirm something on the user's behalf. Returns true when the
    /// normal input box is showing.
    async fn clear_dialogs(&self, tmux_name: &str) -> Result<bool> {
        for _ in 0..4 {
            let screen = self.capture(tmux_name).await.unwrap_or_default();
            if !looks_like_dialog(&screen) {
                return Ok(true);
            }
            Self::run({
                let mut c = self.tmux();
                c.args(["send-keys", "-t", tmux_name, "Escape"]);
                c
            })
            .await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(false)
    }

    /// Is the CLI's tmux session alive?
    pub async fn is_running(&self, session_id: &SessionId) -> bool {
        let name = Self::tmux_name(&session_id.to_string());
        let mut c = self.tmux();
        c.args(["has-session", "-t", &name]);
        c.output().await.map(|o| o.status.success()).unwrap_or(false)
    }

    /// Visible screen of the session (diagnostics only).
    pub async fn capture(&self, tmux_name: &str) -> Result<String> {
        let mut c = self.tmux();
        c.args(["capture-pane", "-p", "-t", tmux_name]);
        Self::run(c).await
    }

    /// The transcript path reported by the most recent hook event.
    pub async fn transcript_path(&self, session_id: &SessionId) -> Option<PathBuf> {
        let events = self.cfg.state_dir.join(session_id.to_string()).join("events.jsonl");
        let (evs, _) = read_from(&events, 0).await;
        evs.iter()
            .rev()
            .find_map(|e| e.raw.get("transcript_path").and_then(Value::as_str).map(PathBuf::from))
    }

    /// ACP `session/prompt`: paste, then stream updates until `Stop`/`StopFailure`.
    /// Permission requests are denied (no approver); see `prompt_with_permissions`.
    pub async fn prompt(&self, req: PromptRequest, updates: mpsc::Sender<SessionNotification>) -> Result<PromptResponse> {
        self.prompt_with_permissions(req, updates, None).await
    }

    /// Like `prompt`, but permission requests go to `perms` as ACP
    /// `RequestPermissionRequest`s; answer each with `respond_permission`.
    pub async fn prompt_with_permissions(
        &self,
        req: PromptRequest,
        updates: mpsc::Sender<SessionNotification>,
        perms: Option<mpsc::Sender<RequestPermissionRequest>>,
    ) -> Result<PromptResponse> {
        let id = req.session_id.to_string();
        let events = {
            let live = self.live.lock().await;
            live.get(&id).ok_or_else(|| anyhow!("session {id} is not running"))?.events.clone()
        };
        let offset = file_len(&events).await;
        let text = prompt_text(&req.prompt);
        let buf = events.with_file_name("prompt.txt");
        tokio::fs::write(&buf, text.as_bytes()).await?;
        let name = Self::tmux_name(&id);
        if !self.clear_dialogs(&name).await? {
            bail!("a dialog in the claude TUI could not be dismissed; screen:\n{}", self.capture(&name).await.unwrap_or_default().trim());
        }
        self.paste(&name, &id, &buf).await?;
        // Submit, and confirm via the UserPromptSubmit hook. Right after startup or
        // resume the TUI can swallow an Enter, so retry a few times.
        let mut submitted = false;
        for attempt in 0..8 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if attempt > 0 {
                let screen = self.capture(&name).await.unwrap_or_default();
                if looks_like_dialog(&screen) {
                    // A dialog popped up over the input: dismiss it and paste again.
                    self.clear_dialogs(&name).await?;
                    self.paste(&name, &id, &buf).await?;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
            Self::run({
                let mut c = self.tmux();
                c.args(["send-keys", "-t", &name, "Enter"]);
                c
            })
            .await?;
            if self.wait_for(&events, offset, Duration::from_secs(3), |e| e.name == "UserPromptSubmit").await.is_ok() {
                submitted = true;
                break;
            }
        }
        if !submitted {
            bail!("the prompt was not submitted (no UserPromptSubmit hook)");
        }

        let deadline = tokio::time::Instant::now() + self.cfg.turn_timeout;
        let mut seen = offset;
        loop {
            let (evs, next) = read_from(&events, seen).await;
            seen = next;
            for ev in evs {
                if let Some(u) = to_update(&ev) {
                    let _ = updates.send(SessionNotification::new(req.session_id.clone(), u)).await;
                }
                match ev.name.as_str() {
                    "PermissionRequest" => {
                        if let Some(p) = permission_request(&req.session_id, &ev) {
                            let rid = p.tool_call.tool_call_id.to_string();
                            let delivered = match &perms {
                                Some(tx) => tx.send(p).await.is_ok(),
                                None => false,
                            };
                            if !delivered {
                                let deny = RequestPermissionOutcome::Cancelled;
                                let _ = self.respond_permission(&req.session_id, &rid, &deny, Some("no approver is connected")).await;
                            }
                        }
                    }
                    "Stop" => return Ok(PromptResponse::new(StopReason::EndTurn)),
                    "StopFailure" => bail!("claude turn failed: {}", ev.raw.get("error").unwrap_or(&Value::Null)),
                    _ => {}
                }
            }
            if tokio::time::Instant::now() > deadline {
                bail!("turn timed out");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn paste(&self, tmux_name: &str, id: &str, buf: &Path) -> Result<()> {
        let bufname = format!("kurisu-{}", &id[..8]);
        Self::run({
            let mut c = self.tmux();
            c.args(["load-buffer", "-b", &bufname]).arg(buf);
            c
        })
        .await?;
        Self::run({
            let mut c = self.tmux();
            c.args(["paste-buffer", "-p", "-d", "-b", &bufname, "-t", tmux_name]);
            c
        })
        .await
        .map(|_| ())
    }

    /// Answer a permission request (`tool_call_id` of the `RequestPermissionRequest`).
    /// `allow_once` allows the call; any other outcome denies it with `message`.
    pub async fn respond_permission(
        &self,
        session_id: &SessionId,
        request_id: &str,
        outcome: &RequestPermissionOutcome,
        message: Option<&str>,
    ) -> Result<()> {
        let allow = matches!(outcome, RequestPermissionOutcome::Selected(s) if &*s.option_id.0 == ALLOW_ONCE || &*s.option_id.0 == "allow_always");
        let decision = if allow {
            serde_json::json!({"behavior": "allow"})
        } else {
            serde_json::json!({"behavior": "deny", "message": message.unwrap_or("denied by the user")})
        };
        let dir = self.cfg.state_dir.join(session_id.to_string());
        write_decision(&dir, request_id, &decision)
    }

    /// ACP `session/cancel`: Escape interrupts the running turn in the TUI.
    pub async fn cancel(&self, session_id: &SessionId) -> Result<()> {
        let name = Self::tmux_name(&session_id.to_string());
        Self::run({
            let mut c = self.tmux();
            c.args(["send-keys", "-t", &name, "Escape"]);
            c
        })
        .await
        .map(|_| ())
    }

    /// Stop the CLI process (the transcript stays; `load_session` resumes it).
    pub async fn close(&self, session_id: &SessionId) -> Result<()> {
        let id = session_id.to_string();
        self.live.lock().await.remove(&id);
        let name = Self::tmux_name(&id);
        Self::run({
            let mut c = self.tmux();
            c.args(["kill-session", "-t", &name]);
            c
        })
        .await
        .map(|_| ())
    }

    pub async fn cwd(&self, session_id: &SessionId) -> Option<PathBuf> {
        self.live.lock().await.get(&session_id.to_string()).map(|l| l.cwd.clone())
    }

    async fn wait_for(&self, events: &Path, offset: u64, timeout: Duration, pred: impl Fn(&HookEvent) -> bool) -> Result<HookEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut seen = offset;
        loop {
            let (evs, next) = read_from(events, seen).await;
            seen = next;
            if let Some(e) = evs.into_iter().find(|e| pred(e)) {
                return Ok(e);
            }
            if tokio::time::Instant::now() > deadline {
                bail!("timed out waiting for hook event");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

async fn file_len(p: &Path) -> u64 {
    tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0)
}

/// Read complete lines appended after `offset`; returns events and the new offset.
async fn read_from(p: &Path, offset: u64) -> (Vec<HookEvent>, u64) {
    let Ok(bytes) = tokio::fs::read(p).await else { return (vec![], offset) };
    let start = (offset as usize).min(bytes.len());
    let tail = &bytes[start..];
    let end = tail.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let text = String::from_utf8_lossy(&tail[..end]);
    (parse_events(&text), (start + end) as u64)
}

fn valid_request_id(rid: &str) -> bool {
    !rid.is_empty() && rid.len() <= 64 && rid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn decision_path(dir: &Path, rid: &str) -> PathBuf {
    dir.join("decisions").join(format!("{rid}.json"))
}

/// Write a decision atomically (tmp file, then rename) for the waiting hook.
pub fn write_decision(dir: &Path, rid: &str, decision: &Value) -> Result<()> {
    if !valid_request_id(rid) {
        bail!("bad permission request id");
    }
    let path = decision_path(dir, rid);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(decision)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Hook entry point: append stdin (one JSON object) as a line to `events`.
pub fn run_hook(events: &Path) -> Result<()> {
    run_hook_args(&[events.to_string_lossy().into_owned()])
}

/// Hook entry point with arguments: `<events> [--permission <secs>]`.
/// With `--permission`, the event gets a request id, and the hook waits for
/// the driver's decision file, prints the `PermissionRequest` hook answer,
/// and denies the call if nothing arrives within `secs`.
pub fn run_hook_args(args: &[String]) -> Result<()> {
    use std::io::{Read, Write};
    let events = PathBuf::from(args.first().context("events path")?);
    let perm_secs: Option<u64> = args.iter().position(|a| a == "--permission").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok());
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let mut v: Value = serde_json::from_str(input.trim()).unwrap_or(Value::Null);
    let rid = uuid::Uuid::new_v4().to_string();
    if let (Some(secs), Some(obj)) = (perm_secs, v.as_object_mut()) {
        obj.insert("kurisu_request_id".into(), Value::String(rid.clone()));
        obj.insert("expires_at".into(), Value::from(now_secs() + secs));
    }
    {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&events)?;
        writeln!(f, "{}", serde_json::to_string(&v)?)?;
    }
    let Some(secs) = perm_secs else { return Ok(()) };
    let dir = events.parent().map(Path::to_path_buf).unwrap_or_default();
    let path = decision_path(&dir, &rid);
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let decision = loop {
        if let Ok(bytes) = std::fs::read(&path) {
            let _ = std::fs::remove_file(&path);
            break serde_json::from_slice::<Value>(&bytes).unwrap_or(serde_json::json!({"behavior": "deny", "message": "bad decision"}));
        }
        if std::time::Instant::now() > deadline {
            break serde_json::json!({"behavior": "deny", "message": format!("the approval request expired after {secs}s with no answer")});
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let out = serde_json::json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}});
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_and_maps_updates() {
        let text = concat!(
            r#"{"hook_event_name":"SessionStart","source":"startup"}"#, "\n",
            r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"t1"}"#, "\n",
            r#"{"hook_event_name":"Stop","last_assistant_message":"PONG"}"#, "\n",
            "not json\n"
        );
        let evs = parse_events(text);
        assert_eq!(evs.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["SessionStart", "PostToolUse", "Stop"]);
        assert!(to_update(&evs[0]).is_none());
        assert!(matches!(to_update(&evs[1]), Some(SessionUpdate::ToolCall(_))));
        match to_update(&evs[2]) {
            Some(SessionUpdate::AgentMessageChunk(c)) => match c.content {
                ContentBlock::Text(t) => assert_eq!(t.text, "PONG"),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn hooks_settings_quote_paths_and_cover_lifecycle() {
        let v = hooks_settings(&["/opt/k k/claude-cli-acp".into(), "hook".into()], Path::new("/s/a b/events.jsonl"), 600);
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "'/opt/k k/claude-cli-acp' hook '/s/a b/events.jsonl'");
        let perm = &v["hooks"]["PermissionRequest"][0]["hooks"][0];
        assert_eq!(perm["command"], "'/opt/k k/claude-cli-acp' hook '/s/a b/events.jsonl' --permission 600");
        assert_eq!(perm["timeout"], 630);
        for k in ["SessionStart", "Stop", "StopFailure", "PostToolUse", "Notification"] {
            assert!(v["hooks"].get(k).is_some(), "{k}");
        }
        assert_eq!(v.as_object().unwrap().len(), 1, "only hooks are set");
    }

    #[tokio::test]
    async fn read_from_only_returns_complete_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e.jsonl");
        std::fs::write(&p, "{\"hook_event_name\":\"A\"}\n{\"hook_event_name\":\"B\"").unwrap();
        let (evs, off) = read_from(&p, 0).await;
        assert_eq!(evs.len(), 1);
        std::fs::OpenOptions::new().append(true).open(&p).map(|mut f| {
            use std::io::Write;
            writeln!(f, "}}").unwrap()
        }).unwrap();
        let (evs, _) = read_from(&p, off).await;
        assert_eq!(evs[0].name, "B");
    }

    #[test]
    fn detects_dialogs_but_not_the_input_box() {
        let dialog = "   Teach auto mode about your environment?\n     Continue\n   ←/→ to change · Enter to continue · Esc to cancel\n";
        assert!(looks_like_dialog(dialog));
        let input = "● Canberra\n────\n❯ \n────\n  ⏵⏵ auto mode on (shift+tab to cycle)\n";
        assert!(!looks_like_dialog(input));
    }

    #[test]
    fn permission_events_map_to_acp_requests_and_decisions_round_trip() {
        let ev = &parse_events(r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls -la"},"cwd":"/w","kurisu_request_id":"abc-1"}"#)[0];
        let p = permission_request(&SessionId::new("s"), ev).unwrap();
        assert_eq!(p.tool_call.tool_call_id.to_string(), "abc-1");
        assert_eq!(p.tool_call.fields.title.as_deref(), Some("Bash: ls -la"));
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.meta.unwrap()["cwd"], "/w");
        let dir = tempfile::tempdir().unwrap();
        write_decision(dir.path(), "abc-1", &serde_json::json!({"behavior": "allow"})).unwrap();
        assert!(decision_path(dir.path(), "abc-1").exists());
        assert!(write_decision(dir.path(), "../x", &Value::Null).is_err());
    }

    #[test]
    fn prompt_text_joins_text_blocks() {
        let blocks = vec![ContentBlock::Text(TextContent::new("a")), ContentBlock::Text(TextContent::new("b"))];
        assert_eq!(prompt_text(&blocks), "a\nb");
    }
}
