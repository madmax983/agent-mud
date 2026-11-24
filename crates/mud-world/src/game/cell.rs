//! The store cell: the store, the open turns and the pending backlog
//! behind one leaf lock (ADR 0002, ADR 0011).

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use thiserror::Error;

use crate::durability::{MAX_KEEP_DAYS, pending_overflow};
use crate::engine::EVENT_LOG_CAP;
use crate::event::Event;
use crate::ids::EntityId;
use crate::rewards::MintRecord;
use crate::side::SideOp;
use crate::state::WorldState;
use crate::store::{Commit, Fsync, Store, StoreError};

/// How the events table is kept bounded (`[world]` in `mud.toml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retention {
    /// Events older than this many days are archived, then trimmed; 0
    /// keeps every event in the table.
    pub keep_days: u64,
    /// The newest rows never trimmed, whatever their age. At least
    /// [`EVENT_LOG_CAP`], so a reload always finds a full log.
    pub keep_min: u64,
    /// How often retention runs, in game-clock ms.
    pub every_ms: u64,
    /// Rows per transaction.
    pub batch: u32,
    /// Where `events-YYYY-MM.jsonl` go.
    pub archive_dir: PathBuf,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_days: 7,
            keep_min: 10_000,
            every_ms: 3_600_000,
            batch: 5_000,
            archive_dir: PathBuf::from("archive"),
        }
    }
}

impl Retention {
    /// The policy with its bounds applied: `keep_min` at least
    /// [`EVENT_LOG_CAP`], `keep_days` at most [`MAX_KEEP_DAYS`], `batch`
    /// and `every_ms` at least 1.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.keep_min = self
            .keep_min
            .max(u64::try_from(EVENT_LOG_CAP).unwrap_or(u64::MAX));
        self.keep_days = self.keep_days.min(MAX_KEEP_DAYS);
        self.batch = self.batch.max(1);
        self.every_ms = self.every_ms.max(1);
        self
    }
}

/// Where a [`crate::Game::persist`] write went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// In the file now.
    Durable,
    /// Behind a failed save: it goes with the next commit that works.
    Deferred,
}

/// Why a side lane refused.
#[derive(Debug, Error)]
pub enum SideError {
    /// The entity has no open turn of that wake.
    #[error("{entity} has no open turn for wake {wake_id}")]
    StaleTurn {
        /// Whose.
        entity: EntityId,
        /// The wake asked about.
        wake_id: String,
    },
    /// The store failed; nothing of the call was written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One wake's open turn: the side ops it staged, waiting for the mind's
/// first effectful command or for its close.
#[derive(Debug)]
pub(super) struct Turn {
    pub wake_id: String,
    pub staged: Vec<SideOp>,
}

/// Rows a failed save could not write, retried with the next commit.
#[derive(Debug, Default)]
pub(super) struct Pending {
    /// A world change waits: the next write must carry a snapshot. Set by
    /// a failed world commit even when it had no event (an effectful
    /// command may change the world silently, and its drained ops wait
    /// here with it).
    pub world: bool,
    pub events: VecDeque<Event>,
    pub mints: Vec<MintRecord>,
    pub ops: Vec<SideOp>,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        !self.world && self.events.is_empty() && self.mints.is_empty() && self.ops.is_empty()
    }

    /// Whether only side ops wait: they may be written without a snapshot.
    pub fn ops_only(&self) -> bool {
        !self.world && self.events.is_empty() && self.mints.is_empty()
    }

    /// Drops the oldest events past [`EVENT_LOG_CAP`]; returns how many.
    /// Ledger rows and side ops are never dropped: rows are few, and ops
    /// are merges keyed by entity and booking.
    pub fn cap(&mut self) -> usize {
        let drop = pending_overflow(self.events.len(), EVENT_LOG_CAP);
        self.events.drain(..drop);
        drop
    }
}

/// The store and what waits to go into it.
#[derive(Debug)]
pub(super) struct StoreCell {
    pub store: Store,
    pub turns: BTreeMap<EntityId, Turn>,
    pub pending: Pending,
    pub retention: Retention,
}

/// A failed world save: the error and how many old pending events the
/// backlog cap dropped.
pub(super) struct SaveFailed {
    pub error: StoreError,
    pub dropped: usize,
}

impl StoreCell {
    pub fn new(store: Store, retention: Retention) -> Self {
        Self {
            store,
            turns: BTreeMap::new(),
            pending: Pending::default(),
            retention,
        }
    }

    /// Commits a world change: the pending backlog first, then `events`,
    /// `mints` and `ops`, with the snapshot, in one transaction. On a
    /// failure all of it waits in the backlog for the next commit.
    pub fn commit_world(
        &mut self,
        snapshot: (&str, &WorldState),
        events: &[Event],
        mints: Vec<MintRecord>,
        ops: Vec<SideOp>,
        now_ms: u64,
    ) -> Result<(), SaveFailed> {
        let p = &mut self.pending;
        p.events.extend(events.iter().cloned());
        p.mints.extend(mints);
        p.ops.extend(ops);
        let saved = self.store.commit(&Commit {
            snapshot: Some(snapshot),
            events: p.events.make_contiguous(),
            mints: &p.mints,
            ops: &p.ops,
            now_ms,
            fsync: Fsync::Normal,
        });
        match saved {
            Ok(()) => {
                self.pending = Pending::default();
                Ok(())
            }
            Err(error) => {
                self.pending.world = true;
                Err(SaveFailed {
                    error,
                    dropped: self.pending.cap(),
                })
            }
        }
    }

    /// Writes `ops` durably (`synchronous = FULL`), with the backlog and a
    /// snapshot (made only then) when one waits. On a failure the backlog is kept and `ops`
    /// are not: the caller reports the failure and nothing of `ops` is in
    /// the file, now or later.
    pub fn act(
        &mut self,
        pack: &str,
        snapshot: impl FnOnce() -> WorldState,
        ops: &[SideOp],
        now_ms: u64,
    ) -> Result<(), StoreError> {
        if self.pending.is_empty() {
            return self.store.apply_side(ops, now_ms, Fsync::Full);
        }
        let state = snapshot();
        let mut all = self.pending.ops.clone();
        all.extend(ops.iter().cloned());
        let p = &mut self.pending;
        self.store.commit(&Commit {
            snapshot: Some((pack, &state)),
            events: p.events.make_contiguous(),
            mints: &p.mints,
            ops: &all,
            now_ms,
            fsync: Fsync::Full,
        })?;
        self.pending = Pending::default();
        Ok(())
    }

    /// Writes `ops` now if no world change waits ahead of them, else puts
    /// them in the backlog.
    pub fn persist(&mut self, ops: Vec<SideOp>, now_ms: u64) -> (Durability, Option<StoreError>) {
        if !self.pending.ops_only() {
            self.pending.ops.extend(ops);
            return (Durability::Deferred, None);
        }
        self.pending.ops.extend(ops);
        match self
            .store
            .apply_side(&self.pending.ops, now_ms, Fsync::Normal)
        {
            Ok(()) => {
                self.pending.ops.clear();
                (Durability::Durable, None)
            }
            Err(e) => (Durability::Deferred, Some(e)),
        }
    }
}
