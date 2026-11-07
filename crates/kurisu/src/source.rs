//! `ku source`: this binary knows where it came from and can rebuild itself.
//!
//! Layout under KURISU_HOME (default ~/.kurisu):
//!   src/            a clone of the repo at the commit this binary was built from;
//!                   self-edits live on a `self/<date>` branch
//!   bin/ku.orig     the first binary that was replaced (the shipped one)
//!   bin/ku.prev     the binary before the most recent `ku source install`
//!   bin/target      the installed ku's path (so a kept copy can restore it:
//!                   ~/.kurisu/bin/ku.prev source rollback works even if ku is broken)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::paths::Paths;

pub const REPO: &str = env!("KURISU_REPO");
pub const COMMIT: &str = env!("KURISU_BUILD_COMMIT");
pub const DIRTY: &str = env!("KURISU_BUILD_DIRTY");
pub const TARGET: &str = env!("KURISU_TARGET");

/// "0.0.1 (abc1234, x86_64-unknown-linux-musl)"; "+local changes" when built from a dirty tree.
pub fn version_line() -> String {
    format!("{} ({}{}, {})", env!("CARGO_PKG_VERSION"), short(COMMIT), if DIRTY.is_empty() { "" } else { " +local changes" }, TARGET)
}

fn short(c: &str) -> &str {
    &c[..c.len().min(10)]
}

/// How to rebuild, for --help, `ku status` and the session prompts.
pub fn rebuild_note() -> String {
    format!("Built from {REPO} at commit {COMMIT}. To change Kurisu itself: `ku source get` (clone that commit into \
~/.kurisu/src), `ku source check` (Rust via rustup, a C compiler, git, tmux), edit, then `ku source build` and \
`ku source install` (keeps the previous binary and falls back to it if the new one fails). By hand: install rustup, \
a C toolchain (macOS: xcode-select --install; Debian/Ubuntu: build-essential; Fedora: gcc; Arch: base-devel), then \
`cargo install --path crates/kurisu` in the clone.")
}

fn src_dir(paths: &Paths) -> PathBuf {
    paths.home.join("src")
}

