//! SQLite session store. Every session has a UUID `id` (stable forever) and a
//! human `name` (unique among non-archived sessions; renamable). All lookups
//! accept either; references between rows (messages, lineage, audit) use ids.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

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
    pub created_at: i64,
    /// When the user (not a peer) last wrote to this session.
    pub last_user_at: Option<i64>,
    /// Harness switch links: the session this one replaced, and the one that replaced it.
    pub replaces: Option<String>,
    pub replaced_by: Option<String>,
    /// Permission mode set for this session (auto|default|acceptEdits|plan|bypassPermissions);
    /// None = Kurisu's default (auto when the CLI supports it).
    #[serde(default)]
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Message {
    pub seq: i64,
    pub session: String, // session id (board: author name)
    pub role: String,    // user | agent | <sender session id> | error | approval | compaction | tool
    pub text: String,
    pub at: i64,
    /// Structured detail (tool calls: tool, input, output, status). Absent for plain messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// One message the Telegram channel saw, kept for the `telegram_history` tool.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TgMessage {
    pub seq: i64,
    pub at: i64,
    /// "user" (the owner wrote) or "agent" (Kurisu sent).
    pub direction: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_id: Option<i64>,
    /// {kind, name, mime} for a photo/voice/file, absent for plain text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<serde_json::Value>,
    /// Inbound only: whether an agent reply was sent for it (false = never answered).
    pub answered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<i64>,
}

