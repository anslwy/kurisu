//! SQLite session store. A session is a named peer; the Lead is just one of them.

use std::path::Path;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Session {
    pub name: String,
    pub role: String, // lead | worker
    pub harness: String,
    pub cwd: String,
    /// The harness's own id (claude session uuid / codex thread id). Set once.
    pub harness_session_id: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    pub status: String,
    /// What this session is for (set at creation, kept current by the session itself).
    pub description: String,
    /// The session's own short status / current task (set_my_status).
    pub note: String,
    /// Unix seconds of the last message to or from this session.
    pub last_activity: Option<i64>,
    /// Lineage only: the session that created this one (NULL = created by the user).
    pub created_by_session: Option<String>,
    /// active | paused | archived (set by the user or an ancestor).
    pub lifecycle: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Message {
    pub seq: i64,
    pub session: String,
    pub role: String, // user | agent | peer:<name>
    pub text: String,
}

pub struct Store {
    db: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  name TEXT PRIMARY KEY,
  harness TEXT NOT NULL CHECK (harness IN ('claude','codex','acp')),
  cwd TEXT NOT NULL,
  harness_session_id TEXT UNIQUE,
  role TEXT NOT NULL DEFAULT 'worker' CHECK (role IN ('lead','worker')),
  model TEXT NOT NULL DEFAULT 'claude-haiku-5-5', effort TEXT,
  mcp_json TEXT NOT NULL DEFAULT '[]',
  status TEXT NOT NULL DEFAULT 'stopped',
  description TEXT NOT NULL DEFAULT '',
  note TEXT NOT NULL DEFAULT '',
  created_by_session TEXT REFERENCES sessions(name) ON UPDATE CASCADE,
  lifecycle TEXT NOT NULL DEFAULT 'active' CHECK (lifecycle IN ('active','paused','archived')),
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL REFERENCES sessions(name) ON UPDATE CASCADE,
  role TEXT NOT NULL,
  text TEXT NOT NULL,
  at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS messages_by_session ON messages(session, seq);
CREATE TABLE IF NOT EXISTS audit (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  actor TEXT NOT NULL,      -- 'user' or a session id
  target TEXT NOT NULL,
  field TEXT NOT NULL,
  old TEXT, new TEXT,
  at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS board (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  author TEXT NOT NULL,
  text TEXT NOT NULL,
  at INTEGER NOT NULL DEFAULT (unixepoch())
);
";

/// Default model for every session in Milestone 1 (cheap testing).
pub const DEFAULT_MODEL: &str = "claude-haiku-5-5";

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db })
    }

    #[allow(dead_code)]
    pub fn create(&self, name: &str, harness: &str, cwd: &str) -> Result<Session> {
        self.create_with(name, harness, cwd, "worker", DEFAULT_MODEL)
    }

    #[allow(dead_code)]
    pub fn create_with(&self, name: &str, harness: &str, cwd: &str, role: &str, model: &str) -> Result<Session> {
        self.create_full(name, harness, cwd, role, model, "")
    }

    pub fn create_full(&self, name: &str, harness: &str, cwd: &str, role: &str, model: &str, description: &str) -> Result<Session> {
        self.create_child(name, harness, cwd, role, model, description, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_child(&self, name: &str, harness: &str, cwd: &str, role: &str, model: &str, description: &str,
                        created_by: Option<&str>) -> Result<Session> {
        if model.is_empty() || model.contains(char::is_whitespace) {
            bail!("invalid model {model:?}");
        }
        if !valid_name(name) {
            bail!("invalid session name {name:?}");
        }
        if !Path::new(cwd).is_absolute() {
            bail!("cwd must be absolute");
        }
        self.db.execute(
            "INSERT INTO sessions(name, harness, cwd, role, model, description, created_by_session) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![name, harness, cwd, role, model, description, created_by],
        )?;
        Ok(self.get(name)?.unwrap())
    }

    pub fn get(&self, name: &str) -> Result<Option<Session>> {
        Ok(self
            .db
            .query_row(
                "SELECT name, role, harness, cwd, harness_session_id, model, effort, status, description, note, \
                 (SELECT MAX(at) FROM messages m WHERE m.session = sessions.name), created_by_session, lifecycle \
                 FROM sessions WHERE name=?1",
                params![name],
                |r| {
                    Ok(Session {
                        name: r.get(0)?,
                        role: r.get(1)?,
                        harness: r.get(2)?,
                        cwd: r.get(3)?,
                        harness_session_id: r.get(4)?,
                        model: r.get(5)?,
                        effort: r.get(6)?,
                        status: r.get(7)?,
                        description: r.get(8)?,
                        note: r.get(9)?,
                        last_activity: r.get(10)?,
                        created_by_session: r.get(11)?,
                        lifecycle: r.get(12)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn list(&self) -> Result<Vec<Session>> {
        let mut st = self.db.prepare("SELECT name FROM sessions ORDER BY rowid")?;
        let names: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        names.iter().map(|n| Ok(self.get(n)?.unwrap())).collect()
    }

    /// Record the harness id once; a different id later is refused (restart identity).
    pub fn bind_harness_id(&self, name: &str, id: &str) -> Result<()> {
        let s = self.get(name)?.ok_or_else(|| anyhow::anyhow!("no session {name}"))?;
        match s.harness_session_id.as_deref() {
            Some(existing) if existing == id => Ok(()),
            Some(existing) => bail!("session {name} is bound to {existing}; refusing to rebind to {id}"),
            None => {
                self.db.execute("UPDATE sessions SET harness_session_id=?2 WHERE name=?1", params![name, id])?;
                Ok(())
            }
        }
    }

    pub fn set_status(&self, name: &str, status: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET status=?2 WHERE name=?1", params![name, status])?;
        Ok(())
    }

    /// Is `ancestor` the creator, or the creator's creator, ... of `target`?
    pub fn is_ancestor(&self, ancestor: &str, target: &str) -> Result<bool> {
        let mut cur = self.get(target)?.and_then(|s| s.created_by_session);
        let mut hops = 0;
        while let Some(c) = cur {
            if c == ancestor {
                return Ok(true);
            }
            hops += 1;
            if hops > 64 {
                bail!("lineage loop at {target}");
            }
            cur = self.get(&c)?.and_then(|s| s.created_by_session);
        }
        Ok(false)
    }

    pub fn children(&self, name: &str) -> Result<Vec<String>> {
        let mut st = self.db.prepare("SELECT name FROM sessions WHERE created_by_session=?1 ORDER BY rowid")?;
        let rows = st.query_map(params![name], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Apply one field change with an audit record. `actor` is 'user' or a session id.
    /// Authorization is the caller's job (see `Server::authorize_update`).
    pub fn update_field(&self, actor: &str, target: &str, field: &str, value: &str) -> Result<()> {
        let cur = self.get(target)?.ok_or_else(|| anyhow::anyhow!("no session {target}"))?;
        let (column, old) = match field {
            "model" => {
                if value.is_empty() || value.contains(char::is_whitespace) {
                    bail!("invalid model");
                }
                ("model", cur.model.clone())
            }
            "effort" => {
                if !["low", "medium", "high", "xhigh", "max"].contains(&value) {
                    bail!("effort must be low|medium|high|xhigh|max");
                }
                ("effort", cur.effort.clone().unwrap_or_default())
            }
            "description" => ("description", cur.description.clone()),
            "task" => ("note", cur.note.clone()),
            "role" => {
                if !["lead", "worker"].contains(&value) {
                    bail!("role must be lead|worker");
                }
                ("role", cur.role.clone())
            }
            "lifecycle" => {
                if !["active", "paused", "archived"].contains(&value) {
                    bail!("lifecycle must be active|paused|archived");
                }
                ("lifecycle", cur.lifecycle.clone())
            }
            "name" => {
                if !valid_name(value) {
                    bail!("invalid name {value:?}");
                }
                ("name", cur.name.clone())
            }
            f => bail!("field {f} cannot be updated"),
        };
        let tx = self.db.unchecked_transaction()?;
        tx.execute(&format!("UPDATE sessions SET {column}=?2 WHERE name=?1"), params![target, clip(value, 500)])?;
        tx.execute(
            "INSERT INTO audit(actor, target, field, old, new) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![actor, target, field, old, value],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn audit(&self, target: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let mut st = self.db.prepare(
            "SELECT seq, actor, target, field, old, new, at FROM audit WHERE ?1 IS NULL OR target=?1 ORDER BY seq",
        )?;
        let rows = st.query_map(params![target], |r| {
            Ok(serde_json::json!({"seq": r.get::<_, i64>(0)?, "actor": r.get::<_, String>(1)?, "target": r.get::<_, String>(2)?,
                "field": r.get::<_, String>(3)?, "old": r.get::<_, Option<String>>(4)?, "new": r.get::<_, Option<String>>(5)?,
                "at": r.get::<_, i64>(6)?}))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_description(&self, name: &str, text: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET description=?2 WHERE name=?1", params![name, clip(text, 500)])?;
        Ok(())
    }

    pub fn set_note(&self, name: &str, text: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET note=?2 WHERE name=?1", params![name, clip(text, 300)])?;
        Ok(())
    }

    pub fn board_post(&self, author: &str, text: &str) -> Result<i64> {
        self.db.execute("INSERT INTO board(author, text) VALUES (?1, ?2)", params![author, clip(text, 4000)])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn board_read(&self, tail: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, author, 'board', text FROM (SELECT * FROM board ORDER BY seq DESC LIMIT ?1) ORDER BY seq",
        )?;
        let rows = st.query_map(params![tail], |r| {
            Ok(Message { seq: r.get(0)?, session: r.get(1)?, role: r.get(2)?, text: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The last `tail` messages, skipping the newest `offset`.
    pub fn history_tail(&self, session: &str, tail: i64, offset: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, session, role, text FROM (SELECT * FROM messages WHERE session=?1 ORDER BY seq DESC LIMIT ?2 OFFSET ?3) ORDER BY seq",
        )?;
        let rows = st.query_map(params![session, tail, offset], |r| {
            Ok(Message { seq: r.get(0)?, session: r.get(1)?, role: r.get(2)?, text: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn append(&self, session: &str, role: &str, text: &str) -> Result<i64> {
        self.db.execute("INSERT INTO messages(session, role, text) VALUES (?1, ?2, ?3)", params![session, role, text])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn history(&self, session: &str, after: i64, limit: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, session, role, text FROM messages WHERE session=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = st.query_map(params![session, after, limit], |r| {
            Ok(Message { seq: r.get(0)?, session: r.get(1)?, role: r.get(2)?, text: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_list_bind_history() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("k.db")).unwrap();
        s.create("lead", "claude", "/w/lead").unwrap();
        s.create("dev-app", "codex", "/w/app").unwrap();
        assert!(s.create("lead", "claude", "/x").is_err(), "unique names");
        assert!(s.create("Bad Name", "claude", "/x").is_err());
        assert!(s.create("ok", "claude", "rel/path").is_err());
        assert!(s.create("ok", "gemini", "/x").is_err(), "harness checked by schema");
        assert_eq!(s.get("lead").unwrap().unwrap().model, "claude-haiku-5-5", "Haiku by default");
        assert_eq!(s.get("lead").unwrap().unwrap().role, "worker");
        s.create_with("boss", "claude", "/w/b", "lead", "claude-opus-5-5").unwrap();
        assert!(s.create_with("x", "claude", "/w/x", "admin", "m").is_err(), "role checked");
        assert!(s.create_with("y", "claude", "/w/y", "worker", "two words").is_err());
        assert_eq!(s.list().unwrap().len(), 3);
        assert_eq!(s.list().unwrap()[..2].iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["lead", "dev-app"]);
        s.bind_harness_id("lead", "u1").unwrap();
        s.bind_harness_id("lead", "u1").unwrap();
        assert!(s.bind_harness_id("lead", "u2").is_err(), "restart identity");
        s.append("lead", "user", "hi").unwrap();
        let a = s.append("lead", "agent", "hello").unwrap();
        s.append("dev-app", "peer:lead", "do x").unwrap();
        let h = s.history("lead", 0, 10).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(s.history("lead", a, 10).unwrap().len(), 0);
        s.set_description("dev-app", "builds the app").unwrap();
        s.set_note("dev-app", "writing tests").unwrap();
        let d = s.get("dev-app").unwrap().unwrap();
        assert_eq!((d.description.as_str(), d.note.as_str()), ("builds the app", "writing tests"));
        assert!(d.last_activity.is_some());
        s.board_post("lead", "standup at 9").unwrap();
        s.board_post("dev-app", "blocked on X").unwrap();
        assert_eq!(s.board_read(1).unwrap()[0].text, "blocked on X");
        assert_eq!(s.history_tail("lead", 1, 0).unwrap()[0].text, "hello");
        assert_eq!(s.history_tail("lead", 1, 1).unwrap()[0].text, "hi");
        // lineage and authority
        s.create_child("w1", "claude", "/w/w1", "worker", DEFAULT_MODEL, "", Some("boss")).unwrap();
        s.create_child("w1a", "claude", "/w/w1a", "worker", DEFAULT_MODEL, "", Some("w1")).unwrap();
        assert!(s.is_ancestor("boss", "w1a").unwrap(), "grandparent");
        assert!(s.is_ancestor("w1", "w1a").unwrap());
        assert!(!s.is_ancestor("w1a", "boss").unwrap(), "no upward authority");
        assert!(!s.is_ancestor("lead", "w1").unwrap(), "not in the chain");
        assert_eq!(s.children("boss").unwrap(), ["w1"]);
        s.update_field("boss", "w1", "effort", "low").unwrap();
        assert!(s.update_field("boss", "w1", "effort", "extreme").is_err());
        assert!(s.update_field("boss", "w1", "cwd", "/etc").is_err(), "cwd is not updatable");
        s.update_field("boss", "w1", "name", "w-one").unwrap();
        assert_eq!(s.get("w1a").unwrap().unwrap().created_by_session.as_deref(), Some("w-one"), "rename cascades");
        let a = s.audit(Some("w1")).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0]["field"], "effort");
        assert_eq!(a[0]["actor"], "boss");
        drop(s);
        let s = Store::open(&dir.path().join("k.db")).unwrap();
        assert_eq!(s.get("lead").unwrap().unwrap().harness_session_id.as_deref(), Some("u1"), "persists");
    }
}
