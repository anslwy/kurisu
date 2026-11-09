//! Gateway receipts and recovery cursors commit together before sequence ACKs advance.
use super::*;

fn enqueue(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    event: &str,
    data: &Value,
) -> Result<bool> {
    let kind = match event {
        "MESSAGE_CREATE" => "message",
        "INTERACTION_CREATE" => "interaction",
        _ => return Ok(false),
    };
    let id = data["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing Discord event ID"))?;
    let destination = data["channel_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing Discord destination"))?;
    Ok(tx.execute("INSERT OR IGNORE INTO discord_events(account,key,platform_id,destination,event,payload) VALUES(?1,?2,?3,?4,?5,?6)",params![account,format!("{kind}:{id}"),id,destination,event,data.to_string()])?>0)
}
fn cursor(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    destination: &str,
    guild: Option<&str>,
    id: &str,
) -> Result<()> {
    if id.parse::<u64>().is_err() {
        return Ok(());
    }
    tx.execute("INSERT INTO discord_cursors VALUES(?1,?2,?3,?4) ON CONFLICT(account,destination) DO UPDATE SET watermark=excluded.watermark WHERE length(excluded.watermark)>length(discord_cursors.watermark) OR (length(excluded.watermark)=length(discord_cursors.watermark) AND excluded.watermark>discord_cursors.watermark)",params![account,destination,guild,id])?;
    Ok(())
}
impl State {
    pub fn gateway_checkpoint(&self, account: &str) -> Result<Option<Value>> {
        let data: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT data FROM discord_gateway WHERE account=?1",
                [account],
                |r| r.get(0),
            )
            .optional()?;
        Ok(data.map(|s| serde_json::from_str(&s)).transpose()?)
    }
    pub fn gateway_record(
        &self,
        account: &str,
        checkpoint: &Value,
        event: &str,
        data: &Value,
    ) -> Result<bool> {
        self.gateway_record_inner(account,checkpoint,event,data,None)
    }
    pub fn gateway_reject(&self,account:&str,checkpoint:&Value,event:&str,data:&Value,reason:&str)->Result<bool> {
        self.gateway_record_inner(account,checkpoint,event,data,Some(reason))
    }
    fn gateway_record_inner(&self,account:&str,checkpoint:&Value,event:&str,data:&Value,rejection:Option<&str>)->Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let inserted = enqueue(&tx, account, event, data)?;
        if let Some(reason)=rejection.filter(|_|inserted) {
            let kind=if event=="MESSAGE_CREATE"{"message"}else{"interaction"};
            let key=format!("{kind}:{}",data["id"].as_str().unwrap());
            tx.execute("UPDATE discord_events SET status='rejected',error=?3 WHERE account=?1 AND key=?2 AND status='pending'",params![account,key,reason])?;
        }
        if event == "MESSAGE_CREATE" {
            cursor(
                &tx,
                account,
                data["channel_id"].as_str().unwrap(),
                data["guild_id"].as_str(),
                data["id"].as_str().unwrap(),
            )?;
        }
        tx.execute("INSERT INTO discord_gateway VALUES(?1,?2) ON CONFLICT(account) DO UPDATE SET data=excluded.data",params![account,checkpoint.to_string()])?;
        tx.commit()?;
        Ok(inserted)
    }
    pub fn gateway_clear(&self, account: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM discord_gateway WHERE account=?1", [account])?;
        Ok(())
    }
    pub fn gateway_pending(&self, account: &str) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut q=db.prepare("SELECT key,event,payload FROM discord_events e WHERE account=?1 AND status='pending' AND NOT EXISTS(SELECT 1 FROM discord_backfill b WHERE b.account=e.account AND b.destination=e.destination) ORDER BY length(platform_id),platform_id LIMIT 128")?;
        let rows=q.query_map([account],|r|Ok(json!({"key":r.get::<_,String>(0)?,"event":r.get::<_,String>(1)?,"data":serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(Value::Null)})))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn gateway_done(
        &self,
        account: &str,
        key: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE discord_events SET status=?3,error=?4 WHERE account=?1 AND key=?2",
            params![account, key, status, error],
        )?;
        Ok(())
    }
    pub fn gateway_watch(
        &self,
        account: &str,
        destination: &str,
        guild: Option<&str>,
        id: &str,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        cursor(&tx, account, destination, guild, id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn backfill_begin(&self, account: &str) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT OR IGNORE INTO discord_backfill(account,destination,guild,lower_bound) SELECT account,destination,guild,watermark FROM discord_cursors WHERE account=?1",[account])?;
        Ok(())
    }
    pub fn backfills(&self, account: &str) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut q=db.prepare("SELECT destination,guild,lower_bound,before_id FROM discord_backfill WHERE account=?1 ORDER BY destination")?;
        let rows=q.query_map([account],|r|Ok(json!({"destination":r.get::<_,String>(0)?,"guild":r.get::<_,Option<String>>(1)?,"lower":r.get::<_,String>(2)?,"before":r.get::<_,Option<String>>(3)?})))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn backfill_page(
        &self,
        account: &str,
        job: &Value,
        messages: &[Value],
        before: Option<&str>,
        done: bool,
    ) -> Result<()> {
        let destination = job["destination"].as_str().unwrap();
        let lower = job["lower"].as_str().unwrap().parse::<u64>()?;
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        for message in messages {
            let id = message["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing history message ID"))?;
            if message["channel_id"] != destination {
                bail!("History response changed destination");
            }
            if id.parse::<u64>()? <= lower {
                continue;
            }
            let mut message = message.clone();
            message["guild_id"] = job["guild"].clone();
            enqueue(&tx, account, "MESSAGE_CREATE", &message)?;
        }
        if done {
            let latest:Option<String>=tx.query_row("SELECT platform_id FROM discord_events WHERE account=?1 AND destination=?2 AND event='MESSAGE_CREATE' ORDER BY length(platform_id) DESC,platform_id DESC LIMIT 1",params![account,destination],|r|r.get(0)).optional()?;
            if let Some(latest)=latest { cursor(&tx,account,destination,job["guild"].as_str(),&latest)?; }
            tx.execute(
                "DELETE FROM discord_backfill WHERE account=?1 AND destination=?2",
                params![account, destination],
            )?;
        } else {
            tx.execute(
                "UPDATE discord_backfill SET before_id=?3 WHERE account=?1 AND destination=?2",
                params![account, destination, before],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl State {
    pub fn gateway_seed_inbox(&self, account: &str) -> Result<()> {
        let rows = {
            let db = self.db.lock().unwrap();
            let mut q=db.prepare("SELECT destination,substr(id,9),json_extract(payload,'$.guild') FROM inbox WHERE id LIKE 'message:%'")?;
            let rows = q.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (destination, id, guild) in rows {
            self.gateway_watch(account, &destination, guild.as_deref(), &id)?;
        }
        Ok(())
    }
    pub fn gateway_watch_new(
        &self,
        account: &str,
        destination: &str,
        guild: Option<&str>,
        id: &str,
    ) -> Result<()> {
        if id.parse::<u64>().is_ok() {
            self.db.lock().unwrap().execute(
                "INSERT OR IGNORE INTO discord_cursors VALUES(?1,?2,?3,?4)",
                params![account, destination, guild, id],
            )?;
        }
        Ok(())
    }
}
