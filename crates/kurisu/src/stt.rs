//! Optional speech-to-text. Off until the user opts in (`ku stt setup` or `ku stt use`),
//! which writes `<KURISU_HOME>/stt.json`. Kurisu never bundles a model: it runs an
//! external command (default: the `stt` CLI) or calls a stdio MCP server's tool.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Audio files Kurisu will transcribe (voice notes and common formats).
pub const AUDIO_EXTS: &[&str] = &["ogg", "oga", "opus", "mp3", "m4a", "wav", "flac", "aac", "webm", "amr", "mp4"];

/// How long one transcription may take (long voice notes on CPU are slow).
pub const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum Config {
    /// argv; `{file}` is replaced by the audio path, else the path is appended. Prints the transcript.
    Command { command: Vec<String> },
    /// A stdio MCP server (argv) and the tool to call with `{"path": <file>}`.
    Mcp { command: Vec<String>, tool: String },
    /// Explicitly off (`ku stt off`): not even auto-detected.
    Off,
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("stt.json")
}

/// The saved config, or None when speech-to-text is off.
pub fn load(home: &Path) -> Option<Config> {
    serde_json::from_slice(&std::fs::read(config_path(home)).ok()?).ok()
}

pub fn save(home: &Path, cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(home)?;
    std::fs::write(config_path(home), serde_json::to_vec_pretty(cfg)?)?;
    Ok(())
}

/// The configured backend, unless it is off (or not set up).
pub fn active(home: &Path) -> Option<Config> {
    load(home).filter(|c| *c != Config::Off)
}

/// `ku stt off`: remembered, so the Telegram channel's auto-detection stays off too.
pub fn disable(home: &Path) -> Result<bool> {
    let was = active(home).is_some();
    save(home, &Config::Off)?;
    Ok(was)
}

/// A working `stt` CLI (on PATH or ~/.local/bin, and `stt status` exits 0), as the
/// Python bridge detects it.
pub async fn detect() -> Option<Config> {
    let cand = which_bin("stt").or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/bin/stt")).filter(|p| p.is_file()))?;
    let out = tokio::time::timeout(Duration::from_secs(10), Command::new(&cand).arg("status").stdin(Stdio::null()).output()).await.ok()?.ok()?;
    out.status.success().then(|| Config::Command { command: vec![cand.to_string_lossy().into_owned()] })
}

/// A binary on PATH, or in a well-known install dir (the daemon's PATH is narrow).
pub fn which_bin(bin: &str) -> Option<PathBuf> {
    crate::harness::resolve(bin)
}

