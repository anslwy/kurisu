//! Locations (XDG by default, `KURISU_HOME` to override) and small OS helpers.

use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::net::UnixListener;

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        if let Some(h) = std::env::var_os("KURISU_HOME") {
            return Self { home: PathBuf::from(h) };
        }
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
        Self { home: base.join("kurisu") }
    }
    pub fn socket(&self) -> PathBuf {
        self.home.join("kurisu.sock")
    }
    pub fn db(&self) -> PathBuf {
        self.home.join("kurisu.db")
    }
    pub fn log(&self) -> PathBuf {
        self.home.join("server.log")
    }
    pub fn session_dir(&self, name: &str) -> PathBuf {
        self.home.join("sessions").join(name)
    }
    pub fn workspace(&self, name: &str) -> PathBuf {
        self.home.join("workspaces").join(name)
    }
    /// Private tmux server per Kurisu home, so the user's own tmux is untouched.
    pub fn tmux_socket(&self) -> String {
        let h = self.home.to_string_lossy().bytes().fold(5381u32, |a, b| a.wrapping_mul(33) ^ b as u32);
        format!("kurisu-{h:08x}")
    }
}

extern "C" {
    fn umask(mask: u32) -> u32;
}

/// Bind a unix socket that is 0600 from the first moment.
pub fn bind_private(path: &Path) -> Result<UnixListener> {
    let old = unsafe { umask(0o177) };
    let l = UnixListener::bind(path);
    unsafe { umask(old) };
    Ok(l?)
}

/// Resident set size of this process in kB (Linux).
pub fn rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}
