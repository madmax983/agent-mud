//! The SQLite world store (ADR 0011).
//!
//! One transaction per command: the whole changing state (a few dozen
//! rows' worth of JSON), the events it emitted, the ledger rows it minted
//! and the supervisor side ops it carried. A crash loses at most the
//! command in flight, and a restart resumes the same dice stream.
//!
//! The file carries its schema version in `PRAGMA user_version`. Opening it
//! runs the missing steps of [`MIGRATIONS`] and, in the same transaction
//! (with `synchronous = FULL`), mints the file's epoch once and bumps its
//! boot number. A file newer than this code is refused.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use crate::archive::{self, ArchiveRow};
use crate::durability::{SchemaVerdict, schema_verdict};
use crate::event::Event;
use crate::hash::epoch;
use crate::ids::EntityId;
use crate::rewards::MintRecord;
use crate::side::{self, SideOp, SideState, bits, int, uint, unbits};
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
    /// The file was written by a newer server.
    #[error(
        "the world database has schema version {found}; this server knows versions up to {SCHEMA_VERSION}: run a newer server, or restore a backup"
    )]
    Schema {
        /// The file's `user_version`.
        found: u32,
    },
    /// The events archive could not be written; nothing was trimmed.
    #[error("events archive: {0}")]
    Archive(#[from] std::io::Error),
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
    /// Every supervisor side row.
    pub side: SideState,
}

/// How hard a commit is pushed to the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fsync {
    /// WAL with `synchronous = NORMAL`: a process crash loses nothing; an
    /// OS crash may lose the last commits, world and side rows together.
    #[default]
    Normal,
    /// `synchronous = FULL`: durable before it returns. For the commits
    /// behind a promise made outside the process.
    Full,
}

/// One atomic write.
#[derive(Debug, Clone, Copy, Default)]
pub struct Commit<'a> {
    /// The pack name and the state, when the snapshot is written.
    pub snapshot: Option<(&'a str, &'a WorldState)>,
    /// Events to append (a seq stored before is kept as it was).
    pub events: &'a [Event],
    /// Ledger rows to append (likewise).
    pub mints: &'a [MintRecord],
    /// Side ops, applied in order after the rest.
    pub ops: &'a [SideOp],
    /// Now, by the game clock.
    pub now_ms: u64,
    /// How hard to sync.
    pub fsync: Fsync,
}

/// What one [`Store::trim_events`] batch did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Trimmed {
    /// Rows the batch selected (then archived and deleted).
    pub rows: usize,
    /// Lines written to the archive (fewer than `rows` after a crash
    /// re-run: rows already archived are skipped).
    pub archived: usize,
    /// The highest seq trimmed.
    pub through: Option<u64>,
}

/// The world database.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    epoch: u64,
    boot: u64,
}

/// The schema version this code writes.
pub const SCHEMA_VERSION: u32 = 1;

