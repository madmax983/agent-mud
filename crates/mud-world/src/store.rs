//! The SQLite world store.
//!
//! One transaction per command: the whole changing state (a few dozen
//! rows' worth of JSON) and the events it emitted. A crash loses at most
//! the command in flight, and a restart resumes the same dice stream.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::event::Event;
use crate::ids::EntityId;
use crate::rewards::MintRecord;
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
CREATE TABLE IF NOT EXISTS mints (
    seq INTEGER PRIMARY KEY,
    pool TEXT NOT NULL,
    period INTEGER NOT NULL,
    entity TEXT NOT NULL,
    body TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS mints_pool ON mints (pool, period);
CREATE TABLE IF NOT EXISTS script_memory (
    entity TEXT PRIMARY KEY,
    schema INTEGER NOT NULL,
    body TEXT NOT NULL,
    saved_ms INTEGER NOT NULL
);
";

/// The script memory of every mind: its schema and its body, by entity.
pub type ScriptMemory = BTreeMap<EntityId, (u32, serde_json::Value)>;

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

    /// Saves the state and appends events and ledger rows, atomically. A
    /// row saved before (by `seq`) is kept as it was.
    ///
    /// # Errors
    /// SQLite or JSON encoding fails; nothing is written then.
    pub fn save(
        &mut self,
        pack: &str,
        state: &WorldState,
        events: &[Event],
        mints: &[MintRecord],
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
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO mints (seq, pool, period, entity, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for m in mints {
                ins.execute(params![
                    i64::try_from(m.seq.0).unwrap_or(i64::MAX),
                    m.pool.as_str(),
                    i64::try_from(m.period).unwrap_or(i64::MAX),
                    m.entity.as_str(),
                    serde_json::to_string(m)?,
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

    /// The whole ledger, oldest first: every mint since the world began.
    ///
    /// # Errors
    /// SQLite fails or a row does not decode.
    pub fn mints(&self) -> Result<Vec<MintRecord>, StoreError> {
        let mut q = self.conn.prepare("SELECT body FROM mints ORDER BY seq")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Every mind's script memory.
    ///
    /// # Errors
    /// SQLite fails or a row does not decode.
    pub fn load_script_memory(&self) -> Result<ScriptMemory, StoreError> {
        let mut q = self
            .conn
            .prepare("SELECT entity, schema, body FROM script_memory ORDER BY entity")?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (entity, schema, body) = row?;
            out.insert(
                EntityId::new(entity),
                (
                    u32::try_from(schema).unwrap_or(0),
                    serde_json::from_str(&body)?,
                ),
            );
        }
        Ok(out)
    }

    /// Saves one mind's script memory.
    ///
    /// # Errors
    /// SQLite or JSON encoding fails.
    pub fn save_script_memory(
        &mut self,
        entity: &EntityId,
        schema: u32,
        body: &serde_json::Value,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO script_memory (entity, schema, body, saved_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (entity) DO UPDATE SET schema = ?2, body = ?3, saved_ms = ?4",
            params![
                entity.as_str(),
                i64::from(schema),
                serde_json::to_string(body)?,
                i64::try_from(now_ms).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    /// Forgets one mind's script memory.
    ///
    /// # Errors
    /// SQLite fails.
    pub fn clear_script_memory(&mut self, entity: &EntityId) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM script_memory WHERE entity = ?1",
            params![entity.as_str()],
        )?;
        Ok(())
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
