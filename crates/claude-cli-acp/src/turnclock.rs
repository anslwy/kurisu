//! When a running turn counts as hung.
//!
//! A turn is not killed by a fixed wall clock while it is still making
//! progress: [`TurnClock`] fails a turn only after `idle` without any sign of
//! progress (an event from the CLI, streamed output, or work in a process the
//! turn started), or after an optional, much larger `max` age.
//!
//! [`ProcProbe`] is the "work in a process" signal: it watches the processes
//! below an agent process that were started after the turn began (the tool
//! commands) and reports a change in their set, CPU time or bytes written. A
//! long build or a command that keeps printing changes it; a command blocked on
//! `sleep` or a stalled network read does not. It is Linux-only (/proc); on
//! other systems it never reports progress and the CLI's own events decide.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use tokio::time::Instant;

/// Default: a turn with no progress for this long is failed.
pub const DEFAULT_IDLE_SECS: u64 = 30 * 60;
/// Default hard cap on one turn's age (0 = none).
pub const DEFAULT_MAX_SECS: u64 = 24 * 60 * 60;

/// Inactivity and age limits for one turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Fail the turn after this long without progress.
    pub idle: Duration,
    /// Fail the turn after this age whatever it is doing (None = no cap).
    pub max: Option<Duration>,
}

impl Default for Limits {
    fn default() -> Self {
        Self { idle: Duration::from_secs(DEFAULT_IDLE_SECS), max: Some(Duration::from_secs(DEFAULT_MAX_SECS)) }
    }
}

impl Limits {
    /// `KURISU_TURN_IDLE_TIMEOUT_SECS` (default 1800) and `KURISU_TURN_MAX_SECS`
    /// (default 86400, 0 = no cap). `legacy` is an older per-harness variable
    /// (e.g. `KURISU_OPENCODE_TIMEOUT_SECS`) read as the idle timeout when the
    /// new one is unset.
    pub fn from_env(legacy: Option<&str>) -> Self {
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok());
        Self::from_values(get("KURISU_TURN_IDLE_TIMEOUT_SECS").or_else(|| legacy.and_then(get)), get("KURISU_TURN_MAX_SECS"))
    }

    pub fn from_values(idle: Option<u64>, max: Option<u64>) -> Self {
        let d = Self::default();
        Self {
            idle: idle.filter(|s| *s > 0).map(Duration::from_secs).unwrap_or(d.idle),
            max: match max {
                Some(0) => None,
                Some(s) => Some(Duration::from_secs(s)),
                None => d.max,
            },
        }
    }
}

/// Tracks one running turn against its [`Limits`].
#[derive(Debug)]
pub struct TurnClock {
    limits: Limits,
    started: Instant,
    last: Instant,
}

impl TurnClock {
    pub fn new(limits: Limits) -> Self {
        let now = Instant::now();
        Self { limits, started: now, last: now }
    }

    /// The turn made progress just now.
    pub fn touch(&mut self) {
        self.last = Instant::now();
    }

    /// When the turn expires unless it makes progress before then.
    pub fn deadline(&self) -> Instant {
        let idle = self.last + self.limits.idle;
        match self.limits.max {
            Some(max) => idle.min(self.started + max),
            None => idle,
        }
    }

    /// `Some(reason)` once the turn has hung (no progress for `idle`) or is too old.
    pub fn expired(&self) -> Option<String> {
        self.expired_at(Instant::now())
    }

    fn expired_at(&self, now: Instant) -> Option<String> {
        if let Some(max) = self.limits.max {
            if now >= self.started + max {
                return Some(format!("turn timed out: it ran longer than the {} cap (KURISU_TURN_MAX_SECS)", show(max)));
            }
        }
        if now >= self.last + self.limits.idle {
            return Some(format!(
                "turn timed out: no progress for {} (no output, tool activity or events; KURISU_TURN_IDLE_TIMEOUT_SECS)",
                show(self.limits.idle)
            ));
        }
        None
    }
}

fn show(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s >= 3600 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s}s"),
    }
}

/// Signature of the processes a turn started below an agent process.
pub struct ProcProbe {
    root: u32,
    /// Only processes started at or after this many clock ticks since boot count.
    since_ticks: u64,
    /// Processes whose command line contains one of these (and everything
    /// below them) are not the turn's work: e.g. the driver's own hook and
    /// status-line commands, which the CLI runs on its own while idle.
    ignore: Vec<String>,
    last: BTreeMap<u32, (u64, u64)>,
}

impl ProcProbe {
    /// Watch the descendants of `root` started from now on.
    pub fn new(root: u32) -> Self {
        Self::ignoring(root, vec![])
    }

    /// Same, skipping processes whose command line (args joined by spaces)
    /// contains one of `ignore` (e.g. a helper binary's path, which also
    /// matches the `sh -c` wrapper that runs it), and their descendants.
    pub fn ignoring(root: u32, ignore: Vec<String>) -> Self {
        let since_ticks = uptime_ticks().unwrap_or(0);
        let ignore = ignore.into_iter().filter(|p| !p.is_empty()).collect();
        let mut p = Self { root, since_ticks, ignore, last: BTreeMap::new() };
        p.last = p.sample();
        p
    }

    /// True if the turn's processes changed since the last call (a process
    /// started or ended, used CPU, or wrote output).
    pub fn changed(&mut self) -> bool {
        let now = self.sample();
        let changed = now != self.last;
        self.last = now;
        changed
    }

