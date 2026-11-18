//! SQLite audit trail: every message, tool call, command (with its approval
//! decision), and error, in ~/.linen/linen.db.
//!
//! A new file name (not v1's audit.db) because v0.1 and v0.2 wrote two
//! incompatible schemas to audit.db; this avoids migrating either.

use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub struct Audit {
    conn: Mutex<Connection>,
    session_id: i64,
    path: PathBuf,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl Audit {
    pub fn open(dir: &Path, model: &str, workspace: &Path) -> Result<Audit> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("linen.db");
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id             INTEGER PRIMARY KEY,
                 started_at     TEXT NOT NULL,
                 ended_at       TEXT,
                 model          TEXT,
                 claude_session TEXT,
                 workspace      TEXT
             );
             CREATE TABLE IF NOT EXISTS turns (
                 id         INTEGER PRIMARY KEY,
                 session_id INTEGER NOT NULL,
                 ts         TEXT NOT NULL,
                 role       TEXT NOT NULL,
                 content    TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS commands (
                 id         INTEGER PRIMARY KEY,
                 session_id INTEGER NOT NULL,
                 ts         TEXT NOT NULL,
                 kind       TEXT NOT NULL,
                 cmd        TEXT NOT NULL,
                 decision   TEXT,
                 output     TEXT,
                 exit_code  INTEGER,
                 elapsed_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS errors (
                 id         INTEGER PRIMARY KEY,
                 session_id INTEGER,
                 ts         TEXT NOT NULL,
                 message    TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_turns_session    ON turns(session_id);
             CREATE INDEX IF NOT EXISTS idx_commands_session ON commands(session_id);",
        )?;
        conn.execute(
            "INSERT INTO sessions (started_at, model, workspace) VALUES (?1, ?2, ?3)",
            params![now(), model, workspace.to_string_lossy()],
        )?;
        let session_id = conn.last_insert_rowid();
        Ok(Audit { conn: Mutex::new(conn), session_id, path })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn turn(&self, role: &str, content: &str) {
        let _ = self.db().execute(
            "INSERT INTO turns (session_id, ts, role, content) VALUES (?1, ?2, ?3, ?4)",
            params![self.session_id, now(), role, content],
        );
    }

    pub fn command_start(&self, kind: &str, cmd: &str) -> i64 {
        let db = self.db();
        let _ = db.execute(
            "INSERT INTO commands (session_id, ts, kind, cmd) VALUES (?1, ?2, ?3, ?4)",
            params![self.session_id, now(), kind, cmd],
        );
        db.last_insert_rowid()
    }

    pub fn command_decision(&self, id: i64, decision: &str) {
        let _ = self.db().execute(
            "UPDATE commands SET decision = ?1 WHERE id = ?2",
            params![decision, id],
        );
    }

    pub fn command_done(&self, id: i64, output: &str, exit_code: Option<i64>, elapsed_ms: i64) {
        let _ = self.db().execute(
            "UPDATE commands SET output = ?1, exit_code = ?2, elapsed_ms = ?3 WHERE id = ?4",
            params![output, exit_code, elapsed_ms, id],
        );
    }

    pub fn error(&self, msg: &str) {
        let _ = self.db().execute(
            "INSERT INTO errors (session_id, ts, message) VALUES (?1, ?2, ?3)",
            params![self.session_id, now(), msg],
        );
    }

    pub fn set_model(&self, model: &str) {
        let _ = self.db().execute(
            "UPDATE sessions SET model = ?1 WHERE id = ?2",
            params![model, self.session_id],
        );
    }

    pub fn set_claude_session(&self, sid: &str) {
        let _ = self.db().execute(
            "UPDATE sessions SET claude_session = ?1 WHERE id = ?2",
            params![sid, self.session_id],
        );
    }

    pub fn close(&self) {
        let _ = self.db().execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![now(), self.session_id],
        );
    }

    /// Rows logged for this launch (shown as "log entries" on the instance card).
    pub fn entry_count(&self) -> i64 {
        self.db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM turns WHERE session_id = ?1)
                      + (SELECT COUNT(*) FROM commands WHERE session_id = ?1)",
                params![self.session_id],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// Size on disk of the database (plus its WAL).
    pub fn bytes(&self) -> u64 {
        let wal = self.path.with_extension("db-wal");
        [&self.path, &wal]
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum()
    }
}
