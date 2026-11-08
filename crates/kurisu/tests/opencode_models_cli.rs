//! Runtime proof for `ku models opencode`: a real server on an isolated home,
//! talking to the `opencode models` fixture (no network, no provider, no auth).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn fixture(name: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures").join(name);
    assert!(p.is_file(), "fixture missing at {}", p.display());
    p
}

/// Keep the unix socket path under the 100-byte limit. `/tmp` is the default;
/// KURISU_TEST_TMPDIR lets a machine with a tiny tmpfs point this at a bigger
/// disk while keeping the same isolation.
fn short_tempdir() -> tempfile::TempDir {
    let base = std::env::var_os("KURISU_TEST_TMPDIR").map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = tempfile::Builder::new().prefix("kt-").tempdir_in(&base).unwrap();
    let sock = dir.path().join("home").join("kurisu.sock");
    assert!(sock.as_os_str().len() < 100, "test home too long for a unix socket: {}", sock.display());
    dir
}

/// Kill the server on every exit path, including an assertion panic.
struct ServerProcess(Child);
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_server(home: &Path, fixtures: &Path, bin: &Path, extra: &[(&str, &str)]) -> ServerProcess {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ku"));
    cmd.arg("serve")
        .env("KURISU_HOME", home)
        .env("KURISU_OPENCODE_BIN", bin)
        .env("KURISU_FIXTURE_HOME", fixtures)
        .env("KURISU_OPENCODE_CONFIG", fixtures.join("no-opencode.json"))
        .env("KURISU_OPENCODE_MODELS_TTL_SECS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("start ku serve");
    let sock = home.join("kurisu.sock");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            return ServerProcess(child);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("ku serve did not listen on {}", sock.display());
}

fn ku_models(home: &Path) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_ku"))
        .args(["models", "opencode"])
        .env("KURISU_HOME", home)
        .output()
        .expect("run ku models opencode");
    assert!(out.status.success(), "ku models failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("ku models prints JSON")
}

fn ids(v: &serde_json::Value) -> Vec<String> {
    v["models"].as_array().into_iter().flatten()
        .filter_map(|m| m["id"].as_str().map(String::from)).collect()
}

#[test]
fn ku_models_opencode_reads_the_cli_and_refreshes_without_restart() {
    let dir = short_tempdir();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(dir.path().join("fixture-models.txt"),
                   "fixture/cli-alpha\nfixture/cli-novel-42\n").unwrap();
    let _server = start_server(&home, dir.path(), &fixture("opencode-models.py"), &[]);
    let v = ku_models(&home);
    assert_eq!(ids(&v), vec!["fixture/cli-alpha", "fixture/cli-novel-42"], "{v}");
    assert!(v["note"].as_str().unwrap().contains("opencode"), "{v}");
    // The server stays up; a provider/model added to opencode.json appears.
    std::fs::write(dir.path().join("fixture-models.txt"),
                   "fixture/cli-alpha\nfixture/cli-beta-43\n").unwrap();
    let v = ku_models(&home);
    assert!(ids(&v).contains(&"fixture/cli-beta-43".to_string()), "{v}");
}

#[test]
fn ku_models_opencode_falls_back_when_the_cli_fails() {
    let dir = short_tempdir();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let _server = start_server(&home, dir.path(), &fixture("opencode-models-fail.py"), &[]);
    let v = ku_models(&home);
    assert_eq!(ids(&v), vec!["default"], "{v}");
    let note = v["note"].as_str().unwrap();
    assert!(note.contains("Could not list OpenCode models") && note.contains("falling back"), "{v}");
}
