//! Per-turn snapshots of a session's folder, for `/undo`.
//!
//! Before each turn Kurisu records the folder's files in a git repository of its
//! own (`<home>/undo/<session>.git`, with the folder as its work tree), so the
//! session's folder and any `.git` in it are never touched. Each snapshot is a
//! commit under `refs/kurisu/turns/<seq>`, where `seq` is the history row of the
//! message that started the turn. `restore` puts the folder back to one snapshot:
//! files changed or deleted since are rewritten, files created since are removed.
//!
//! Not covered: files ignored by the folder's `.gitignore` or by the default
//! excludes below, anything outside the folder, and nested git repositories
//! (their contents are not snapshotted and a repository created by the turn is
//! left in place).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

/// Turn snapshots kept per session; older ones are dropped.
pub const KEEP: usize = 20;
/// A snapshot or restore that takes longer is abandoned (a huge folder).
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Never snapshotted: build output and caches that are large and regenerable.
const EXCLUDES: &str = "node_modules/\ntarget/\n.venv/\nvenv/\n__pycache__/\n.cache/\n";

pub struct Snapshots {
    git_dir: PathBuf,
    work: PathBuf,
}

/// What `restore` changed, as paths relative to the folder.
#[derive(Debug, Default, PartialEq)]
pub struct Restored {
    /// Rewritten to their snapshot content (changed during the turn).
    pub modified: Vec<String>,
    /// Recreated (deleted during the turn).
    pub recreated: Vec<String>,
    /// Removed (created during the turn).
    pub removed: Vec<String>,
    /// Left alone: a nested git repository created during the turn.
    pub skipped: Vec<String>,
}

impl Restored {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.recreated.is_empty() && self.removed.is_empty() && self.skipped.is_empty()
    }
}

/// Folders too broad to snapshot: the filesystem root and the home directory.
pub fn too_broad(dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if dir.parent().is_none() {
        return true;
    }
    std::env::var_os("HOME").map(PathBuf::from).and_then(|h| h.canonicalize().ok()).is_some_and(|h| h == dir)
}

impl Snapshots {
    pub fn new(home: &Path, session: &str, work: &Path) -> Self {
        Snapshots { git_dir: home.join("undo").join(format!("{session}.git")), work: work.to_path_buf() }
    }

    /// Drop every snapshot of a session (its folder is unaffected).
    pub fn remove_all(home: &Path, session: &str) {
        let _ = std::fs::remove_dir_all(home.join("undo").join(format!("{session}.git")));
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        self.run(args, true).await
    }

    async fn run(&self, args: &[&str], work_tree: bool) -> Result<String> {
        let mut cmd = Command::new("git");
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env("GIT_DIR", &self.git_dir);
        if work_tree {
            cmd.env("GIT_WORK_TREE", &self.work);
        }
        cmd.env("GIT_AUTHOR_NAME", "kurisu").env("GIT_AUTHOR_EMAIL", "kurisu@localhost")
            .env("GIT_COMMITTER_NAME", "kurisu").env("GIT_COMMITTER_EMAIL", "kurisu@localhost")
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["-c", "core.autocrlf=false", "-c", "core.quotepath=off", "-c", "gc.auto=0"])
            .args(args)
            .current_dir(&self.work)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await
            .map_err(|_| anyhow!("git {} timed out after {}s", args[0], GIT_TIMEOUT.as_secs()))?
            .context("run git")?;
        if !out.status.success() {
            bail!("git {} failed: {}", args[0], String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn init(&self) -> Result<()> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.git_dir)?;
        self.run(&["init", "--quiet", "--bare"], false).await?;
        // The work tree comes from GIT_WORK_TREE on every later call.
        self.run(&["config", "core.bare", "false"], false).await?;
        std::fs::create_dir_all(self.git_dir.join("info"))?;
        std::fs::write(self.git_dir.join("info").join("exclude"), EXCLUDES)?;
        Ok(())
    }

    /// The folder's current files as a tree id.
    async fn tree(&self) -> Result<String> {
        self.init().await?;
        self.git(&["add", "-A", "--", "."]).await?;
        Ok(self.git(&["write-tree"]).await?.trim().to_string())
    }

