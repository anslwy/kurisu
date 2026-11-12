//! Background jobs: a shell command a session starts with the kurisu-mcp
//! `run_background` tool, so it can end its turn and be woken when the command
//! exits. The job runs in its own process group, detached from the CLI, with
//! its output in a log file. Its state lives on disk (one folder per job under
//! `<KURISU_HOME>/sessions/<id>/jobs/`), so a Kurisu restart neither kills the
//! job's bookkeeping nor loses its wake-up: the server's ticker finds a job
//! whose exit file appeared (or whose process vanished) and delivers it once.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Lines of the job's output quoted in the wake-up message.
pub const TAIL_LINES: usize = 40;
/// Most jobs one session may have running at once.
pub const MAX_RUNNING: usize = 20;

/// What was started, by whom, and for whose message.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    /// Session id that started the job (and is woken when it exits).
    pub session: String,
    pub command: String,
    pub cwd: String,
    pub pid: u32,
    pub started: i64,
    /// The session whose message the starting turn was answering: it gets the
    /// follow-up turn's reply as a callback. None for the user.
    pub requester: Option<String>,
    /// That message's seq.
    pub request_seq: i64,
}

/// A job that has ended and was not delivered yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub job: Job,
    pub dir: PathBuf,
    /// None: the process vanished without writing its exit status (killed, or
    /// the machine restarted).
    pub code: Option<i32>,
    pub ended: i64,
}

pub fn jobs_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("jobs")
}

pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("output.log")
}

/// Start `command` with `sh` in `cwd`, detached, its stdout and stderr in the
/// job's log. `id` must be unique within the session.
pub fn start(session_dir: &Path, id: &str, session: &str, command: &str, cwd: &Path,
             requester: Option<String>, request_seq: i64, now: i64) -> Result<Job> {
    if command.trim().is_empty() {
        bail!("missing command");
    }
    if !cwd.is_dir() {
        bail!("{} is not a folder", cwd.display());
    }
    if running(session_dir).len() >= MAX_RUNNING {
        bail!("{MAX_RUNNING} background jobs are already running for this session: wait for one to finish");
    }
    let dir = jobs_dir(session_dir).join(id);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    use std::os::unix::process::CommandExt;
    // `$1` is the command, `$2` the log, `$3` the exit file: nothing is spliced
    // into the script text. The exit file is written atomically.
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"(eval "$1") >"$2" 2>&1 </dev/null; echo $? >"$3.tmp"; mv "$3.tmp" "$3""#)
        .arg("kurisu-job")
        .arg(command)
        .arg(log_path(&dir))
        .arg(dir.join("exit"))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("start sh")?;
    let job = Job {
        id: id.to_string(),
        session: session.to_string(),
        command: command.to_string(),
        cwd: cwd.display().to_string(),
        pid: child.id(),
        started: now,
        requester,
        request_seq,
    };
    std::fs::write(dir.join("job.json"), serde_json::to_vec_pretty(&job)?)?;
    // Reap it in the background so it never lingers as a zombie of the server.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(job)
}

fn read_job(dir: &Path) -> Option<Job> {
    serde_json::from_slice(&std::fs::read(dir.join("job.json")).ok()?).ok()
}

fn alive(pid: u32) -> bool {
    // A reaped process has no /proc entry; a zombie still does, so read its state.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => {
            if Path::new("/proc/self").exists() {
                return false;
            }
            // No /proc (macOS): ask the kernel.
            std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null())
                .status().is_ok_and(|s| s.success())
        }
    }
}

/// Jobs of one session that are still running.
pub fn running(session_dir: &Path) -> Vec<Job> {
    let Ok(rd) = std::fs::read_dir(jobs_dir(session_dir)) else { return vec![] };
    rd.flatten()
        .map(|e| e.path())
        .filter(|d| !d.join("exit").exists() && !d.join("delivered").exists())
        .filter_map(|d| read_job(&d))
        .filter(|j| alive(j.pid))
        .collect()
}

/// Every job under `sessions_root/*/jobs/` that ended and was not delivered.
pub fn finished(sessions_root: &Path, now: i64) -> Vec<Finished> {
    let mut out = vec![];
    let Ok(sessions) = std::fs::read_dir(sessions_root) else { return out };
    for s in sessions.flatten() {
        let Ok(jobs) = std::fs::read_dir(jobs_dir(&s.path())) else { continue };
        for j in jobs.flatten() {
            let dir = j.path();
            if dir.join("delivered").exists() {
                continue;
            }
            let Some(job) = read_job(&dir) else { continue };
            let code = std::fs::read_to_string(dir.join("exit")).ok().and_then(|c| c.trim().parse::<i32>().ok());
            if code.is_none() && alive(job.pid) {
                continue;
            }
            // Give a just-exited shell a moment to finish writing its exit file.
            if code.is_none() && !dir.join("exit.tmp").exists() {
                let gone = dir.join("gone");
                match std::fs::read_to_string(&gone).ok().and_then(|t| t.trim().parse::<i64>().ok()) {
                    Some(t) if now - t >= 3 => {}
                    Some(_) => continue,
                    None => {
                        let _ = std::fs::write(&gone, now.to_string());
                        continue;
                    }
                }
            } else if code.is_none() {
                continue;
            }
            out.push(Finished { job, dir, code, ended: now });
        }
    }
    out
}

/// Claim a finished job for delivery; false if it was already claimed.
pub fn claim(dir: &Path) -> bool {
    std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join("delivered")).is_ok()
}

