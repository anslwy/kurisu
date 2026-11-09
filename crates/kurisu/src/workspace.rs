//! Read-only workspace inspection shared by the token and paired-cookie APIs.
use std::{fs, io::Read, path::{Component, Path, PathBuf}, process::{Command, Stdio}};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const HARD_CAP: usize = 1024 * 1024;
const VIEW_CAP: usize = 512 * 1024;
const NOISY: &[&str] = &[".git", "node_modules", "target", "dist", ".svelte-kit"];

fn relative(path: &str) -> Result<&Path> {
    let p = Path::new(path);
    if p.is_absolute() || p.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        bail!("path refused: use a relative path without '..'");
    }
    Ok(p)
}

fn resolve(root: &Path, path: &str, missing: bool) -> Result<PathBuf> {
    let joined = root.join(relative(path)?);
    let mut existing = joined.as_path();
    while missing && !existing.exists() && existing.symlink_metadata().is_err() {
        existing = existing.parent().context("path has no existing parent")?;
    }
    let canonical = existing.canonicalize()?;
    if !canonical.starts_with(root) { bail!("path refused: outside session workspace"); }
    Ok(if existing == joined { canonical } else { joined })
}

/// Stream a file for download. Returns an Axum Response with appropriate headers.
pub(crate) async fn download_file(cwd: &str, path: &str) -> Result<axum::response::Response> {
    use axum::{
        body::Body,
        http::{header, StatusCode},
        response::IntoResponse,
    };
    use tokio::fs::File;
    use tokio_util::io::ReaderStream;

    let root = Path::new(cwd).canonicalize().context("cannot open session workspace")?;
    let resolved = resolve(&root, path, false)?;
    let meta = fs::metadata(&resolved).context("cannot stat file")?;
    if meta.is_dir() {
        bail!("cannot download a directory");
    }
    if !meta.is_file() {
        bail!("not a regular file");
    }
    let len = meta.len();
    let file = File::open(&resolved).await.context("cannot open file for download")?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    let filename = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("download");
    let content_type = content_type_for(&resolved);
    let mut headers = header::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert(header::CONTENT_LENGTH, len.to_string().parse().unwrap());
    headers.insert(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", filename).parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok((StatusCode::OK, headers, body).into_response())
}

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "gif" => "image/gif",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "md" | "txt" | "json" | "rs" | "ts" | "toml" | "yaml" | "yml" | "html" | "css" | "js" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn read(root: &Path, path: &str) -> Result<Value> {
    let path = resolve(root, path, false)?;
    if !fs::metadata(&path)?.is_file() { bail!("not a regular file"); }
    let file = fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() { bail!("not a regular file"); }
    let size = meta.len();
    if size > VIEW_CAP as u64 { return Ok(json!({"kind":"large", "size":size})); }
    let mut bytes = Vec::new();
    file.take((HARD_CAP + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > VIEW_CAP { return Ok(json!({"kind":"large", "size":bytes.len()})); }
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(json!({"kind":"text", "size":size, "text":text})),
        _ => Ok(json!({"kind":"binary", "size":size})),
    }
}

// Never invoke a shell, external diff, textconv, pager, network, or optional index writes.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = Command::new("git").current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0").env("GIT_TERMINAL_PROMPT", "0")
        .args(["--no-pager", "--literal-pathspecs", "-c", "core.fsmonitor=false"])
        .args(args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut bytes = Vec::new();
    child.stdout.take().context("missing git output")?.take((HARD_CAP + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > HARD_CAP {
        let _ = child.kill(); let _ = child.wait(); bail!("git output exceeds 1 MB");
    }
    if !child.wait()?.success() { bail!("git command failed: workspace may not be a Git repository"); }
    Ok(bytes)
}

fn status(root: &Path) -> Result<Value> {
    let prefix = String::from_utf8(git(root, &["rev-parse", "--show-prefix"])?)?;
    let prefix = prefix.trim_end_matches('\n');
    let bytes = git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=all", "--", "."])?;
    let mut parts = bytes.split(|b| *b == 0).filter(|p| !p.is_empty());
    let mut entries = Vec::new();
    while let Some(row) = parts.next() {
        if row.len() < 4 { continue; }
        let code = std::str::from_utf8(&row[..2])?;
        let name = std::str::from_utf8(&row[3..])?;
        if code.contains('R') || code.contains('C') { parts.next(); }
        let Some(path) = name.strip_prefix(prefix) else { continue; };
        // Status names are untrusted too; never expose paths outside the workspace.
        if relative(path).is_err() { continue; }
        entries.push(json!({"path":path, "status":code.trim()}));
    }
    Ok(json!({"entries":entries}))
}

pub(crate) fn inspect(cwd: &str, action: &str, path: &str, hidden: bool) -> Result<Value> {
    let root = Path::new(cwd).canonicalize().context("cannot open session workspace")?;
    match action {
        "list-dir" => {
            let dir = resolve(&root, path, false)?;
            let mut entries = Vec::new();
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue; };
                if !hidden && NOISY.contains(&name.as_str()) { continue; }
                let entry_path = Path::new(path).join(&name).to_string_lossy().into_owned();
                let Ok(canonical) = resolve(&root, &entry_path, false) else { continue; };
                let meta = fs::metadata(canonical)?;
                if !meta.is_dir() && !meta.is_file() { continue; }
                entries.push(json!({"name":name,"path":entry_path,"directory":meta.is_dir()}));
                if entries.len() > 10000 { bail!("directory has more than 10000 entries"); }
            }
            entries.sort_by(|a,b| b["directory"].as_bool().cmp(&a["directory"].as_bool())
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str())));
            Ok(json!({"entries":entries}))
        }
        "read-file" => read(&root, path),
        "git-status" => status(&root),
        "git-diff" => {
            resolve(&root, path, true)?;
            let changes = status(&root)?;
            let entry = changes["entries"].as_array().unwrap().iter().find(|e| e["path"] == path).context("file is not changed")?;
            let code = entry["status"].as_str().unwrap_or("");
            if code == "??" { return read(&root, path); }
            if code.contains('D') { return Ok(json!({"kind":"text","text":"Deleted file"})); }
            // HEAD includes staged changes. An unborn repository has only the index diff.
            let head = git(&root, &["rev-parse", "--verify", "HEAD"]).is_ok();
            let args = if head { vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color", "HEAD", "--", path] }
                else { vec!["diff", "--cached", "--no-ext-diff", "--no-textconv", "--no-color", "--", path] };
            let bytes = git(&root, &args)?;
            if bytes.len() > VIEW_CAP { return Ok(json!({"kind":"large", "size":bytes.len()})); }
            Ok(json!({"kind":"text","text":String::from_utf8_lossy(&bytes)}))
        }
        _ => bail!("unknown workspace action"),
    }
}

pub(crate) async fn response(cwd: String, action: String, path: String, hidden: bool) -> axum::response::Response {
    use axum::response::IntoResponse;
    let result = tokio::task::spawn_blocking(move || inspect(&cwd, &action, &path, hidden)).await;
    let mut response = match result {
        Ok(Ok(value)) => axum::Json(json!({"ok":value})).into_response(),
        other => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error":match other {
            Ok(Err(e)) => e.to_string(), Err(e) => e.to_string(), _ => unreachable!(),
        }}))).into_response(),
    };
    response.headers_mut().insert(axum::http::header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("workspace-").tempdir_in(std::env::var("KURISU_TEST_TMPDIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned())).unwrap()
    }
    #[test]
    fn confinement_and_file_limits() {
        let dir = fixture(); let root = dir.path(); let cwd = root.to_str().unwrap();
        fs::write(root.join("text"), "hello\nworld").unwrap();
        fs::write(root.join("binary"), [0, 1, 2]).unwrap();
        fs::write(root.join("large"), vec![b'x'; VIEW_CAP + 1]).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        assert_eq!(inspect(cwd,"read-file","text",false).unwrap()["text"], "hello\nworld");
        assert_eq!(inspect(cwd,"read-file","binary",false).unwrap()["kind"], "binary");
        assert_eq!(inspect(cwd,"read-file","large",false).unwrap()["kind"], "large");
        for path in ["../outside", "/etc/passwd", "a/../../etc/passwd"] { assert!(inspect(cwd,"read-file",path,false).is_err()); }
        #[cfg(unix)] {
            std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
            assert!(inspect(cwd,"read-file","escape/passwd",false).is_err());
            assert!(inspect(cwd,"git-diff","escape/missing",false).is_err());
        }
        let list = inspect(cwd,"list-dir","",false).unwrap();
        assert!(!list["entries"].as_array().unwrap().iter().any(|e| e["name"] == "target" || e["name"] == "escape"));
        assert!(inspect(cwd,"list-dir","",true).unwrap()["entries"].as_array().unwrap().iter().any(|e| e["name"] == "target"));
    }
    #[test]
    fn git_changes_staged_untracked_deleted_and_literal_names() {
        let dir = fixture(); let root = dir.path(); let cwd = root.to_str().unwrap();
        git(root, &["init", "-q"]).unwrap();
        fs::write(root.join("tracked"), "before\n").unwrap();
        fs::write(root.join("gone"), "gone\n").unwrap();
        git(root, &["add", "."]).unwrap();
        git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "core.hooksPath=/dev/null", "commit", "-qm", "initial"]).unwrap();
        fs::write(root.join("tracked"), "after\n").unwrap();
        git(root, &["add", "tracked"]).unwrap();
        fs::remove_file(root.join("gone")).unwrap();
        fs::write(root.join("[new]\nfile"), "untracked\n").unwrap();
        assert_eq!(status(root).unwrap()["entries"].as_array().unwrap().len(), 3);
        assert!(inspect(cwd,"git-diff","tracked",false).unwrap()["text"].as_str().unwrap().contains("+after"));
        assert_eq!(inspect(cwd,"git-diff","gone",false).unwrap()["text"], "Deleted file");
        assert_eq!(inspect(cwd,"git-diff","[new]\nfile",false).unwrap()["text"], "untracked\n");
        assert!(inspect(cwd,"git-diff","../outside",false).is_err());
    }

    #[tokio::test]
    async fn download_refuses_traversal_and_absolute_paths() {
        let dir = fixture(); let root = dir.path(); let cwd = root.to_str().unwrap();
        fs::write(root.join("file.txt"), "data").unwrap();
        assert!(download_file(cwd, "../Cargo.toml").await.is_err());
        assert!(download_file(cwd, "/etc/passwd").await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_refuses_symlink_outside_workspace() {
        let dir = fixture(); let root = dir.path(); let cwd = root.to_str().unwrap();
        std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
        assert!(download_file(cwd, "escape/passwd").await.is_err());
    }

    #[tokio::test]
    async fn download_refuses_directories() {
        let dir = fixture(); let root = dir.path(); let cwd = root.to_str().unwrap();
        fs::create_dir(root.join("subdir")).unwrap();
        match download_file(cwd, "subdir").await {
            Ok(_) => panic!("directory download should be refused"),
            Err(e) => assert!(e.to_string().contains("directory"), "unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn download_sets_attachment_headers_for_a_regular_file() {
        let dir = fixture(); let root = dir.path(); let cwd = root.to_str().unwrap();
        fs::write(root.join("notes.txt"), "hello").unwrap();
        let resp = download_file(cwd, "notes.txt").await.unwrap();
        let h = resp.headers();
        assert_eq!(h.get(axum::http::header::CONTENT_DISPOSITION).unwrap(), "attachment; filename=\"notes.txt\"");
        assert_eq!(h.get(axum::http::header::CONTENT_TYPE).unwrap(), "text/plain; charset=utf-8");
        assert_eq!(h.get(axum::http::header::CONTENT_LENGTH).unwrap(), "5");
    }

    #[tokio::test]
    async fn download_uses_image_content_type_for_png() {
        let dir = fixture(); let root = dir.path(); let cwd = root.to_str().unwrap();
        fs::write(root.join("pic.png"), b"png").unwrap();
        let resp = download_file(cwd, "pic.png").await.unwrap();
        assert_eq!(resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(), "image/png");
    }
}
