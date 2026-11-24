//! The SQLite world store.
//!
//! One transaction per command: the whole changing state (a few dozen
//! rows' worth of JSON) and the events it emitted. A crash loses at most
//! the command in flight, and a restart resumes the same dice stream.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::event::Event;
use crate::state::WorldState;

/// A store failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A row did not decode.
    #[error("decoding a stored row: {0}")]
    Decode(#[from] serde_json::Error),
}

/// What [`Store::load`] returns.
#[derive(Debug)]
pub struct Loaded {
    /// The pack the state belongs to.
    pub pack: String,
    /// The state.
    pub state: WorldState,
    /// The most recent events, oldest first.
    pub recent: Vec<Event>,
}

/// The world database.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS snapshot (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    pack TEXT NOT NULL,
    state TEXT NOT NULL,
    saved_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY,
    at_ms INTEGER NOT NULL,
    room TEXT,
    kind TEXT NOT NULL,
    body TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_room ON events (room, seq);
";

impl Store {
    /// Opens or creates the database at `path`.
    ///
    /// # Errors
    /// SQLite cannot open or migrate the file.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Self::init(Connection::open(path)?)
    }

    /// An in-memory store, for tests.
    ///
    /// # Errors
    /// SQLite fails.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Saves the state and appends events, atomically.
    ///
    /// # Errors
    /// SQLite or JSON encoding fails; nothing is written then.
    pub fn save(
        &mut self,
        pack: &str,
        state: &WorldState,
        events: &[Event],
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let state_json = serde_json::to_string(state)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO snapshot (id, pack, state, saved_ms) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET pack = ?1, state = ?2, saved_ms = ?3",
            params![pack, state_json, i64::try_from(now_ms).unwrap_or(i64::MAX)],
        )?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO events (seq, at_ms, room, kind, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in events {
                ins.execute(params![
                    i64::try_from(e.seq.0).unwrap_or(i64::MAX),
                    i64::try_from(e.at_ms).unwrap_or(i64::MAX),
                    e.room.as_ref().map(crate::ids::RoomId::as_str),
                    e.kind.name(),
                    serde_json::to_string(e)?,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads the saved state and the last `recent` events.
    ///
    /// # Errors
    /// SQLite fails or a row does not decode.
    pub fn load(&self, recent: usize) -> Result<Option<Loaded>, StoreError> {
        let row: Option<(String, String)> = self
            .conn
            .query_row("SELECT pack, state FROM snapshot WHERE id = 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((pack, state)) = row else {
            return Ok(None);
        };
        let state: WorldState = serde_json::from_str(&state)?;
        let mut recent = self.tail(recent)?;
        recent.reverse();
        Ok(Some(Loaded {
            pack,
            state,
            recent,
        }))
    }

    fn tail(&self, n: usize) -> Result<Vec<Event>, StoreError> {
        let mut q = self
            .conn
            .prepare("SELECT body FROM events ORDER BY seq DESC LIMIT ?1")?;
        let rows = q.query_map([i64::try_from(n).unwrap_or(i64::MAX)], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Events after `seq`, oldest first, at most `limit`.
    ///
    /// # Errors
    /// SQLite fails or a row does not decode.
    pub fn events_after(&self, seq: u64, limit: usize) -> Result<Vec<Event>, StoreError> {
        let mut q = self
            .conn
            .prepare("SELECT body FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let rows = q.query_map(
            params![
                i64::try_from(seq).unwrap_or(i64::MAX),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |r| r.get::<_, String>(0),
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// The number of stored events.
    ///
    /// # Errors
    /// SQLite fails.
    pub fn event_count(&self) -> Result<u64, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
}
