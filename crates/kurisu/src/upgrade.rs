//! `ku upgrade`: build and install a newer Kurisu from source.
//!
//! Downloadable release assets are not published, so this is the upgrade path that
//! works today: resolve a repo/ref to a commit, shallow-clone it into a temp dir
//! (never touching `~/.kurisu/src`, the developer's `ku source` tree), build it and
//! hand the result to `source::install`, which keeps `ku.prev` and restarts the
//! running server. A failed build changes nothing.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::paths::Paths;

/// Where `ku upgrade` looks by default.
pub const DEFAULT_REPO: &str = "getkurisu/kurisu";
pub const DEFAULT_REF: &str = "main";
pub const USAGE: &str = "usage: kurisu upgrade [--repo <owner/name|url>] [--ref <branch|tag|sha>] [--check] [--yes]";

#[derive(Clone, Debug, PartialEq)]
pub struct UpgradeArgs {
    pub owner: String,
    pub name: String,
    pub reference: String,
    pub check: bool,
    pub yes: bool,
}

impl UpgradeArgs {
    pub fn url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.name)
    }
    pub fn repo(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// Parse `ku upgrade` arguments (all of `args`, including argv[0] and "upgrade").
pub fn parse_args(args: &[String]) -> Result<UpgradeArgs> {
    let mut out = UpgradeArgs {
        owner: DEFAULT_REPO.split('/').next().unwrap_or("getkurisu").to_string(),
        name: DEFAULT_REPO.split('/').nth(1).unwrap_or("kurisu").to_string(),
        reference: DEFAULT_REF.to_string(),
        check: false,
        yes: false,
    };
    let mut rest = args.iter().skip(2);
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--repo" => {
                let v = rest.next().with_context(|| format!("--repo needs a value\n{USAGE}"))?;
                (out.owner, out.name) = repo_parts(v)?;
            }
            "--ref" => {
                out.reference = rest.next().with_context(|| format!("--ref needs a value\n{USAGE}"))?.clone();
            }
            "--check" => out.check = true,
            "--yes" | "-y" => out.yes = true,
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    if out.reference.trim().is_empty() {
        bail!("--ref must not be empty");
    }
    Ok(out)
}

/// `ku upgrade --help` and `-h` print the usage instead of failing on an unknown flag.
pub fn wants_help(args: &[String]) -> bool {
    args.iter().skip(2).any(|a| a == "--help" || a == "-h")
}

/// `owner/name`, `https://github.com/owner/name[.git]` or `git@github.com:owner/name.git`.
pub fn repo_parts(s: &str) -> Result<(String, String)> {
    let t = s.trim();
    let t = t.strip_prefix("git@github.com:").map(|r| format!("https://github.com/{r}")).unwrap_or_else(|| t.to_string());
    let t = t.strip_prefix("https://github.com/").or_else(|| t.strip_prefix("http://github.com/")).unwrap_or(&t);
    let t = t.strip_suffix(".git").unwrap_or(t).trim_end_matches('/');
    let mut parts = t.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        bail!("--repo must be an owner/name pair or a GitHub URL (for example {DEFAULT_REPO}), got {s:?}");
    };
    if owner.is_empty() || name.is_empty() || owner.contains(':') || name.contains(':') {
        bail!("--repo must be an owner/name pair or a GitHub URL (for example {DEFAULT_REPO}), got {s:?}");
    }
    Ok((owner.to_string(), name.to_string()))
}

/// A ref that names a commit directly, so `git ls-remote` cannot resolve it.
fn looks_like_sha(r: &str) -> bool {
    (7..=40).contains(&r.len()) && r.chars().all(|c| c.is_ascii_hexdigit())
}