/// Version 1: the world tables of version 0 (the file before versions;
/// every statement is `IF NOT EXISTS`, so a version 0 file upgrades in
/// place) and the supervisor side tables.
const V1: &str = "
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
CREATE TABLE IF NOT EXISTS sup_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sup_entity (
    entity TEXT PRIMARY KEY,
    next_macro_ms INTEGER,
    macro_queued INTEGER NOT NULL DEFAULT 0,
    macro_cleared INTEGER NOT NULL DEFAULT 0,
    macro_ticks INTEGER NOT NULL DEFAULT 0,
    seen_seq INTEGER NOT NULL DEFAULT 0,
    budget_day INTEGER NOT NULL DEFAULT 0,
    local_tokens INTEGER NOT NULL DEFAULT 0,
    local_cost INTEGER NOT NULL DEFAULT 0,
    local_calls INTEGER NOT NULL DEFAULT 0,
    lg_tokens INTEGER NOT NULL DEFAULT 0,
    lg_cost INTEGER NOT NULL DEFAULT 0,
    lg_calls INTEGER NOT NULL DEFAULT 0,
    harvest_tokens_seen INTEGER,
    booked_day INTEGER NOT NULL DEFAULT 0,
    booked_today INTEGER NOT NULL DEFAULT 0,
    saved_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sup_booking (
    id INTEGER PRIMARY KEY,
    entity TEXT NOT NULL,
    due_ms INTEGER NOT NULL,
    note TEXT NOT NULL,
    booked_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sup_booking_entity ON sup_booking (entity, due_ms, id);
CREATE TABLE IF NOT EXISTS sup_plan (
    entity TEXT PRIMARY KEY,
    wake_id TEXT NOT NULL,
    lines TEXT NOT NULL,
    cursor INTEGER NOT NULL,
    errand INTEGER NOT NULL,
    started_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sup_rule_count (
    entity TEXT NOT NULL,
    digest INTEGER NOT NULL,
    dup INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (entity, digest, dup)
);
";

/// The migration steps: step `i` takes a file from version `i` to `i + 1`.
/// A later change appends a step; a step, once shipped, never changes.
pub const MIGRATIONS: &[&str] = &[V1];

const _: () = assert!(MIGRATIONS.len() == SCHEMA_VERSION as usize);

/// The script memory of every mind: its schema and its body, by entity.
pub type ScriptMemory = BTreeMap<EntityId, (u32, serde_json::Value)>;

impl Store {
    /// Opens or creates the database at `path`, upgrades it, and bumps its
    /// boot number. A new file mints its epoch from `seed` and `now_ms`.
    ///
    /// # Errors
    /// SQLite cannot open or migrate the file, or the file is newer than
    /// this code ([`StoreError::Schema`]); a refused file is not changed.
    pub fn open(path: &Path, seed: u64, now_ms: u64) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Self::init(Connection::open(path)?, seed, now_ms)
    }

    /// An in-memory store, for tests.
    ///
    /// # Errors
    /// SQLite fails.
    pub fn open_in_memory(seed: u64, now_ms: u64) -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?, seed, now_ms)
    }

    fn version(conn: &Connection) -> Result<u32, StoreError> {
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        // A negative version is no version this code wrote: refuse it.
        Ok(u32::try_from(v).unwrap_or(u32::MAX))
    }

    fn init(mut conn: Connection, seed: u64, now_ms: u64) -> Result<Self, StoreError> {
        // A newer file is refused before anything, the journal mode
        // included, is written to it.
        let found = Self::version(&conn)?;
        if schema_verdict(found, SCHEMA_VERSION) == SchemaVerdict::Refuse {
            return Err(StoreError::Schema { found });
        }
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        conn.busy_timeout(Duration::from_millis(1_000))?;
        // The opening transaction backs the boot number of every wake id
        // this process makes: it must survive an OS crash.
        conn.pragma_update(None, "synchronous", "FULL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Read again under the write lock: another process may have
        // upgraded the file meanwhile.
        let found = Self::version(&tx)?;
        match schema_verdict(found, SCHEMA_VERSION) {
            SchemaVerdict::Refuse => return Err(StoreError::Schema { found }),
            SchemaVerdict::Upgrade => {
                let from = usize::try_from(found).unwrap_or(usize::MAX);
                for step in MIGRATIONS.iter().skip(from) {
                    tx.execute_batch(step)?;
                }
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SchemaVerdict::Current => {}
        }
        tx.execute(
            "INSERT OR IGNORE INTO sup_meta (key, value) VALUES ('epoch', ?1)",
            [bits(epoch(seed, now_ms))],
        )?;
        tx.execute(
            "INSERT INTO sup_meta (key, value) VALUES ('boot', 1)
             ON CONFLICT (key) DO UPDATE SET value = value + 1",
            [],
        )?;
        let (epoch, boot): (i64, i64) = tx.query_row(
            "SELECT (SELECT value FROM sup_meta WHERE key = 'epoch'),
                    (SELECT value FROM sup_meta WHERE key = 'boot')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.commit()?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self {
            conn,
            epoch: unbits(epoch),
            boot: uint(boot),
        })
    }

    /// This file's epoch: minted once, when the file was made.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// This open's boot number: one more than the open before it.
    #[must_use]
    pub const fn boot(&self) -> u64 {
        self.boot
    }

    /// The file's schema version.
    ///
    /// # Errors
    /// SQLite fails.
    pub fn user_version(&self) -> Result<u32, StoreError> {
        Self::version(&self.conn)
    }

    /// Makes every later write fail, as a full or broken disk would
    /// (`PRAGMA query_only`), or lets writes through again. For fault
    /// drills and tests of the store's retry path.
    ///
    /// # Errors
    /// SQLite fails.
    pub fn refuse_writes(&self, on: bool) -> Result<(), StoreError> {
        self.conn.pragma_update(None, "query_only", on)?;
        Ok(())
    }

    /// Writes `c` in one transaction: nothing of it, or all of it.
    ///
    /// # Errors
    /// SQLite or JSON encoding fails; nothing is written then.
    pub fn commit(&mut self, c: &Commit<'_>) -> Result<(), StoreError> {
        let full = c.fsync == Fsync::Full;
        if full {
            self.conn.pragma_update(None, "synchronous", "FULL")?;
        }
        let out = self.commit_inner(c);
        if full && let Err(e) = self.conn.pragma_update(None, "synchronous", "NORMAL") {
            // The commit's own result stands: it is on the disk, or not.
            tracing::warn!(error = %e, "restoring synchronous = NORMAL failed");
        }
        out
    }

    fn commit_inner(&mut self, c: &Commit<'_>) -> Result<(), StoreError> {
        let state_json = c
            .snapshot
            .map(|(_, s)| serde_json::to_string(s))
            .transpose()?;
        let tx = self.conn.transaction()?;
        if let (Some((pack, _)), Some(state)) = (c.snapshot, &state_json) {
            tx.execute(
                "INSERT INTO snapshot (id, pack, state, saved_ms) VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT (id) DO UPDATE SET pack = ?1, state = ?2, saved_ms = ?3",
                params![pack, state, int(c.now_ms)],
            )?;
        }
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO events (seq, at_ms, room, kind, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in c.events {
                ins.execute(params![
                    int(e.seq.0),
                    int(e.at_ms),
                    e.room.as_ref().map(crate::ids::RoomId::as_str),
                    e.kind.name(),
                    serde_json::to_string(e)?,
                ])?;
            }
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO mints (seq, pool, period, entity, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for m in c.mints {
                ins.execute(params![
                    int(m.seq.0),
                    m.pool.as_str(),
                    int(m.period),
                    m.entity.as_str(),
                    serde_json::to_string(m)?,
                ])?;
            }
        }
        for op in c.ops {
            side::apply_sql(&tx, op, c.now_ms)?;
        }
        tx.commit()?;
        Ok(())
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
        self.commit(&Commit {
            snapshot: Some((pack, state)),
            events,
            mints,
            now_ms,
            ..Commit::default()
        })
    }

    /// Applies side ops alone, atomically.
    ///
    /// # Errors
    /// SQLite or JSON encoding fails; nothing is written then.
    pub fn apply_side(
        &mut self,
        ops: &[SideOp],
        now_ms: u64,
        fsync: Fsync,
    ) -> Result<(), StoreError> {
        self.commit(&Commit {
            ops,
            now_ms,
            fsync,
            ..Commit::default()
        })
    }

    /// Loads the saved state, the last `recent` events and the side rows.
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
            side: self.load_side()?,
        }))
    }

    /// Every supervisor side row, with this open's epoch and boot.
    ///
    /// # Errors
    /// SQLite fails or a row does not decode.
    pub fn load_side(&self) -> Result<SideState, StoreError> {
        side::load_sql(&self.conn)
    }

    /// The stored plan of `entity`, if any.
    ///
    /// # Errors
    /// SQLite fails or the plan does not decode.
    pub fn plan(&self, entity: &EntityId) -> Result<Option<side::PlanRow>, StoreError> {
        side::load_plan(&self.conn, entity)
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
            params![int(seq), i64::try_from(limit).unwrap_or(i64::MAX)],
            |r| r.get::<_, String>(0),
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Archives, then deletes, one batch of old events: at most `batch`
    /// rows, oldest seq first, below the newest `keep_min` rows (never
    /// touched), and stopping before the first row stamped at or after
    /// `cutoff`. So a batch is always a seq prefix of the table, even when
    /// the clock stepped back and `at_ms` does not rise with seq: a row
    /// too young to go holds back every row after it until it ages.
    ///
    /// The rows go to the archive in `archive_dir`, under this file's
    /// epoch, and are synced before the delete commits; the delete removes
    /// exactly those rows. If the archive fails or refuses a row (another
    /// history under this epoch already holds its seq), nothing is deleted.
    ///
    /// # Errors
    /// SQLite fails, or the archive cannot be written or refuses the batch.
    pub fn trim_events(
        &mut self,
        cutoff: u64,
        keep_min: u64,
        batch: u32,
        archive_dir: &Path,
    ) -> Result<Trimmed, StoreError> {
        if cutoff == 0 || batch == 0 {
            return Ok(Trimmed::default());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // The newest row that may go: below the newest `keep_min` rows.
        let bound: Option<i64> = tx
            .query_row(
                "SELECT seq FROM events ORDER BY seq DESC LIMIT 1 OFFSET ?1",
                [int(keep_min)],
                |r| r.get(0),
            )
            .optional()?;
        let Some(bound) = bound else {
            return Ok(Trimmed::default());
        };
        let mut rows: Vec<ArchiveRow> = {
            let mut q = tx.prepare(
                "SELECT seq, at_ms, body FROM events WHERE seq <= ?1 ORDER BY seq LIMIT ?2",
            )?;
            let rows = q.query_map(params![bound, i64::from(batch)], |r| {
                Ok(ArchiveRow {
                    seq: uint(r.get(0)?),
                    at_ms: uint(r.get(1)?),
                    body: r.get(2)?,
                })
            })?;
            rows.collect::<Result<_, _>>()?
        };
        // The prefix: stop at the first row too young to go.
        let old = rows.iter().take_while(|r| r.at_ms < cutoff).count();
        rows.truncate(old);
        let Some(last) = rows.last().map(|r| r.seq) else {
            return Ok(Trimmed::default());
        };
        let archived = archive::append(archive_dir, self.epoch, &rows)?;
        // Exactly the rows selected: every row up to `last`, all archived.
        tx.execute("DELETE FROM events WHERE seq <= ?1", params![int(last)])?;
        tx.commit()?;
        Ok(Trimmed {
            rows: rows.len(),
            archived,
            through: Some(last),
        })
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
                int(now_ms)
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
        Ok(uint(n))
    }

    /// The lowest and highest stored seq.
    ///
    /// # Errors
    /// SQLite fails.
    pub fn event_range(&self) -> Result<Option<(u64, u64)>, StoreError> {
        let r: (Option<i64>, Option<i64>) =
            self.conn
                .query_row("SELECT min(seq), max(seq) FROM events", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        Ok(match r {
            (Some(lo), Some(hi)) => Some((uint(lo), uint(hi))),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests;