    /// Record the folder before the turn started by history row `seq`.
    pub async fn snapshot(&self, seq: i64) -> Result<()> {
        if !self.work.is_dir() {
            bail!("{} is not a folder", self.work.display());
        }
        if too_broad(&self.work) {
            bail!("{} is too broad to snapshot", self.work.display());
        }
        let tree = self.tree().await?;
        let commit = self.git(&["commit-tree", &tree, "-m", &format!("before turn {seq}")]).await?;
        self.git(&["update-ref", &format!("refs/kurisu/turns/{seq}"), commit.trim()]).await?;
        let old = self.turns().await?;
        if old.len() > KEEP {
            for s in &old[..old.len() - KEEP] {
                self.git(&["update-ref", "-d", &format!("refs/kurisu/turns/{s}")]).await?;
            }
        }
        Ok(())
    }

    /// Snapshotted turns, oldest first.
    pub async fn turns(&self) -> Result<Vec<i64>> {
        if !self.git_dir.join("HEAD").exists() {
            return Ok(vec![]);
        }
        let out = self.git(&["for-each-ref", "--format=%(refname)", "refs/kurisu/turns/"]).await?;
        let mut v: Vec<i64> = out.lines().filter_map(|l| l.rsplit('/').next()?.parse().ok()).collect();
        v.sort_unstable();
        Ok(v)
    }

    /// Forget snapshots of turns at or after `seq` (they were undone).
    pub async fn drop_from(&self, seq: i64) -> Result<()> {
        for s in self.turns().await?.into_iter().filter(|s| *s >= seq) {
            self.git(&["update-ref", "-d", &format!("refs/kurisu/turns/{s}")]).await?;
        }
        Ok(())
    }

