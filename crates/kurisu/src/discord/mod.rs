//! Optional Discord DM channel. No bot token is needed by the local test fixtures.
mod gateway;
pub(crate) mod rest;
#[cfg(test)]
mod tests;
use crate::{
    channels::{
        dispatch::{nonce, Dispatcher},
        Address,
    },
    paths::Paths,
    server::Server,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub token: String,
    pub guild: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}
fn yes() -> bool {
    true
}
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &"[REDACTED]")
            .field("guild", &self.guild)
            .field("enabled", &self.enabled)
            .finish()
    }
}
pub fn load(home: &Path) -> Option<Config> {
    serde_json::from_slice(&std::fs::read(home.join("discord.json")).ok()?).ok()
}

/// Secret input never appears in diagnostics. Prefer a private file or stdin over argv.
pub(crate) fn token_input(args: &[String], previous: Option<&Config>) -> Result<String> {
    let option = |name: &str| {
        args.iter()
            .position(|s| s == name)
            .and_then(|i| args.get(i + 1))
    };
    let literal = option("--token");
    let file = option("--token-file");
    let stdin = args.iter().any(|s| s == "--token-stdin");
    if usize::from(literal.is_some()) + usize::from(file.is_some()) + usize::from(stdin) > 1 {
        bail!("Choose one of --token, --token-file or --token-stdin");
    }
    let token = if let Some(path) = file {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|_| anyhow!("Could not read the token file"))?;
        if !meta.is_file() || meta.len() > 16384 || meta.permissions().mode() & 0o077 != 0 {
            bail!("Token file must be private (mode 0600) and at most 16 KiB");
        }
        std::fs::read_to_string(path).map_err(|_| anyhow!("Could not read the token file"))?
    } else if stdin {
        use std::io::Read;
        let mut text = String::new();
        std::io::stdin()
            .take(16385)
            .read_to_string(&mut text)
            .map_err(|_| anyhow!("Could not read token from stdin"))?;
        text
    } else if let Some(value) = literal {
        value.clone()
    } else {
        previous
            .map(|c| c.token.clone())
            .context("Supply --token-file FILE, --token-stdin or --token TOKEN")?
    };
    let token = token.trim().to_string();
    if token.len() < 20 || token.len() > 16384 || token.chars().any(char::is_whitespace) {
        bail!("Invalid Discord token format");
    }
    Ok(token)
}

async fn reload_saved(paths: &Paths) -> Result<()> {
    if !paths.socket().exists() {
        println!("discord: configured; starts with the Kurisu server");
        return Ok(());
    }
    let result = crate::server::call(&paths.socket(), json!({"op":"discord_reload"})).await?;
    println!(
        "discord: {}",
        result["state"].as_str().unwrap_or("reloaded")
    );
    Ok(())
}

