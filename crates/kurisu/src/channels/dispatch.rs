//! Common person/conversation dispatcher. Transports normalize input; every operation uses the existing server.
mod cards;
mod remote;

pub const PAIRING_NOTICE: &str = "Unpaired sender. Ask the local owner for a code from ku discord pair, then send /start <code>.";

use super::{
    command,
    stream::Relay,
    text::{self, Chunk},
    Address, Channel,
};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incoming {
    pub id: String,
    pub user: String,
    pub destination: String,
    #[serde(default)]
    pub guild: Option<String>,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Value>,
    #[serde(default)]
    pub action: Option<String>,
}

pub struct Dispatcher {
    pub server: Arc<crate::server::Server>,
    pub wire: Arc<dyn Channel>,
    active: Mutex<HashSet<String>>,
    ui: Mutex<()>,
    ingress: Mutex<()>,
    pub(crate) guild_routes: Mutex<()>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

pub fn nonce(key: &str) -> String {
    let mut h = 0xcbf29ce484222325u64;
    let mut j = 0x84222325cbf29ce4u64;
    for byte in key.bytes() {
        h = (h ^ byte as u64).wrapping_mul(0x100000001b3);
        j = (j ^ byte as u64).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}{j:016x}")[..25].to_string()
}

impl Dispatcher {
    pub fn new(server: Arc<crate::server::Server>, wire: Arc<dyn Channel>) -> Arc<Self> {
        Arc::new(Self {
            server,
            wire,
            active: Mutex::default(),
            ui: Mutex::default(),
            ingress: Mutex::default(),
            guild_routes: Mutex::default(),
            tasks: Mutex::default(),
        })
    }
    async fn op(&self,to:&Address,request:Value)->Result<Value> {
        self.server.handle_person_pub(to.person.clone(),request).await
    }
    pub async fn stop(&self) {
        let tasks:Vec<_>=self.tasks.lock().await.drain(..).collect();
        for task in &tasks {task.abort();}
        for task in tasks {let _=task.await;}
    }
    pub async fn accept(&self, input: Incoming) -> Result<bool> {
        let _guard = self.ingress.lock().await;
        let state = &self.server.channels;
        if state.seen(&input.id)? {
            return Ok(false);
        }
        let mut person = state.person(self.wire.name(), &input.user)?;
        if let Some((name, code)) = command(&input.text).filter(|_| input.attachments.is_empty()) {
            if name == "/start" && !code.is_empty() {
                if input.guild.is_some() { bail!("Redeem pairing codes only in the bot DM."); }
                person = Some(state.redeem(code, &input.user, &input.destination)?);
            }
        }
        let person=person.ok_or_else(||anyhow!(PAIRING_NOTICE))?;
        if let Some(action) = &input.action {
            let data = state.peek_action(&person, action)?;
            if data["destination"]
                .as_str()
                .is_some_and(|d| d != input.destination)
            {
                bail!("This action belongs to a different conversation.");
            }
        }
        let to = Address {
            channel: self.wire.name().into(),
            destination: input.destination.clone(),
            person: person.clone(),
        };
        if let Some(guild) = &input.guild {
            let route=state.thread_owner(&to.destination)?.ok_or_else(||anyhow!(super::state::Denied))?;
            if route["guild"]!=*guild || route["person"]!=person || route["user"]!=input.user || route["status"]!="active" {return Err(anyhow!(super::state::Denied));}
            if state.conversation(&to)?.is_none() {
                let base=state.session(&person)?.ok_or_else(||anyhow!("No paired session"))?;
                let snap=self.op(&to,json!({"op":"status","name":base})).await?;
                let created=self.op(&to,json!({"op":"create","name":format!("discord-{}",&uuid::Uuid::new_v4().simple().to_string()[..16]),"role":"worker","description":format!("Discord guild conversation for {person}"),"harness":snap["harness"],"model":snap["model"],"permission_mode":snap["permission_mode_effective"]})).await?;
                let session=created["id"].as_str().ok_or_else(||anyhow!("Session create returned no id"))?;
                state.register_conversation(&to,guild,session)?;
            }
        }
        let session=self.current(&to).await?;
        state.inbound(
            &input.id,
            &to,
            Some(&session),
            &serde_json::to_value(&input)?,
        )
    }
    pub async fn run(self: Arc<Self>) {
        loop {
            if let Err(e) = self.step().await {
                eprintln!("ERROR {} inbox worker: {e}", self.wire.name());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    pub async fn step(self: &Arc<Self>) -> Result<()> {
        for job in self.server.channels.jobs()? {
            let id = job["id"].as_str().unwrap().to_string();
            let to = Address {
                channel: self.wire.name().into(),
                destination: job["destination"].as_str().unwrap().into(),
                person: job["person"].as_str().unwrap().into(),
            };
            if job["status"] == "posted" {
                let mut active = self.active.lock().await;
                if active.insert(id.clone()) {
                    let me = self.clone();
                    let job = job.clone();
                    let task = tokio::spawn(async move {
                        if let Err(e) = me.relay(&job, &to).await {
                            me.fail(&id, &to, &e).await;
                        }
                        me.active.lock().await.remove(&id);
                    });
                    let mut tasks = self.tasks.lock().await;
                    tasks.retain(|t| !t.is_finished());
                    tasks.push(task);
                }
                continue;
            }
            if let Err(e) = self.process(&job, &to).await {
                self.fail(&id, &to, &e).await;
            }
        }
        self.refresh_cards().await?;
        Ok(())
    }
    async fn fail(&self, id: &str, to: &Address, error: &anyhow::Error) {
        let error = self.wire.redact(&error.to_string());
        eprintln!(
            "ERROR {}_undelivered person={} event={} error={} metric={}_undelivered_total+1",
            self.wire.name(),
            to.person,
            id,
            error,
            self.wire.name()
        );
        let _ = self.server.channels.job(id, "failed", None, Some(&error));
        let notice = format!(
            "Could not deliver reply: {}",
            error.chars().take(220).collect::<String>()
        );
        if let Err(e) = self
            .wire
            .send(to, &notice, &nonce(&format!("fallback:{id}")))
            .await
        {
            eprintln!("ERROR {} delivery fallback failed: {e}", self.wire.name());
        }
    }
    async fn process(&self, job: &Value, to: &Address) -> Result<()> {
        let id = job["id"].as_str().unwrap();
        let mut input: Incoming = serde_json::from_value(job["payload"].clone())?;
        if let Some(action) = &input.action {
            let data = self.server.channels.peek_action(&to.person, action)?;
            if matches!(
                data["kind"].as_str(),
                Some("approval" | "answer" | "release")
            ) {
                if data["kind"] == "release" {
                    self.release_action(to, &data).await?;
                } else {
                    self.prompt_action(to, &data).await?;
                }
                self.server.channels.forget_action(&to.person, action)?;
            } else if data["kind"]
                .as_str()
                .is_some_and(|k| k.starts_with("remote_"))
            {
                self.remote_action(to, id, &data).await?;
                self.server.channels.forget_action(&to.person, action)?;
            } else {
                let data = self.server.channels.take_action(&to.person, action)?;
                self.model_action(to, id, data).await?;
            }
            self.server.channels.job(id, "done", None, None)?;
            return Ok(());
        }
        if let Some((name, arg)) = command(&input.text).filter(|_| input.attachments.is_empty()) {
            let result = self.command(to, id, &name, arg).await?;
            if let Some(text) = result {
                self.publish(to, id, &text).await?;
            }
            self.server.channels.job(id, "done", None, None)?;
            return Ok(());
        }
        let bound = if job["payload"]["session_bound"] == true {
            job["session"].as_str().map(str::to_string)
        } else {
            Some(self.current(to).await?)
        };
        let session = bound.as_deref().ok_or_else(|| {
            anyhow!("No session is bound to this person; run ku discord pair again.")
        })?;
        if !self.server.channels.selectable(to,session)? {return Err(anyhow!(super::state::Denied));}
        self.server.channels.bind_job(id, session)?;
        let snap = self.op(to, json!({"op":"status","name":session})).await?;
        let force = job["force"] == true;
        let other_relay = self.server.channels.posted(session)?;
        if !force
            && (other_relay
                || matches!(
                    snap["status"].as_str(),
                    Some("working" | "starting" | "awaiting_approval")
                ))
        {
            self.queued_notice(to,id,"⏳ Queued — the session is finishing another turn first. Use /send_now to send it now.").await?;
            return Ok(());
        }
        // Materialize downloads once, before recording the turn. All files use the server's upload path.
        for attachment in &mut input.attachments {
            if attachment["path"].is_string() {
                continue;
            }
            const MAX: usize = 25 * 1024 * 1024;
            if attachment["size"].as_u64().is_some_and(|n| n > MAX as u64) {
                bail!("Attachment exceeds 25 MiB download limit");
            }
            let url = attachment["url"]
                .as_str()
                .ok_or_else(|| anyhow!("Attachment URL missing"))?;
            let bytes = self.wire.download(url, MAX).await?;
            let filename = attachment["filename"].as_str().unwrap_or("file");
            let filename = std::path::Path::new(filename)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file");
            use base64::Engine;
            let saved=self.op(to,json!({"op":"upload","name":session,"filename":filename,"data_b64":base64::engine::general_purpose::STANDARD.encode(bytes)})).await?;
            attachment["path"] = saved["path"].clone();
        }
        let mut payload = serde_json::to_value(&input)?;
        payload["session_bound"] = json!(true);
        self.server
            .channels
            .job(id, "pending", Some(&payload), None)?;
        let paths: Vec<&str> = input
            .attachments
            .iter()
            .filter_map(|a| a["path"].as_str())
            .collect();
        let mut text = input.text.clone();
        if !paths.is_empty() {
            text.push_str(&format!(
                "\n\n[Attached (in your folder; read them with the Read tool): {}]",
                paths.join(", ")
            ));
        }
        if text.trim().is_empty() {
            bail!("Send text or an attachment.");
        }
        self.server.channels.route(session, to)?;
        let sent=self.op(to,json!({"op":"send","name":session,"text":text,"wait":false,"mode":if force{"steer"}else{"queue"},"channel_event":format!("{}:{id}",self.wire.name())})).await?;
        payload["dispatch_seq"] = sent["seq"].clone();
        self.server.channels.job(
            id,
            if force && other_relay {
                "done"
            } else {
                "posted"
            },
            Some(&payload),
            None,
        )?;
        self.publish(
            to,
            &format!("{id}:ack"),
            if sent["queued"] == true {
                "⏳ Queued."
            } else {
                "⏳ On it."
            },
        )
        .await?;
        Ok(())
    }

    async fn current(&self, to: &Address) -> Result<String> {
        let session=self.server.channels.conversation(to)?.or(self.server.channels.session(&to.person)?)
            .ok_or_else(||anyhow!("No session selected."))?;
        if !self.server.channels.selectable(to,&session)? {return Err(anyhow!(super::state::Denied));}
        Ok(session)
    }
    async fn command(
        &self,
        to: &Address,
        id: &str,
        name: &str,
        arg: &str,
    ) -> Result<Option<String>> {
        let sid = self.current(to).await?;
        let answer=match name {
            "/start"|"/help"=>"Kurisu Discord\n/start, /help — help and pairing\n/new — fresh session\n/use <id|prefix> — select a session\n/id, /list, /status — sessions\n/model [model-id] — choose harness, model and effort\n/usage — installed CLI plan usage\n/stop — interrupt current run\n/send_now — send pending input now\nText and attachments reach your Kurisu sessions. Approval/question cards and queue release are supported.".to_string(),
            "/id"=>sid,
            "/new"|"/reset"=>{
                let snap=self.op(&to,json!({"op":"status","name":sid})).await?;
                let name=format!("discord-{}",id.chars().filter(|c|c.is_ascii_alphanumeric()).take(23).collect::<String>());
                let fresh=match self.op(to,json!({"op":"status","name":name})).await { Ok(v)=>v,Err(_)=>self.op(to,json!({"op":"create","name":name,"role":"worker","description":"Discord chat","harness":snap["harness"],"model":snap["model"],"permission_mode":snap["permission_mode_effective"]})).await? };
                let new=fresh["id"].as_str().ok_or_else(||anyhow!("Session create returned no id"))?;
                self.server.channels.select_conversation(to,new)?; format!("Started a new session: {new}")
            }
            "/use"=>{
                if arg.is_empty() { return Ok(Some("Usage: /use <session-id-or-prefix>".into())); }
                let peers=self.op(to,json!({"op":"peers","include_archived":false})).await?;
                let matches:Vec<&Value>=peers.as_array().into_iter().flatten().filter(|s|s["id"].as_str().is_some_and(|sid|self.server.channels.selectable(to,sid).unwrap_or(false)&&(sid==arg||sid.starts_with(arg)))).collect();
                if matches.len()!=1 { bail!("{} session matches {arg}",if matches.is_empty(){"No"}else{"Ambiguous"}); }
                let new=matches[0]["id"].as_str().unwrap(); self.server.channels.select_conversation(to,new)?; format!("Now using {new} ({})",matches[0]["status"].as_str().unwrap_or("?"))
            }
            "/list"=>{
                let peers=self.op(to,json!({"op":"peers","include_archived":false})).await?;
                let mut rows=peers.as_array().cloned().unwrap_or_default();
                rows.retain(|s|s["id"].as_str().is_some_and(|sid|self.server.channels.selectable(to,sid).unwrap_or(false)));
                rows.sort_by_key(|s|std::cmp::Reverse(s["last_user_at"].as_i64().or(s["last_activity"].as_i64()).unwrap_or(0)));
                let page=arg.parse::<usize>().unwrap_or(0); let mut lines=vec![format!("Recent sessions (page {})",page+1)];
                for s in rows.iter().skip(page*5).take(5) {
                    let mut block=format!("{}{} · {} · {}\n{}",if s["id"]==sid{"✓ "}else{""},s["name"].as_str().unwrap_or("?"),s["status"].as_str().unwrap_or("?"),s["model"].as_str().unwrap_or("?"),s["id"].as_str().unwrap_or("?"));
                    if let Some(note)=super::note_line(s["note"].as_str().unwrap_or("")) { block.push('\n'); block.push_str(&note); }
                    lines.push(block);
                }
                if rows.len()>(page+1)*5 { lines.push(format!("/list {} for more",page+1)); } lines.join("\n\n")
            }
            "/status"=>{
                let s=self.op(to,json!({"op":"status","name":sid})).await?;
                format!("status: {}\ntitle: {}\nharness: {}\nmodel: {}\neffort: {}\npermissions: {}\ncontext: {} / {}\nsession: {sid}",s["status"].as_str().unwrap_or("?"),s["name"].as_str().unwrap_or("?"),s["harness"].as_str().unwrap_or("?"),s["model"].as_str().unwrap_or("default"),s["effort"].as_str().unwrap_or("default"),s["permission_mode_effective"].as_str().unwrap_or("default"),s["context_tokens"],s["context_window"])
            }
            "/web"|"/terminal"=>{self.remote_command(to,id,&name[1..],arg).await?;return Ok(None);}
            "/usage"=>if to.person=="owner" {self.op(to,json!({"op":"usage_report"})).await?["text"].as_str().unwrap_or("Usage unavailable.").to_string()}else{serde_json::to_string_pretty(&self.op(to,json!({"op":"usage","name":sid})).await?)?},
            "/stop"=>{ self.op(to,json!({"op":"stop","name":sid})).await?; format!("🛑 Stop requested for {sid}. The session remains active.") }
            "/send_now"|"/send-now"=>format!("🚀 Released {} pending messages to the active session.",self.server.channels.force_conversation(to)?),
            "/model"=>{
                if !arg.is_empty() { self.apply_model(to,&sid,None,arg,None).await?; return Ok(Some(format!("Model set to {arg}."))); }
                let s=self.op(to,json!({"op":"status","name":sid})).await?;
                let clis=self.op(to,json!({"op":"clis"})).await?;
                let mut harnesses:Vec<String>=clis["clis"].as_array().into_iter().flatten().filter(|c|c["installed"]==true && matches!(c["name"].as_str(),Some("claude"|"codex"|"opencode"|"agy"))).filter_map(|c|c["name"].as_str().map(str::to_string)).collect();
                let cur=s["harness"].as_str().unwrap_or("claude").to_string(); if !harnesses.contains(&cur) {harnesses.push(cur);}
                let options=harnesses.into_iter().map(|h|(h.clone(),json!({"kind":"models","session":sid,"harness":h,"page":0}))).collect();
                self.card(to,id,"Choose a harness:",options).await?; return Ok(None);
            }
            _=>"Unknown command. Try /help.".into(),
        };
        Ok(Some(answer))
    }
    async fn card(
        &self,
        to: &Address,
        key: &str,
        text: &str,
        options: Vec<(String, Value)>,
    ) -> Result<()> {
        let mut choices = Vec::new();
        for (label, mut data) in options {
            data["destination"]=json!(to.destination);
            choices.push((label, self.server.channels.action(&to.person, &data)?));
        }
        let messages = self.publish(to, key, text).await?;
        self.wire
            .choices(
                to,
                messages
                    .last()
                    .ok_or_else(|| anyhow!("No model card receipt"))?,
                choices,
            )
            .await
    }
    async fn model_action(&self, to: &Address, key: &str, data: Value) -> Result<()> {
        let sid = data["session"]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid model action"))?;
        if !self.server.channels.selectable(to,sid)? || self.current(to).await?!=sid {return Err(anyhow!(super::state::Denied));}
        let harness = data["harness"].as_str().unwrap_or("claude");
        let model = data["model"].as_str().unwrap_or("default");
        match data["kind"].as_str().unwrap_or("") {
            "models" => {
                let options = self.op(to, json!({"op":"models","harness":harness})).await?;
                let rows: Vec<&Value> = options["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| {
                        !m["id"]
                            .as_str()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains("kimi")
                    })
                    .collect();
                let page = data["page"].as_u64().unwrap_or(0) as usize;
                let mut choices:Vec<(String,Value)>=rows.iter().skip(page*8).take(8).map(|m|(m["label"].as_str().or(m["id"].as_str()).unwrap_or("model").to_string(),json!({"kind":"effort","session":sid,"harness":harness,"model":m["id"]}))).collect();
                if page > 0 {
                    let mut prev = data.clone();
                    prev["page"] = json!(page - 1);
                    choices.push(("Previous page".into(), prev));
                }
                if rows.len() > (page + 1) * 8 {
                    let mut next = data.clone();
                    next["page"] = json!(page + 1);
                    choices.push(("Next page".into(), next));
                }
                if choices.is_empty() {
                    bail!("No models available for {harness}");
                }
                self.card(
                    to,
                    key,
                    &format!("Choose a {harness} model (page {}):", page + 1),
                    choices,
                )
                .await?;
            }
            "effort" => {
                let mut levels: Vec<String> = crate::store::effort_levels(harness)
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                if harness == "opencode" {
                    let opts = self.op(to,
                        json!({"op":"models","harness":harness,"model":model}),
                    )
                    .await?;
                    levels = opts["config_options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|o| o["id"] == "effort")
                        .and_then(|o| o["options"].as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|o| o["value"].as_str().map(str::to_string))
                        .collect();
                }
                levels.insert(0, "default".into());
                levels.dedup();
                self.card(to,key,&format!("Choose effort for {model}:"),levels.into_iter().map(|e|(e.clone(),json!({"kind":"confirm","session":sid,"harness":harness,"model":model,"effort":e}))).collect()).await?;
            }
            "confirm" => {
                let s = self.op(to, json!({"op":"status","name":sid})).await?;
                if s["harness"] != harness {
                    let mut apply = data.clone();
                    apply["kind"] = json!("apply");
                    self.card(to,key,&format!("Switch this session to {harness} ({model})? Kurisu will create its normal history-linked replacement."),vec![("Switch".into(),apply),("Cancel".into(),json!({"kind":"cancel","session":sid}))]).await?;
                } else {
                    self.apply_model(to, sid, Some(harness), model, data["effort"].as_str())
                        .await?;
                    self.publish(to, key, "✅ Model and effort updated.")
                        .await?;
                }
            }
            "apply" => {
                self.apply_model(to, sid, Some(harness), model, data["effort"].as_str())
                    .await?;
                self.publish(to, key, "✅ Harness, model and effort updated.")
                    .await?;
            }
            _ => {
                self.publish(to, key, "Model selection cancelled.").await?;
            }
        }
        Ok(())
    }
    async fn apply_model(
        &self,
        to: &Address,
        sid: &str,
        harness: Option<&str>,
        model: &str,
        effort: Option<&str>,
    ) -> Result<()> {
        if model.to_lowercase().contains("kimi") {
            bail!("Kimi models are disabled");
        }
        let s = self.op(to, json!({"op":"status","name":sid})).await?;
        if matches!(
            s["status"].as_str(),
            Some("working" | "starting" | "awaiting_approval")
        ) {
            bail!("A run is active; use /stop or wait.");
        }
        let mut fields = json!({"model":model});
        if let Some(h) = harness.filter(|h| s["harness"] != *h) {
            fields["harness"] = json!(h);
        }
        let r = self.op(
            to,
            json!({"op":"update","name":sid,"fields":fields}),
        )
        .await?;
        let target = r["new"]["id"].as_str().unwrap_or(sid);
        self.server.channels.select_conversation(to, target)?;
        if let Some(e) = effort.filter(|e| *e != "default") {
            self.op(
                to,
                json!({"op":"update","name":target,"fields":{"effort":e}}),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn publish(&self, to: &Address, key: &str, source: &str) -> Result<Vec<String>> {
        if let Some(route)=self.server.channels.thread_owner(&to.destination)? {
            if route["person"]!=to.person || route["status"]!="active" {return Err(anyhow!(super::state::Denied));}
        }
        let source = self.wire.redact(source);
        let mut messages = Vec::new();
        for (i, chunk) in text::discord(&source).into_iter().enumerate() {
            let (text, file) = match chunk {
                Chunk::Text(t) => (t, false),
                Chunk::File(t) => (t, true),
            };
            let key = format!("{}:{key}:{i}", to.destination);
            let old = self.server.channels.prepare(&key, to, &text)?;
            if let Some(mid) = old["message"].as_str() {
                if old["content"] != text && !file {
                    self.wire.edit(to, mid, &text).await?;
                    self.server
                        .channels
                        .delivery(&key, "accepted", Some(mid), &text, None)?;
                }
                messages.push(mid.to_string());
                continue;
            }
            if old["status"] == "sending" || old["status"] == "uncertain" {
                bail!("A previous send has uncertain acceptance; refusing duplicate replay.");
            }
            self.server
                .channels
                .delivery(&key, "sending", None, &text, None)?;
            let result = if file {
                let dir = self.server.home().join("discord/replies");
                tokio::fs::create_dir_all(&dir).await?;
                let path = dir.join(format!("{}.txt", nonce(&key)));
                crate::telegram::write_private(&path, text.as_bytes())?;
                self.wire.file(to,&path,"Reply contains indivisible text longer than Discord's message limit; exact text is attached.",&nonce(&key)).await.map(|v|v["message_id"].as_str().unwrap_or("").to_string())
            } else {
                self.wire.send(to, &text, &nonce(&key)).await
            };
            match result {
                Ok(mid) if !mid.is_empty() => {
                    self.server
                        .channels
                        .delivery(&key, "accepted", Some(&mid), &text, None)?;
                    messages.push(mid);
                }
                Ok(_) => {
                    self.server.channels.delivery(
                        &key,
                        "uncertain",
                        None,
                        &text,
                        Some("missing receipt"),
                    )?;
                    bail!("Transport returned no accepted message ID");
                }
                Err(e) => {
                    let status = if e.downcast_ref::<super::Uncertain>().is_some() {
                        "uncertain"
                    } else {
                        "failed"
                    };
                    self.server.channels.delivery(
                        &key,
                        status,
                        None,
                        &text,
                        Some(&e.to_string()),
                    )?;
                    return Err(e);
                }
            }
        }
        Ok(messages)
    }

    async fn relay(&self, job: &Value, to: &Address) -> Result<()> {
        let id = job["id"].as_str().unwrap();
        let sid = job["session"].as_str().unwrap();
        if !self.server.channels.selectable(to,sid)? {return Err(anyhow!(super::state::Denied));}
        let mut stream: Relay = self
            .server
            .channels
            .relay(id)?
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        if stream.cursor == 0 {
            stream.cursor = job["payload"]["dispatch_seq"].as_i64().unwrap_or(0);
        }
        let mut events = self.server.subscribe_events();
        let started = tokio::time::Instant::now();
        let mut idle = None;
        loop {
            while let Ok(ev) = events.try_recv() {
                if ev["session"] != sid {
                    continue;
                }
                if ev["type"] == "delta" && ev["kind"] == "text" {
                    stream.delta(ev["text"].as_str().unwrap_or(""), ev["message_id"].as_str());
                } else if ev["type"] == "tool" {
                    stream.boundary();
                }
            }
            let rows = self.op(
                to,
                json!({"op":"history","name":sid,"after":stream.cursor}),
            )
            .await?;
            for row in rows.as_array().into_iter().flatten() {
                let seq = row["seq"].as_i64().unwrap_or(0);
                stream.cursor = stream.cursor.max(seq);
                match row["role"].as_str() {
                    Some("agent" | "error") => stream.row(seq, row["text"].as_str().unwrap_or("")),
                    Some("user") => {
                        stream.boundary();
                    }
                    _ => {}
                }
            }
            self.server
                .channels
                .save_relay(id, &serde_json::to_value(&stream)?)?;
            for (i, segment) in stream.segments.iter().enumerate() {
                let visible = Relay::visible(segment);
                if !visible.is_empty() {
                    self.publish(to, &format!("{id}:segment:{i}"), visible)
                        .await?;
                }
            }
            self.sync_prompts(to, sid).await?;
            let s = self.op(to, json!({"op":"status","name":sid})).await?;
            if s["status"] == "stopped" {
                break;
            }
            if matches!(s["status"].as_str(), Some("idle" | "error" | "new"))
                && stream.segments.iter().any(|s| s.row.is_some())
            {
                if idle.is_none() {
                    idle = Some(tokio::time::Instant::now());
                }
                if idle.is_some_and(|t: tokio::time::Instant| t.elapsed() > Duration::from_secs(2))
                {
                    break;
                }
            } else {
                idle = None;
            }
            if started.elapsed() > Duration::from_secs(900) {
                bail!("Timed out waiting for agent response (15 minutes).");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        self.server.channels.job(id, "done", None, None)?;
        Ok(())
    }
}
