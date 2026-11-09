//! Durable Gateway resume and tracked-channel history recovery.
use super::rest::Rest;
use crate::channels::{
    dispatch::{Dispatcher, Incoming},
    Channel,
};
use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::Message;

pub const COMMANDS: &[&str] = &[
    "start", "help", "new", "use", "id", "list", "status", "model", "usage", "stop", "send_now",
    "web", "terminal",
];

pub fn commands() -> Value {
    json!(COMMANDS.iter().map(|name| {
        let mut value=json!({"name":name,"description":format!("Kurisu /{name}"),"type":1,"contexts":[0,1],"integration_types":[0]});
        let opt=match *name {"start"=>Some(("code","Local pairing code")),"use"=>Some(("session","Session ID or prefix")),"list"=>Some(("page","Page number (zero based)")),"model"=>Some(("model","Model ID; omit for the harness/model/effort picker")),"web"|"terminal"=>Some(("argument","Path, close or off")),_=>None};
        if let Some((name,description))=opt {value["options"]=json!([{"name":name,"description":description,"type":3,"required":name=="session"}]);} value
    }).collect::<Vec<_>>())
}

pub async fn register(rest: &Rest) -> Result<String> {
    let bot = rest
        .request(reqwest::Method::GET, "/users/@me", json!({}))
        .await?;
    if bot["bot"] != true {
        bail!("Discord credential does not identify a bot");
    }
    let id = bot["id"]
        .as_str()
        .ok_or_else(|| anyhow!("Discord application ID missing"))?;
    rest.request(
        reqwest::Method::PUT,
        &format!("/applications/{id}/commands"),
        commands(),
    )
    .await?;
    Ok(id.to_string())
}

#[derive(Debug)]
pub struct Rejected(pub &'static str);
impl std::fmt::Display for Rejected {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { f.write_str(self.0) }
}
impl std::error::Error for Rejected {}

pub async fn acknowledge(rest: &Rest, data: &Value) -> Result<()> {
    let id=data["id"].as_str().ok_or_else(||anyhow!("Missing interaction ID"))?;
    let token=data["token"].as_str().ok_or_else(||anyhow!("Missing interaction credential"))?;
    let kind=data["type"].as_u64().unwrap_or(0);
    if !matches!(kind,2|3) {return Ok(());}
    tokio::time::timeout(Duration::from_secs(2),rest.acknowledge(id,token,if kind==3{6}else{5},if kind==3{None}else{Some(json!({"flags":64}))}))
        .await.map_err(|_|anyhow!("Discord interaction ACK deadline exceeded"))??;
    Ok(())
}

pub async fn dispatch(engine: &Arc<Dispatcher>,rest: &Arc<Rest>,guild: &str,event: &str,data: Value) -> Result<()> {
    if event=="INTERACTION_CREATE" {acknowledge(rest,&data).await?;}
    dispatch_saved(engine,rest,guild,event,data).await
}

pub async fn dispatch_saved(engine: &Arc<Dispatcher>,rest: &Arc<Rest>,guild: &str,event: &str,data: Value) -> Result<()> {
    let source_guild=data["guild_id"].as_str();
    if source_guild.is_some_and(|g|g!=guild) {return Ok(());}
    let (id,author,text,attachments,action)=if event=="MESSAGE_CREATE" {
        if data["author"]["bot"]==true || data.get("webhook_id").is_some_and(|v|!v.is_null())
            || data["type"].as_u64().is_some_and(|t|!matches!(t,0|19)) {return Ok(());}
        let text=data["content"].as_str().unwrap_or("").to_string();
        let attachments=data["attachments"].as_array().cloned().unwrap_or_default();
        if text.trim().is_empty() && attachments.is_empty() {return Ok(());}
        (format!("message:{}",data["id"].as_str().ok_or_else(||anyhow!("Missing message ID"))?),&data["author"],text,attachments,None)
    } else if event=="INTERACTION_CREATE" {
        let author=if source_guild.is_some(){&data["member"]["user"]}else{&data["user"]};
        if author["bot"]==true {return Ok(());}
        let kind=data["type"].as_u64().unwrap_or(0);
        let (text,action)=if kind==2 {
            let name=data["data"]["name"].as_str().unwrap_or("");
            if !COMMANDS.contains(&name) {return Err(anyhow!(Rejected("Unsupported Discord command.")));}
            (format!("/{name} {}",data["data"]["options"][0]["value"].as_str().unwrap_or("")),None)
        }else if kind==3 && data["data"]["custom_id"]=="model-choice" {
            (String::new(),Some(data["data"]["values"][0].as_str().ok_or_else(||anyhow!("Missing model choice"))?.to_string()))
        }else if kind==3 && data["data"]["custom_id"].as_str().is_some_and(|s|s.starts_with("action:")) {
            (String::new(),Some(data["data"]["custom_id"].as_str().unwrap()[7..].to_string()))
        }else{return Ok(());};
        (format!("interaction:{}",data["id"].as_str().ok_or_else(||anyhow!("Missing interaction ID"))?),author,text,vec![],action)
    }else{return Ok(());};
    if engine.server.channels.seen(&id)? {return Ok(());}
    let user=author["id"].as_str().ok_or_else(||anyhow!("Missing author ID"))?;
    if source_guild.is_some() && engine.server.channels.person("discord",user)?.is_none() {return Ok(());}
    if source_guild.is_some() && crate::channels::command(&text).is_some_and(|(n,_)|n=="/start") {
        return Err(anyhow!(Rejected("Redeem pairing codes only in the bot DM.")));
    }
    let source=data["channel_id"].as_str().ok_or_else(||anyhow!("Missing channel ID"))?;
    let destination=if let Some(guild)=source_guild {
        super::threads::destination(engine,rest,guild,user,source).await?
    }else{source.into()};
    let input=Incoming{id,user:user.into(),destination,guild:source_guild.map(str::to_string),text,attachments,action};
    if let Err(e)=engine.accept(input.clone()).await {
        if source_guild.is_some() {return Err(e);}
        let to=crate::channels::Address{channel:"discord".into(),destination:input.destination,person:"unpaired".into()};
        rest.send(&to,&e.to_string(),&crate::channels::dispatch::nonce(&input.id)).await?;
    }
    Ok(())
}

fn gateway_url(raw: &str, local: bool) -> Result<String> {
    let mut url=reqwest::Url::parse(raw).map_err(|_|anyhow!("Invalid Discord gateway URL"))?;
    let host=url.host_str().unwrap_or("");
    let official=url.scheme()=="wss" && (host=="gateway.discord.gg" || host.ends_with(".discord.gg"));
    let fixture=local && matches!(url.scheme(),"ws"|"wss") && matches!(host,"127.0.0.1"|"localhost"|"[::1]");
    if !official && !fixture {bail!("Untrusted Discord gateway URL");}
    url.set_query(None);url.query_pairs_mut().append_pair("v","10").append_pair("encoding","json");
    Ok(url.to_string())
}
#[derive(Debug)]
struct FatalClose(u16);
impl std::fmt::Display for FatalClose {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"Discord gateway fatal close {}",self.0)}
}
impl std::error::Error for FatalClose {}

