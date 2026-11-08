//! Recycle an existing standalone web frontend without restarting the session
//! daemon. Environment values, especially the web token, never enter diagnostics.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use crate::paths::Paths;

pub struct Frontend {
    pid: u32,
    start: u64,
    env: BTreeMap<OsString, OsString>,
    cwd: PathBuf,
    addr: SocketAddr,
    old_assets: Vec<String>,
    expected: Value,
}

fn one_pid(pids: &[u32]) -> Result<Option<u32>> {
    match pids { [] => Ok(None), [pid] => Ok(Some(*pid)), _ => bail!("multiple standalone /web processes for this home; refusing recycle: pids {pids:?}") }
}
fn token(env: &BTreeMap<OsString, OsString>) -> Result<&str> {
    env.get(&OsString::from("KURISU_WEB_TOKEN")).and_then(|v| v.to_str()).filter(|v| v.len() >= 16)
        .context("standalone /web token missing or invalid; refusing to mint a replacement KURISU_WEB_TOKEN")
}
fn command_is_web(args: &[OsString]) -> bool {
    args.get(1).is_some_and(|a| a == "web") && args.iter().skip(2).any(|a| a == "--serve" || a == "--port" || a == "--host")
}
#[cfg(target_os="linux")]
fn identity(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((fields.first()?.chars().next()?, fields.get(19)?.parse().ok()?))
}
#[cfg(not(target_os="linux"))]
fn identity(_: u32) -> Option<(char,u64)> { None }
fn alive(pid: u32, start: u64) -> bool { identity(pid).is_some_and(|(state, ticks)| ticks == start && !matches!(state, 'Z'|'X')) }

#[cfg(target_os="linux")]
fn processes(paths: &Paths) -> Result<Vec<(u32, Vec<OsString>, BTreeMap<OsString,OsString>)>> {
    use std::os::unix::ffi::OsStringExt;
    let mut result = vec![];
    let wanted = paths.home.canonicalize().unwrap_or_else(|_|paths.home.clone());
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_str().and_then(|s|s.parse::<u32>().ok()) else { continue };
        let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let args: Vec<_> = bytes.split(|b| *b == 0).filter(|v|!v.is_empty()).map(|v|OsString::from_vec(v.to_vec())).collect();
        if !command_is_web(&args) { continue; }
        let raw = std::fs::read(entry.path().join("environ")).with_context(||format!("cannot inspect standalone /web environment for pid {pid}"))?;
        let env: BTreeMap<_,_> = raw.split(|b|*b==0).filter_map(|v| {
            let at=v.iter().position(|b|*b==b'=')?;
            Some((OsString::from_vec(v[..at].to_vec()),OsString::from_vec(v[at+1..].to_vec())))
        }).collect();
        let home = env.get(&OsString::from("KURISU_HOME")).map(PathBuf::from).unwrap_or_else(|| {
            let base = env.get(&OsString::from("XDG_DATA_HOME")).map(PathBuf::from)
                .unwrap_or_else(||PathBuf::from(env.get(&OsString::from("HOME")).cloned().unwrap_or_default()).join(".local/share"));
            base.join("kurisu")
        });
        if home.canonicalize().unwrap_or(home) == wanted && identity(pid).is_some_and(|(s,_)|!matches!(s,'Z'|'X')) {
            result.push((pid,args,env));
        }
    }
    result.sort_by_key(|v|v.0);
    Ok(result)
}
#[cfg(not(target_os="linux"))]
fn processes(paths: &Paths) -> Result<Vec<(u32,Vec<OsString>,BTreeMap<OsString,OsString>)>> {
    if paths.home.join("web.json").exists() { bail!("standalone /web recycle requires Linux process inspection on this platform"); }
    Ok(vec![])
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(2)).build()?)
}
fn local_addr(mut addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() { addr.set_ip(if addr.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() }); }
    addr
}
pub(crate) async fn fetch_assets(addr: SocketAddr) -> Result<Vec<String>> {
    let body = client()?.get(format!("http://{}/web",local_addr(addr))).header("cache-control","no-cache")
        .send().await?.error_for_status()?.text().await?;
    let assets=crate::web::asset_set(&body);
    if assets.is_empty() { bail!("standalone /web did not serve hashed assets"); }
    Ok(assets)
}
pub(crate) async fn fetch_stamp(addr: SocketAddr) -> Result<Value> {
    Ok(client()?.get(format!("http://{}/_ku/web-build",local_addr(addr))).send().await?.error_for_status()?.json().await?)
}
pub(crate) fn same_build(expected: &Value, actual: &Value) -> bool {
    ["git_sha","built","target","profile","dirty"].iter().all(|k|expected["build"][*k] == actual["build"][*k])
        && expected["assets"] == actual["assets"]
}
fn assets_match(expected: &Value, before: &[String], after: &[String]) -> Result<bool> {
    let wanted: Vec<String> = serde_json::from_value(expected["assets"].clone())?;
    if after != wanted { bail!("standalone /web asset verification failed: served asset set does not match candidate; old binary path remains intact"); }
    // A same-commit redeploy must still recycle the PID. Identical candidate
    // assets are valid only when they already matched before the recycle.
    Ok(before != after)
}