pub fn is_audio(name: &str) -> bool {
    Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Split a command line on whitespace, honouring simple '…' and "…" quotes.
pub fn split_command(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Transcribe one audio file with the configured backend.
pub async fn transcribe(cfg: &Config, path: &Path) -> Result<String> {
    if !path.is_file() {
        bail!("no such file: {}", path.display());
    }
    if !is_audio(&path.to_string_lossy()) {
        bail!("{} is not an audio file ({})", path.display(), AUDIO_EXTS.join(", "));
    }
    tokio::time::timeout(TIMEOUT, async {
        match cfg {
            Config::Command { command } => run_command(command, path).await,
            Config::Mcp { command, tool } => call_mcp(command, tool, path).await,
            Config::Off => bail!("speech-to-text is off"),
        }
    })
    .await
    .map_err(|_| anyhow!("speech-to-text took longer than {}s", TIMEOUT.as_secs()))?
}

async fn run_command(argv: &[String], path: &Path) -> Result<String> {
    let p = path.to_string_lossy().into_owned();
    let mut args: Vec<String> = argv.iter().map(|a| a.replace("{file}", &p)).collect();
    if !argv.iter().any(|a| a.contains("{file}")) {
        args.push(p);
    }
    let (bin, rest) = args.split_first().ok_or_else(|| anyhow!("empty speech-to-text command"))?;
    let out = Command::new(bin).args(rest).stdin(Stdio::null()).kill_on_drop(true).output().await
        .with_context(|| format!("run {bin}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{bin} failed ({}): {}", out.status, tail(err.trim(), 400));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn call_mcp(argv: &[String], tool: &str, path: &Path) -> Result<String> {
    let (bin, rest) = argv.split_first().ok_or_else(|| anyhow!("empty MCP command"))?;
    let mut child = Command::new(bin).args(rest).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .kill_on_drop(true).spawn().with_context(|| format!("start MCP server {bin}"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let mut lines = BufReader::new(child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?).lines();
    let msgs = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "kurisu", "version": env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": tool, "arguments": {"path": path}}}),
    ];
    for m in msgs {
        stdin.write_all(format!("{m}\n").as_bytes()).await?;
    }
    stdin.flush().await?;
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("id") != Some(&json!(2)) {
            continue;
        }
        if let Some(e) = v.get("error") {
            bail!("MCP {tool} failed: {}", e.get("message").and_then(Value::as_str).unwrap_or("error"));
        }
        let r = &v["result"];
        let text = r["content"].as_array().into_iter().flatten()
            .filter_map(|c| c.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n");
        if r["isError"] == json!(true) {
            bail!("MCP {tool} failed: {}", tail(text.trim(), 400));
        }
        return Ok(text.trim().to_string());
    }
    bail!("MCP server {bin} closed without answering")
}

fn tail(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    c[c.len().saturating_sub(n)..].iter().collect()
}

/// Text that goes into the message for one transcribed (or failed) audio attachment.
pub fn note(path: &str, res: &Result<String>) -> String {
    match res {
        Ok(t) if t.is_empty() => format!("[Voice/audio {path}: local speech-to-text heard no speech]"),
        Ok(t) => format!("[Voice/audio {path}, local speech-to-text transcript (may contain mistakes):]\n{t}"),
        Err(e) => format!("[Voice/audio {path}: speech-to-text failed ({e:#}); the file is passed through unchanged]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip_and_helpers() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_none());
        let c = Config::Mcp { command: vec!["stt".into(), "mcp".into()], tool: "transcribe".into() };
        save(dir.path(), &c).unwrap();
        assert_eq!(load(dir.path()), Some(c));
        assert!(std::fs::read_to_string(config_path(dir.path())).unwrap().contains("\"backend\": \"mcp\""));
        assert!(disable(dir.path()).unwrap() && !disable(dir.path()).unwrap());
        assert_eq!(load(dir.path()), Some(Config::Off));
        assert!(active(dir.path()).is_none());
        assert!(is_audio("x/20261107-voice.OGG") && is_audio("a.m4a") && !is_audio("notes.txt") && !is_audio("ogg"));
        assert_eq!(split_command(r#"whisper-cli -m "/my models/base.bin" -f {file}"#), ["whisper-cli", "-m", "/my models/base.bin", "-f", "{file}"]);
    }

    #[tokio::test]
    async fn command_and_mcp_backends() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("v.ogg");
        std::fs::write(&audio, b"x").unwrap();
        // A command: the path is appended, or put where {file} is.
        let echo = Config::Command { command: vec!["echo".into(), "heard".into()] };
        assert_eq!(transcribe(&echo, &audio).await.unwrap(), format!("heard {}", audio.display()));
        let sh = Config::Command { command: split_command("sh -c 'echo [$1]' _ {file}") };
        assert_eq!(transcribe(&sh, &audio).await.unwrap(), format!("[{}]", audio.display()));
        let fail = Config::Command { command: split_command("sh -c 'echo nope >&2; exit 3'") };
        assert!(transcribe(&fail, &audio).await.unwrap_err().to_string().contains("nope"));
        assert!(transcribe(&echo, &dir.path().join("notes.txt")).await.is_err());
        // A tiny stdio MCP server that answers tools/call.
        let script = dir.path().join("mcp.sh");
        std::fs::write(&script, "while read l; do case \"$l\" in *'\"id\":2'*) echo '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hello there\"}]}}';; *'\"id\":1'*) echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}';; esac; done\n").unwrap();
        let mcp = Config::Mcp { command: vec!["sh".into(), script.to_string_lossy().into_owned()], tool: "transcribe".into() };
        assert_eq!(transcribe(&mcp, &audio).await.unwrap(), "hello there");
        assert!(note("v.ogg", &Ok("hi".into())).contains("transcript"));
    }
}