pub async fn run(engine: Arc<Dispatcher>,rest: Arc<Rest>,token: String,guild: String,override_url: Option<String>) -> Result<()> {
    let app=register(&rest).await?;
    let account=format!("{app}:{guild}");
    engine.server.channels.gateway_seed_inbox(&account)?;
    let local=override_url.is_some();
    let raw=match override_url {
        Some(url)=>url,
        None=>rest.request(reqwest::Method::GET,"/gateway/bot",json!({})).await?["url"].as_str().ok_or_else(||anyhow!("Discord gateway URL missing"))?.into(),
    };
    let url=gateway_url(&raw,local)?;
    tokio::select! {
        result=connections(&engine,&rest,&token,&guild,&account,&url,local)=>result,
        _=super::recovery::run(&engine,&rest,&guild,&account)=>bail!("Discord recovery worker stopped"),
    }
}
async fn connections(engine:&Arc<Dispatcher>,rest:&Arc<Rest>,token:&str,guild:&str,account:&str,url:&str,local:bool)->Result<()> {
    let mut resume=engine.server.channels.gateway_checkpoint(account)?.unwrap_or(json!({}));
    let mut failures=0u32;
    loop {
        let result=connection(engine,rest,token,guild,account,url,local,&mut resume).await;
        match result {
            Err(e) if e.downcast_ref::<FatalClose>().is_some()=>return Err(e),
            Err(e)=>{eprintln!("Discord gateway disconnected: {}",rest.safe(&e.to_string()));failures=failures.saturating_add(1);}
            Ok(())=>failures=0,
        }
        let delay=if cfg!(test){Duration::from_millis(20)}else{Duration::from_secs(5+2u64.pow(failures.min(4)))};
        tokio::time::sleep(delay).await;
    }
}
async fn connection(engine:&Arc<Dispatcher>,rest:&Arc<Rest>,token:&str,guild:&str,account:&str,initial:&str,local:bool,resume:&mut Value)->Result<()> {
    let resumable=resume["session_id"].is_string() && resume["seq"].is_u64();
    let url=if resumable {gateway_url(resume["url"].as_str().ok_or_else(||anyhow!("Missing resume URL"))?,local)?}else{initial.into()};
    let (mut ws,_)=tokio_tungstenite::connect_async(&url).await.map_err(|_|anyhow!("Discord WebSocket connect failed"))?;
    let hello=tokio::time::timeout(Duration::from_secs(20),ws.next()).await.map_err(|_|anyhow!("Discord HELLO timed out"))?.ok_or_else(||anyhow!("Discord closed before HELLO"))?.map_err(|_|anyhow!("Discord HELLO failed"))?;
    let hello:Value=serde_json::from_str(hello.to_text().map_err(|_|anyhow!("Invalid HELLO frame"))?)?;
    if hello["op"]!=10 {bail!("Expected Discord HELLO");}
    let millis=hello["d"]["heartbeat_interval"].as_u64().filter(|n|*n>=100).ok_or_else(||anyhow!("Invalid heartbeat interval"))?;
    let identify=if resumable {json!({"op":6,"d":{"token":token,"session_id":resume["session_id"],"seq":resume["seq"]}})}
        else{json!({"op":2,"d":{"token":token,"intents":37377,"properties":{"os":std::env::consts::OS,"browser":"kurisu","device":"kurisu"}}})};
    ws.send(Message::Text(identify.to_string().into())).await.map_err(|_|anyhow!("Discord authentication send failed"))?;
    let jitter=uuid::Uuid::new_v4().as_u128() % u128::from(millis);
    let mut interval=tokio::time::interval_at(tokio::time::Instant::now()+Duration::from_millis(jitter as u64),Duration::from_millis(millis));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut acknowledged=true;
    loop {
        tokio::select! {
            _=interval.tick()=>{
                if !acknowledged {bail!("Discord missed heartbeat ACK");}
                ws.send(Message::Text(json!({"op":1,"d":resume["seq"]}).to_string().into())).await.map_err(|_|anyhow!("Discord heartbeat failed"))?;acknowledged=false;
            }
            frame=ws.next()=>{
                let frame=frame.ok_or_else(||anyhow!("Discord gateway closed"))?.map_err(|_|anyhow!("Discord gateway read failed"))?;
                match frame {
                    Message::Text(text)=>{
                        let v:Value=serde_json::from_str(&text).map_err(|_|anyhow!("Invalid Discord gateway JSON"))?;
                        match v["op"].as_u64() {
                            Some(0)=>{
                                let event=v["t"].as_str().unwrap_or("");let data=&v["d"];
                                let sequence=v["s"].as_u64().ok_or_else(||anyhow!("Missing Discord sequence"))?;
                                let mut next=resume.clone();
                                if event=="READY" {
                                    let session=data["session_id"].as_str().ok_or_else(||anyhow!("Missing READY session"))?;
                                    let url=data["resume_gateway_url"].as_str().ok_or_else(||anyhow!("Missing READY resume URL"))?;
                                    gateway_url(url,local)?;
                                    next=json!({"session_id":session,"url":url});
                                    engine.server.channels.backfill_begin(account)?;
                                }
                                if event=="GUILD_CREATE" && data["id"]==guild {
                                    for channel in data["channels"].as_array().into_iter().flatten() {
                                        if channel["type"]==0 {
                                            if let (Some(id),Some(last))=(channel["id"].as_str(),channel["last_message_id"].as_str()) {
                                                engine.server.channels.gateway_watch_new(account,id,Some(guild),last)?;
                                            }
                                        }
                                    }
                                }
                                next["seq"]=json!(sequence.max(next["seq"].as_u64().unwrap_or(0)));
                                let allowed=data["guild_id"].as_str().is_none_or(|g|g==guild);
                                let inserted=engine.server.channels.gateway_record(account,&next,if allowed{event}else{"IGNORED"},data)?;
                                *resume=next;
                                if event=="INTERACTION_CREATE" && inserted {
                                    if let Err(e)=acknowledge(rest,data).await {eprintln!("ERROR Discord interaction ACK failed: {}",rest.safe(&e.to_string()));}
                                }
                            }
                            Some(11)=>acknowledged=true,
                            Some(1)=>{ws.send(Message::Text(json!({"op":1,"d":resume["seq"]}).to_string().into())).await.map_err(|_|anyhow!("Discord heartbeat request failed"))?;acknowledged=false;}
                            Some(7)=>return Ok(()),
                            Some(9)=>{
                                if v["d"]!=true {*resume=json!({});engine.server.channels.gateway_clear(account)?;}
                                return Ok(());
                            }
                            _=>{}
                        }
                    }
                    Message::Ping(bytes)=>{ws.send(Message::Pong(bytes)).await.map_err(|_|anyhow!("Discord ping failed"))?;}
                    Message::Close(close)=>{
                        let code=close.map(|c|u16::from(c.code)).unwrap_or(1006);
                        if matches!(code,4004|4010|4011|4012|4013|4014) {return Err(anyhow!(FatalClose(code)));}
                        if matches!(code,1000|1001|4007|4009) {*resume=json!({});engine.server.channels.gateway_clear(account)?;}
                        bail!("Discord gateway closed with code {code}");
                    }
                    _=>{}
                }
            }
        }
    }
}