/// The last `n` lines of a file (lossy UTF-8).
pub fn tail(path: &Path, n: usize) -> String {
    let Ok(bytes) = std::fs::read(path) else { return String::new() };
    // Only the end matters; keep memory bounded for huge logs.
    let start = bytes.len().saturating_sub(64 * 1024);
    let text = String::from_utf8_lossy(&bytes[start..]);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn show_secs(s: i64) -> String {
    match s {
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// How the job ended, in a few words (used in the callback outcome).
pub fn outcome(f: &Finished) -> String {
    match f.code {
        Some(c) => format!("background job {} exited with code {c}", f.job.id),
        None => format!("background job {} ended without an exit status (killed?)", f.job.id),
    }
}

/// The message that wakes the session when its job ended.
pub fn wake_text(f: &Finished) -> String {
    let log = log_path(&f.dir);
    let out = tail(&log, TAIL_LINES);
    let out = if out.trim().is_empty() { "(no output)".to_string() } else { out };
    format!(
        "[Kurisu background job {} finished: {} after {}]\ncommand: {}\nfolder: {}\nfull output: {}\n--- last {} lines ---\n{}\n---\n\
You started this job with run_background and ended your turn. Check the result and continue the task; your reply \
goes to {}.",
        f.job.id,
        match f.code { Some(c) => format!("exit code {c}"), None => "no exit status (killed?)".into() },
        show_secs(f.ended - f.job.started),
        f.job.command,
        f.job.cwd,
        log.display(),
        TAIL_LINES,
        out,
        if f.job.requester.is_some() { "whoever sent the message you were working on, as a callback" } else { "the user" },
    )
}

/// Resolve a job's folder: `cwd` (absolute, or relative to the session's folder) or the folder itself.
pub fn resolve_cwd(session_cwd: &Path, cwd: Option<&str>) -> Result<PathBuf> {
    let p = match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        None => session_cwd.to_path_buf(),
        Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
        Some(c) => session_cwd.join(c),
    };
    p.canonicalize().map_err(|e| anyhow!("folder {}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        match std::env::var_os("KURISU_TEST_TMPDIR") {
            Some(d) => tempfile::tempdir_in(d).unwrap(),
            None => tempfile::tempdir().unwrap(),
        }
    }

    fn wait_finished(root: &Path) -> Vec<Finished> {
        let t0 = std::time::Instant::now();
        let mut now = 1000;
        loop {
            let f = finished(root, now);
            if !f.is_empty() {
                return f;
            }
            assert!(t0.elapsed().as_secs() < 20, "job never finished");
            std::thread::sleep(std::time::Duration::from_millis(100));
            now += 1;
        }
    }

    #[test]
    fn a_job_runs_detached_and_is_delivered_once() {
        let home = tmp();
        let root = home.path().join("sessions");
        let sdir = root.join("s1");
        let cwd = home.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let job = start(&sdir, "j1", "s1", "echo start; pwd; sleep 0.5; echo 'it''s done' >&2; exit 3", &cwd,
                        Some("lead".into()), 42, 1000).unwrap();
        assert_eq!(running(&sdir).len(), 1);
        assert!(finished(&root, 1000).is_empty(), "still running");
        let f = wait_finished(&root);
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].code, f[0].job.requester.as_deref(), f[0].job.request_seq), (Some(3), Some("lead"), 42));
        assert_eq!(f[0].job, job);
        let text = wake_text(&f[0]);
        assert!(text.contains("exit code 3"), "{text}");
        assert!(text.contains("its done") && text.contains("start"), "{text}");
        assert!(text.contains(&cwd.display().to_string()), "ran in its folder: {text}");
        assert!(outcome(&f[0]).contains("code 3"));
        assert!(claim(&f[0].dir));
        assert!(!claim(&f[0].dir), "claimed once");
        assert!(finished(&root, 2000).is_empty(), "never delivered twice");
        assert!(running(&sdir).is_empty());
    }

    #[test]
    fn a_killed_job_is_reported_without_a_code() {
        let home = tmp();
        let root = home.path().join("sessions");
        let sdir = root.join("s1");
        let job = start(&sdir, "j2", "s1", "sleep 30", home.path(), None, 1, 1000).unwrap();
        // Kill the whole process group (the wrapper shell and the command).
        std::process::Command::new("kill").args(["-s", "KILL", "--", &format!("-{}", job.pid)]).status().unwrap();
        let f = wait_finished(&root);
        assert_eq!(f[0].code, None);
        assert!(wake_text(&f[0]).contains("killed?"));
        assert!(wake_text(&f[0]).contains("the user"));
    }

    #[test]
    fn bad_input_is_refused() {
        let home = tmp();
        let sdir = home.path().join("sessions/s1");
        assert!(start(&sdir, "j", "s1", "  ", home.path(), None, 1, 0).is_err());
        assert!(start(&sdir, "j", "s1", "true", &home.path().join("nope"), None, 1, 0).is_err());
        assert!(resolve_cwd(home.path(), Some("nope")).is_err());
        assert_eq!(resolve_cwd(home.path(), None).unwrap(), home.path().canonicalize().unwrap());
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        let home = tmp();
        let p = home.path().join("log");
        std::fs::write(&p, (1..=100).map(|i| format!("l{i}\n")).collect::<String>()).unwrap();
        assert_eq!(tail(&p, 3), "l98\nl99\nl100");
        assert_eq!(tail(&home.path().join("missing"), 3), "");
    }
}