fn bin_dir(paths: &Paths) -> PathBuf {
    paths.home.join("bin")
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("run {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn which(bin: &str) -> Option<PathBuf> {
    crate::stt::which_bin(bin).or_else(|| {
        let h = std::env::var("HOME").ok()?;
        [".cargo/bin", ".local/bin"].iter().map(|d| PathBuf::from(&h).join(d).join(bin)).find(|p| p.is_file())
    })
}

fn confirm(question: &str, yes: bool) -> bool {
    if yes {
        return true;
    }
    eprint!("{question} [y/N] ");
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok();
    s.trim().eq_ignore_ascii_case("y") || s.trim().eq_ignore_ascii_case("yes")
}

/// (name, found, how to install) for each prerequisite.
fn prerequisites() -> Vec<(&'static str, bool, String)> {
    let mac = cfg!(target_os = "macos");
    let cc = std::env::var("CC").ok().filter(|c| !c.is_empty()).map(|c| c.split_whitespace().next().unwrap_or("").to_string())
        .and_then(|c| which(&c)).or_else(|| ["cc", "gcc", "clang"].iter().find_map(|c| which(c)));
    let distro = std::fs::read_to_string("/etc/os-release").unwrap_or_default().to_lowercase();
    let pkg = |debian: &str, fedora: &str, arch: &str, brew: &str| -> String {
        if mac {
            brew.to_string()
        } else if distro.contains("fedora") || distro.contains("rhel") {
            fedora.to_string()
        } else if distro.contains("arch") || distro.contains("steamos") {
            arch.to_string()
        } else {
            debian.to_string()
        }
    };
    vec![
        ("git", which("git").is_some(), pkg("sudo apt install git", "sudo dnf install git", "sudo pacman -S git", "xcode-select --install")),
        ("cargo (Rust)", which("cargo").is_some(), "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   (ku source check --install does this)".into()),
        ("C compiler", cc.is_some(), format!("{}   (or, without sudo: install zig and export CC=\"zig cc\")",
            pkg("sudo apt install build-essential", "sudo dnf install gcc", "sudo pacman -S base-devel", "xcode-select --install"))),
        ("tmux", which("tmux").is_some(), pkg("sudo apt install tmux", "sudo dnf install tmux", "sudo pacman -S tmux", "brew install tmux")),
    ]
}

fn cargo_bin() -> Option<PathBuf> {
    which("cargo")
}

pub async fn cli(paths: &Paths, args: &[String]) -> Result<()> {
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let src = src_dir(paths);
    match args.get(2).map(String::as_str).unwrap_or("status") {
        "status" => {
            println!("kurisu {}", version_line());
            println!("{}", rebuild_note());
            if src.join(".git").exists() {
                let branch = run(Command::new("git").arg("-C").arg(&src).args(["rev-parse", "--abbrev-ref", "HEAD"])).unwrap_or_default();
                let changes = run(Command::new("git").arg("-C").arg(&src).args(["status", "--short"])).unwrap_or_default();
                println!("\nsource: {} (branch {branch}, {})", src.display(), if changes.is_empty() { "no uncommitted edits".to_string() } else { format!("{} changed files", changes.lines().count()) });
            } else {
                println!("\nsource: not cloned yet (ku source get)");
            }
            for f in ["ku.prev", "ku.orig"] {
                let p = bin_dir(paths).join(f);
                if p.exists() {
                    println!("kept: {}", p.display());
                }
            }
            Ok(())
        }
        "check" => {
            let mut missing = 0;
            for (name, ok, how) in prerequisites() {
                println!("{} {name}{}", if ok { "✅" } else { "❌" }, if ok { String::new() } else { format!(" — install: {how}") });
                missing += usize::from(!ok);
            }
            if args.iter().any(|a| a == "--install") && cargo_bin().is_none() {
                if confirm("Install Rust with rustup into your home folder (no sudo)?", yes) {
                    let st = Command::new("sh").arg("-c").arg("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal").status()?;
                    if !st.success() {
                        bail!("rustup install failed");
                    }
                    println!("Rust installed (~/.cargo/bin).");
                    missing -= 1;
                }
            }
            if missing > 0 {
                println!("\n{missing} missing. The system packages (C compiler, git, tmux) need your package manager; Kurisu prints the command but does not run sudo.");
            } else {
                println!("\nready to build: ku source build");
            }
            Ok(())
        }
        "get" => {
            if which("git").is_none() {
                bail!("git is not installed (ku source check)");
            }
            if src.join(".git").exists() {
                println!("already cloned: {} (ku source status)", src.display());
                return Ok(());
            }
            if COMMIT == "unknown" {
                bail!("this binary does not know its commit; clone {REPO} yourself");
            }
            std::fs::create_dir_all(&paths.home)?;
            println!("cloning {REPO} into {} ...", src.display());
            run(Command::new("git").args(["clone", "--quiet", REPO]).arg(&src))?;
            let branch = format!("self/{}", chrono::Local::now().format("%Y%m%d-%H%M"));
            run(Command::new("git").arg("-C").arg(&src).args(["checkout", "--quiet", "-b", &branch, COMMIT]))
                .context("that commit is not in the repo (a local build?); the clone is there, check out a branch yourself")?;
            println!("on branch {branch} at {}: edit away, then ku source build && ku source install", short(COMMIT));
            Ok(())
        }
        "build" => {
            if !src.join("Cargo.toml").exists() {
                bail!("no source yet: ku source get");
            }
            let cargo = cargo_bin().context("cargo is not installed: ku source check --install")?;
            println!("building (release) in {} ...", src.display());
            let st = Command::new(cargo).current_dir(&src).args(["build", "--release", "-p", "kurisu"]).status()?;
            if !st.success() {
                bail!("the build failed; fix it, or `ku source reset` to go back to the shipped commit");
            }
            println!("built {}: ku source install", src.join("target/release/ku").display());
            Ok(())
        }
        "install" => install(paths, &src.join("target/release/ku"), yes).await,
        "rollback" => {
            let prev = bin_dir(paths).join("ku.prev");
            if !prev.exists() {
                bail!("no previous binary kept");
            }
            // Copy first: when this *is* ku.prev, swap_in reads it while replacing the target.
            let tmp = bin_dir(paths).join("ku.rollback");
            std::fs::copy(&prev, &tmp)?;
            swap_in(paths, &tmp, false).await?;
            let _ = std::fs::remove_file(&tmp);
            println!("restored the previous binary");
            Ok(())
        }
        "reset" => {
            if !confirm("Go back to the shipped binary and the shipped source? (self-edits stay on their branch)", yes) {
                return Ok(());
            }
            let orig = bin_dir(paths).join("ku.orig");
            if orig.exists() {
                let tmp = bin_dir(paths).join("ku.reset");
                std::fs::copy(&orig, &tmp)?;
                swap_in(paths, &tmp, false).await?;
                let _ = std::fs::remove_file(&tmp);
                println!("restored the original binary");
            }
            if src.join(".git").exists() {
                let _ = run(Command::new("git").arg("-C").arg(&src).args(["add", "-A"]));
                let _ = run(Command::new("git").arg("-C").arg(&src).args(["commit", "--quiet", "-m", "ku source reset: keep self-edits"]));
                let _ = run(Command::new("git").arg("-C").arg(&src).args(["fetch", "--quiet", "origin"]));
                let branch = format!("self/reset-{}", chrono::Local::now().format("%Y%m%d-%H%M"));
                run(Command::new("git").arg("-C").arg(&src).args(["checkout", "--quiet", "-b", &branch, COMMIT]))?;
                println!("source back at {} on {branch}; earlier edits are kept on their branches", short(COMMIT));
            }
            Ok(())
        }
        _ => bail!("usage: kurisu source [status] | check [--install] | get | build | install | rollback | reset   (-y: don't ask)"),
    }
}

/// Install a freshly built binary: check it runs, keep the current one, swap, restart the
/// server on it, and fall back if that server does not come up.
async fn install(paths: &Paths, new: &Path, yes: bool) -> Result<()> {
    if !new.is_file() {
        bail!("no built binary at {} (ku source build)", new.display());
    }
    let out = Command::new(new).arg("--version").output().context("run the new binary")?;
    if !out.status.success() {
        bail!("the new binary does not run (--version failed); nothing was changed");
    }
    println!("new: {}", String::from_utf8_lossy(&out.stdout).trim());
    if !confirm("Install it in place of the running ku (the current one is kept)?", yes) {
        return Ok(());
    }
    swap_in(paths, new, true).await
}

/// The installed `ku` to replace: recorded by the first install (so a kept binary such as
/// ~/.kurisu/bin/ku.prev can restore it), else the running binary.
fn install_target(paths: &Paths) -> Result<PathBuf> {
    if let Some(p) = std::fs::read_to_string(bin_dir(paths).join("target")).ok().map(|t| PathBuf::from(t.trim())).filter(|p| p.is_file()) {
        return Ok(p);
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.starts_with(bin_dir(paths)) {
        bail!("run this with your installed ku (this is a kept copy and no install path is recorded)");
    }
    Ok(exe)
}

async fn swap_in(paths: &Paths, new: &Path, keep_current: bool) -> Result<()> {
    let exe = install_target(paths)?;
    let bin = bin_dir(paths);
    std::fs::create_dir_all(&bin)?;
    std::fs::write(bin.join("target"), exe.to_string_lossy().as_bytes())?;
    if keep_current {
        if !bin.join("ku.orig").exists() {
            std::fs::copy(&exe, bin.join("ku.orig"))?;
        }
        std::fs::copy(&exe, bin.join("ku.prev"))?;
    }
    replace(&exe, new)?;
    // Restart the server on the new binary; fall back if it does not answer.
    let sock = paths.socket();
    if tokio::net::UnixStream::connect(&sock).await.is_ok() {
        let _ = crate::server::call(&sock, serde_json::json!({"op": "shutdown"})).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let started = Command::new(&exe).arg("status").env("KURISU_HOME", &paths.home).output();
    let ok = started.as_ref().is_ok_and(|o| o.status.success()) && {
        let mut up = false;
        for _ in 0..20 {
            if crate::server::call(&sock, serde_json::json!({"op": "ping"})).await.is_ok() {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        up || Command::new(&exe).arg("--version").output().is_ok_and(|o| o.status.success())
    };
    if !ok && keep_current {
        replace(&exe, &bin.join("ku.prev"))?;
        bail!("the new binary failed to start; the previous one is back in place");
    }
    println!("installed {} (previous kept in {})", exe.display(), bin.display());
    Ok(())
}

/// Atomically replace `exe` with a copy of `from` (copy next to it, then rename).
fn replace(exe: &Path, from: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = exe.with_extension("new");
    std::fs::copy(from, &tmp).with_context(|| format!("copy to {} (is that folder writable?)", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, exe)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_is_stamped() {
        assert!(!COMMIT.is_empty() && REPO.starts_with("https://"));
        assert!(version_line().contains(short(COMMIT)));
        assert!(rebuild_note().contains(COMMIT) && rebuild_note().contains("cargo install --path crates/kurisu"));
        let names: Vec<&str> = prerequisites().iter().map(|p| p.0).collect();
        assert_eq!(names, ["git", "cargo (Rust)", "C compiler", "tmux"]);
    }

    #[test]
    fn replace_is_atomic_copy() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("ku"), d.path().join("new"));
        std::fs::write(&a, b"old").unwrap();
        std::fs::write(&b, b"new").unwrap();
        replace(&a, &b).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"new");
        assert!(b.exists());
    }
}
