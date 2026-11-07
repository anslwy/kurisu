//! SQLite session store. A session is a named peer; the Lead is just one of them.

use std::path::Path;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Session {
    pub name: String,
    pub harness: String,
    pub cwd: String,
    /// The harness's own id (claude session uuid / codex thread id). Set once.
    pub harness_session_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: String,
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
  model TEXT, effort TEXT,
  mcp_json TEXT NOT NULL DEFAULT '[]',
  status TEXT NOT NULL DEFAULT 'stopped',
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL REFERENCES sessions(name),
  role TEXT NOT NULL,
  text TEXT NOT NULL,
  at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS messages_by_session ON messages(session, seq);
";

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

    pub fn create(&self, name: &str, harness: &str, cwd: &str) -> Result<Session> {
        if !valid_name(name) {
            bail!("invalid session name {name:?}");
        }
        if !Path::new(cwd).is_absolute() {
            bail!("cwd must be absolute");
        }
        self.db.execute("INSERT INTO sessions(name, harness, cwd) VALUES (?1, ?2, ?3)", params![name, harness, cwd])?;
        Ok(self.get(name)?.unwrap())
    }

    pub fn get(&self, name: &str) -> Result<Option<Session>> {
        Ok(self
            .db
            .query_row(
                "SELECT name, harness, cwd, harness_session_id, model, effort, status FROM sessions WHERE name=?1",
                params![name],
                |r| {
                    Ok(Session {
                        name: r.get(0)?,
                        harness: r.get(1)?,
                        cwd: r.get(2)?,
                        harness_session_id: r.get(3)?,
                        model: r.get(4)?,
                        effort: r.get(5)?,
                        status: r.get(6)?,
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
        assert_eq!(s.list().unwrap().iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["lead", "dev-app"]);
        s.bind_harness_id("lead", "u1").unwrap();
        s.bind_harness_id("lead", "u1").unwrap();
        assert!(s.bind_harness_id("lead", "u2").is_err(), "restart identity");
        s.append("lead", "user", "hi").unwrap();
        let a = s.append("lead", "agent", "hello").unwrap();
        s.append("dev-app", "peer:lead", "do x").unwrap();
        let h = s.history("lead", 0, 10).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(s.history("lead", a, 10).unwrap().len(), 0);
        drop(s);
        let s = Store::open(&dir.path().join("k.db")).unwrap();
        assert_eq!(s.get("lead").unwrap().unwrap().harness_session_id.as_deref(), Some("u1"), "persists");
    }
}
