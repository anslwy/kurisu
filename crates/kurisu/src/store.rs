//! SQLite session store. Every session has a UUID `id` (stable forever) and a
//! human `name` (unique among non-archived sessions; renamable). All lookups
//! accept either; references between rows (messages, lineage, audit) use ids.

use std::path::Path;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// Default model for every session in Milestone 1 (cheap testing).
pub const DEFAULT_MODEL: &str = "claude-haiku-5-5";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub role: String, // lead | worker
    pub harness: String,
    pub cwd: String,
    /// The harness's own id (claude session uuid / codex thread id). Set once.
    pub harness_session_id: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    pub status: String,
    pub description: String,
    /// The session's own short status / current task (set_my_status).
    pub note: String,
    pub last_activity: Option<i64>,
    /// Lineage only: id of the session that created this one (None = the user).
    pub created_by_session: Option<String>,
    /// Name of the creator, for display.
    pub created_by_name: Option<String>,
    pub lifecycle: String, // active | paused | archived
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Message {
    pub seq: i64,
    pub session: String, // session id (board: author name)
    pub role: String,    // user | agent | peer:<name>
    pub text: String,
    pub at: i64,
}

pub struct Store {
    db: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  harness TEXT NOT NULL CHECK (harness IN ('claude','codex','acp')),
  cwd TEXT NOT NULL,
  harness_session_id TEXT UNIQUE,
  role TEXT NOT NULL DEFAULT 'worker' CHECK (role IN ('lead','worker')),
  model TEXT NOT NULL DEFAULT 'claude-haiku-5-5', effort TEXT,
  status TEXT NOT NULL DEFAULT 'stopped',
  description TEXT NOT NULL DEFAULT '',
  note TEXT NOT NULL DEFAULT '',
  created_by_session TEXT REFERENCES sessions(id),
  lifecycle TEXT NOT NULL DEFAULT 'active' CHECK (lifecycle IN ('active','paused','archived')),
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE UNIQUE INDEX IF NOT EXISTS sessions_live_name ON sessions(name) WHERE lifecycle != 'archived';
CREATE TABLE IF NOT EXISTS messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL REFERENCES sessions(id),
  role TEXT NOT NULL,
  text TEXT NOT NULL,
  at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS messages_by_session ON messages(session, seq);
CREATE TABLE IF NOT EXISTS audit (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  actor TEXT NOT NULL,      -- 'user' or a session id
  target TEXT NOT NULL,     -- session id
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

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
}

pub fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

const COLS: &str = "s.id, s.name, s.role, s.harness, s.cwd, s.harness_session_id, s.model, s.effort, s.status, \
    s.description, s.note, (SELECT MAX(at) FROM messages m WHERE m.session = s.id), s.created_by_session, \
    (SELECT p.name FROM sessions p WHERE p.id = s.created_by_session), s.lifecycle";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        name: r.get(1)?,
        role: r.get(2)?,
        harness: r.get(3)?,
        cwd: r.get(4)?,
        harness_session_id: r.get(5)?,
        model: r.get(6)?,
        effort: r.get(7)?,
        status: r.get(8)?,
        description: r.get(9)?,
        note: r.get(10)?,
        last_activity: r.get(11)?,
        created_by_session: r.get(12)?,
        created_by_name: r.get(13)?,
        lifecycle: r.get(14)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        // Pre-UUID dev databases (sessions keyed by name) are not migrated.
        let old: bool = db
            .query_row("SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name='name' AND pk=1", [], |r| r.get::<_, i64>(0))
            .map(|n| n > 0)
            .unwrap_or(false);
        if old {
            bail!("{} uses the pre-UUID schema (M1 dev data); delete that KURISU_HOME and start fresh", path.display());
        }
        db.execute_batch(SCHEMA)?;
        Ok(Self { db })
    }

    /// Resolve a UUID or a live name to the session id.
    pub fn resolve(&self, key: &str) -> Result<Option<String>> {
        if looks_like_uuid(key) {
            return Ok(self.db.query_row("SELECT id FROM sessions WHERE id=?1", params![key], |r| r.get(0)).optional()?);
        }
        Ok(self
            .db
            .query_row("SELECT id FROM sessions WHERE name=?1 AND lifecycle != 'archived'", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn resolve_req(&self, key: &str) -> Result<String> {
        self.resolve(key)?.ok_or_else(|| anyhow::anyhow!("no session {key:?}"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(&self, name: &str, harness: &str, cwd: &str, role: &str, model: &str, description: &str,
                  created_by: Option<&str>) -> Result<Session> {
        if !valid_name(name) {
            bail!("invalid session name {name:?} (lowercase letters, digits, '-')");
        }
        if model.is_empty() || model.contains(char::is_whitespace) {
            bail!("invalid model {model:?}");
        }
        if !Path::new(cwd).is_absolute() {
            bail!("cwd must be absolute");
        }
        if self.resolve(name)?.is_some() {
            bail!("a live session is already named {name:?}");
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.db.execute(
            "INSERT INTO sessions(id, name, harness, cwd, role, model, description, created_by_session) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, name, harness, cwd, role, model, description, created_by],
        )?;
        Ok(self.get(&id)?.unwrap())
    }

    /// Get by id (use `resolve` first for names).
    pub fn get(&self, id: &str) -> Result<Option<Session>> {
        Ok(self.db.query_row(&format!("SELECT {COLS} FROM sessions s WHERE s.id=?1"), params![id], row).optional()?)
    }

    pub fn list(&self) -> Result<Vec<Session>> {
        let mut st = self.db.prepare(&format!("SELECT {COLS} FROM sessions s ORDER BY s.rowid"))?;
        let rows = st.query_map([], row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn bind_harness_id(&self, id: &str, hid: &str) -> Result<()> {
        let s = self.get(id)?.ok_or_else(|| anyhow::anyhow!("no session {id}"))?;
        match s.harness_session_id.as_deref() {
            Some(existing) if existing == hid => Ok(()),
            Some(existing) => bail!("session {} is bound to {existing}; refusing to rebind to {hid}", s.name),
            None => {
                self.db.execute("UPDATE sessions SET harness_session_id=?2 WHERE id=?1", params![id, hid])?;
                Ok(())
            }
        }
    }

    pub fn set_status(&self, id: &str, status: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET status=?2 WHERE id=?1", params![id, status])?;
        Ok(())
    }

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

    /// (id, name) of the sessions this one created.
    pub fn children(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let mut st = self.db.prepare("SELECT id, name FROM sessions WHERE created_by_session=?1 ORDER BY rowid")?;
        let rows = st.query_map(params![id], |r| Ok(serde_json::json!({"id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?})))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

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
                if value != "archived" && cur.lifecycle == "archived" {
                    if let Some(other) = self.resolve(&cur.name)? {
                        bail!("cannot unarchive: name {:?} is in use by {other}", cur.name);
                    }
                }
                ("lifecycle", cur.lifecycle.clone())
            }
            "name" => {
                if !valid_name(value) {
                    bail!("invalid name {value:?}");
                }
                if self.resolve(value)?.is_some_and(|o| o != target) {
                    bail!("a live session is already named {value:?}");
                }
                ("name", cur.name.clone())
            }
            f => bail!("field {f} cannot be updated"),
        };
        let tx = self.db.unchecked_transaction()?;
        tx.execute(&format!("UPDATE sessions SET {column}=?2 WHERE id=?1"), params![target, clip(value, 500)])?;
        tx.execute("INSERT INTO audit(actor, target, field, old, new) VALUES (?1, ?2, ?3, ?4, ?5)",
                   params![actor, target, field, old, value])?;
        tx.commit()?;
        Ok(())
    }

    pub fn audit(&self, target: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let mut st = self.db.prepare(
            "SELECT a.seq, a.actor, COALESCE((SELECT name FROM sessions WHERE id=a.actor), a.actor), a.target, \
             (SELECT name FROM sessions WHERE id=a.target), a.field, a.old, a.new, a.at \
             FROM audit a WHERE ?1 IS NULL OR a.target=?1 ORDER BY a.seq",
        )?;
        let rows = st.query_map(params![target], |r| {
            Ok(serde_json::json!({"seq": r.get::<_, i64>(0)?, "actor": r.get::<_, String>(1)?, "actor_name": r.get::<_, String>(2)?,
                "target": r.get::<_, String>(3)?, "target_name": r.get::<_, Option<String>>(4)?, "field": r.get::<_, String>(5)?,
                "old": r.get::<_, Option<String>>(6)?, "new": r.get::<_, Option<String>>(7)?, "at": r.get::<_, i64>(8)?}))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_description(&self, id: &str, text: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET description=?2 WHERE id=?1", params![id, clip(text, 500)])?;
        Ok(())
    }

    pub fn set_note(&self, id: &str, text: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET note=?2 WHERE id=?1", params![id, clip(text, 300)])?;
        Ok(())
    }

    pub fn board_post(&self, author: &str, text: &str) -> Result<i64> {
        self.db.execute("INSERT INTO board(author, text) VALUES (?1, ?2)", params![author, clip(text, 4000)])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn board_read(&self, tail: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, author, 'board', text, at FROM (SELECT * FROM board ORDER BY seq DESC LIMIT ?1) ORDER BY seq",
        )?;
        let rows = st.query_map(params![tail], msg_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn append(&self, session: &str, role: &str, text: &str) -> Result<i64> {
        self.db.execute("INSERT INTO messages(session, role, text) VALUES (?1, ?2, ?3)", params![session, role, text])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn history(&self, session: &str, after: i64, limit: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, session, role, text, at FROM messages WHERE session=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = st.query_map(params![session, after, limit], msg_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn history_tail(&self, session: &str, tail: i64, offset: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, session, role, text, at FROM (SELECT * FROM messages WHERE session=?1 ORDER BY seq DESC LIMIT ?2 OFFSET ?3) ORDER BY seq",
        )?;
        let rows = st.query_map(params![session, tail, offset], msg_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn msg_row(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    Ok(Message { seq: r.get(0)?, session: r.get(1)?, role: r.get(2)?, text: r.get(3)?, at: r.get(4)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_ids_names_lineage_audit() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("k.db")).unwrap();
        let lead = s.create("lead", "claude", "/w/lead", "lead", DEFAULT_MODEL, "boss", None).unwrap();
        assert!(looks_like_uuid(&lead.id));
        assert_eq!(s.resolve("lead").unwrap().as_deref(), Some(lead.id.as_str()));
        assert_eq!(s.resolve(&lead.id).unwrap().as_deref(), Some(lead.id.as_str()));
        assert!(s.resolve("nobody").unwrap().is_none());
        assert!(s.create("lead", "claude", "/x", "worker", DEFAULT_MODEL, "", None).is_err(), "live names unique");
        assert!(s.create("Bad Name", "claude", "/x", "worker", DEFAULT_MODEL, "", None).is_err());
        assert!(s.create("ok", "claude", "rel", "worker", DEFAULT_MODEL, "", None).is_err());
        assert!(s.create("ok", "gemini", "/x", "worker", DEFAULT_MODEL, "", None).is_err());
        let w = s.create("w1", "claude", "/w/w1", "worker", DEFAULT_MODEL, "", Some(&lead.id)).unwrap();
        let g = s.create("w1a", "claude", "/w/w1a", "worker", DEFAULT_MODEL, "", Some(&w.id)).unwrap();
        assert_eq!(s.get(&w.id).unwrap().unwrap().created_by_name.as_deref(), Some("lead"));
        assert!(s.is_ancestor(&lead.id, &g.id).unwrap());
        assert!(!s.is_ancestor(&g.id, &lead.id).unwrap());
        assert_eq!(s.children(&lead.id).unwrap()[0]["name"], "w1");
        // rename keeps the id and all references
        s.append(&w.id, "peer:lead", "hi").unwrap();
        s.update_field(&lead.id, &w.id, "name", "w-one").unwrap();
        assert_eq!(s.resolve("w-one").unwrap().as_deref(), Some(w.id.as_str()));
        assert!(s.resolve("w1").unwrap().is_none());
        assert_eq!(s.history(&w.id, 0, 10).unwrap().len(), 1);
        assert!(s.update_field(&lead.id, &w.id, "name", "lead").is_err(), "name collision");
        // archived names can be reused
        s.update_field("user", &g.id, "lifecycle", "archived").unwrap();
        let g2 = s.create("w1a", "claude", "/w/x", "worker", DEFAULT_MODEL, "", None).unwrap();
        assert_ne!(g.id, g2.id);
        assert!(s.update_field("user", &g.id, "lifecycle", "active").is_err(), "unarchive collision");
        s.update_field(&lead.id, &w.id, "effort", "low").unwrap();
        assert!(s.update_field(&lead.id, &w.id, "effort", "extreme").is_err());
        assert!(s.update_field(&lead.id, &w.id, "cwd", "/etc").is_err());
        let a = s.audit(Some(&w.id)).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0]["actor_name"], "lead");
        assert_eq!(a[0]["target_name"], "w-one");
        // identity persists
        s.bind_harness_id(&lead.id, "u1").unwrap();
        assert!(s.bind_harness_id(&lead.id, "u2").is_err());
        s.board_post("lead", "note").unwrap();
        assert_eq!(s.board_read(5).unwrap()[0].text, "note");
        drop(s);
        let s = Store::open(&dir.path().join("k.db")).unwrap();
        assert_eq!(s.get(&lead.id).unwrap().unwrap().harness_session_id.as_deref(), Some("u1"));
    }

    #[test]
    fn refuses_pre_uuid_databases() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k.db");
        Connection::open(&p).unwrap().execute_batch("CREATE TABLE sessions (name TEXT PRIMARY KEY)").unwrap();
        assert!(Store::open(&p).err().unwrap().to_string().contains("pre-UUID"));
    }
}