pub async fn cli(paths: &Paths, args: &[String]) -> Result<()> {
    let sub = args.get(2).map(String::as_str).unwrap_or("status");
    let option = |name: &str| {
        args.iter()
            .position(|s| s == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    match sub {
        "on"=>{
            let previous=load(&paths.home);
            let token=token_input(args,previous.as_ref())?;
            let guild=option("--guild").or_else(||previous.as_ref().map(|c|c.guild.clone())).context("--guild GUILD_ID is required")?;
            if token.len()<20 || token.chars().any(char::is_whitespace) {bail!("Invalid Discord token format");}
            if guild.is_empty() || !guild.bytes().all(|c|c.is_ascii_digit()) {bail!("Guild ID must contain digits only");}
            crate::telegram::write_private(&paths.home.join("discord.json"),&serde_json::to_vec_pretty(&Config{token,guild,enabled:true})?)?;
            reload_saved(paths).await?;
        }
        "off"=>{
            let mut cfg=load(&paths.home).context("Discord is not configured")?;cfg.enabled=false;
            crate::telegram::write_private(&paths.home.join("discord.json"),&serde_json::to_vec_pretty(&cfg)?)?;
            reload_saved(paths).await?;
        }
        "status"=>{let v=crate::server::call(&paths.socket(),json!({"op":"discord_status"})).await?;println!("discord: {} (failed deliveries: {}, uncertain: {}, failed input: {})",v["state"].as_str().unwrap_or("unknown"),v["delivery"]["failed_deliveries"],v["delivery"]["uncertain_deliveries"],v["delivery"]["failed_jobs"]);}
        "pair"=>{
            let person=option("--person").context("usage: ku discord pair --person NAME [--session ID] [--user DISCORD_USER_ID]")?;
            let v=crate::server::call(&paths.socket(),json!({"op":"discord_pair","person":person,"session":option("--session"),"user":option("--user")})).await?;
            println!("Pair {} with session {}. In the bot DM within 5 minutes send:\n/start {}",v["person"].as_str().unwrap_or(""),v["session"].as_str().unwrap_or(""),v["code"].as_str().unwrap_or(""));
        }
        "notify"=>{let text=args.get(3).context("usage: ku discord notify TEXT")?;crate::server::call(&paths.socket(),json!({"op":"discord_notify","text":text})).await?;println!("discord: sent");}
        _=>bail!("usage: ku discord on --token TOKEN --guild GUILD_ID | off | status | pair --person NAME [--session ID] [--user ID] | notify TEXT"),
    }
    Ok(())
}

struct Running {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    pub engine: Arc<Dispatcher>,
}
#[derive(Default)]
pub struct Channel {
    inner: tokio::sync::Mutex<Option<Running>>,
}
impl Channel {
    pub async fn state(&self) -> &'static str {
        match self.inner.lock().await.as_ref() {
            None => "stopped",
            Some(r) if r.tasks.iter().any(|t| t.is_finished()) => "error",
            Some(_) => "running",
        }
    }
    pub async fn reload(&self, server: Arc<Server>, home: PathBuf) -> Result<&'static str> {
        self.stop().await;
        let Some(cfg) = load(&home) else {
            return Ok("not set up");
        };
        if !cfg.enabled {
            return Ok("off");
        }
        let api = std::env::var("KURISU_DISCORD_API")
            .unwrap_or_else(|_| "https://discord.com/api/v10".into());
        let gateway = std::env::var("KURISU_DISCORD_GATEWAY").ok();
        let rest = Arc::new(rest::Rest::new(cfg.token.clone(), api)?);
        let engine = Dispatcher::new(server, rest.clone());
        let worker = tokio::spawn(engine.clone().run());
        let e = engine.clone();
        let gateway = tokio::spawn(async move {
            if let Err(err) = gateway::run(e, rest, cfg.token, gateway).await {
                eprintln!("ERROR Discord channel stopped: {err}");
            }
        });
        *self.inner.lock().await = Some(Running {
            tasks: vec![worker, gateway],
            engine,
        });
        Ok("running")
    }
    pub async fn stop(&self) {
        if let Some(r) = self.inner.lock().await.take() {
            for t in r.tasks {
                t.abort();
            }
            r.engine.stop().await;
        }
    }
    pub async fn remote_card(
        &self,
        to: &Address,
        id: &str,
        kind: &str,
        ip: &str,
        ua: &str,
        code: &str,
        expires: i64,
    ) -> Result<()> {
        let engine = self
            .inner
            .lock()
            .await
            .as_ref()
            .map(|r| r.engine.clone())
            .ok_or_else(|| anyhow!("Discord channel is not running"))?;
        engine
            .remote_card(to, id, kind, ip, ua, code, expires)
            .await
    }
    pub async fn remote_close(&self, id: &str, text: &str) {
        let engine = self.inner.lock().await.as_ref().map(|r| r.engine.clone());
        if let Some(engine) = engine {
            if let Err(e) = engine
                .finish_control_card(&format!("remote:{id}"), text)
                .await
            {
                eprintln!("ERROR Discord remote card update failed: {e}");
            }
        }
    }
    pub async fn notify(&self, text: &str) -> Result<()> {
        let engine = self
            .inner
            .lock()
            .await
            .as_ref()
            .map(|r| r.engine.clone())
            .ok_or_else(|| anyhow!("Discord channel is not running"))?;
        let addresses = engine.server.channels.addresses()?;
        if addresses.is_empty() {
            bail!("No paired Discord people");
        }
        let key = uuid::Uuid::new_v4().simple().to_string();
        for to in addresses {
            engine.publish(&to, &key, text).await?;
        }
        Ok(())
    }
    pub async fn send_file(&self, to: &Address, path: &Path, caption: &str) -> Result<Value> {
        let engine = self
            .inner
            .lock()
            .await
            .as_ref()
            .map(|r| r.engine.clone())
            .ok_or_else(|| anyhow!("Discord channel is not running"))?;
        let key = format!("file:{}:{}", to.destination, uuid::Uuid::new_v4().simple());
        engine
            .server
            .channels
            .prepare(&key, to, &path.to_string_lossy())?;
        engine
            .server
            .channels
            .delivery(&key, "sending", None, &path.to_string_lossy(), None)?;
        match engine.wire.file(to, path, caption, &nonce(&key)).await {
            Ok(sent) => {
                let mid = sent["message_id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Discord returned no message ID"))?;
                engine.server.channels.delivery(
                    &key,
                    "accepted",
                    Some(mid),
                    &path.to_string_lossy(),
                    None,
                )?;
                Ok(sent)
            }
            Err(e) => {
                let status = if e.downcast_ref::<crate::channels::Uncertain>().is_some() {
                    "uncertain"
                } else {
                    "failed"
                };
                engine.server.channels.delivery(
                    &key,
                    status,
                    None,
                    &path.to_string_lossy(),
                    Some(&e.to_string()),
                )?;
                eprintln!(
                    "ERROR discord_undelivered file person={} error={e}",
                    to.person
                );
                Err(e)
            }
        }
    }
}