impl Frontend {
    pub async fn inspect(paths: &Paths, candidate: &Path) -> Result<Option<Self>> {
        let mut found=processes(paths)?;
        let Some(pid)=one_pid(&found.iter().map(|v|v.0).collect::<Vec<_>>())? else {
            println!("standalone /web: no process running; skipping recycle (no frontend spawned)");
            return Ok(None);
        };
        let (_,args,env)=found.remove(0);
        token(&env)?;
        let cfg: crate::remote::Config=serde_json::from_slice(&std::fs::read(paths.home.join("web.json"))?)?;
        let addr: SocketAddr=cfg.listen.parse()?;
        if let Some(port)=args.iter().position(|a|a=="--port").and_then(|i|args.get(i+1)).and_then(|s|s.to_str()).and_then(|s|s.parse::<u16>().ok()) {
            if port != 0 && port != addr.port() { bail!("standalone /web pid {pid} port differs from web.json; refusing recycle"); }
        }
        let old_assets=fetch_assets(addr).await.context("probe existing standalone /web before any swap")?;
        let output=Command::new(candidate).arg("--web-build").output()?;
        if !output.status.success() { bail!("candidate cannot report its embedded standalone /web build"); }
        let expected: Value=serde_json::from_slice(&output.stdout).context("candidate web build identity")?;
        let (_,start)=identity(pid).context("standalone /web process exited during preflight")?;
        let cwd=std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap_or_else(|_|paths.home.clone());
        println!("standalone /web: will recycle pid {pid} on {addr}; token preserved from process environment");
        Ok(Some(Self {pid,start,env,cwd,addr,old_assets,expected}))
    }