pub struct Store {
    db: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS channel_dispatch (
  event TEXT PRIMARY KEY,
  message INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  harness TEXT NOT NULL CHECK (harness IN ('claude','codex','acp','opencode','agy','fx','pi')),
  cwd TEXT NOT NULL,
  harness_session_id TEXT UNIQUE,
  role TEXT NOT NULL DEFAULT 'worker' CHECK (role IN ('lead','worker')),
  model TEXT NOT NULL DEFAULT 'claude-haiku-5-5', effort TEXT,
  status TEXT NOT NULL DEFAULT 'new',
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
CREATE TABLE IF NOT EXISTS imports (
  source TEXT NOT NULL,            -- e.g. 'omnigent'
  source_id TEXT NOT NULL,         -- the source's session id
  session TEXT NOT NULL REFERENCES sessions(id),
  title TEXT NOT NULL,
  items INTEGER NOT NULL DEFAULT 0,
  transcript TEXT,                 -- the copied CLI transcript's session id, if any
  imported_at INTEGER NOT NULL DEFAULT (unixepoch()),
  PRIMARY KEY (source, source_id)
);
CREATE TABLE IF NOT EXISTS wakes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL REFERENCES sessions(id),
  created_by TEXT NOT NULL,          -- 'user' or a session id
  at INTEGER NOT NULL,               -- next due time, unix seconds
  message TEXT NOT NULL,
  repeat TEXT,                       -- 5-field cron (local time) or NULL
  status TEXT NOT NULL DEFAULT 'pending',  -- pending | fired | cancelled | failed
  fired_count INTEGER NOT NULL DEFAULT 0,
  last_fired_at INTEGER, last_late INTEGER NOT NULL DEFAULT 0, last_error TEXT,
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS wakes_due ON wakes(status, at);
CREATE TABLE IF NOT EXISTS audit (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  actor TEXT NOT NULL,      -- 'user' or a session id
  target TEXT NOT NULL,     -- session id
  field TEXT NOT NULL,
  old TEXT, new TEXT,
  at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS approvals (
  id TEXT PRIMARY KEY,            -- short id typed by the user (8 hex)
  session TEXT NOT NULL REFERENCES sessions(id),
  request_id TEXT NOT NULL,       -- the driver's request id (hook)
  tool TEXT NOT NULL,
  title TEXT NOT NULL,
  input TEXT NOT NULL,            -- tool input JSON
  cwd TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending','approved','denied','expired','cancelled')),
  decided_by TEXT, reason TEXT, rule_id INTEGER,
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  expires_at INTEGER NOT NULL,
  decided_at INTEGER,
  kind TEXT NOT NULL DEFAULT 'permission',  -- permission | question (AskUserQuestion)
  answer TEXT                               -- question answers (JSON)
);
CREATE INDEX IF NOT EXISTS approvals_pending ON approvals(status, expires_at);
CREATE TABLE IF NOT EXISTS approval_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  scope TEXT NOT NULL CHECK (scope IN ('role','session')),
  target TEXT NOT NULL,           -- role name or session id
  tool TEXT NOT NULL,             -- tool name, '*' or a 'prefix*'
  in_cwd INTEGER NOT NULL DEFAULT 0,
  command_prefix TEXT,
  decision TEXT NOT NULL DEFAULT 'allow' CHECK (decision IN ('allow','deny')),
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS session_mcp (
  session TEXT NOT NULL REFERENCES sessions(id),
  name TEXT NOT NULL,
  config TEXT NOT NULL,            -- an mcpServers entry: {command, args?, env?} or {type, url, headers?, client_id?}
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  PRIMARY KEY (session, name)
);
CREATE TABLE IF NOT EXISTS callbacks (  -- session_send wait:false: tell the sender when the turn ends
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  target TEXT NOT NULL,              -- the session that runs the turn
  sender TEXT NOT NULL,              -- the session to notify
  seq INTEGER NOT NULL,              -- the message that started the turn
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS board (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  author TEXT NOT NULL,
  text TEXT NOT NULL,
  at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS telegram_messages (  -- what the Telegram channel saw (owner DM only)
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  chat_id TEXT NOT NULL,
  direction TEXT NOT NULL CHECK (direction IN ('user','agent')),
  message_id INTEGER,        -- Telegram's message id
  update_id INTEGER,         -- the inbound update this belongs to (NULL for unsolicited sends)
  text TEXT NOT NULL DEFAULT '',
  attachment TEXT,           -- JSON {kind, name, mime}, NULL for plain text
  answered INTEGER NOT NULL DEFAULT 0,   -- inbound: an agent reply was sent for it
  answered_at INTEGER,
  at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS harness_metrics (
  session TEXT PRIMARY KEY REFERENCES sessions(id),
  data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS telegram_messages_by_chat ON telegram_messages(chat_id, seq);
CREATE TABLE IF NOT EXISTS telegram_sent (  -- bot messages sent in reply to an update (for /undo)
  chat_id TEXT NOT NULL,
  update_id INTEGER NOT NULL,
  message_id INTEGER NOT NULL,
  at INTEGER NOT NULL DEFAULT (unixepoch()),
  PRIMARY KEY (chat_id, update_id, message_id)
);
CREATE TABLE IF NOT EXISTS pending_notes (  -- told to the session at the start of its next turn (/undo)
  session TEXT PRIMARY KEY REFERENCES sessions(id),
  text TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS telegram_messages_once ON telegram_messages(update_id) WHERE update_id IS NOT NULL AND direction = 'user';
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

/// Per-session permission modes (Claude Code's names; Codex gets the equivalent).
pub const PERMISSION_MODES: &[&str] = &["auto", "default", "acceptEdits", "plan", "bypassPermissions"];
/// Effort levels a harness supports (Codex: model_reasoning_effort, which has no "max").
pub fn effort_levels(harness: &str) -> &'static [&'static str] {
    if harness == "agy" { &["low", "medium", "high"] } else if harness == "opencode" { &[] } else if harness == "fx" { &["low", "medium", "high", "xhigh"] } else if harness == "pi" { crate::pi::EFFORTS } else if harness == "codex" { &["low", "medium", "high", "xhigh"] } else { &["low", "medium", "high", "xhigh", "max"] }
}

/// Accepts Claude model ids (`claude-…`) and the CLI's aliases; for other
/// harnesses any token without whitespace. Blocks typos like `not-a-model`
/// before they break the next turn.
pub fn validate_model(harness: &str, model: &str) -> Result<()> {
    if model.is_empty() || model.len() > 80 || model.contains(char::is_whitespace) {
        bail!("invalid model {model:?}");
    }
    if harness == "agy" { crate::agy::model_options(model, None)?; }
    if harness == "claude" {
        let base = model.strip_suffix("[1m]").unwrap_or(model);
        let alias = ["haiku", "sonnet", "opus", "fable", "default", "opusplan"].contains(&base);
        let id = base.starts_with("claude-") && base[7..].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.') && base.len() > 9;
        if !alias && !id {
            bail!("unknown Claude model {model:?}: use a model id like claude-haiku-5-5 or an alias (haiku, sonnet, opus, fable)");
        }
    }
    Ok(())
}

/// Claude models Kurisu knows exist (the CLI has no offline list command).
pub const KNOWN_CLAUDE_MODELS: &[&str] =
    &["claude-fable-5-2", "claude-fable-5-1", "claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "haiku", "sonnet", "opus", "fable", "default", "opusplan"];

/// Codex models from `codex debug models` (0.166.0), list-visible entries only.
pub const KNOWN_CODEX_MODELS: &[&str] = &[
    "gpt-6.1-sol", "gpt-6-astra", "gpt-6.1-terra", "gpt-6-sol", "gpt-6-luna",
    "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna",
];

/// The id that means "use the CLI's own configured default model".
pub fn default_model(harness: &str) -> &'static str {
    if harness == "agy" { "gemini-3.8-flash" } else if harness != "claude" { "default" } else { DEFAULT_MODEL }
}

/// The model catalogue for pickers: `(id, label)`; ids first, then the CLI's aliases.
/// For agy the list is whatever `agy models` reports right now (cached, never invented).
pub fn model_catalogue(harness: &str) -> Vec<(String, String)> {
    if harness == "agy" { return crate::agy::catalogue(); }
    if harness == "fx" { return crate::fx::catalogue(); }
    if harness == "pi" { return crate::pi::catalogue(); }
    match harness {
        "claude" => vec![
            ("claude-haiku-5-5", "Haiku 5.5 (default)"), ("claude-sonnet-5-5", "Sonnet 5.5"), ("claude-opus-5-5", "Opus 5.5"),
            ("claude-fable-5-2", "Fable 5.2"), ("claude-fable-5-1", "Fable 5.1"),
            ("haiku", "alias: latest Haiku"), ("sonnet", "alias: latest Sonnet"),
            ("opus", "alias: latest Opus"), ("fable", "alias: latest Fable"), ("default", "alias: the CLI's default"),
            ("opusplan", "alias: Opus to plan, Sonnet to act"),
        ],
        "codex" => vec![
            ("default", "Codex default (configured model)"), ("gpt-6.1-sol", "GPT-6.1-Sol"),
            ("gpt-6-astra", "GPT-6-Astra"), ("gpt-6.1-terra", "GPT-6.1 Terra"), ("gpt-6-sol", "GPT-6-Sol"),
            ("gpt-6-luna", "GPT-6-Luna"), ("gpt-5.6-sol", "GPT-5.6-Sol"), ("gpt-5.6-terra", "GPT-5.6-Terra"),
            ("gpt-5.6-luna", "GPT-5.6-Luna"),
        ],
        _ => vec![],
    }.into_iter().map(|(id, label)| (id.to_string(), label.to_string())).collect()
}

/// A warning for well-formed but unknown model ids (they may fail with model_not_found).
pub fn model_warning(harness: &str, model: &str) -> Option<String> {
    let base = model.strip_suffix("[1m]").unwrap_or(model);
    (harness == "claude" && !KNOWN_CLAUDE_MODELS.contains(&base))
        .then(|| format!("unverified model {model:?}: not in Kurisu's known list ({}); its first turn may fail with model_not_found",
                         KNOWN_CLAUDE_MODELS[..4].join(", ")))
}

pub fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

const COLS: &str = "s.id, s.name, s.role, s.harness, s.cwd, s.harness_session_id, s.model, s.effort, s.status, \
    s.description, s.note, (SELECT MAX(at) FROM messages m WHERE m.session = s.id), s.created_by_session, \
    (SELECT p.name FROM sessions p WHERE p.id = s.created_by_session), s.lifecycle, s.created_at, \
    (SELECT MAX(at) FROM messages m WHERE m.session = s.id AND m.role = 'user'), s.replaces, s.replaced_by, s.permission_mode";

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
        created_at: r.get(15)?,
        last_user_at: r.get(16)?,
        replaces: r.get(17)?,
        replaced_by: r.get(18)?,
        permission_mode: r.get(19)?,
    })
}

impl Store {
    /// Open without migrations or journal changes, sharing the startup schema guard.
    pub fn check_read_only(path: &Path) -> Result<()> {
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::check_schema(&db, path)?;
        let result: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if result != "ok" { bail!("{result}"); }
        Ok(())
    }

    fn check_schema(db: &Connection, path: &Path) -> Result<()> {
        // Pre-UUID dev databases (sessions keyed by name) are not migrated.
        let old: bool = db
            .query_row("SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name='name' AND pk=1", [], |r| r.get::<_, i64>(0))
            .map(|n| n > 0)
            .unwrap_or(false);
        if old {
            bail!("{} uses the pre-UUID schema (M1 dev data); delete that KURISU_HOME and start fresh", path.display());
        }
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Self::check_schema(&db, path)?;
        db.execute_batch(SCHEMA)?;
        // Columns added after the first approvals build.
        for (table, col, ddl) in [("approvals", "kind", "kind TEXT NOT NULL DEFAULT 'permission'"), ("approvals", "answer", "answer TEXT"),
                                  ("sessions", "cost_base", "cost_base REAL NOT NULL DEFAULT 0"),
                                  ("sessions", "cost_folded_at", "cost_folded_at INTEGER NOT NULL DEFAULT 0"),
                                  ("messages", "data", "data TEXT"),
                                  ("sessions", "replaces", "replaces TEXT"), ("sessions", "replaced_by", "replaced_by TEXT"),
                                  ("sessions", "seen_fp", "seen_fp TEXT"),
                                  ("sessions", "launch_fp", "launch_fp TEXT"),
                                  ("sessions", "permission_mode", "permission_mode TEXT")] {
            let has: i64 = db.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name='{col}'"), [], |r| r.get(0))?;
            if has == 0 {
                db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"))?;
            }
        }
        let sql: String = db.query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='sessions'", [], |r|r.get(0))?;
        if !sql.contains("'pi'") {
            db.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE;")?;
            let migrated = (|| -> Result<()> {
                // Rewrite the whole harness list, whichever harnesses the old store knew.
                let mut sql = sql.clone();
                let start = sql.find("harness IN (").context("sessions.harness CHECK not found")?;
                let end = start + sql[start..].find(')').context("sessions.harness CHECK not closed")? + 1;
                sql.replace_range(start..end, "harness IN ('claude','codex','acp','opencode','agy','fx','pi')");
                let create = sql.replacen("CREATE TABLE \"sessions\"", "CREATE TABLE sessions_acp_migration", 1)
                    .replacen("CREATE TABLE sessions (", "CREATE TABLE sessions_acp_migration (", 1);
                db.execute_batch(&create)?;
                db.execute_batch("INSERT INTO sessions_acp_migration SELECT * FROM sessions; DROP TABLE sessions; ALTER TABLE sessions_acp_migration RENAME TO sessions;")?;
                db.execute_batch(SCHEMA)?;
                Ok(())
            })();
            if let Err(e)=migrated { let _=db.execute_batch("ROLLBACK; PRAGMA foreign_keys=ON;"); return Err(e); }
            db.execute_batch("COMMIT; PRAGMA foreign_keys=ON;")?;
        }
        Ok(Self { db })
    }

    /// Resolve a UUID or a live name to the session id.
    pub fn resolve(&self, key: &str) -> Result<Option<String>> {
        if looks_like_uuid(key) {
            return Ok(self.db.query_row("SELECT id FROM sessions WHERE id=?1", params![key], |r| r.get(0)).optional()?);
        }
        if let Some(id) = self
            .db
            .query_row("SELECT id FROM sessions WHERE name=?1 AND lifecycle != 'archived'", params![key], |r| r.get(0))
            .optional()?
        {
            return Ok(Some(id));
        }
        // A unique UUID prefix (6+ hex chars, e.g. the short id `ku sessions` prints).
        if key.len() >= 6 && key.len() < 36 && key.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            let mut st = self.db.prepare("SELECT id FROM sessions WHERE id LIKE ?1 || '%' LIMIT 2")?;
            let ids: Vec<String> = st.query_map(params![key.to_ascii_lowercase()], |r| r.get(0))?.collect::<Result<_, _>>()?;
            if ids.len() > 1 {
                bail!("id prefix {key:?} is ambiguous; use more characters");
            }
            return Ok(ids.into_iter().next());
        }
        Ok(None)
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
        validate_model(harness, model)?;
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
        self.log_event(created_by.unwrap_or("user"), &id, "created", &format!("({role}, {model})"))?;
        Ok(self.get(&id)?.unwrap())
    }

    /// After a harness switch: link the two sessions and hand the old one's children,
    /// pending wakes, MCP servers and session rules to the new one (one transaction).
    /// Returns (children moved, wakes moved, MCP servers copied, rules copied).
    pub fn transfer(&self, old: &str, new: &str) -> Result<(usize, usize, usize, usize)> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("UPDATE sessions SET replaced_by=?2 WHERE id=?1", params![old, new])?;
        tx.execute("UPDATE sessions SET replaces=?1 WHERE id=?2", params![old, new])?;
        let kids = tx.execute("UPDATE sessions SET created_by_session=?2 WHERE created_by_session=?1 AND id != ?2", params![old, new])?;
        let wakes = tx.execute("UPDATE wakes SET session=?2 WHERE session=?1 AND status='pending'", params![old, new])?;
        // Callbacks owed to the old session go to its replacement.
        tx.execute("UPDATE callbacks SET sender=?2 WHERE sender=?1", params![old, new])?;
        let mcp = tx.execute("INSERT OR IGNORE INTO session_mcp(session, name, config, enabled) SELECT ?2, name, config, enabled FROM session_mcp WHERE session=?1",
                             params![old, new])?;
        let rules = tx.execute("INSERT INTO approval_rules(scope, target, tool, in_cwd, command_prefix, decision, created_by) \
                                SELECT scope, ?2, tool, in_cwd, command_prefix, decision, created_by FROM approval_rules WHERE scope='session' AND target=?1",
                               params![old, new])?;
        tx.commit()?;
        Ok((kids, wakes, mcp, rules))
    }

    /// The tools/prompt fingerprint of a session's last turn (None before its first).
    pub fn seen_fp(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT seen_fp FROM sessions WHERE id=?1", params![id], |r| r.get(0)).optional()?.flatten())
    }

    pub fn set_seen_fp(&self, id: &str, fp: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET seen_fp=?2 WHERE id=?1", params![id, fp])?;
        Ok(())
    }

    /// The toolset fingerprint its harness session (ACP session / thread) was created with.
    /// Persisted so a Kurisu restart does not hide that an ACP session still holds old tools.
    pub fn launch_fp(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT launch_fp FROM sessions WHERE id=?1", params![id], |r| r.get(0)).optional()?.flatten())
    }

    pub fn set_launch_fp(&self, id: &str, fp: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET launch_fp=?2 WHERE id=?1", params![id, fp])?;
        Ok(())
    }

    /// Get by id (use `resolve` first for names).
    pub fn get(&self, id: &str) -> Result<Option<Session>> {
        Ok(self.db.query_row(&format!("SELECT {COLS} FROM sessions s WHERE s.id=?1"), params![id], row).optional()?)
    }

    /// Follow harness-switch links forward to the live session. A self-update
    /// may name either the old id or the session's name; both resolve into the
    /// same chain, and the update must land on the session that will run next.
    pub fn replacement_tip(&self, id: &str) -> Result<String> {
        let mut cur = id.to_string();
        for _ in 0..64 {
            let Some(s) = self.get(&cur)? else { return Ok(cur) };
            match s.replaced_by.filter(|next| !next.is_empty()) {
                Some(next) => cur = next,
                None => return Ok(cur),
            }
        }
        bail!("session replacement chain from {id} is too long (cycle?)")
    }

    pub fn list(&self) -> Result<Vec<Session>> {
        let mut st = self.db.prepare(&format!("SELECT {COLS} FROM sessions s ORDER BY s.rowid"))?;
        let rows = st.query_map([], row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Start the session on a fresh CLI conversation (the old harness id is kept in the audit).
    pub fn rebind_harness_id(&self, id: &str, hid: &str, why: &str) -> Result<()> {
        let old = self.get(id)?.and_then(|s| s.harness_session_id).unwrap_or_default();
        self.db.execute("UPDATE sessions SET harness_session_id=?2 WHERE id=?1", params![id, hid])?;
        self.log_event("kurisu", id, "conversation_restarted", &format!("{old} -> {hid}: {why}"))?;
        Ok(())
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

    /// Returns Ok(false) when the value is unchanged (no write, no audit row).
    pub fn update_field(&self, actor: &str, target: &str, field: &str, value: &str) -> Result<bool> {
        let cur = self.get(target)?.ok_or_else(|| anyhow::anyhow!("no session {target}"))?;
        let (column, old) = match field {
            "model" => {
                validate_model(&cur.harness, value)?;
                ("model", cur.model.clone())
            }
            "effort" => {
                let levels = effort_levels(&cur.harness);
                if cur.harness != "opencode" && !levels.contains(&value) {
                    bail!("effort must be {} on {}", levels.join("|"), cur.harness);
                }
                if value.is_empty() || value.len()>40 || value.contains(char::is_whitespace) { bail!("invalid effort"); }
                ("effort", cur.effort.clone().unwrap_or_default())
            }
            "permission_mode" => {
                if cur.harness == "agy" && !["default", "auto", "acceptEdits", "plan"].contains(&value) { bail!("agy supports default|auto|acceptEdits|plan"); }
                if cur.harness == "fx" && value == "plan" { bail!("fx has no plan mode; use default|auto|acceptEdits|bypassPermissions"); }
                if cur.harness == "pi" { crate::pi::check_mode(Some(value))?; }
                if cur.harness == "opencode" && !["default", "plan"].contains(&value) { bail!("OpenCode ACP supports default (upstream permissions) or plan; other permission modes are unavailable"); }
                if !PERMISSION_MODES.contains(&value) {
                    bail!("permission mode must be one of {}", PERMISSION_MODES.join("|"));
                }
                ("permission_mode", cur.permission_mode.clone().unwrap_or_default())
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
        if old == value {
            return Ok(false); // no-op: nothing written, nothing audited
        }
        let tx = self.db.unchecked_transaction()?;
        tx.execute(&format!("UPDATE sessions SET {column}=?2 WHERE id=?1"), params![target, clip(value, 500)])?;
        if field=="model" && cur.harness=="opencode" {
            // Upstream variants belong to a model; carrying one across can make the next load fail.
            tx.execute("UPDATE sessions SET effort=NULL WHERE id=?1", [target])?;
        }
        tx.execute("INSERT INTO audit(actor, target, field, old, new) VALUES (?1, ?2, ?3, ?4, ?5)",
                   params![actor, target, field, old, value])?;
        tx.commit()?;
        Ok(true)
    }

    /// Lifecycle events (created, started, resumed, stopped, turn failed) in the audit trail.
    pub fn log_event(&self, actor: &str, target: &str, event: &str, detail: &str) -> Result<()> {
        self.db.execute("INSERT INTO audit(actor, target, field, old, new) VALUES (?1, ?2, ?3, NULL, ?4)",
                        params![actor, target, format!("event:{event}"), clip(detail, 300)])?;
        Ok(())
    }

    pub fn audit(&self, target: Option<&str>) -> Result<Vec<serde_json::Value>> {
        self.audit_after(target, 0)
    }

    pub fn audit_after(&self, target: Option<&str>, after: i64) -> Result<Vec<serde_json::Value>> {
        let mut st = self.db.prepare(
            "SELECT a.seq, a.actor, COALESCE((SELECT name FROM sessions WHERE id=a.actor), a.actor), a.target, \
             (SELECT name FROM sessions WHERE id=a.target), a.field, a.old, a.new, a.at \
             FROM audit a WHERE (?1 IS NULL OR a.target=?1) AND a.seq > ?2 ORDER BY a.seq LIMIT 1000",
        )?;
        let rows = st.query_map(params![target, after], |r| {
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

    /// Messages across all sessions after a global sequence number (live sync).
    /// Messages after `after` in any session, with the session's name, for global polling.
    pub fn messages_after_named(&self, after: i64, limit: i64) -> Result<Vec<serde_json::Value>> {
        let mut st = self.db.prepare(
            "SELECT m.seq, m.session, m.role, m.text, m.at, s.name FROM messages m JOIN sessions s ON s.id = m.session \
             WHERE m.seq > ?1 AND m.role != 'tool' ORDER BY m.seq LIMIT ?2",
        )?;
        let rows = st.query_map(params![after, limit], |r| {
            Ok(serde_json::json!({"seq": r.get::<_, i64>(0)?, "session": r.get::<_, String>(1)?, "role": r.get::<_, String>(2)?,
                                  "text": r.get::<_, String>(3)?, "at": r.get::<_, i64>(4)?, "session_name": r.get::<_, String>(5)?}))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn messages_after(&self, after: i64, limit: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare("SELECT seq, session, role, text, at FROM messages WHERE seq>?1 AND role != 'tool' ORDER BY seq LIMIT ?2")?;
        let rows = st.query_map(params![after, limit], msg_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn max_message_seq(&self) -> Result<i64> {
        Ok(self.db.query_row("SELECT COALESCE(MAX(seq), 0) FROM messages", [], |r| r.get(0))?)
    }

    pub fn max_audit_seq(&self) -> Result<i64> {
        Ok(self.db.query_row("SELECT COALESCE(MAX(seq), 0) FROM audit", [], |r| r.get(0))?)
    }

    /// Number of non-user messages (agent, peer, error): drives unread badges.
    pub fn reply_count(&self, id: &str) -> Result<i64> {
        Ok(self.db.query_row("SELECT COUNT(*) FROM messages WHERE session=?1 AND role NOT IN ('user', 'tool')", params![id], |r| r.get(0))?)
    }

    /// (seq, role, text, at) of a session's newest message.
    pub fn last_message(&self, id: &str) -> Result<Option<(i64, String, String, i64)>> {
        Ok(self
            .db
            .query_row("SELECT seq, role, text, at FROM messages WHERE session=?1 AND role != 'tool' ORDER BY seq DESC LIMIT 1", params![id],
                       |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?)
    }

    /// Cost of earlier CLI runs, and the capture time of the last run folded in.
    pub fn cost_state(&self, id: &str) -> Result<(f64, i64)> {
        Ok(self.db.query_row("SELECT cost_base, cost_folded_at FROM sessions WHERE id=?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))?)
    }

    /// Add a finished CLI run's cost (captured at `captured_at`) to the session's total, once.
    pub fn cost_fold(&self, id: &str, cost: f64, captured_at: i64) -> Result<()> {
        self.db.execute("UPDATE sessions SET cost_base = cost_base + ?2, cost_folded_at = ?3 WHERE id=?1 AND cost_folded_at < ?3",
                        params![id, cost, captured_at])?;
        Ok(())
    }

    /// The latest turn outcome (an agent reply or an error) of a session.
    pub fn last_outcome(&self, id: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .db
            .query_row("SELECT role, text FROM messages WHERE session=?1 AND role IN ('agent','error') ORDER BY seq DESC LIMIT 1",
                       params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    /// Atomically record the message and its channel event key. Telegram does not use this path.
    pub fn append_channel_once(&self, key: &str, session: &str, role: &str, text: &str) -> Result<(i64, bool, String)> {
        let tx = self.db.unchecked_transaction()?;
        let previous: Option<(i64,String,String)> = tx.query_row(
            "SELECT m.seq,m.session,COALESCE(json_extract(m.data,'$.delivery'),'delivered') FROM channel_dispatch c JOIN messages m ON m.seq=c.message WHERE c.event=?1",
            [key], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((seq, target, delivery)) = previous {
            if target != session { bail!("channel event was already bound to another session"); }
            tx.commit()?; return Ok((seq, false, delivery));
        }
        tx.execute("INSERT INTO messages(session,role,text,data) VALUES(?1,?2,?3,?4)", params![session,role,text,serde_json::json!({"delivery":"queued"}).to_string()])?;
        let seq=tx.last_insert_rowid();
        tx.execute("INSERT INTO channel_dispatch VALUES(?1,?2)", params![key,seq])?;
        tx.commit()?; Ok((seq,true,"queued".into()))
    }

    pub fn append(&self, session: &str, role: &str, text: &str) -> Result<i64> {
        self.db.execute("INSERT INTO messages(session, role, text) VALUES (?1, ?2, ?3)", params![session, role, text])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn history(&self, session: &str, after: i64, limit: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, session, role, text, at FROM messages WHERE session=?1 AND seq>?2 AND role != 'tool' ORDER BY seq LIMIT ?3",
        )?;
        let rows = st.query_map(params![session, after, limit], msg_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Like `history`, but also returns `tool` rows (channel relays show them as progress).
    pub fn history_with_tools(&self, session: &str, after: i64, limit: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, session, role, text, at FROM messages WHERE session=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = st.query_map(params![session, after, limit], msg_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Latest CLI-reported cumulative usage; survives process restarts.
    pub fn harness_metrics(&self, session: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = self.db.query_row("SELECT data FROM harness_metrics WHERE session=?1", [session], |r| r.get(0)).optional()?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into)).transpose()
    }

    pub fn set_harness_metrics(&self, session: &str, data: &serde_json::Value) -> Result<()> {
        self.db.execute("INSERT INTO harness_metrics(session,data) VALUES (?1,?2) ON CONFLICT(session) DO UPDATE SET data=excluded.data", params![session, data.to_string()])?;
        Ok(())
    }

    /// A message with structured data (tool calls).
    pub fn append_data(&self, session: &str, role: &str, text: &str, data: &serde_json::Value) -> Result<i64> {
        self.db.execute("INSERT INTO messages(session, role, text, data) VALUES (?1, ?2, ?3, ?4)",
                        params![session, role, text, data.to_string()])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn set_message_data(&self, seq: i64, data: &serde_json::Value) -> Result<()> {
        self.db.execute("UPDATE messages SET data=?2 WHERE seq=?1", params![seq, data.to_string()])?;
        Ok(())
    }

    /// Merge fields into a message's data (other fields are kept).
    pub fn merge_message_data(&self, seq: i64, data: &serde_json::Value) -> Result<()> {
        self.db.execute("UPDATE messages SET data=json_patch(COALESCE(data,'{}'), ?2) WHERE seq=?1", params![seq, data.to_string()])?;
        Ok(())
    }

    /// The newest message that started (or was queued for) a turn: a prompt from the
    /// user, a peer or a wake (those rows carry a `delivery` state). A message steered
    /// into a running turn belongs to that turn and is skipped.
    pub fn last_prompt(&self, session: &str) -> Result<Option<Message>> {
        Ok(self.db.query_row(
            "SELECT seq, session, role, text, at, data FROM messages WHERE session=?1 AND json_extract(data,'$.delivery') IS NOT NULL \
             AND json_extract(data,'$.delivery') != 'steered' ORDER BY seq DESC LIMIT 1", params![session], msg_row).optional()?)
    }

    /// /undo: delete a session's history rows from `seq` on; returns them (oldest first).
    pub fn remove_messages_from(&self, session: &str, seq: i64) -> Result<Vec<Message>> {
        let tx = self.db.unchecked_transaction()?;
        let rows = {
            let mut st = tx.prepare("SELECT seq, session, role, text, at, data FROM messages WHERE session=?1 AND seq>=?2 ORDER BY seq")?;
            let rows = st.query_map(params![session, seq], msg_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        tx.execute("DELETE FROM channel_dispatch WHERE message IN (SELECT seq FROM messages WHERE session=?1 AND seq>=?2)", params![session, seq])?;
        tx.execute("DELETE FROM callbacks WHERE target=?1 AND seq>=?2", params![session, seq])?;
        tx.execute("DELETE FROM messages WHERE session=?1 AND seq>=?2", params![session, seq])?;
        tx.commit()?;
        Ok(rows)
    }

    /// Queue a note for the session's next turn (joined to any already queued).
    pub fn add_pending_note(&self, session: &str, text: &str) -> Result<()> {
        self.db.execute("INSERT INTO pending_notes(session, text) VALUES (?1, ?2) \
                         ON CONFLICT(session) DO UPDATE SET text = text || char(10) || excluded.text", params![session, text])?;
        Ok(())
    }

    /// Take (and clear) the session's queued note.
    pub fn take_pending_note(&self, session: &str) -> Result<Option<String>> {
        let t: Option<String> = self.db.query_row("DELETE FROM pending_notes WHERE session=?1 RETURNING text", params![session], |r| r.get(0)).optional()?;
        Ok(t)
    }

    pub fn set_message_text(&self, seq: i64, text: &str) -> Result<()> {
        self.db.execute("UPDATE messages SET text=?2 WHERE seq=?1", params![seq, text])?;
        Ok(())
    }

    /// Tool rows in a session that never got a completion update.
    pub fn running_tool_rows(&self, session: &str) -> Result<Vec<(i64, serde_json::Value)>> {
        let mut st = self.db.prepare(
            "SELECT seq, data FROM messages WHERE session=?1 AND role='tool' AND data IS NOT NULL ORDER BY seq",
        )?;
        let rows = st.query_map(params![session], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = vec![];
        for row in rows {
            let (seq, raw) = row?;
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) {
                if data["status"] == serde_json::json!("running") {
                    out.push((seq, data));
                }
            }
        }
        Ok(out)
    }

    pub fn count_role(&self, session: &str, role: &str) -> Result<i64> {
        Ok(self.db.query_row("SELECT COUNT(*) FROM messages WHERE session=?1 AND role=?2", params![session, role], |r| r.get(0))?)
    }

    /// One page of a session's history, oldest first, with the total that matches the filters.
    /// `before` pages backwards (newest page first when neither bound is set); `after` pages forwards.
    /// `search` matches text (and tool data) case-insensitively. Tool rows only with `tools`.
    pub fn history_page(&self, session: &str, q: &HistoryQuery) -> Result<(Vec<Message>, i64)> {
        let mut cond = vec!["session = ?1".to_string()];
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(session.to_string())];
        if !q.tools && !q.roles.iter().any(|r| r == "tool") {
            cond.push("role != 'tool'".into());
        }
        if !q.roles.is_empty() {
            let mut ors = vec![];
            for r in &q.roles {
                args.push(Box::new(r.clone()));
                ors.push(format!("role = ?{}", args.len()));
            }
            // "peer" stands for every message another session sent.
            if q.roles.iter().any(|r| r == "peer") {
                ors.push("role GLOB '[0-9a-f]*-*'".into());
            }
            cond.push(format!("({})", ors.join(" OR ")));
        }
        if let Some(t) = q.search.as_deref().filter(|t| !t.is_empty()) {
            let esc = t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            args.push(Box::new(format!("%{esc}%")));
            let n = args.len();
            cond.push(format!("(text LIKE ?{n} ESCAPE '\\' OR COALESCE(data, '') LIKE ?{n} ESCAPE '\\')"));
        }
        let filter = cond.join(" AND ");
        let params_ref: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let total: i64 = self.db.query_row(&format!("SELECT COUNT(*) FROM messages WHERE {filter}"), params_ref.as_slice(), |r| r.get(0))?;
        let mut page_cond = filter.clone();
        if let Some(b) = q.before {
            page_cond.push_str(&format!(" AND seq < {b}"));
        }
        if let Some(a) = q.after {
            page_cond.push_str(&format!(" AND seq > {a}"));
        }
        let limit = q.limit.clamp(1, 500);
        let sql = if q.after.is_some() && q.before.is_none() {
            format!("SELECT seq, session, role, text, at, data FROM messages WHERE {page_cond} ORDER BY seq LIMIT {limit}")
        } else {
            format!("SELECT * FROM (SELECT seq, session, role, text, at, data FROM messages WHERE {page_cond} ORDER BY seq DESC LIMIT {limit}) ORDER BY seq")
        };
        let mut st = self.db.prepare(&sql)?;
        let rows = st.query_map(params_ref.as_slice(), msg_row)?;
        Ok((rows.collect::<Result<_, _>>()?, total))
    }

    /// Insert a message with its original time (imports).
    pub fn append_at(&self, session: &str, role: &str, text: &str, data: Option<&serde_json::Value>, at: i64) -> Result<i64> {
        self.db.execute("INSERT INTO messages(session, role, text, data, at) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![session, role, text, data.map(|d| d.to_string()), at])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn begin(&self) -> Result<()> {
        self.db.execute_batch("BEGIN")?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.db.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback(&self) {
        let _ = self.db.execute_batch("ROLLBACK");
    }

    /// The Kurisu session an earlier import created for a source session (if it still exists).
    pub fn import_of(&self, source: &str, source_id: &str) -> Result<Option<(String, String)>> {
        Ok(self.db.query_row("SELECT i.session, s.name FROM imports i JOIN sessions s ON s.id = i.session WHERE i.source=?1 AND i.source_id=?2",
                             params![source, source_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    pub fn import_record(&self, source: &str, source_id: &str, session: &str, title: &str, items: i64, transcript: Option<&str>) -> Result<()> {
        self.db.execute("INSERT OR REPLACE INTO imports(source, source_id, session, title, items, transcript) VALUES (?1,?2,?3,?4,?5,?6)",
                        params![source, source_id, session, title, items, transcript])?;
        Ok(())
    }

    /// Messages (tool rows included) with `after < seq <= upto`.
    pub fn count_range(&self, session: &str, after: i64, upto: i64) -> Result<i64> {
        Ok(self.db.query_row("SELECT COUNT(*) FROM messages WHERE session=?1 AND seq>?2 AND seq<=?3", params![session, after, upto], |r| r.get(0))?)
    }

    /// The newest compaction summary of a session: `(seq, text)`.
    pub fn last_compaction(&self, session: &str) -> Result<Option<(i64, String)>> {
        Ok(self
            .db
            .query_row("SELECT seq, text FROM messages WHERE session=?1 AND role='compaction' ORDER BY seq DESC LIMIT 1",
                       params![session], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    pub fn history_tail(&self, session: &str, tail: i64, offset: i64) -> Result<Vec<Message>> {
        let mut st = self.db.prepare(
            "SELECT seq, session, role, text, at FROM (SELECT * FROM messages WHERE session=?1 AND role != 'tool' ORDER BY seq DESC LIMIT ?2 OFFSET ?3) ORDER BY seq",
        )?;
        let rows = st.query_map(params![session, tail, offset], msg_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // -------------------------------------------------------------- telegram history

    /// Record a message the owner sent to the bot. Returns None when this update
    /// is already stored (a replayed update must not appear twice).
    pub fn tg_record_inbound(&self, chat: &str, message_id: Option<i64>, update_id: i64, text: &str,
                             attachment: Option<&serde_json::Value>, at: i64) -> Result<Option<i64>> {
        let n = self.db.execute(
            "INSERT OR IGNORE INTO telegram_messages(chat_id, direction, message_id, update_id, text, attachment, at) \
             VALUES (?1, 'user', ?2, ?3, ?4, ?5, ?6)",
            params![chat, message_id, update_id, text, attachment.map(|a| a.to_string()), at],
        )?;
        if n == 0 {
            return Ok(None);
        }
        let seq = self.db.last_insert_rowid();
        self.tg_prune(chat)?;
        Ok(Some(seq))
    }

    /// Record text Kurisu sent to the owner. Chunks of one reply (the relay
    /// streams them) join the previous row when they are close in time. This
    /// does not mark the update answered: a notice is a chat message, not a
    /// reply; the relay calls `tg_mark_answered` when agent text goes out.
    pub fn tg_record_agent(&self, chat: &str, update_id: Option<i64>, text: &str, at: i64) -> Result<i64> {
        if text.trim().is_empty() {
            return Ok(0);
        }
        let last: Option<(i64, Option<i64>, i64)> = self
            .db
            .query_row("SELECT seq, update_id, at FROM telegram_messages WHERE chat_id=?1 AND direction='agent' ORDER BY seq DESC LIMIT 1",
                       params![chat], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        if let Some((seq, last_update, last_at)) = last {
            if last_update == update_id && at - last_at <= TG_REPLY_JOIN_S {
                self.db.execute("UPDATE telegram_messages SET text = text || ?2 || ?3, at = ?4 WHERE seq = ?1",
                                params![seq, "\n\n", text.trim(), at])?;
                return Ok(seq);
            }
        }
        self.db.execute("INSERT INTO telegram_messages(chat_id, direction, update_id, text, at) VALUES (?1, 'agent', ?2, ?3, ?4)",
                        params![chat, update_id, text.trim(), at])?;
        let seq = self.db.last_insert_rowid();
        self.tg_prune(chat)?;
        Ok(seq)
    }

    /// A bot message sent in reply to an inbound update (so /undo can delete it).
    pub fn tg_record_sent(&self, chat: &str, update_id: i64, message_id: i64, at: i64) -> Result<()> {
        self.db.execute("INSERT OR IGNORE INTO telegram_sent(chat_id, update_id, message_id, at) VALUES (?1, ?2, ?3, ?4)",
                        params![chat, update_id, message_id, at])?;
        self.db.execute("DELETE FROM telegram_sent WHERE at < ?1", params![at - 7 * 86400])?;
        Ok(())
    }

    /// Telegram messages of these updates: (message_id, sent at, from the user?),
    /// the user's prompts first.
    pub fn tg_update_messages(&self, chat: &str, updates: &[i64]) -> Result<Vec<(i64, i64, bool)>> {
        let mut out = vec![];
        for u in updates {
            let mut st = self.db.prepare("SELECT message_id, at FROM telegram_messages WHERE chat_id=?1 AND update_id=?2 \
                                          AND direction='user' AND message_id IS NOT NULL")?;
            for r in st.query_map(params![chat, u], |r| Ok((r.get(0)?, r.get(1)?, true)))? { out.push(r?); }
            let mut st = self.db.prepare("SELECT message_id, at FROM telegram_sent WHERE chat_id=?1 AND update_id=?2 ORDER BY message_id")?;
            for r in st.query_map(params![chat, u], |r| Ok((r.get(0)?, r.get(1)?, false)))? { out.push(r?); }
        }
        Ok(out)
    }

    /// /undo: forget what the Telegram history holds for these updates (the
    /// owner's prompt and Kurisu's replies); returns the rows removed.
    pub fn tg_forget_updates(&self, chat: &str, updates: &[i64]) -> Result<usize> {
        let mut n = 0;
        for u in updates {
            n += self.db.execute("DELETE FROM telegram_messages WHERE chat_id=?1 AND update_id=?2", params![chat, u])?;
            self.db.execute("DELETE FROM telegram_sent WHERE chat_id=?1 AND update_id=?2", params![chat, u])?;
        }
        Ok(n)
    }

    /// An agent reply was sent for this inbound message: it is not unanswered any more.
    pub fn tg_mark_answered(&self, chat: &str, update_id: i64, at: i64) -> Result<usize> {
        Ok(self.db.execute(
            "UPDATE telegram_messages SET answered = 1, answered_at = ?3 \
             WHERE chat_id = ?1 AND update_id = ?2 AND direction = 'user' AND answered = 0",
            params![chat, update_id, at],
        )?)
    }

    /// Fill in the speech-to-text transcript of a voice message (the text was not
    /// known when the update arrived).
    pub fn tg_set_transcript(&self, chat: &str, update_id: i64, transcript: &str) -> Result<usize> {
        let current: Option<String> = self
            .db.query_row("SELECT text FROM telegram_messages WHERE chat_id=?1 AND update_id=?2 AND direction='user'",
                          params![chat, update_id], |r| r.get(0))
            .optional()?;
        let Some(current) = current else { return Ok(0) };
        let text = if current.trim().is_empty() {
            transcript.trim().to_string()
        } else {
            format!("{}\n\n{}", current.trim(), transcript.trim())
        };
        Ok(self.db.execute("UPDATE telegram_messages SET text=?3 WHERE chat_id=?1 AND update_id=?2 AND direction='user'",
                           params![chat, update_id, text])?)
    }

    /// One page of the owner's Telegram chat, oldest first within the page.
    /// `before` pages backwards; the newest page is returned when it is unset.
    pub fn tg_page(&self, chat: &str, q: &TgQuery) -> Result<(Vec<TgMessage>, i64)> {
        let mut cond = vec!["chat_id = ?1".to_string()];
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(chat.to_string())];
        if q.unanswered {
            cond.push("direction = 'user' AND answered = 0".into());
        }
        if let Some(t) = q.search.as_deref().filter(|t| !t.trim().is_empty()) {
            let esc = t.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            args.push(Box::new(format!("%{esc}%")));
            let n = args.len();
            cond.push(format!("(text LIKE ?{n} ESCAPE '\\' OR COALESCE(attachment, '') LIKE ?{n} ESCAPE '\\')"));
        }
        let filter = cond.join(" AND ");
        let params_ref: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let total: i64 = self.db.query_row(&format!("SELECT COUNT(*) FROM telegram_messages WHERE {filter}"), params_ref.as_slice(), |r| r.get(0))?;
        let mut page_cond = filter.clone();
        if let Some(b) = q.before {
            page_cond.push_str(&format!(" AND seq < {b}"));
        }
        let limit = q.limit.clamp(1, 500);
        let sql = format!(
            "SELECT * FROM (SELECT seq, at, direction, text, message_id, update_id, attachment, answered, answered_at \
             FROM telegram_messages WHERE {page_cond} ORDER BY seq DESC LIMIT {limit}) ORDER BY seq"
        );
        let mut st = self.db.prepare(&sql)?;
        let rows = st.query_map(params_ref.as_slice(), |r| {
            Ok(TgMessage {
                seq: r.get(0)?,
                at: r.get(1)?,
                direction: r.get(2)?,
                text: r.get(3)?,
                message_id: r.get(4)?,
                update_id: r.get(5)?,
                attachment: r.get::<_, Option<String>>(6)?.and_then(|a| serde_json::from_str(&a).ok()),
                answered: r.get::<_, i64>(7)? != 0,
                answered_at: r.get(8)?,
            })
        })?;
        Ok((rows.collect::<Result<_, _>>()?, total))
    }

    /// Keep the newest `TG_KEEP` rows per chat: a long-lived chat must not grow forever.
    fn tg_prune(&self, chat: &str) -> Result<()> {
        let cutoff: Option<i64> = self
            .db.query_row("SELECT seq FROM telegram_messages WHERE chat_id=?1 ORDER BY seq DESC LIMIT 1 OFFSET ?2",
                          params![chat, TG_KEEP], |r| r.get(0))
            .optional()?;
        if let Some(cutoff) = cutoff {
            self.db.execute("DELETE FROM telegram_messages WHERE chat_id=?1 AND seq <= ?2", params![chat, cutoff])?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ scheduled wakes

#[derive(Debug, Clone, Serialize)]
pub struct Wake {
    pub id: i64,
    pub session: String,
    pub session_name: String,
    pub created_by: String,
    pub created_by_name: String,
    pub at: i64,
    pub message: String,
    pub repeat: Option<String>,
    pub status: String,
    pub fired_count: i64,
    pub last_fired_at: Option<i64>,
    pub last_late: bool,
    pub last_error: Option<String>,
    pub created_at: i64,
}

const WAKE_COLS: &str = "w.id, w.session, COALESCE(s.name, ''), w.created_by, \
    CASE WHEN w.created_by = 'user' THEN 'user' ELSE COALESCE(c.name, w.created_by) END, \
    w.at, w.message, w.repeat, w.status, w.fired_count, w.last_fired_at, w.last_late, w.last_error, w.created_at \
    FROM wakes w LEFT JOIN sessions s ON s.id = w.session LEFT JOIN sessions c ON c.id = w.created_by";

fn wake_row(r: &rusqlite::Row) -> rusqlite::Result<Wake> {
    Ok(Wake { id: r.get(0)?, session: r.get(1)?, session_name: r.get(2)?, created_by: r.get(3)?, created_by_name: r.get(4)?,
              at: r.get(5)?, message: r.get(6)?, repeat: r.get(7)?, status: r.get(8)?, fired_count: r.get(9)?,
              last_fired_at: r.get(10)?, last_late: r.get::<_, i64>(11)? != 0, last_error: r.get(12)?, created_at: r.get(13)? })
}

impl Store {
    pub fn wake_add(&self, session: &str, created_by: &str, at: i64, message: &str, repeat: Option<&str>) -> Result<Wake> {
        self.db.execute("INSERT INTO wakes(session, created_by, at, message, repeat) VALUES (?1,?2,?3,?4,?5)",
                        params![session, created_by, at, message, repeat])?;
        self.wake_get(self.db.last_insert_rowid())?.ok_or_else(|| anyhow!("wake vanished"))
    }

    pub fn wake_get(&self, id: i64) -> Result<Option<Wake>> {
        Ok(self.db.query_row(&format!("SELECT {WAKE_COLS} WHERE w.id=?1"), params![id], wake_row).optional()?)
    }

    /// Wakes, soonest first. `sessions`: only these targets (None: all). `all`: also past ones (newest 200).
    pub fn wakes(&self, sessions: Option<&[String]>, all: bool) -> Result<Vec<Wake>> {
        let sql = if all {
            format!("SELECT {WAKE_COLS} ORDER BY (w.status = 'pending') DESC, CASE WHEN w.status = 'pending' THEN w.at ELSE -w.id END LIMIT 200")
        } else {
            format!("SELECT {WAKE_COLS} WHERE w.status = 'pending' ORDER BY w.at")
        };
        let mut st = self.db.prepare(&sql)?;
        let rows: Vec<Wake> = st.query_map([], wake_row)?.collect::<Result<_, _>>()?;
        Ok(match sessions {
            Some(ids) => rows.into_iter().filter(|w| ids.contains(&w.session)).collect(),
            None => rows,
        })
    }

    pub fn wakes_due(&self, now: i64) -> Result<Vec<Wake>> {
        let mut st = self.db.prepare(&format!("SELECT {WAKE_COLS} WHERE w.status = 'pending' AND w.at <= ?1 ORDER BY w.at"))?;
        let rows = st.query_map(params![now], wake_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn wake_pending_count(&self, session: &str) -> Result<i64> {
        Ok(self.db.query_row("SELECT COUNT(*) FROM wakes WHERE session=?1 AND status='pending'", params![session], |r| r.get(0))?)
    }

    /// Record a firing: `next` is the next due time of a repeating wake (None: done).
    pub fn wake_fired(&self, id: i64, now: i64, late: bool, error: Option<&str>, next: Option<i64>) -> Result<()> {
        let status = match (next, error) {
            (Some(_), _) => "pending",
            (None, Some(_)) => "failed",
            (None, None) => "fired",
        };
        self.db.execute(
            "UPDATE wakes SET status=?2, fired_count = fired_count + 1, last_fired_at=?3, last_late=?4, last_error=?5, at = COALESCE(?6, at) WHERE id=?1",
            params![id, status, now, late as i64, error, next])?;
        Ok(())
    }

    #[cfg(test)]
    pub fn db_exec_for_tests(&self, sql: &str) {
        self.db.execute_batch(sql).unwrap();
    }

    /// Cancel a pending wake; false if it was not pending.
    pub fn wake_cancel(&self, id: i64) -> Result<bool> {
        Ok(self.db.execute("UPDATE wakes SET status='cancelled' WHERE id=?1 AND status='pending'", params![id])? > 0)
    }
}

// ------------------------------------------------------------------ send callbacks

#[derive(Debug, Clone, PartialEq)]
pub struct Callback {
    pub id: i64,
    pub target: String,
    pub sender: String,
    pub seq: i64,
}

impl Store {
    pub fn callback_add(&self, target: &str, sender: &str, seq: i64) -> Result<i64> {
        self.db.execute("INSERT INTO callbacks(target, sender, seq) VALUES (?1, ?2, ?3)", params![target, sender, seq])?;
        Ok(self.db.last_insert_rowid())
    }

    /// Remove and return a callback (None if already delivered).
    pub fn callback_take(&self, id: i64) -> Result<Option<Callback>> {
        let c = self.db.query_row("SELECT id, target, sender, seq FROM callbacks WHERE id=?1", params![id],
                                  |r| Ok(Callback { id: r.get(0)?, target: r.get(1)?, sender: r.get(2)?, seq: r.get(3)? })).optional()?;
        self.db.execute("DELETE FROM callbacks WHERE id=?1", params![id])?;
        Ok(c)
    }

    pub fn callbacks(&self) -> Result<Vec<Callback>> {
        let mut q = self.db.prepare("SELECT id, target, sender, seq FROM callbacks ORDER BY id")?;
        let rows = q.query_map([], |r| Ok(Callback { id: r.get(0)?, target: r.get(1)?, sender: r.get(2)?, seq: r.get(3)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The newest message *to* a session (user, peer or wake), not its own output.
    pub fn last_inbound(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT text FROM messages WHERE session=?1 AND role NOT IN ('agent','tool','error','approval','compaction') \
ORDER BY seq DESC LIMIT 1", params![id], |r| r.get(0)).optional()?)
    }
}

// ------------------------------------------------------------------ MCP servers per session

/// OAuth state of an MCP server on a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum AuthState {
    None,
    NeedsAuth,
    Authorizing {
        authorize_url: String,
        expires_at: i64,
    },
    Authorized {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<i64>,
    },
    Expired,
    Failed {
        reason: String,
    },
}

impl AuthState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NeedsAuth => "needs-auth",
            Self::Authorizing { .. } => "authorizing",
            Self::Authorized { .. } => "authorized",
            Self::Expired => "expired",
            Self::Failed { .. } => "failed",
        }
    }
}

pub fn strip_auth_header(cfg: &mut serde_json::Value) {
    for k in ["headers", "http_headers", "env_http_headers"] {
        if let Some(headers) = cfg.get_mut(k).and_then(|h| h.as_object_mut()) {
            headers.remove("Authorization");
            headers.remove("authorization");
        }
    }
}

impl Store {
    /// A session's extra MCP servers: `(name, config, enabled)`.
    pub fn mcp_list(&self, session: &str) -> Result<Vec<(String, serde_json::Value, bool)>> {
        let mut st = self.db.prepare("SELECT name, config, enabled FROM session_mcp WHERE session=?1 ORDER BY name")?;
        let rows = st.query_map(params![session], |r| {
            Ok((r.get::<_, String>(0)?, serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or(serde_json::Value::Null), r.get::<_, i64>(2)? != 0))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Add or change one server. A new server needs a config. Returns false if nothing changed.
    pub fn mcp_set(&self, session: &str, name: &str, config: Option<&serde_json::Value>, enabled: Option<bool>) -> Result<bool> {
        let cur = self.mcp_list(session)?.into_iter().find(|(n, ..)| n == name);
        match (cur, config) {
            (None, None) => bail!("no MCP server {name:?} on this session: add it with a config first"),
            (None, Some(c)) => {
                let mut c = c.clone();
                let e = enabled.unwrap_or(true);
                if !e {
                    strip_auth_header(&mut c);
                }
                self.db.execute("INSERT INTO session_mcp(session, name, config, enabled) VALUES (?1,?2,?3,?4)",
                                params![session, name, c.to_string(), e as i64])?;
                Ok(true)
            }
            (Some((_, old_c, old_e)), c) => {
                let mut new_c = c.cloned().unwrap_or(old_c.clone());
                let new_e = enabled.unwrap_or(old_e);
                if !new_e {
                    strip_auth_header(&mut new_c);
                }
                if new_c == old_c && new_e == old_e {
                    return Ok(false);
                }
                self.db.execute("UPDATE session_mcp SET config=?3, enabled=?4 WHERE session=?1 AND name=?2",
                                params![session, name, new_c.to_string(), new_e as i64])?;
                Ok(true)
            }
        }
    }

    /// Turn one server off in every session (returns how many rows changed).
    pub fn mcp_disable_all(&self, name: &str) -> Result<usize> {
        Ok(self.mcp_disable_all_sessions(name)?.len())
    }

    /// Turn one server off in every session and return the session IDs that were disabled.
    pub fn mcp_disable_all_sessions(&self, name: &str) -> Result<Vec<String>> {
        let mut st = self.db.prepare("SELECT session, config FROM session_mcp WHERE name=?1 AND enabled=1")?;
        let rows = st.query_map(params![name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let items: Vec<(String, String)> = rows.collect::<Result<_, _>>()?;
        for (session, config_str) in &items {
            let mut cfg: serde_json::Value = serde_json::from_str(config_str).unwrap_or(serde_json::Value::Null);
            strip_auth_header(&mut cfg);
            self.db.execute("UPDATE session_mcp SET config=?3, enabled=0 WHERE session=?1 AND name=?2",
                            params![session, name, cfg.to_string()])?;
        }
        Ok(items.into_iter().map(|(s, _)| s).collect())
    }

    /// Whether one server was explicitly switched off for a session (a user's mcp_disabled
    /// event), so a default-on migration does not override that choice.
    pub fn mcp_switched_off(&self, session: &str, name: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM audit WHERE target=?1 AND field='event:mcp_disabled' AND new=?2)",
            params![session, name], |r| r.get::<_, i64>(0))? != 0)
    }

    pub fn mcp_remove(&self, session: &str, name: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM session_mcp WHERE session=?1 AND name=?2", params![session, name])? == 1)
    }
}

// ------------------------------------------------------------------ approvals

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Approval {
    pub id: String,
    pub session: String,
    pub session_name: Option<String>,
    pub request_id: String,
    pub tool: String,
    pub title: String,
    pub input: serde_json::Value,
    pub cwd: String,
    pub status: String,
    pub decided_by: Option<String>,
    pub reason: Option<String>,
    pub rule_id: Option<i64>,
    pub created_at: i64,
    pub expires_at: i64,
    pub decided_at: Option<i64>,
    /// `permission` (a tool call) or `question` (AskUserQuestion: see `questions`).
    pub kind: String,
    /// A short, display-safe summary of the input (command, path, or the questions).
    pub preview: String,
    /// For questions: `[{question, header?, options:[{label, description?}], multiSelect}]`.
    pub questions: Option<serde_json::Value>,
    /// For answered questions: `{question: answer}`.
    pub answer: Option<serde_json::Value>,
}

/// Short display-safe preview of a tool input (no full file contents).
pub fn input_preview(tool: &str, input: &serde_json::Value) -> String {
    use serde_json::Value;
    if tool == "AskUserQuestion" {
        let qs: Vec<String> = input["questions"].as_array().into_iter().flatten().map(|q| {
            let opts: Vec<&str> = q["options"].as_array().into_iter().flatten().filter_map(|o| o["label"].as_str()).collect();
            format!("{} [{}]", q["question"].as_str().unwrap_or(""), opts.join(" / "))
        }).collect();
        return clip(&qs.join("; "), 300);
    }
    let main = ["command", "file_path", "path", "notebook_path", "url", "pattern", "query", "description"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str).map(String::from));
    let text = match main {
        Some(m) => m,
        None => input.to_string(),
    };
    let mut p = clip(&text, 300);
    if text.chars().count() > 300 {
        p.push('…');
    }
    p
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rule {
    pub id: i64,
    pub scope: String,
    pub target: String,
    pub target_name: Option<String>,
    pub tool: String,
    pub in_cwd: bool,
    pub command_prefix: Option<String>,
    pub decision: String,
    pub created_by: String,
    pub created_at: i64,
}

const APPROVAL_COLS: &str = "a.id, a.session, (SELECT name FROM sessions WHERE id=a.session), a.request_id, a.tool, a.title, a.input, \
    a.cwd, a.status, a.decided_by, a.reason, a.rule_id, a.created_at, a.expires_at, a.decided_at, a.kind, a.answer";

fn approval_row(r: &rusqlite::Row) -> rusqlite::Result<Approval> {
    Ok(Approval {
        id: r.get(0)?, session: r.get(1)?, session_name: r.get(2)?, request_id: r.get(3)?, tool: r.get(4)?, title: r.get(5)?,
        input: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or(serde_json::Value::Null),
        cwd: r.get(7)?, status: r.get(8)?, decided_by: r.get(9)?, reason: r.get(10)?, rule_id: r.get(11)?,
        created_at: r.get(12)?, expires_at: r.get(13)?, decided_at: r.get(14)?,
        kind: r.get(15)?,
        answer: r.get::<_, Option<String>>(16)?.and_then(|s| serde_json::from_str(&s).ok()),
        preview: String::new(),
        questions: None,
    })
    .map(|mut a| {
        a.preview = input_preview(&a.tool, &a.input);
        if a.kind == "question" {
            a.questions = a.input.get("questions").cloned();
        }
        a
    })
}

fn rule_row(r: &rusqlite::Row) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: r.get(0)?, scope: r.get(1)?, target: r.get(2)?, target_name: r.get(3)?, tool: r.get(4)?, in_cwd: r.get::<_, i64>(5)? != 0,
        command_prefix: r.get(6)?, decision: r.get(7)?, created_by: r.get(8)?, created_at: r.get(9)?,
    })
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn approval_create(&self, session: &str, request_id: &str, tool: &str, title: &str, input: &serde_json::Value,
                           cwd: &str, expires_at: i64) -> Result<Approval> {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let kind = if tool == "AskUserQuestion" { "question" } else { "permission" };
        self.db.execute(
            "INSERT INTO approvals(id, session, request_id, tool, title, input, cwd, expires_at, kind) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id, session, request_id, clip(tool, 120), clip(title, 300), input.to_string(), cwd, expires_at, kind],
        )?;
        Ok(self.approval_get(&id)?.expect("just inserted"))
    }

    pub fn approval_get(&self, id: &str) -> Result<Option<Approval>> {
        Ok(self.db.query_row(&format!("SELECT {APPROVAL_COLS} FROM approvals a WHERE a.id=?1"), params![id], approval_row).optional()?)
    }

    /// An approval by id or unique id prefix (4+ characters).
    pub fn approval_resolve(&self, key: &str) -> Result<Approval> {
        if key.len() < 4 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("no approval {key:?} (use the id shown by `ku approvals`)");
        }
        let mut st = self.db.prepare(&format!("SELECT {APPROVAL_COLS} FROM approvals a WHERE a.id LIKE ?1 || '%' LIMIT 2"))?;
        let rows: Vec<Approval> = st.query_map(params![key.to_ascii_lowercase()], approval_row)?.collect::<Result<_, _>>()?;
        match rows.len() {
            1 => Ok(rows.into_iter().next().unwrap()),
            0 => bail!("no approval {key:?}"),
            _ => bail!("approval id {key:?} is ambiguous; type more characters"),
        }
    }

    /// Pending approvals (all sessions, or one), oldest first; `all` adds the last 100 decided ones.
    pub fn approvals(&self, session: Option<&str>, all: bool) -> Result<Vec<Approval>> {
        let sql = if all {
            format!("SELECT {APPROVAL_COLS} FROM approvals a WHERE (?1 IS NULL OR a.session=?1) ORDER BY a.created_at DESC, a.rowid DESC LIMIT 100")
        } else {
            format!("SELECT {APPROVAL_COLS} FROM approvals a WHERE (?1 IS NULL OR a.session=?1) AND a.status='pending' ORDER BY a.created_at, a.rowid")
        };
        let mut st = self.db.prepare(&sql)?;
        let rows = st.query_map(params![session], approval_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Settle a pending approval. Returns false if it was no longer pending (single use).
    pub fn approval_decide(&self, id: &str, status: &str, by: &str, reason: Option<&str>, rule_id: Option<i64>) -> Result<bool> {
        let n = self.db.execute(
            "UPDATE approvals SET status=?2, decided_by=?3, reason=?4, rule_id=?5, decided_at=unixepoch() WHERE id=?1 AND status='pending'",
            params![id, status, by, reason.map(|r| clip(r, 300)), rule_id],
        )?;
        Ok(n == 1)
    }

    pub fn approval_set_answer(&self, id: &str, answer: &serde_json::Value) -> Result<()> {
        self.db.execute("UPDATE approvals SET answer=?2 WHERE id=?1", params![id, answer.to_string()])?;
        Ok(())
    }

    /// Pending approvals past their expiry.
    pub fn approvals_expired(&self, now: i64) -> Result<Vec<Approval>> {
        let mut st = self.db.prepare(&format!("SELECT {APPROVAL_COLS} FROM approvals a WHERE a.status='pending' AND a.expires_at <= ?1"))?;
        let rows = st.query_map(params![now], approval_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn rule_add(&self, scope: &str, target: &str, tool: &str, in_cwd: bool, command_prefix: Option<&str>, decision: &str,
                    by: &str) -> Result<Rule> {
        if !["role", "session"].contains(&scope) {
            bail!("rule scope must be role or session");
        }
        if !["allow", "deny"].contains(&decision) {
            bail!("rule decision must be allow or deny");
        }
        if tool.is_empty() || tool.len() > 120 || tool.contains(char::is_whitespace) {
            bail!("invalid tool {tool:?} (a tool name like Bash or Edit, a prefix like mcp__github__*, or *)");
        }
        if decision == "allow" && tool == "*" && !in_cwd && command_prefix.is_none() {
            bail!("refusing an allow-everything rule: name a tool, or add --in-cwd");
        }
        self.db.execute(
            "INSERT INTO approval_rules(scope, target, tool, in_cwd, command_prefix, decision, created_by) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![scope, target, tool, in_cwd as i64, command_prefix, decision, by],
        )?;
        let id = self.db.last_insert_rowid();
        Ok(self.rules()?.into_iter().find(|r| r.id == id).expect("just inserted"))
    }

    pub fn rules(&self) -> Result<Vec<Rule>> {
        let mut st = self.db.prepare(
            "SELECT r.id, r.scope, r.target, (SELECT name FROM sessions WHERE id=r.target), r.tool, r.in_cwd, r.command_prefix, \
             r.decision, r.created_by, r.created_at FROM approval_rules r ORDER BY r.id",
        )?;
        let rows = st.query_map([], rule_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn rule_remove(&self, id: i64) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM approval_rules WHERE id=?1", params![id])? == 1)
    }
}

fn msg_row(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    let data = r.get::<_, Option<String>>(5).ok().flatten().and_then(|d| serde_json::from_str(&d).ok());
    Ok(Message { seq: r.get(0)?, session: r.get(1)?, role: r.get(2)?, text: r.get(3)?, at: r.get(4)?, data })
}

/// Paged history query (see `Store::history_page`).
#[derive(Debug, Default, Clone)]
pub struct HistoryQuery {
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub limit: i64,
    pub search: Option<String>,
    pub roles: Vec<String>,
    pub tools: bool,
}

/// Messages the Telegram channel keeps per chat (the `telegram_history` tool).
const TG_KEEP: i64 = 5000;
/// Chunks of one reply sent within this many seconds are stored as one message.
const TG_REPLY_JOIN_S: i64 = 120;

/// Paged query over the owner's Telegram chat (see `Store::tg_page`).
#[derive(Debug, Default, Clone)]
pub struct TgQuery {
    /// Only messages older than this seq (page backwards).
    pub before: Option<i64>,
    pub limit: i64,
    /// Case-insensitive substring in the text or the attachment marker.
    pub search: Option<String>,
    /// Only messages from the user that never got an agent reply.
    pub unanswered: bool,
}

#[cfg(test)]
mod tests {
    #[test]
    fn undo_removes_the_turns_history_rows_and_its_telegram_rows() {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::open(&dir.path().join("k.db")).unwrap();
        let id = st.create("s1", "claude", "/w", "worker", default_model("claude"), "", None).unwrap().id;
        let p1 = st.append_data(&id, "user", "first prompt", &serde_json::json!({"delivery": "queued"})).unwrap();
        st.append(&id, "agent", "first reply").unwrap();
        let p2 = st.append_data(&id, "user", "make secret-file", &serde_json::json!({"delivery": "queued"})).unwrap();
        st.merge_message_data(p2, &serde_json::json!({"telegram_update": 22})).unwrap();
        st.merge_message_data(p2, &serde_json::json!({"delivery": "delivered"})).unwrap();
        st.append_data(&id, "tool", "Write secret-file", &serde_json::json!({"status": "completed"})).unwrap();
        st.append(&id, "agent", "created secret-file").unwrap();
        let last = st.last_prompt(&id).unwrap().unwrap();
        assert_eq!(last.seq, p2);
        assert_eq!(last.data.as_ref().unwrap()["telegram_update"], 22, "merge keeps earlier fields");
        assert_eq!(last.data.as_ref().unwrap()["delivery"], "delivered");
        let removed = st.remove_messages_from(&id, p2).unwrap();
        assert_eq!(removed.len(), 3);
        assert_eq!(removed[0].text, "make secret-file");
        let q = HistoryQuery { limit: 50, search: Some("secret-file".into()), ..Default::default() };
        assert_eq!(st.history_page(&id, &q).unwrap().0.len(), 0, "a search no longer finds the undone turn");
        assert_eq!(st.last_prompt(&id).unwrap().unwrap().seq, p1);
        assert_eq!(st.history(&id, 0, 50).unwrap().len(), 2);

        // Telegram side: the prompt, the bot's replies, and the stored rows.
        let chat = "7";
        st.tg_record_inbound(chat, Some(500), 21, "first prompt", None, 1000).unwrap();
        st.tg_record_inbound(chat, Some(502), 22, "make secret-file", None, 1100).unwrap();
        st.tg_record_agent(chat, Some(22), "created secret-file", 1110).unwrap();
        st.tg_record_sent(chat, 22, 503, 1110).unwrap();
        st.tg_record_sent(chat, 22, 504, 1111).unwrap();
        st.tg_record_sent(chat, 21, 501, 1001).unwrap();
        let msgs = st.tg_update_messages(chat, &[22]).unwrap();
        assert_eq!(msgs, vec![(502, 1100, true), (503, 1110, false), (504, 1111, false)]);
        assert_eq!(st.tg_forget_updates(chat, &[22]).unwrap(), 2);
        assert!(st.tg_update_messages(chat, &[22]).unwrap().is_empty());
        let q = TgQuery { limit: 20, search: Some("secret-file".into()), ..Default::default() };
        assert_eq!(st.tg_page(chat, &q).unwrap().1, 0);
        assert_eq!(st.tg_page(chat, &TgQuery { limit: 20, ..Default::default() }).unwrap().1, 1);
    }

    #[test]
    fn pending_notes_join_and_are_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::open(&dir.path().join("k.db")).unwrap();
        let id = st.create("s1", "agy", "/w", "worker", default_model("agy"), "", None).unwrap().id;
        assert_eq!(st.take_pending_note(&id).unwrap(), None);
        st.add_pending_note(&id, "a").unwrap();
        st.add_pending_note(&id, "b").unwrap();
        assert_eq!(st.take_pending_note(&id).unwrap().as_deref(), Some("a\nb"));
        assert_eq!(st.take_pending_note(&id).unwrap(), None);
    }

    #[test]
    fn telegram_rows_keep_order_answer_flags_and_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::open(&dir.path().join("k.db")).unwrap();
        let chat = "1";
        let att = serde_json::json!({"kind": "voice", "name": "voice.ogg", "mime": "audio/ogg"});
        assert!(st.tg_record_inbound(chat, Some(101), 11, "", Some(&att), 1000).unwrap().is_some());
        // A replayed update is ignored, not duplicated.
        assert!(st.tg_record_inbound(chat, Some(101), 11, "", Some(&att), 1000).unwrap().is_none());
        assert!(st.tg_record_inbound(chat, Some(102), 12, "hello", None, 1010).unwrap().is_some());
        assert_eq!(st.tg_mark_answered(chat, 11, 1015).unwrap(), 1);
        // Marking twice is a no-op.
        assert_eq!(st.tg_mark_answered(chat, 11, 1016).unwrap(), 0);
        // Voice transcript fills the empty text later.
        assert_eq!(st.tg_set_transcript(chat, 11, "buy milk").unwrap(), 1);
        // Two chunks of one reply become one message; a later one stands alone.
        st.tg_record_agent(chat, Some(12), "first half", 1020).unwrap();
        st.tg_record_agent(chat, Some(12), "second half", 1021).unwrap();
        st.tg_record_agent(chat, Some(12), "much later", 1021 + TG_REPLY_JOIN_S + 1).unwrap();

        let q = TgQuery { limit: 20, ..Default::default() };
        let (all, total) = st.tg_page(chat, &q).unwrap();
        assert_eq!(total, 4);
        let texts: Vec<&str> = all.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts[0], "buy milk");
        assert_eq!(texts[1], "hello");
        assert!(texts[2].contains("first half\n\nsecond half"), "{:?}", texts[2]);
        assert_eq!(texts[3], "much later");
        assert_eq!((all[0].answered, all[1].answered), (true, false), "a recorded message is not a reply; only tg_mark_answered answers");
        assert_eq!(all[0].attachment.as_ref().unwrap()["kind"], "voice");
        // Newest page, then back.
        let (page, _) = st.tg_page(chat, &TgQuery { limit: 2, ..Default::default() }).unwrap();
        assert_eq!(page.len(), 2);
        let (older, _) = st.tg_page(chat, &TgQuery { limit: 2, before: Some(page[0].seq), ..Default::default() }).unwrap();
        assert_eq!(older.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![all[0].seq, all[1].seq]);
        // Search and unanswered filters.
        assert_eq!(st.tg_page(chat, &TgQuery { limit: 20, search: Some("MILK".into()), ..Default::default() }).unwrap().1, 1);
        assert_eq!(st.tg_page(chat, &TgQuery { limit: 20, search: Some("voice.%".into()), ..Default::default() }).unwrap().1, 0, "LIKE wildcards are literal");
        assert_eq!(st.tg_page(chat, &TgQuery { limit: 20, unanswered: true, ..Default::default() }).unwrap().1, 1, "update 12 never got a reply");
        // Another chat's rows are never returned.
        assert_eq!(st.tg_page("2", &q).unwrap().1, 0);
    }

    #[test]
    fn agy_migration_preserves_existing_sessions_and_metrics_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old-opencode.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&SCHEMA.replace(",'agy','fx','pi'", "")).unwrap();
        // SQLite quotes table names after ALTER TABLE RENAME, as in live stores.
        db.execute_batch("ALTER TABLE sessions RENAME TO legacy_sessions; ALTER TABLE legacy_sessions RENAME TO sessions;").unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        db.execute("INSERT INTO sessions(id,name,harness,cwd) VALUES (?1,'existing','opencode','/work')", [&id]).unwrap();
        db.execute("INSERT INTO harness_metrics(session,data) VALUES (?1,'{}')", [&id]).unwrap();
        drop(db);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().harness, "opencode");
        assert_eq!(store.harness_metrics(&id).unwrap(), Some(serde_json::json!({})));
        store.create("agy", "agy", "/work", "worker", "gemini-3.8-flash", "", None).unwrap();
        store.create("fx", "fx", "/work", "worker", "codex/gpt-6-luna", "", None).unwrap();
        store.create("pi", "pi", "/work", "worker", "ollama/deepseek-v4.1-flash:cloud", "", None).unwrap();
        let violations: i64 = store.db.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
        assert_eq!(violations, 0);
    }

    #[test]
    fn opencode_migration_preserves_history_and_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&SCHEMA.replace("'acp','opencode','agy','fx','pi'", "'acp'")).unwrap();
        let parent = uuid::Uuid::new_v4().to_string();
        let child = uuid::Uuid::new_v4().to_string();
        db.execute("INSERT INTO sessions(id,name,harness,cwd) VALUES (?1,'parent','claude','/tmp/parent')", [&parent]).unwrap();
        db.execute("INSERT INTO sessions(id,name,harness,cwd,created_by_session) VALUES (?1,'child','codex','/tmp/child',?2)", params![child,parent]).unwrap();
        db.execute("INSERT INTO messages(session,role,text) VALUES (?1,'user','preserved')", [&child]).unwrap();
        drop(db);
        let st = Store::open(&path).unwrap();
        assert_eq!(st.history(&child, 0, 10).unwrap()[0].text, "preserved");
        assert_eq!(st.get(&child).unwrap().unwrap().created_by_session, Some(parent));
        st.create("open", "opencode", "/tmp/open", "worker", "fixture/free", "", None).unwrap();
        let violations: i64 = st.db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r|r.get(0)).unwrap();
        assert_eq!(violations, 0);
    }
    use super::*;

    #[test]
    fn running_tool_rows_lists_only_unfinished_tools() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("k.db")).unwrap();
        let lead = s.create("lead", "claude", "/w/lead", "lead", DEFAULT_MODEL, "boss", None).unwrap();
        let running = s.append_data(&lead.id, "tool", "Shell", &serde_json::json!({"tool": "Shell", "status": "running"})).unwrap();
        s.append_data(&lead.id, "tool", "Read", &serde_json::json!({"tool": "Read", "status": "completed"})).unwrap();
        let rows = s.running_tool_rows(&lead.id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, running);
        s.set_message_data(running, &serde_json::json!({"tool": "Shell", "status": "completed"})).unwrap();
        assert!(s.running_tool_rows(&lead.id).unwrap().is_empty());
    }

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
        assert_eq!(effort_levels("codex"), ["low", "medium", "high", "xhigh"]);
        assert!(effort_levels("claude").contains(&"max"));
        assert!(s.update_field(&lead.id, &w.id, "cwd", "/etc").is_err());
        assert!(!s.update_field(&lead.id, &w.id, "effort", "low").unwrap(), "no-op is not written");
        assert!(s.update_field(&lead.id, &w.id, "model", "not-a-model").is_err(), "model validated");
        s.update_field(&lead.id, &w.id, "model", "sonnet").unwrap();
        // prefix resolution
        assert_eq!(s.resolve(&w.id[..8]).unwrap().as_deref(), Some(w.id.as_str()));
        assert!(s.resolve("zzzzzz").unwrap().is_none());
        let a: Vec<_> = s.audit(Some(&w.id)).unwrap().into_iter().filter(|e| !e["field"].as_str().unwrap().starts_with("event:")).collect();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0]["actor_name"], "lead");
        assert!(s.audit(Some(&w.id)).unwrap().iter().any(|e| e["field"] == "event:created"));
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

#[cfg(test)]
mod model_tests {
    use super::validate_model;
    #[test]
    fn models() {
        for ok in ["claude-haiku-5-5", "claude-opus-5-5[1m]", "haiku", "opus", "sonnet[1m]"] {
            assert!(validate_model("claude", ok).is_ok(), "{ok}");
        }
        for bad in ["not-a-model", "", "claude-", "gpt 5", "claude-Haiku"] {
            assert!(validate_model("claude", bad).is_err(), "{bad}");
        }
        assert!(validate_model("codex", "gpt-5.5-codex").is_ok());
    }

    #[test]
    fn codex_catalogue() {
        let ids: Vec<String> = super::model_catalogue("codex").into_iter().map(|(id, _)| id).collect();
        assert!(ids.iter().any(|id| id == "default") && ids.iter().any(|id| id == "gpt-6.1-sol"));
        assert_eq!(super::default_model("codex"), "default");
        assert_eq!(super::default_model("claude"), super::DEFAULT_MODEL);
        assert!(super::model_catalogue("acp").is_empty());
    }
}

#[cfg(test)]
mod warn_tests {
    #[test]
    fn unverified_models_warn() {
        assert!(super::model_warning("claude", "claude-haiku-5-5").is_none());
        assert!(super::model_warning("claude", "opus[1m]").is_none());
        assert!(super::model_warning("claude", "claude-fable-5-2").is_none());
        assert!(super::model_warning("claude", "claude-fable-5-1").is_none());
        let ids: Vec<String> = super::model_catalogue("claude").into_iter().map(|(id, _)| id).collect();
        let pos = |m: &str| ids.iter().position(|id| id == m).unwrap();
        assert!(pos("claude-fable-5-2") < pos("claude-fable-5-1"));
        assert!(super::model_warning("claude", "claude-haiku-9-9").unwrap().contains("unverified"));
        assert!(super::model_warning("codex", "anything").is_none());
    }
}
