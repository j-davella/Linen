"""
Structured SQLite audit trail for Linen sessions.

Every user message, assistant response, shell command + output, and error
is persisted to ~/.linen/audit.db.  Thread-safe via a per-connection lock.
"""

import sqlite3
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

_DB_PATH = Path.home() / '.linen' / 'audit.db'
_lock = threading.Lock()
_conn: sqlite3.Connection | None = None


def _get() -> sqlite3.Connection:
    global _conn
    if _conn is None:
        _DB_PATH.parent.mkdir(parents=True, exist_ok=True)
        c = sqlite3.connect(str(_DB_PATH), check_same_thread=False)
        c.execute("PRAGMA journal_mode=WAL")
        c.execute("PRAGMA synchronous=NORMAL")
        c.executescript("""
            CREATE TABLE IF NOT EXISTS sessions (
                id             INTEGER PRIMARY KEY,
                started_at     TEXT NOT NULL,
                ended_at       TEXT,
                model          TEXT,
                claude_session TEXT
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
                turn_id    INTEGER,
                ts         TEXT NOT NULL,
                cmd        TEXT NOT NULL,
                output     TEXT,
                elapsed_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS errors (
                id         INTEGER PRIMARY KEY,
                session_id INTEGER,
                ts         TEXT NOT NULL,
                message    TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_turns_session   ON turns(session_id);
            CREATE INDEX IF NOT EXISTS idx_commands_session ON commands(session_id);
        """)
        c.commit()
        _conn = c
    return _conn


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec='milliseconds')


class Session:
    """Represents one Linen launch.  Create once; call close() on exit."""

    def __init__(self, model: str):
        with _lock:
            cur = _get().execute(
                "INSERT INTO sessions (started_at, model) VALUES (?,?)",
                (_now(), model),
            )
            _get().commit()
        self.id: int = cur.lastrowid
        self._last_turn_id: int | None = None
        self._pending_cmd_id: int | None = None
        self._cmd_t0: float = 0.0

    # ── session metadata ───────────────────────────────────────────────────

    def update_model(self, model: str):
        with _lock:
            _get().execute("UPDATE sessions SET model=? WHERE id=?", (model, self.id))
            _get().commit()

    def update_claude_session(self, claude_id: str):
        with _lock:
            _get().execute(
                "UPDATE sessions SET claude_session=? WHERE id=?",
                (claude_id, self.id),
            )
            _get().commit()

    # ── turn logging ───────────────────────────────────────────────────────

    def log_turn(self, role: str, content: str) -> int:
        """role: 'user' | 'assistant'"""
        with _lock:
            cur = _get().execute(
                "INSERT INTO turns (session_id, ts, role, content) VALUES (?,?,?,?)",
                (self.id, _now(), role, content),
            )
            _get().commit()
        self._last_turn_id = cur.lastrowid
        return self._last_turn_id

    # ── command logging ────────────────────────────────────────────────────

    def log_command_start(self, cmd: str) -> int:
        self._cmd_t0 = time.monotonic()
        with _lock:
            cur = _get().execute(
                "INSERT INTO commands (session_id, turn_id, ts, cmd) VALUES (?,?,?,?)",
                (self.id, self._last_turn_id, _now(), cmd),
            )
            _get().commit()
        self._pending_cmd_id = cur.lastrowid
        return self._pending_cmd_id

    def log_command_done(self, output: str):
        if self._pending_cmd_id is None:
            return
        elapsed = int((time.monotonic() - self._cmd_t0) * 1000)
        with _lock:
            _get().execute(
                "UPDATE commands SET output=?, elapsed_ms=? WHERE id=?",
                (output, elapsed, self._pending_cmd_id),
            )
            _get().commit()
        self._pending_cmd_id = None

    # ── error logging ──────────────────────────────────────────────────────

    def log_error(self, message: str):
        with _lock:
            _get().execute(
                "INSERT INTO errors (session_id, ts, message) VALUES (?,?,?)",
                (self.id, _now(), message),
            )
            _get().commit()

    # ── lifecycle ──────────────────────────────────────────────────────────

    def close(self):
        with _lock:
            _get().execute(
                "UPDATE sessions SET ended_at=? WHERE id=?",
                (_now(), self.id),
            )
            _get().commit()