    fn sample(&self) -> BTreeMap<u32, (u64, u64)> {
        let mut out = BTreeMap::new();
        let Ok(dir) = std::fs::read_dir("/proc") else { return out };
        // pid -> (ppid, start ticks, cpu ticks)
        let mut procs: HashMap<u32, (u32, u64, u64)> = HashMap::new();
        for e in dir.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if let Some(st) = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|s| parse_stat(&s)) {
                procs.insert(pid, st);
            }
        }
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, (ppid, _, _)) in &procs {
            children.entry(*ppid).or_default().push(*pid);
        }
        let mut stack = vec![self.root];
        while let Some(p) = stack.pop() {
            for c in children.get(&p).into_iter().flatten() {
                let (_, start, cpu) = procs[c];
                if start >= self.since_ticks && !self.ignore.is_empty() && self.ignored(*c) {
                    continue;
                }
                stack.push(*c);
                if start >= self.since_ticks {
                    out.insert(*c, (cpu, written(*c)));
                }
            }
        }
        out
    }
}

impl ProcProbe {
    fn ignored(&self, pid: u32) -> bool {
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return false };
        let line = String::from_utf8_lossy(&raw).replace('\0', " ");
        self.ignore.iter().any(|p| line.contains(p.as_str()))
    }
}

/// (ppid, start time, utime+stime) from /proc/<pid>/stat.
fn parse_stat(s: &str) -> Option<(u32, u64, u64)> {
    // The command name is in parentheses and may contain spaces.
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state(0) ppid(1) ... utime(11) stime(12) ... starttime(19).
    let ppid = f.get(1)?.parse().ok()?;
    let cpu = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    let start = f.get(19)?.parse().ok()?;
    Some((ppid, start, cpu))
}

/// Bytes the process wrote (wchar), 0 if unreadable.
fn written(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/io"))
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("wchar:").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(0)
}

/// Now, in clock ticks since boot (the unit of /proc/<pid>/stat starttime).
fn uptime_ticks() -> Option<u64> {
    let up: f64 = std::fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
    // USER_HZ is 100 on every Linux ABI Kurisu runs on. Round down and allow a
    // tick of slack so a process started in the same tick still counts.
    Some(((up * 100.0) as u64).saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe tests watch this test process's children: run them one at a time.
    static PROCS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn limits_from_values() {
        let d = Limits::from_values(None, None);
        assert_eq!(d, Limits::default());
        let l = Limits::from_values(Some(60), Some(0));
        assert_eq!((l.idle, l.max), (Duration::from_secs(60), None));
        let l = Limits::from_values(Some(0), Some(120));
        assert_eq!((l.idle, l.max), (Duration::from_secs(DEFAULT_IDLE_SECS), Some(Duration::from_secs(120))));
    }

    #[test]
    fn idle_resets_on_progress_and_max_caps() {
        let mut c = TurnClock::new(Limits { idle: Duration::from_secs(10), max: Some(Duration::from_secs(25)) });
        let t0 = c.started;
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(c.expired_at(at(8)).is_none());
        c.last = at(8);
        assert!(c.expired_at(at(16)).is_none(), "progress resets the idle clock");
        assert!(c.expired_at(at(19)).unwrap().contains("no progress for 10s"));
        c.last = at(19);
        assert_eq!(c.deadline(), at(25));
        assert!(c.expired_at(at(26)).unwrap().contains("cap"), "the age cap wins over progress");
        let c = TurnClock::new(Limits { idle: Duration::from_secs(10), max: None });
        assert!(c.expired_at(c.started + Duration::from_secs(9)).is_none());
    }

    #[test]
    fn stat_parses_names_with_spaces() {
        let s = "1234 (a b) c) S 99 1 1 0 -1 0 0 0 0 0 7 5 0 0 20 0 1 0 4242 0 0";
        assert_eq!(parse_stat(s), Some((99, 4242, 12)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_ignores_the_drivers_own_helper_commands() {
        let _one = PROCS.lock().unwrap_or_else(|e| e.into_inner());
        let me = std::process::id();
        let mut p = ProcProbe::ignoring(me, vec!["sleep 31".into()]);
        let mut helper = std::process::Command::new("sleep").arg("31").spawn().unwrap();
        assert!(!p.changed(), "an ignored helper is not the turn's work");
        let mut work = std::process::Command::new("sleep").arg("32").spawn().unwrap();
        assert!(p.changed(), "other new processes still are");
        for c in [&mut helper, &mut work] {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_sees_new_children_and_their_output_but_not_a_sleeper() {
        let _one = PROCS.lock().unwrap_or_else(|e| e.into_inner());
        let me = std::process::id();
        let mut p = ProcProbe::new(me);
        assert!(!p.changed());
        let mut sleeper = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        assert!(p.changed(), "a new child process is progress");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!p.changed(), "a process blocked in sleep is not");
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert!(p.changed(), "a child ending is progress");
        let mut printer = std::process::Command::new("sh")
            .args(["-c", "while :; do echo tick; sleep 0.2; done"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        p.changed();
        let mut seen = 0;
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(250));
            seen += p.changed() as u32;
        }
        printer.kill().unwrap();
        printer.wait().unwrap();
        assert!(seen >= 3, "a process that keeps printing is progress ({seen}/5)");
    }
}
