// Rebuild when the embedded web UI changes (include_dir! alone does not track it).
fn main() {
    // Where this binary came from: the repo and exact commit (release CI can set
    // KURISU_BUILD_COMMIT / KURISU_REPO), so a user (or the Lead) can rebuild it.
    println!("cargo:rerun-if-env-changed=KURISU_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=KURISU_REPO");
    let git = |args: &[&str]| std::process::Command::new("git").args(args).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
    let commit = std::env::var("KURISU_BUILD_COMMIT").ok().filter(|c| !c.is_empty()).or_else(|| git(&["rev-parse", "HEAD"])).unwrap_or_else(|| "unknown".into());
    let dirty = std::env::var("KURISU_BUILD_COMMIT").is_err() && git(&["status", "--porcelain", "--untracked-files=no"]).is_some();
    let repo = std::env::var("KURISU_REPO").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| "https://github.com/anslwy/kurisu".into());
    println!("cargo:rustc-env=KURISU_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=KURISU_BUILD_DIRTY={}", if dirty { "1" } else { "" });
    println!("cargo:rustc-env=KURISU_REPO={repo}");
    println!("cargo:rustc-env=KURISU_TARGET={}", std::env::var("TARGET").unwrap_or_default());
    println!("cargo:rustc-env=KURISU_PROFILE={}", std::env::var("PROFILE").unwrap_or_default());
    // Unix UTC seconds: available on every build host without another dependency.
    let built = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    println!("cargo:rustc-env=KURISU_BUILT={built}");
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(r) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={r}/refs/heads");
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=web/dist");
    for entry in walk("web/dist") {
        println!("cargo:rerun-if-changed={entry}");
    }
}

fn walk(dir: &str) -> Vec<String> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p.to_string_lossy()));
            } else {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}
