use anyhow::Result;
use rusqlite::{Connection, params};
use chrono::Utc;
use std::path::PathBuf;

pub struct AuditSession {
    conn: Connection,
    session_id: i64,
}

impl AuditSession {
    pub fn open(data_dir: &PathBuf, model: &str) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join("audit.db"))?;
        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS sessions (
                id INTEGER PRIMARY KEY,
                started_at TEXT NOT NULL,
                model TEXT NOT NULL,
                claude_session_id TEXT
            );
            CREATE TABLE IF NOT EXISTS turns (
                id INTEGER PRIMARY KEY,
                session_id INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                ts TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS commands (
                id INTEGER PRIMARY KEY,
                session_id INTEGER NOT NULL,
                command TEXT NOT NULL,
                output TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT
            );
            CREATE TABLE IF NOT EXISTS errors (
                id INTEGER PRIMARY KEY,
                session_id INTEGER NOT NULL,
                message TEXT NOT NULL,
                ts TEXT NOT NULL
            );
        ")?;

        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sessions (started_at, model) VALUES (?1, ?2)",
            params![now, model],
        )?;
        let session_id = conn.last_insert_rowid();
        Ok(AuditSession { conn, session_id })
    }

    pub fn log_turn(&self, role: &str, content: &str) {
        let _ = self.conn.execute(
            "INSERT INTO turns (session_id, role, content, ts) VALUES (?1, ?2, ?3, ?4)",
            params![self.session_id, role, content, Utc::now().to_rfc3339()],
        );
    }

    pub fn log_command_start(&self, cmd: &str) -> i64 {
        let now = Utc::now().to_rfc3339();
        let _ = self.conn.execute(
            "INSERT INTO commands (session_id, command, started_at) VALUES (?1, ?2, ?3)",
            params![self.session_id, cmd, now],
        );
        self.conn.last_insert_rowid()
    }

    pub fn log_command_done(&self, cmd_id: i64, output: &str) {
        let _ = self.conn.execute(
            "UPDATE commands SET output = ?1, finished_at = ?2 WHERE id = ?3",
            params![output, Utc::now().to_rfc3339(), cmd_id],
        );
    }

    pub fn log_error(&self, msg: &str) {
        let _ = self.conn.execute(
            "INSERT INTO errors (session_id, message, ts) VALUES (?1, ?2, ?3)",
            params![self.session_id, msg, Utc::now().to_rfc3339()],
        );
    }

    pub fn update_model(&self, model: &str) {
        let _ = self.conn.execute(
            "UPDATE sessions SET model = ?1 WHERE id = ?2",
            params![model, self.session_id],
        );
    }

    pub fn update_claude_session(&self, claude_sid: &str) {
        let _ = self.conn.execute(
            "UPDATE sessions SET claude_session_id = ?1 WHERE id = ?2",
            params![claude_sid, self.session_id],
        );
    }

    pub fn db_size_mb(&self) -> f64 {
        self.conn
            .query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get::<_, i64>(0))
            .map(|b| b as f64 / 1_000_000.0)
            .unwrap_or(0.0)
    }
}
