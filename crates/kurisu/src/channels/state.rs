//! Durable person bindings, pairing, inbox and per-destination outbox.
use super::{now, Address};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{path::Path, sync::Mutex};

pub struct State {
    db: Mutex<Connection>,
}

impl State {
    pub fn open(home: &Path) -> Result<Self> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::create_dir_all(home)?;
        let path = home.join("channels.sqlite3");
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS people(id TEXT PRIMARY KEY, session TEXT);
          CREATE TABLE IF NOT EXISTS identities(channel TEXT, user TEXT, person TEXT REFERENCES people(id), destination TEXT,
            PRIMARY KEY(channel,user));
          CREATE TABLE IF NOT EXISTS scopes(scope TEXT PRIMARY KEY, session TEXT);
          CREATE TABLE IF NOT EXISTS routes(session TEXT PRIMARY KEY, person TEXT, channel TEXT, destination TEXT);
          CREATE TABLE IF NOT EXISTS pairings(code TEXT PRIMARY KEY, person TEXT, user TEXT, expires INTEGER);
          CREATE TABLE IF NOT EXISTS inbox(seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE, person TEXT, destination TEXT,
            session TEXT, payload TEXT, status TEXT DEFAULT 'pending', force INTEGER DEFAULT 0, error TEXT);
          CREATE TABLE IF NOT EXISTS outbox(key TEXT PRIMARY KEY, person TEXT, destination TEXT, content TEXT,
            message TEXT, status TEXT DEFAULT 'prepared', error TEXT);
          CREATE TABLE IF NOT EXISTS relay(id TEXT PRIMARY KEY, data TEXT);
          CREATE TABLE IF NOT EXISTS actions(id TEXT PRIMARY KEY, person TEXT, data TEXT, expires INTEGER);")?;
        let state = Self { db: Mutex::new(db) };
        state.import_telegram(home)?;
        Ok(state)
    }

    pub fn import_telegram(&self, home: &Path) -> Result<()> {
        let Some(cfg) = crate::telegram::load_config(home) else {
            return Ok(());
        };
        let value: Value = std::fs::read(home.join("telegram/state.json"))
            .ok()
            .and_then(|x| serde_json::from_slice(&x).ok())
            .unwrap_or(json!({}));
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO people(id,session) VALUES('owner',?1)",
            params![value["session_id"].as_str()],
        )?;
        db.execute("INSERT OR IGNORE INTO identities(channel,user,person,destination) VALUES('telegram',?1,'owner',?1)", [&cfg.chat_id])?;
        for (scope, session) in value["scopes"].as_object().into_iter().flatten() {
            db.execute(
                "INSERT OR IGNORE INTO scopes VALUES(?1,?2)",
                params![scope, session.as_str()],
            )?;
        }
        Ok(())
    }

    pub fn person(&self, channel: &str, user: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT person FROM identities WHERE channel=?1 AND user=?2",
                params![channel, user],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn session(&self, person: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT session FROM people WHERE id=?1", [person], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten())
    }
    pub fn select(&self, person: &str, session: &str) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO people(id,session) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET session=excluded.session", params![person,session])?;
        Ok(())
    }
    pub fn scope(&self, scope: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT session FROM scopes WHERE scope=?1", [scope], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub fn select_scope(&self, scope: &str, session: &str) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO scopes VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET session=excluded.session", params![scope,session])?;
        Ok(())
    }
    pub fn replaced(&self, old: &str, new: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE people SET session=?2 WHERE session=?1",
            params![old, new],
        )?;
        tx.execute(
            "UPDATE scopes SET session=?2 WHERE session=?1",
            params![old, new],
        )?;
        tx.execute(
            "UPDATE routes SET session=?2 WHERE session=?1",
            params![old, new],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn route(&self, session: &str, to: &Address) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO routes VALUES(?1,?2,?3,?4) ON CONFLICT(session) DO UPDATE SET person=excluded.person,channel=excluded.channel,destination=excluded.destination", params![session,to.person,to.channel,to.destination])?;
        Ok(())
    }
    pub fn destination(&self, session: &str) -> Result<Option<Address>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT channel,destination,person FROM routes WHERE session=?1",
                [session],
                |r| {
                    Ok(Address {
                        channel: r.get(0)?,
                        destination: r.get(1)?,
                        person: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }
    pub fn addresses(&self) -> Result<Vec<Address>> {
        let db = self.db.lock().unwrap();
        let mut q=db.prepare("SELECT channel,destination,person FROM identities WHERE channel='discord' AND destination IS NOT NULL")?;
        let rows = q.query_map([], |r| {
            Ok(Address {
                channel: r.get(0)?,
                destination: r.get(1)?,
                person: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn mint(
        &self,
        person: &str,
        session: &str,
        user: Option<&str>,
        ttl: i64,
    ) -> Result<String> {
        if person.is_empty()
            || person.len() > 80
            || !person
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            bail!("person must be a short name containing letters, digits, '-' or '_'");
        }
        let code = uuid::Uuid::new_v4().simple().to_string();
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("INSERT INTO people(id,session) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET session=COALESCE(people.session,excluded.session)", params![person,session])?;
        let current: Option<String> =
            tx.query_row("SELECT session FROM people WHERE id=?1", [person], |r| {
                r.get(0)
            })?;
        if current.as_deref() != Some(session) {
            bail!("person already has a different active session; pair with that session to link identities");
        }
        tx.execute(
            "DELETE FROM pairings WHERE person=?1 OR expires<?2",
            params![person, now()],
        )?;
        tx.execute(
            "INSERT INTO pairings VALUES(?1,?2,?3,?4)",
            params![code, person, user, now() + ttl],
        )?;
        tx.commit()?;
        Ok(code)
    }
    pub fn redeem(&self, code: &str, user: &str, destination: &str) -> Result<String> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let pair: Option<(String, Option<String>, i64)> = tx
            .query_row(
                "SELECT person,user,expires FROM pairings WHERE code=?1",
                [code],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((person, expected, expires)) = pair else {
            bail!("Invalid or expired pairing code.");
        };
        if expires <= now() || expected.as_deref().is_some_and(|u| u != user) {
            bail!("Invalid or expired pairing code.");
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT person FROM identities WHERE channel='discord' AND user=?1",
                [user],
                |r| r.get(0),
            )
            .optional()?;
        if existing.as_ref().is_some_and(|p| p != &person) {
            bail!("This Discord identity is already paired to another person.");
        }
        tx.execute("INSERT INTO identities VALUES('discord',?1,?2,?3) ON CONFLICT(channel,user) DO UPDATE SET destination=excluded.destination",params![user,person,destination])?;
        tx.execute("DELETE FROM pairings WHERE code=?1", [code])?;
        tx.commit()?;
        Ok(person)
    }
    pub fn inbound(
        &self,
        id: &str,
        to: &Address,
        session: Option<&str>,
        payload: &Value,
    ) -> Result<bool> {
        Ok(self.db.lock().unwrap().execute("INSERT OR IGNORE INTO inbox(id,person,destination,session,payload) VALUES(?1,?2,?3,?4,?5)", params![id,to.person,to.destination,session,payload.to_string()])?>0)
    }
    pub fn seen(&self, id: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM inbox WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?)
    }
    pub fn bind_job(&self, id: &str, session: &str) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE inbox SET session=?2,payload=json_set(payload,'$.session_bound',json('true')) WHERE id=?1 AND status='pending'",params![id,session])?;
        Ok(())
    }
    pub fn posted(&self, session: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM inbox WHERE session=?1 AND status='posted')",
            [session],
            |r| r.get(0),
        )?)
    }
    pub fn health(&self) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let failed: i64 = db.query_row(
            "SELECT COUNT(*) FROM outbox WHERE status='failed'",
            [],
            |r| r.get(0),
        )?;
        let uncertain: i64 = db.query_row(
            "SELECT COUNT(*) FROM outbox WHERE status IN ('sending','uncertain')",
            [],
            |r| r.get(0),
        )?;
        let failed_jobs: i64 = db.query_row(
            "SELECT COUNT(*) FROM inbox WHERE status='failed'",
            [],
            |r| r.get(0),
        )?;
        Ok(
            json!({"failed_deliveries":failed,"uncertain_deliveries":uncertain,"failed_jobs":failed_jobs}),
        )
    }
    pub fn job_status(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT status FROM inbox WHERE id=?1", [id], |r| r.get(0))
            .optional()?)
    }
    pub fn jobs(&self) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut q=db.prepare("SELECT id,person,destination,session,payload,status,force FROM inbox WHERE status IN ('pending','posted') ORDER BY seq")?;
        let rows=q.query_map([], |r|Ok(json!({"id":r.get::<_,String>(0)?,"person":r.get::<_,String>(1)?,"destination":r.get::<_,String>(2)?,"session":r.get::<_,Option<String>>(3)?,"payload":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"status":r.get::<_,String>(5)?,"force":r.get::<_,bool>(6)?})))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn job(
        &self,
        id: &str,
        status: &str,
        payload: Option<&Value>,
        error: Option<&str>,
    ) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE inbox SET status=?2,payload=COALESCE(?3,payload),error=?4 WHERE id=?1",
            params![id, status, payload.map(Value::to_string), error],
        )?;
        Ok(())
    }
    pub fn force(&self, person: &str) -> Result<usize> {
        Ok(self.db.lock().unwrap().execute("UPDATE inbox SET force=1 WHERE person=?1 AND status='pending' AND (json_extract(payload,'$.text') NOT LIKE '/%' OR json_array_length(payload,'$.attachments')>0) AND json_extract(payload,'$.action') IS NULL", [person])?)
    }
    pub fn relay(&self, id: &str) -> Result<Option<Value>> {
        let data: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT data FROM relay WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        Ok(data.and_then(|x| serde_json::from_str(&x).ok()))
    }
    pub fn save_relay(&self, id: &str, data: &Value) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO relay VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            params![id, data.to_string()],
        )?;
        Ok(())
    }
    pub fn prepare(&self, key: &str, to: &Address, text: &str) -> Result<Value> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO outbox(key,person,destination,content) VALUES(?1,?2,?3,?4)",
            params![key, to.person, to.destination, text],
        )?;
        Ok(db.query_row("SELECT message,status,content FROM outbox WHERE key=?1",[key],|r|Ok(json!({"message":r.get::<_,Option<String>>(0)?,"status":r.get::<_,String>(1)?,"content":r.get::<_,String>(2)?})))?)
    }
    pub fn delivery(
        &self,
        key: &str,
        status: &str,
        message: Option<&str>,
        text: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE outbox SET status=?2,message=COALESCE(?3,message),content=?4,error=?5 WHERE key=?1", params![key,status,message,text,error])?;
        Ok(())
    }
    pub fn action(&self, person: &str, data: &Value) -> Result<String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.db.lock().unwrap().execute(
            "INSERT INTO actions VALUES(?1,?2,?3,?4)",
            params![id, person, data.to_string(), now() + 86400],
        )?;
        Ok(id)
    }
    pub fn peek_action(&self, person: &str, id: &str) -> Result<Value> {
        let data: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT data FROM actions WHERE id=?1 AND person=?2 AND expires>?3",
                params![id, person, now()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(serde_json::from_str(&data.ok_or_else(|| {
            anyhow::anyhow!("This action expired or belongs to another person.")
        })?)?)
    }
    pub fn forget_action(&self, person: &str, id: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "DELETE FROM actions WHERE id=?1 AND person=?2",
            params![id, person],
        )?;
        Ok(())
    }
    pub fn pending_cards(&self) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT data FROM relay WHERE id LIKE 'ui:%'")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        let mut cards = Vec::new();
        for row in rows {
            let v: Value = serde_json::from_str(&row?)?;
            if v["status"] == "pending" {
                cards.push(v);
            }
        }
        Ok(cards)
    }
    pub fn take_action(&self, person: &str, id: &str) -> Result<Value> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let data: Option<String> = tx
            .query_row(
                "SELECT data FROM actions WHERE id=?1 AND person=?2 AND expires>?3",
                params![id, person, now()],
                |r| r.get(0),
            )
            .optional()?;
        let data =
            data.ok_or_else(|| anyhow::anyhow!("This model choice expired; run /model again."))?;
        tx.execute("DELETE FROM actions WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(serde_json::from_str(&data)?)
    }
}