    fn spawn(&self, bin: &Path, paths: &Paths) -> Result<Child> {
        use std::os::unix::{fs::OpenOptionsExt,process::CommandExt};
        let log=std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(paths.home.join("web-recycle.log"))?;
        Ok(Command::new(bin).args(["web","--serve","--host",&self.addr.ip().to_string(),"--port",&self.addr.port().to_string()])
            .env_clear().envs(&self.env).current_dir(&self.cwd).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log)
            .process_group(0).spawn().context("start replacement standalone /web")?)
    }

    pub async fn recycle(&self, paths: &Paths, candidate: &Path) -> Result<()> {
        if !alive(self.pid,self.start) { bail!("standalone /web pid {} changed before recycle; refusing to signal another process",self.pid); }
        let backup=paths.home.join("bin").join(format!("web-prev-{}",self.pid));
        std::fs::create_dir_all(backup.parent().unwrap())?;
        std::fs::copy(format!("/proc/{}/exe",self.pid),&backup).context("keep previous frontend executable")?;
        // SIGTERM ends even a held SSE stream. EventSource clients reconnect;
        // the daemon and its session processes are never signalled here.
        if unsafe { libc::kill(self.pid as i32,libc::SIGTERM) } != 0 { return Err(std::io::Error::last_os_error().into()); }
        let deadline=Instant::now()+Duration::from_secs(5);
        while alive(self.pid,self.start) {
            if Instant::now()>=deadline { bail!("standalone /web pid {} did not exit after SIGTERM",self.pid); }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let exited=Instant::now();
        let mut child=match self.spawn(candidate,paths) {
            Ok(c)=>c,
            Err(e)=> { let _=self.spawn(&backup,paths); return Err(e); }
        };
        let result=async {
            let deadline=Instant::now()+Duration::from_secs(15);
            let (after,gap_ms)=loop {
                if child.try_wait()?.is_some() { bail!("replacement standalone /web exited before answering"); }
                if let Ok(after)=fetch_assets(self.addr).await { break (after,exited.elapsed().as_secs_f64()*1000.0); }
                if Instant::now()>=deadline { bail!("replacement standalone /web did not answer 200 within 15s"); }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            let changed=assets_match(&self.expected,&self.old_assets,&after)?;
            let stamp=fetch_stamp(self.addr).await.context("replacement frontend build endpoint")?;
            if !same_build(&self.expected,&stamp) || stamp["pid"].as_u64()!=Some(child.id() as u64) {
                bail!("standalone /web build verification failed: frontend is stale");
            }
            let auth=client()?.get(format!("http://{}/api/v1/sessions",local_addr(self.addr)))
                .bearer_auth(token(&self.env)?).send().await?;
            if auth.status()!=reqwest::StatusCode::OK { bail!("recycled standalone /web rejected the preserved token"); }
            let record=serde_json::json!({"old_pid":self.pid,"new_pid":child.id(),"ui_gap_ms":gap_ms,"asset_hash_swap":changed,
                "assets":after,"build":stamp["build"],"token_preserved":true});
            crate::telegram::write_private(&paths.home.join("web-recycle.json"),&serde_json::to_vec_pretty(&record)?)?;
            let pidfile=paths.home.parent().unwrap_or(&paths.home).join("kurisu.pid");
            if std::fs::read_to_string(&pidfile).ok().is_some_and(|s|s.trim()==self.pid.to_string()) {
                std::fs::write(pidfile,format!("{}\n",child.id()))?;
            }
            println!("standalone /web: recycled {} -> {}; UI gap {:.3}ms; asset-hash swap {}; candidate assets verified; token preserved",self.pid,child.id(),gap_ms,changed);
            Ok(())
        }.await;
        if let Err(e)=result {
            let _=child.kill(); let _=child.wait();
            let _=self.spawn(&backup,paths);
            eprintln!("standalone /web recycle failed: {e:#}; previous frontend restored; old binary path remains intact");
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_decision_refuses_multiple_and_skips_absent() {
        assert_eq!(one_pid(&[]).unwrap(),None);
        assert_eq!(one_pid(&[17]).unwrap(),Some(17));
        assert!(one_pid(&[17,29]).unwrap_err().to_string().contains("17, 29"));
        assert!(command_is_web(&["ku".into(),"web".into(),"--serve".into()]));
        assert!(!command_is_web(&["ku".into(),"serve".into()]));
    }
    #[test]
    fn exact_existing_token_is_preserved_missing_token_refused() {
        let mut env=BTreeMap::new();
        assert!(token(&env).is_err());
        env.insert("KURISU_WEB_TOKEN".into(),"operator-secret-token".into());
        assert_eq!(token(&env).unwrap(),"operator-secret-token");
        env.insert("KURISU_WEB_TOKEN".into(),"short".into());
        assert!(token(&env).is_err());
    }
    #[test]
    fn unchanged_stale_assets_fail_but_same_build_redeploy_is_valid() {
        let expected=serde_json::json!({"assets":["assets/main-new.js"]});
        let old=vec!["assets/main-old.js".to_string()];
        let new=vec!["assets/main-new.js".to_string()];
        assert!(assets_match(&expected,&old,&old).is_err());
        assert!(assets_match(&expected,&old,&new).unwrap());
        assert!(!assets_match(&expected,&new,&new).unwrap());
    }
}
