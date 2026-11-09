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

use anyhow::{bail, Context, Result};

use crate::paths::Paths;

pub const REPO: &str = env!("KURISU_REPO");
pub const COMMIT: &str = env!("KURISU_BUILD_COMMIT");
pub const DIRTY: &str = env!("KURISU_BUILD_DIRTY");
pub const TARGET: &str = env!("KURISU_TARGET");

/// "0.0.1 (abc1234, x86_64-unknown-linux-musl)"; "+local changes" when built from a dirty tree.
pub fn version_line() -> String {
    format!("{} ({}{}, {})", env!("CARGO_PKG_VERSION"), short_sha(COMMIT), if DIRTY.is_empty() { "" } else { " +local changes" }, TARGET)
}

pub(crate) fn short_sha(c: &str) -> &str {
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
    crate::harness::resolve(bin)
}

pub(crate) fn confirm(question: &str, yes: bool) -> bool {
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

pub(crate) fn cargo_bin() -> Option<PathBuf> {
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
            println!("on branch {branch} at {}: edit away, then ku source build && ku source install", short_sha(COMMIT));
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
        "install" => {
            let from = crate::flag(args, "--from").map(PathBuf::from).unwrap_or_else(|| src.join("target/release/ku"));
            let target = crate::flag(args, "--target").map(PathBuf::from);
            install(paths, &from, target, yes).await
        }
        "rollback" => rollback(paths).await,
        "reset" => {
            if !confirm("Go back to the shipped binary and the shipped source? (self-edits stay on their branch)", yes) {
                return Ok(());
            }
            let orig = bin_dir(paths).join("ku.orig");
            if orig.exists() {
                let tmp = bin_dir(paths).join("ku.reset");
                std::fs::copy(&orig, &tmp)?;
                let target = install_target(paths)?;
                let opts = crate::deploy::InstallOpts::new(tmp.clone(), target, yes, false);
                let r = crate::deploy::install(paths, &opts).await;
                let _ = std::fs::remove_file(&tmp);
                r?;
                println!("restored the original binary");
            }
            if src.join(".git").exists() {
                let _ = run(Command::new("git").arg("-C").arg(&src).args(["add", "-A"]));
                let _ = run(Command::new("git").arg("-C").arg(&src).args(["commit", "--quiet", "-m", "ku source reset: keep self-edits"]));
                let _ = run(Command::new("git").arg("-C").arg(&src).args(["fetch", "--quiet", "origin"]));
                let branch = format!("self/reset-{}", chrono::Local::now().format("%Y%m%d-%H%M"));
                run(Command::new("git").arg("-C").arg(&src).args(["checkout", "--quiet", "-b", &branch, COMMIT]))?;
                println!("source back at {} on {branch}; earlier edits are kept on their branches", short_sha(COMMIT));
            }
            Ok(())
        }
        _ => bail!("usage: kurisu source [status] | check [--install] | get | build | install | rollback | reset   (-y: don't ask)"),
    }
}

/// Restore the binary kept by the last install (`ku.prev`).
pub(crate) async fn rollback(paths: &Paths) -> Result<()> {
    let prev = bin_dir(paths).join("ku.prev");
    if !prev.exists() {
        bail!("no previous binary kept");
    }
    // Copy first: when this *is* ku.prev, the swap reads it while replacing the target.
    let tmp = bin_dir(paths).join("ku.rollback");
    std::fs::copy(&prev, &tmp)?;
    let target = install_target(paths)?;
    let opts = crate::deploy::InstallOpts::new(tmp.clone(), target, true, false);
    let r = crate::deploy::install(paths, &opts).await;
    let _ = std::fs::remove_file(&tmp);
    r?;
    println!("restored the previous binary");
    Ok(())
}

/// Install a freshly built binary through the zero-downtime path: probe it in a shadow,
/// hand the socket over atomically, drain the old server, roll back by swapping back.
pub(crate) async fn install(paths: &Paths, new: &Path, target: Option<PathBuf>, yes: bool) -> Result<()> {
    if !new.is_file() {
        bail!("no built binary at {} (ku source build)", new.display());
    }
    let target = match target {
        Some(t) => t,
        None => install_target(paths)?,
    };
    let opts = crate::deploy::InstallOpts::new(new.to_path_buf(), target.clone(), yes, true);
    let report = crate::deploy::install(paths, &opts).await?;
    println!("installed {} (previous kept in {})", target.display(), bin_dir(paths).display());
    println!(
        "deployed {}: shadow {}ms, handover {}ms, {} session(s), drained: {}, telegram: {}",
        report.commit, report.shadow_ms, report.handover_ms, report.sessions, report.drained, report.telegram
    );
    Ok(())
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

/// Atomically replace `exe` with a copy of `from` (copy next to it, then rename).
#[cfg(test)]
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
        assert!(version_line().contains(short_sha(COMMIT)));
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