/// Same commit, allowing the short sha a build can carry to match the full one.
pub fn same_commit(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim().to_ascii_lowercase(), b.trim().to_ascii_lowercase());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || (a.len() >= 7 && b.starts_with(&a)) || (b.len() >= 7 && a.starts_with(&b))
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("run {:?}", cmd.get_program()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{:?} failed: {}", cmd.get_program(), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Ask `git ls-remote` for the ref's commit; a sha is accepted as given.
pub fn resolve_remote(url: &str, reference: &str) -> Result<String> {
    let out = run(Command::new("git").args(["ls-remote", url, reference]))
        .with_context(|| format!("resolve {reference} in {url}"))?;
    if let Some(line) = out.lines().next() {
        if let Some(sha) = line.split_whitespace().next().filter(|s| looks_like_sha(s)) {
            return Ok(sha.to_ascii_lowercase());
        }
    }
    if looks_like_sha(reference) {
        return Ok(reference.to_ascii_lowercase());
    }
    bail!("no ref {reference:?} in {url} (a branch, tag or commit sha)");
}

/// Fetch just the commit into `dir` and check it out; shallow when the server allows it.
fn fetch_commit(dir: &Path, url: &str, sha: &str) -> Result<()> {
    run(Command::new("git").arg("init").arg("--quiet").arg(dir))?;
    run(Command::new("git").arg("-C").arg(dir).args(["remote", "add", "origin", url]))?;
    let shallow = Command::new("git")
        .arg("-C").arg(dir)
        .args(["fetch", "--quiet", "--depth", "1", "origin", sha])
        .output()?;
    if !shallow.status.success() {
        // Some servers refuse fetching an unreachable sha in a shallow clone: fall back.
        run(Command::new("git").arg("-C").arg(dir).args(["fetch", "--quiet", "origin"]))?;
    }
    run(Command::new("git").arg("-C").arg(dir).args(["checkout", "--quiet", sha]))?;
    Ok(())
}

/// A Kurisu checkout has a workspace Cargo.toml and a `kurisu` package in it.
fn validate_checkout(dir: &Path, repo: &str, reference: &str) -> Result<()> {
    if !dir.join("Cargo.toml").is_file() {
        bail!("{repo} at {reference} is not a Kurisu checkout (no Cargo.toml); nothing was built");
    }
    let cargo = crate::source::cargo_bin().context("cargo is not installed: ku source check --install")?;
    let out = Command::new(cargo)
        .current_dir(dir)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .stderr(Stdio::piped())
        .output()
        .context("read the checkout's Cargo metadata")?;
    if !out.status.success() {
        bail!("{repo} at {reference} has a Cargo.toml that cargo cannot read; nothing was built");
    }
    let meta: Value = serde_json::from_slice(&out.stdout).context("parse cargo metadata")?;
    let has_kurisu = meta["packages"].as_array().into_iter().flatten().any(|p| p["name"] == "kurisu");
    if !has_kurisu {
        bail!("{repo} at {reference} is not a Kurisu checkout (no `kurisu` package in the workspace); nothing was built");
    }
    Ok(())
}

/// Build in `dir`; on failure print the last lines cargo said and change nothing.
fn build(dir: &Path, repo_url: &str, sha: &str) -> Result<std::path::PathBuf> {
    let cargo = crate::source::cargo_bin().context("cargo is not installed: ku source check --install")?;
    println!("building {sha} (this can take a few minutes) ...");
    let out = Command::new(cargo)
        .current_dir(dir)
        .args(["build", "--release", "-p", "kurisu"])
        // The binary must record the commit and repo it was built from, whatever the caller's env says.
        .env("KURISU_BUILD_COMMIT", sha)
        .env("KURISU_REPO", repo_url)
        // A caller-wide target dir would put the binary somewhere else than the checkout.
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .context("run cargo build")?;
    if !out.status.success() {
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        for line in lines.iter().skip(lines.len().saturating_sub(20)) {
            eprintln!("{line}");
        }
        bail!("the build failed; the running ku and its source were not changed");
    }
    let bin = dir.join("target/release/ku");
    if !bin.is_file() {
        bail!("cargo succeeded but produced no {}; nothing was changed", bin.display());
    }
    Ok(bin)
}

/// A temp build folder that removes itself on drop. `tempfile` stays a test-only
/// dependency; the release binary only needs create_dir/remove_dir_all.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        for n in 0..100 {
            let p = base.join(format!("kurisu-upgrade-{pid}-{n}"));
            match std::fs::create_dir(&p) {
                Ok(()) => return Ok(Self(p)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("make a temporary build folder in {}", base.display())),
            }
        }
        bail!("could not make a temporary build folder in {}", base.display());
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn cli(paths: &Paths, args: &[String]) -> Result<()> {
    if wants_help(args) {
        println!("{USAGE}");
        return Ok(());
    }
    let opts = parse_args(args)?;
    let url = opts.url();
    if crate::stt::which_bin("git").is_none() {
        bail!("git is not installed (ku source check)");
    }
    let remote = resolve_remote(&url, &opts.reference)?;
    let local = crate::source::COMMIT;
    println!("local {local} -> remote {remote} ({})", opts.reference);
    if opts.check {
        return Ok(());
    }
    if same_commit(local, &remote) {
        println!("already on {remote}");
        return Ok(());
    }
    if !crate::source::confirm(
        &format!("Build and install {} @ {} in place of the running ku (the current one is kept)?", opts.repo(), opts.reference),
        opts.yes,
    ) {
        return Ok(());
    }
    let dir = TempDir::new()?;
    fetch_commit(dir.path(), &url, &remote)?;
    validate_checkout(dir.path(), &opts.repo(), &opts.reference)?;
    let bin = build(dir.path(), &url, &remote)?;
    // The user already confirmed above; install's own prompt would only ask twice.
    crate::source::install(paths, &bin, true).await?;
    println!(
        "upgraded {} -> {remote} ({} {}). The previous binary is kept: `ku source rollback` restores it.",
        crate::source::short_sha(local),
        opts.repo(),
        opts.reference
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(rest: &[&str]) -> Vec<String> {
        let mut v = vec!["ku".to_string(), "upgrade".to_string()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn defaults_are_getkurisu_main_and_builds() {
        let o = parse_args(&args(&[])).unwrap();
        assert_eq!((o.owner.as_str(), o.name.as_str()), ("getkurisu", "kurisu"));
        assert_eq!(o.reference, "main");
        assert!(!o.check && !o.yes);
        assert_eq!(o.url(), "https://github.com/getkurisu/kurisu.git");
    }

    #[test]
    fn flags_parse_in_any_order_and_do_not_swallow_values() {
        let o = parse_args(&args(&["--check", "--repo", "anslwy/kurisu", "--ref", "feat/x", "--yes"])).unwrap();
        assert_eq!(o.repo(), "anslwy/kurisu");
        assert_eq!(o.reference, "feat/x");
        assert!(o.check && o.yes);
        let o = parse_args(&args(&["--repo", "getkurisu/kurisu", "-y", "--check"])).unwrap();
        assert_eq!(o.repo(), "getkurisu/kurisu");
        assert!(o.check && o.yes);
    }

    #[test]
    fn repo_accepts_owner_name_urls_and_rejects_junk() {
        for s in ["getkurisu/kurisu", "https://github.com/getkurisu/kurisu", "https://github.com/getkurisu/kurisu.git",
                  "git@github.com:getkurisu/kurisu.git"] {
            assert_eq!(repo_parts(s).unwrap(), ("getkurisu".into(), "kurisu".into()), "{s}");
        }
        for s in ["kurisu", "a/b/c", "https://gitlab.com/a/b", ""] {
            let e = repo_parts(s).unwrap_err().to_string();
            assert!(e.contains("owner/name"), "{s}: {e}");
        }
    }

    #[test]
    fn sha_refs_resolve_without_ls_remote_and_compare_by_prefix() {
        assert!(looks_like_sha("c74f747232c28bce1d747c8b3a73964756a39386"));
        assert!(looks_like_sha("c74f747"));
        assert!(!looks_like_sha("main") && !looks_like_sha("v1.2") && !looks_like_sha("abc"));
        let e = resolve_remote("https://invalid.invalid/none.git", "c74f747232c28bce1d747c8b3a73964756a39386").unwrap_err().to_string();
        assert!(e.contains("resolve"), "{e}");
        assert!(same_commit("C74F747232C28BCE1D747C8B3A73964756A39386", "c74f747"));
        assert!(same_commit("c74f747", "c74f747232c28bce1d747c8b3a73964756a39386"));
        assert!(!same_commit("c74f747", "c74f748"));
        assert!(!same_commit("", "c74f747"));
    }

    #[test]
    fn help_is_not_an_unknown_flag() {
        assert!(wants_help(&args(&["--help"])) && wants_help(&args(&["-h"])));
        assert!(wants_help(&args(&["--repo", "getkurisu/kurisu", "--help"])));
        assert!(!wants_help(&args(&["--check"])));
        // The parser itself still rejects it when asked directly; cli() handles help first.
        assert!(parse_args(&args(&["--help"])).is_err());
    }

    #[test]
    fn unknown_arguments_and_missing_values_are_usage_errors() {
        assert!(parse_args(&args(&["--nope"])).unwrap_err().to_string().contains("unknown argument"));
        assert!(parse_args(&args(&["--repo"])).unwrap_err().to_string().contains("--repo needs a value"));
        assert!(parse_args(&args(&["--ref"])).unwrap_err().to_string().contains("--ref needs a value"));
        assert!(parse_args(&args(&["main"])).unwrap_err().to_string().contains("unknown argument"));
    }
}
