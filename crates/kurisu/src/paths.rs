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
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let base = match std::env::var_os("XDG_DATA_HOME") {
            Some(x) => PathBuf::from(x),
            None if cfg!(target_os = "macos") => home.join("Library/Application Support"),
            None => home.join(".local/share"),
        };
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

#[cfg(target_os = "macos")]
type ModeT = u16; // mode_t is 16-bit on macOS
#[cfg(not(target_os = "macos"))]
type ModeT = u32;

extern "C" {
    fn umask(mask: ModeT) -> ModeT;
}

/// Bind a unix socket that is 0600 from the first moment.
pub fn bind_private(path: &Path) -> Result<UnixListener> {
    check_socket_path(path)?;
    let old = unsafe { umask(0o177) };
    let l = UnixListener::bind(path);
    unsafe { umask(old) };
    Ok(l?)
}

/// Resident set size of this process in kB (Linux: /proc; macOS and others: `ps`).
pub fn rss_kb() -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        return s
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok());
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Unix socket paths are limited (104 bytes on macOS, 108 on Linux).
pub fn check_socket_path(p: &Path) -> Result<()> {
    let n = p.as_os_str().len();
    if n >= 100 {
        anyhow::bail!("socket path is {n} bytes, too long for a unix socket; set KURISU_HOME to a shorter directory");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_is_measurable_and_socket_length_checked() {
        assert!(rss_kb().unwrap_or(0) > 0);
        assert!(check_socket_path(Path::new("/tmp/k/kurisu.sock")).is_ok());
        assert!(check_socket_path(&Path::new("/x").join("a".repeat(120))).is_err());
    }
}