    /// Put the folder back to the snapshot taken before turn `seq`.
    pub async fn restore(&self, seq: i64) -> Result<Restored> {
        let target = self.git(&["rev-parse", "--verify", "--quiet", &format!("refs/kurisu/turns/{seq}^{{tree}}")]).await
            .map_err(|_| anyhow!("no snapshot for turn {seq}"))?;
        let target = target.trim().to_string();
        let current = self.tree().await?;
        let mut r = Restored::default();
        // `--raw -z`: ":<old mode> <new mode> <old> <new> <status>\0<path>\0" per entry.
        let raw = self.git(&["diff-tree", "-r", "--raw", "-z", "--no-renames", &target, &current]).await?;
        let mut parts = raw.split('\0');
        while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
            let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            if f.len() < 5 {
                continue;
            }
            let (old_mode, new_mode, status) = (f[0], f[1], f[4]);
            let path = path.to_string();
            if old_mode == "160000" || new_mode == "160000" {
                r.skipped.push(path);
                continue;
            }
            match status {
                "A" => r.removed.push(path),
                "D" => r.recreated.push(path),
                _ => r.modified.push(path),
            }
        }
        for p in &r.removed {
            let full = self.work.join(p);
            if std::fs::symlink_metadata(&full).is_ok() {
                std::fs::remove_file(&full).with_context(|| format!("remove {p}"))?;
            }
            // Remove directories the turn left empty.
            let mut dir = full.parent();
            while let Some(d) = dir {
                if d == self.work || std::fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
        if !(r.modified.is_empty() && r.recreated.is_empty()) {
            self.git(&["read-tree", &target]).await?;
            let mut args = vec!["checkout-index", "-f", "--"];
            args.extend(r.modified.iter().chain(&r.recreated).map(String::as_str));
            self.git(&args).await?;
        }
        // The index now matches the folder again.
        self.git(&["read-tree", &target]).await?;
        Ok(r)
    }
}

/// One line per kind of change, for the user's /undo reply.
pub fn describe(r: &Restored) -> String {
    fn list(v: &[String]) -> String {
        let mut s = v.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
        if v.len() > 8 {
            s.push_str(&format!(" and {} more", v.len() - 8));
        }
        s
    }
    if r.is_empty() {
        return "no file in the session folder had changed".into();
    }
    let mut parts = vec![];
    if !r.modified.is_empty() { parts.push(format!("restored {}", list(&r.modified))); }
    if !r.recreated.is_empty() { parts.push(format!("recreated {}", list(&r.recreated))); }
    if !r.removed.is_empty() { parts.push(format!("removed {}", list(&r.removed))); }
    if !r.skipped.is_empty() { parts.push(format!("left nested git repo(s) in place: {}", list(&r.skipped))); }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new().prefix(prefix)
            .tempdir_in(std::env::var("KURISU_TEST_TMPDIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned()))
            .unwrap()
    }

    fn read(dir: &Path, p: &str) -> Option<String> {
        std::fs::read_to_string(dir.join(p)).ok()
    }

    #[tokio::test]
    async fn restore_undoes_edits_creations_and_deletions() {
        let (home, work) = (tmp("undo-home-"), tmp("undo-work-"));
        let w = work.path();
        std::fs::write(w.join("keep.txt"), "same").unwrap();
        std::fs::write(w.join("edit.txt"), "before").unwrap();
        std::fs::write(w.join("gone.txt"), "deleted later").unwrap();
        std::fs::create_dir_all(w.join("node_modules")).unwrap();
        std::fs::write(w.join("node_modules/big.js"), "x").unwrap();
        let s = Snapshots::new(home.path(), "s1", w);
        s.snapshot(10).await.unwrap();
        // The turn:
        std::fs::write(w.join("edit.txt"), "after").unwrap();
        std::fs::remove_file(w.join("gone.txt")).unwrap();
        std::fs::create_dir_all(w.join("new/deep")).unwrap();
        std::fs::write(w.join("new/deep/file.txt"), "created").unwrap();
        std::fs::write(w.join("node_modules/big.js"), "y").unwrap();
        let r = s.restore(10).await.unwrap();
        assert_eq!(r.modified, vec!["edit.txt"]);
        assert_eq!(r.recreated, vec!["gone.txt"]);
        assert_eq!(r.removed, vec!["new/deep/file.txt"]);
        assert_eq!(read(w, "edit.txt").as_deref(), Some("before"));
        assert_eq!(read(w, "gone.txt").as_deref(), Some("deleted later"));
        assert_eq!(read(w, "keep.txt").as_deref(), Some("same"));
        assert!(!w.join("new").exists(), "empty folders the turn created are removed");
        // Excluded folders are neither snapshotted nor restored.
        assert_eq!(read(w, "node_modules/big.js").as_deref(), Some("y"));
        // The folder holds no git data of Kurisu's.
        assert!(!w.join(".git").exists());
        assert!(describe(&r).contains("restored edit.txt"));
    }

    #[tokio::test]
    async fn snapshots_stack_and_are_dropped_after_undo() {
        let (home, work) = (tmp("undo-home-"), tmp("undo-work-"));
        let w = work.path();
        let s = Snapshots::new(home.path(), "s2", w);
        std::fs::write(w.join("a.txt"), "1").unwrap();
        s.snapshot(1).await.unwrap();
        std::fs::write(w.join("a.txt"), "2").unwrap();
        s.snapshot(5).await.unwrap();
        std::fs::write(w.join("a.txt"), "3").unwrap();
        assert_eq!(s.turns().await.unwrap(), vec![1, 5]);
        s.restore(5).await.unwrap();
        s.drop_from(5).await.unwrap();
        assert_eq!(read(w, "a.txt").as_deref(), Some("2"));
        assert_eq!(s.turns().await.unwrap(), vec![1]);
        s.restore(1).await.unwrap();
        assert_eq!(read(w, "a.txt").as_deref(), Some("1"));
        assert!(s.restore(5).await.is_err());
        // Nothing changed since: an empty result.
        assert!(s.restore(1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_folders_own_git_repo_and_gitignore_are_respected() {
        let (home, work) = (tmp("undo-home-"), tmp("undo-work-"));
        let w = work.path();
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(w).status().unwrap();
        assert!(ok.success());
        std::fs::write(w.join(".gitignore"), "secret.log\n").unwrap();
        std::fs::write(w.join("secret.log"), "keep me").unwrap();
        let head = std::fs::read(w.join(".git/HEAD")).unwrap();
        let s = Snapshots::new(home.path(), "s3", w);
        s.snapshot(2).await.unwrap();
        std::fs::write(w.join("secret.log"), "changed").unwrap();
        std::fs::write(w.join("x.txt"), "new").unwrap();
        let r = s.restore(2).await.unwrap();
        assert_eq!(r.removed, vec!["x.txt"]);
        assert_eq!(read(w, "secret.log").as_deref(), Some("changed"), "ignored files are left alone");
        assert_eq!(std::fs::read(w.join(".git/HEAD")).unwrap(), head);
        assert!(w.join(".git").is_dir());
    }

    #[tokio::test]
    async fn old_snapshots_are_pruned() {
        let (home, work) = (tmp("undo-home-"), tmp("undo-work-"));
        let s = Snapshots::new(home.path(), "s4", work.path());
        for i in 0..(KEEP as i64 + 3) {
            s.snapshot(i).await.unwrap();
        }
        let t = s.turns().await.unwrap();
        assert_eq!(t.len(), KEEP);
        assert_eq!(t[0], 3);
    }
}
