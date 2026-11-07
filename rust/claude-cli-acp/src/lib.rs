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
//! * **resume**: `claude --resume <uuid>` with the same hooks.
//!
//! Hook events are translated to ACP `session/update`s (agent message chunks,
//! tool calls) and the turn ends with an ACP `StopReason`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, LoadSessionRequest, TextContent, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, ToolCall,
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
pub fn hooks_settings(hook_cmd: &[String], events: &Path) -> Value {
    let mut argv: Vec<String> = hook_cmd.iter().map(|a| shell_quote(a)).collect();
    argv.push(shell_quote(&events.to_string_lossy()));
    let cmd = argv.join(" ");
    let entry = serde_json::json!([{ "hooks": [{ "type": "command", "command": cmd }] }]);
    let mut hooks = serde_json::Map::new();
    for name in ["SessionStart", "UserPromptSubmit", "Stop", "StopFailure", "PostToolUse", "Notification", "PreCompact"] {
        hooks.insert(name.into(), entry.clone());
    }
    serde_json::json!({ "hooks": hooks })
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
        tokio::fs::write(&settings, serde_json::to_vec(&hooks_settings(&self.cfg.hook_cmd, &events))?).await?;
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
    pub async fn prompt(&self, req: PromptRequest, updates: mpsc::Sender<SessionNotification>) -> Result<PromptResponse> {
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

/// Hook entry point: append stdin (one JSON object) as a line to `events`.
pub fn run_hook(events: &Path) -> Result<()> {
    use std::io::{Read, Write};
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let v: Value = serde_json::from_str(input.trim()).unwrap_or(Value::Null);
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(events)?;
    writeln!(f, "{}", serde_json::to_string(&v)?)?;
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
        let v = hooks_settings(&["/opt/k k/claude-cli-acp".into(), "hook".into()], Path::new("/s/a b/events.jsonl"));
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "'/opt/k k/claude-cli-acp' hook '/s/a b/events.jsonl'");
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
    fn prompt_text_joins_text_blocks() {
        let blocks = vec![ContentBlock::Text(TextContent::new("a")), ContentBlock::Text(TextContent::new("b"))];
        assert_eq!(prompt_text(&blocks), "a\nb");
    }
}
