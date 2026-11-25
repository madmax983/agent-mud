//! Supervisor side rows: what the supervisor keeps in the world store
//! beside the world (ADR 0011).
//!
//! A [`SideOp`] is plain data. The store applies it as an SQL upsert whose
//! merge is idempotent and monotone, so an op applied twice (a retried
//! save), two ops applied in either order (a deferred op behind a later
//! commit), or the newest ops lost in an OS crash all leave a row the
//! supervisor can trust. The merges are the kernels below, mirrored in
//! `formal/verus/src/side.rs`; [`SideState::apply`] folds them in memory,
//! and a test checks that the SQL agrees with it.
//!
//! Side rows never enter [`crate::state::WorldState`]: they are not
//! physics, no view shows them, and a world reset (a new `world.db`) resets
//! them with the world.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::ids::EntityId;
use crate::store::StoreError;

/// A `u64` as an SQLite integer, saturating at `i64::MAX`.
pub(crate) fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// An SQLite integer as a `u64`; a negative one reads as 0.
pub(crate) fn uint(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// An SQLite integer as a `u32`, saturating.
fn uint32(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}

/// A full-range `u64` (a hash) stored bit for bit.
pub(crate) const fn bits(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

/// The `u64` of [`bits`].
pub(crate) const fn unbits(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
// mirror-begin macro_row
/// The macro-tick counters of one entity, as `sup_entity` keeps them.
pub struct MacroRow {
    /// Macro ticks the scheduler queued (monotone).
    pub queued: u32,
    /// Queued ticks a consumed wake covered (monotone, at most `queued`).
    pub cleared: u32,
    /// Macro wakes consumed: what a script's `nth` sees (monotone).
    pub ticks: u32,
}
// mirror-end macro_row

// mirror-begin macro_merges
/// `MacroQueued`: the scheduler has queued `queued` ticks in all.
#[must_use]
pub const fn macro_queue(row: MacroRow, queued: u32) -> MacroRow {
    MacroRow {
        queued: if queued > row.queued {
            queued
        } else {
            row.queued
        },
        cleared: row.cleared,
        ticks: row.ticks,
    }
}

/// `Consume`: a wake that carried tick number `macro_no` (0: none) and
/// covered the queued ticks up to `cover` acted, or closed.
#[must_use]
pub const fn macro_consume(row: MacroRow, macro_no: u32, cover: u32) -> MacroRow {
    MacroRow {
        queued: if cover > row.queued {
            cover
        } else {
            row.queued
        },
        cleared: if cover > row.cleared {
            cover
        } else {
            row.cleared
        },
        ticks: if macro_no > row.ticks {
            macro_no
        } else {
            row.ticks
        },
    }
}
// mirror-end macro_merges

// mirror-begin dated_max
/// The value of the later of two dated counters, or the larger of the two
/// on the same day. The day of the result is the later day.
#[must_use]
pub const fn dated_max(a_day: u64, a: u64, b_day: u64, b: u64) -> u64 {
    if a_day > b_day {
        a
    } else if b_day > a_day {
        b
    } else if a > b {
        a
    } else {
        b
    }
}
// mirror-end dated_max

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
// mirror-begin tally
/// One UTC day's spend through one channel.
pub struct Tally {
    /// Tokens.
    pub tokens: u64,
    /// Cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// Model calls.
    pub calls: u64,
}
// mirror-end tally

impl Tally {
    /// Joins two dated tallies field by field with [`dated_max`]: the
    /// later day wins whole; on the same day, each field's larger value.
    #[must_use]
    pub const fn join(a_day: u64, a: Self, b_day: u64, b: Self) -> Self {
        Self {
            tokens: dated_max(a_day, a.tokens, b_day, b.tokens),
            cost_usd_micros: dated_max(a_day, a.cost_usd_micros, b_day, b.cost_usd_micros),
            calls: dated_max(a_day, a.calls, b_day, b.calls),
        }
    }
}

/// A wake an agent booked for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookingRow {
    /// The booking id, unique in this `world.db`.
    pub id: u64,
    /// Who booked it.
    pub entity: EntityId,
    /// When it is due, by the game clock.
    pub due_ms: u64,
    /// The agent's note: untrusted text, data only, escaped on render.
    pub note: String,
    /// When it was booked.
    pub booked_ms: u64,
}

/// How often one scripted rule has fired for one entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuleCountRow {
    /// The digest of the rule's canonical text.
    pub digest: u64,
    /// Which copy, for identical rules.
    pub dup: u32,
    /// The count (monotone).
    pub count: u32,
}

/// A deterministic plan in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    /// The wake that made it; the key of its cursor.
    pub wake_id: String,
    /// The command lines, pack or script output.
    pub lines: Vec<String>,
    /// Lines `[0, cursor)` have run (monotone per wake id).
    pub cursor: u32,
    /// Whether the wake carried a macro tick, a booking or a login (an
    /// errand resumes after any outage).
    pub errand: bool,
    /// When the plan started.
    pub started_ms: u64,
}

/// One entity's `sup_entity` row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityRow {
    /// The next macro tick, by the game clock; `None`: not scheduled yet.
    pub next_macro_ms: Option<u64>,
    /// The macro-tick counters.
    pub macro_row: MacroRow,
    /// The last event seq the entity was woken for (monotone).
    pub seen_seq: u64,
    /// The UTC day of the spend tallies.
    pub budget_day: u64,
    /// Spend the supervisor metered itself that day.
    pub local: Tally,
    /// Spend littleguy's ledger reported that day.
    pub lg: Tally,
    /// The harvest agent's lifetime token count last charged.
    pub harvest_tokens_seen: Option<u64>,
    /// The UTC day of `booked_today`.
    pub booked_day: u64,
    /// Wakes booked that day.
    pub booked_today: u32,
    /// When the row last changed.
    pub saved_ms: u64,
}

/// Which `sup_meta` counter a [`SideOp::MetaMax`] raises. `epoch` and
/// `boot` are the store's own and no op touches them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaKey {
    /// The next booking id.
    NextBookingId,
}

impl MetaKey {
    /// The key in `sup_meta`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NextBookingId => "next_booking_id",
        }
    }
}

/// One change to the side rows. Every variant is idempotent; the merges
/// are monotone where the row's meaning is (see each variant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideOp {
    /// Sets the next macro tick (last writer wins).
    MacroSchedule {
        /// Whose.
        entity: EntityId,
        /// When.
        next_ms: u64,
    },
    /// Raises the queued macro ticks ([`macro_queue`]) and sets the next.
    MacroQueued {
        /// Whose.
        entity: EntityId,
        /// The ticks queued in all.
        queued: u32,
        /// The next macro tick.
        next_ms: u64,
    },
    /// A wake's obligations, consumed: [`macro_consume`], the bookings it
    /// carried deleted, and `seen_seq` raised.
    Consume {
        /// Whose wake.
        entity: EntityId,
        /// The macro tick number it carried (0: none).
        macro_no: u32,
        /// The queued ticks it covered.
        macro_cover: u32,
        /// The bookings it carried.
        booked: Vec<u64>,
        /// The last event seq it saw.
        seen_seq: u64,
    },
    /// Starts a plan. A plan of the same wake already stored keeps its
    /// cursor; another wake's plan is replaced.
    PlanSet {
        /// Whose.
        entity: EntityId,
        /// The wake.
        wake_id: String,
        /// The lines.
        lines: Vec<String>,
        /// Whether it is an errand.
        errand: bool,
        /// When it started.
        started_ms: u64,
    },
    /// Raises a plan's cursor (only the plan of `wake_id`).
    PlanCursor {
        /// Whose.
        entity: EntityId,
        /// The wake.
        wake_id: String,
        /// Lines `[0, cursor)` have run.
        cursor: u32,
    },
    /// Forgets a plan (only the plan of `wake_id`).
    PlanDone {
        /// Whose.
        entity: EntityId,
        /// The wake.
        wake_id: String,
    },
    /// Raises rule counts.
    RuleCounts {
        /// Whose.
        entity: EntityId,
        /// The counts.
        rows: Vec<RuleCountRow>,
    },
    /// Forgets the counts of rules that are gone (edited or removed from
    /// the pack): their `(digest, dup)` keys.
    RuleCountsDrop {
        /// Whose.
        entity: EntityId,
        /// The keys.
        keys: Vec<(u64, u32)>,
    },
    /// Saves a mind's script memory (last writer wins). It rides on the
    /// wake's turn, so a decision's memory commits with the decision's
    /// first effect, or at the close, and never without it. It is not a
    /// side row: [`SideState`] does not hold it.
    ScriptMemory {
        /// Whose.
        entity: EntityId,
        /// The memory schema of the mind script.
        schema: u32,
        /// The memory.
        body: serde_json::Value,
    },
    /// Joins a day's spend ([`Tally::join`]); `harvest_tokens_seen` is
    /// raised.
    Spend {
        /// Whose.
        entity: EntityId,
        /// The UTC day.
        day: u64,
        /// Metered spend.
        local: Tally,
        /// littleguy ledger spend.
        lg: Tally,
        /// The harvest agent's lifetime tokens charged so far.
        harvest_tokens_seen: Option<u64>,
    },
    /// Stores a booking (a second insert of the same id is ignored).
    BookingInsert {
        /// The booking.
        row: BookingRow,
    },
    /// Deletes bookings.
    BookingDelete {
        /// Their ids.
        ids: Vec<u64>,
    },
    /// Raises a meta counter.
    MetaMax {
        /// Which.
        key: MetaKey,
        /// To at least this.
        value: u64,
    },
    /// Joins a day's booking count ([`dated_max`]).
    Booked {
        /// Whose.
        entity: EntityId,
        /// The UTC day.
        day: u64,
        /// Wakes booked that day.
        count: u32,
    },
    /// Raises `seen_seq`.
    SeenSeq {
        /// Whose.
        entity: EntityId,
        /// The seq.
        seq: u64,
    },
    /// Forgets every row of an entity the pack no longer has.
    DropEntity {
        /// Whose.
        entity: EntityId,
    },
}

/// Every side row, as loaded at open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideState {
    /// This `world.db`'s epoch, minted once.
    pub epoch: u64,
    /// This process's boot number: one more than the last open's.
    pub boot: u64,
    /// The next booking id, as last raised.
    pub next_booking_id: u64,
    /// `sup_entity`, by entity.
    pub entities: BTreeMap<EntityId, EntityRow>,
    /// `sup_booking`, by id.
    pub bookings: BTreeMap<u64, BookingRow>,
    /// `sup_plan`, by entity.
    pub plans: BTreeMap<EntityId, PlanRow>,
    /// `sup_rule_count`, by entity, ordered by digest and dup.
    pub rule_counts: BTreeMap<EntityId, Vec<RuleCountRow>>,
}

impl SideState {
    /// Every entity with a row anywhere.
    #[must_use]
    pub fn entity_ids(&self) -> Vec<EntityId> {
        let mut ids: Vec<EntityId> = self
            .entities
            .keys()
            .chain(self.plans.keys())
            .chain(self.rule_counts.keys())
            .chain(self.bookings.values().map(|b| &b.entity))
            .cloned()
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Drops the rows of every entity `keep` refuses, and returns the
    /// [`SideOp::DropEntity`] ops that drop them from the store.
    pub fn retain(&mut self, keep: impl Fn(&EntityId) -> bool) -> Vec<SideOp> {
        let ops: Vec<SideOp> = self
            .entity_ids()
            .into_iter()
            .filter(|e| !keep(e))
            .map(|entity| SideOp::DropEntity { entity })
            .collect();
        for op in &ops {
            self.apply(op, 0);
        }
        ops
    }

    fn entity_mut(&mut self, entity: &EntityId, now_ms: u64) -> &mut EntityRow {
        let row = self.entities.entry(entity.clone()).or_default();
        row.saved_ms = now_ms;
        row
    }

    /// Applies one op with the merges the store's SQL applies.
    #[expect(clippy::too_many_lines, reason = "one arm per side op reads best")]
    pub fn apply(&mut self, op: &SideOp, now_ms: u64) {
        match op {
            SideOp::MacroSchedule { entity, next_ms } => {
                self.entity_mut(entity, now_ms).next_macro_ms = Some(*next_ms);
            }
            SideOp::MacroQueued {
                entity,
                queued,
                next_ms,
            } => {
                let row = self.entity_mut(entity, now_ms);
                row.macro_row = macro_queue(row.macro_row, *queued);
                row.next_macro_ms = Some(*next_ms);
            }
            SideOp::Consume {
                entity,
                macro_no,
                macro_cover,
                booked,
                seen_seq,
            } => {
                let row = self.entity_mut(entity, now_ms);
                row.macro_row = macro_consume(row.macro_row, *macro_no, *macro_cover);
                row.seen_seq = row.seen_seq.max(*seen_seq);
                self.bookings
                    .retain(|id, b| !(booked.contains(id) && &b.entity == entity));
            }
            SideOp::PlanSet {
                entity,
                wake_id,
                lines,
                errand,
                started_ms,
            } => {
                if self.plans.get(entity).is_none_or(|p| &p.wake_id != wake_id) {
                    self.plans.insert(
                        entity.clone(),
                        PlanRow {
                            wake_id: wake_id.clone(),
                            lines: lines.clone(),
                            cursor: 0,
                            errand: *errand,
                            started_ms: *started_ms,
                        },
                    );
                }
            }
            SideOp::PlanCursor {
                entity,
                wake_id,
                cursor,
            } => {
                if let Some(p) = self.plans.get_mut(entity)
                    && &p.wake_id == wake_id
                {
                    p.cursor = p.cursor.max(*cursor);
                }
            }
            SideOp::PlanDone { entity, wake_id } => {
                if self
                    .plans
                    .get(entity)
                    .is_some_and(|p| &p.wake_id == wake_id)
                {
                    self.plans.remove(entity);
                }
            }
            SideOp::RuleCounts { entity, rows } => {
                let list = self.rule_counts.entry(entity.clone()).or_default();
                for r in rows {
                    match list
                        .iter_mut()
                        .find(|x| x.digest == r.digest && x.dup == r.dup)
                    {
                        Some(x) => x.count = x.count.max(r.count),
                        None => list.push(*r),
                    }
                }
                list.sort();
            }
            SideOp::RuleCountsDrop { entity, keys } => {
                if let Some(list) = self.rule_counts.get_mut(entity) {
                    list.retain(|r| !keys.contains(&(r.digest, r.dup)));
                    if list.is_empty() {
                        self.rule_counts.remove(entity);
                    }
                }
            }
            SideOp::ScriptMemory { .. } => {}
            SideOp::Spend {
                entity,
                day,
                local,
                lg,
                harvest_tokens_seen,
            } => {
                let row = self.entity_mut(entity, now_ms);
                row.local = Tally::join(row.budget_day, row.local, *day, *local);
                row.lg = Tally::join(row.budget_day, row.lg, *day, *lg);
                row.budget_day = row.budget_day.max(*day);
                row.harvest_tokens_seen = match (row.harvest_tokens_seen, *harvest_tokens_seen) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
            SideOp::BookingInsert { row } => {
                self.bookings.entry(row.id).or_insert_with(|| row.clone());
            }
            SideOp::BookingDelete { ids } => {
                for id in ids {
                    self.bookings.remove(id);
                }
            }
            SideOp::MetaMax { key, value } => match key {
                MetaKey::NextBookingId => {
                    self.next_booking_id = self.next_booking_id.max(*value);
                }
            },
            SideOp::Booked { entity, day, count } => {
                let row = self.entity_mut(entity, now_ms);
                let today = dated_max(
                    row.booked_day,
                    u64::from(row.booked_today),
                    *day,
                    u64::from(*count),
                );
                row.booked_today = u32::try_from(today).unwrap_or(u32::MAX);
                row.booked_day = row.booked_day.max(*day);
            }
            SideOp::SeenSeq { entity, seq } => {
                let row = self.entity_mut(entity, now_ms);
                row.seen_seq = row.seen_seq.max(*seq);
            }
            SideOp::DropEntity { entity } => {
                self.entities.remove(entity);
                self.plans.remove(entity);
                self.rule_counts.remove(entity);
                self.bookings.retain(|_, b| &b.entity != entity);
            }
        }
    }
}

/// Makes sure `entity` has a `sup_entity` row, stamped `now_ms`.
fn touch(tx: &Transaction<'_>, entity: &EntityId, now_ms: u64) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "INSERT INTO sup_entity (entity, saved_ms) VALUES (?1, ?2)
         ON CONFLICT (entity) DO UPDATE SET saved_ms = excluded.saved_ms",
    )?
    .execute(params![entity.as_str(), int(now_ms)])?;
    Ok(())
}

/// Applies one op inside `tx`, with the merges of [`SideState::apply`].
///
/// # Errors
/// SQLite or JSON encoding fails.
#[expect(clippy::too_many_lines, reason = "one arm per side op reads best")]
pub(crate) fn apply_sql(tx: &Transaction<'_>, op: &SideOp, now_ms: u64) -> Result<(), StoreError> {
    match op {
        SideOp::MacroSchedule { entity, next_ms } => {
            touch(tx, entity, now_ms)?;
            tx.prepare_cached("UPDATE sup_entity SET next_macro_ms = ?2 WHERE entity = ?1")?
                .execute(params![entity.as_str(), int(*next_ms)])?;
        }
        SideOp::MacroQueued {
            entity,
            queued,
            next_ms,
        } => {
            touch(tx, entity, now_ms)?;
            tx.prepare_cached(
                "UPDATE sup_entity SET macro_queued = max(macro_queued, ?2), next_macro_ms = ?3
                 WHERE entity = ?1",
            )?
            .execute(params![entity.as_str(), i64::from(*queued), int(*next_ms)])?;
        }
        SideOp::Consume {
            entity,
            macro_no,
            macro_cover,
            booked,
            seen_seq,
        } => {
            touch(tx, entity, now_ms)?;
            tx.prepare_cached(
                "UPDATE sup_entity SET
                    macro_queued = max(macro_queued, ?3),
                    macro_cleared = max(macro_cleared, ?3),
                    macro_ticks = max(macro_ticks, ?2),
                    seen_seq = max(seen_seq, ?4)
                 WHERE entity = ?1",
            )?
            .execute(params![
                entity.as_str(),
                i64::from(*macro_no),
                i64::from(*macro_cover),
                int(*seen_seq)
            ])?;
            let mut del =
                tx.prepare_cached("DELETE FROM sup_booking WHERE id = ?1 AND entity = ?2")?;
            for id in booked {
                del.execute(params![int(*id), entity.as_str()])?;
            }
        }
        SideOp::PlanSet {
            entity,
            wake_id,
            lines,
            errand,
            started_ms,
        } => {
            tx.prepare_cached(
                "INSERT INTO sup_plan (entity, wake_id, lines, cursor, errand, started_ms)
                 VALUES (?1, ?2, ?3, 0, ?4, ?5)
                 ON CONFLICT (entity) DO UPDATE SET
                    wake_id = excluded.wake_id, lines = excluded.lines, cursor = 0,
                    errand = excluded.errand, started_ms = excluded.started_ms
                 WHERE sup_plan.wake_id <> excluded.wake_id",
            )?
            .execute(params![
                entity.as_str(),
                wake_id,
                serde_json::to_string(lines)?,
                i64::from(*errand),
                int(*started_ms)
            ])?;
        }
        SideOp::PlanCursor {
            entity,
            wake_id,
            cursor,
        } => {
            tx.prepare_cached(
                "UPDATE sup_plan SET cursor = max(cursor, ?3) WHERE entity = ?1 AND wake_id = ?2",
            )?
            .execute(params![entity.as_str(), wake_id, i64::from(*cursor)])?;
        }
        SideOp::PlanDone { entity, wake_id } => {
            tx.prepare_cached("DELETE FROM sup_plan WHERE entity = ?1 AND wake_id = ?2")?
                .execute(params![entity.as_str(), wake_id])?;
        }
        SideOp::RuleCounts { entity, rows } => {
            let mut up = tx.prepare_cached(
                "INSERT INTO sup_rule_count (entity, digest, dup, count) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (entity, digest, dup) DO UPDATE SET count = max(count, excluded.count)",
            )?;
            for r in rows {
                up.execute(params![
                    entity.as_str(),
                    bits(r.digest),
                    i64::from(r.dup),
                    i64::from(r.count)
                ])?;
            }
        }
        SideOp::RuleCountsDrop { entity, keys } => {
            let mut del = tx.prepare_cached(
                "DELETE FROM sup_rule_count WHERE entity = ?1 AND digest = ?2 AND dup = ?3",
            )?;
            for (digest, dup) in keys {
                del.execute(params![entity.as_str(), bits(*digest), i64::from(*dup)])?;
            }
        }
        SideOp::ScriptMemory {
            entity,
            schema,
            body,
        } => {
            tx.prepare_cached(
                "INSERT INTO script_memory (entity, schema, body, saved_ms) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (entity) DO UPDATE SET schema = ?2, body = ?3, saved_ms = ?4",
            )?
            .execute(params![
                entity.as_str(),
                i64::from(*schema),
                serde_json::to_string(body)?,
                int(now_ms)
            ])?;
        }
        SideOp::Spend {
            entity,
            day,
            local,
            lg,
            harvest_tokens_seen,
        } => {
            touch(tx, entity, now_ms)?;
            tx.prepare_cached(
                "UPDATE sup_entity SET
                    local_tokens = CASE WHEN budget_day > ?2 THEN local_tokens WHEN ?2 > budget_day THEN ?3 ELSE max(local_tokens, ?3) END,
                    local_cost   = CASE WHEN budget_day > ?2 THEN local_cost   WHEN ?2 > budget_day THEN ?4 ELSE max(local_cost, ?4) END,
                    local_calls  = CASE WHEN budget_day > ?2 THEN local_calls  WHEN ?2 > budget_day THEN ?5 ELSE max(local_calls, ?5) END,
                    lg_tokens    = CASE WHEN budget_day > ?2 THEN lg_tokens    WHEN ?2 > budget_day THEN ?6 ELSE max(lg_tokens, ?6) END,
                    lg_cost      = CASE WHEN budget_day > ?2 THEN lg_cost      WHEN ?2 > budget_day THEN ?7 ELSE max(lg_cost, ?7) END,
                    lg_calls     = CASE WHEN budget_day > ?2 THEN lg_calls     WHEN ?2 > budget_day THEN ?8 ELSE max(lg_calls, ?8) END,
                    budget_day = max(budget_day, ?2),
                    harvest_tokens_seen = CASE
                        WHEN ?9 IS NULL THEN harvest_tokens_seen
                        WHEN harvest_tokens_seen IS NULL THEN ?9
                        ELSE max(harvest_tokens_seen, ?9) END
                 WHERE entity = ?1",
            )?
            .execute(params![
                entity.as_str(),
                int(*day),
                int(local.tokens),
                int(local.cost_usd_micros),
                int(local.calls),
                int(lg.tokens),
                int(lg.cost_usd_micros),
                int(lg.calls),
                harvest_tokens_seen.map(int)
            ])?;
        }
        SideOp::BookingInsert { row } => {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO sup_booking (id, entity, due_ms, note, booked_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                int(row.id),
                row.entity.as_str(),
                int(row.due_ms),
                row.note,
                int(row.booked_ms)
            ])?;
        }
        SideOp::BookingDelete { ids } => {
            let mut del = tx.prepare_cached("DELETE FROM sup_booking WHERE id = ?1")?;
            for id in ids {
                del.execute(params![int(*id)])?;
            }
        }
        SideOp::MetaMax { key, value } => {
            tx.prepare_cached(
                "INSERT INTO sup_meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = max(value, excluded.value)",
            )?
            .execute(params![key.name(), int(*value)])?;
        }
        SideOp::Booked { entity, day, count } => {
            touch(tx, entity, now_ms)?;
            tx.prepare_cached(
                "UPDATE sup_entity SET
                    booked_today = CASE WHEN booked_day > ?2 THEN booked_today WHEN ?2 > booked_day THEN ?3 ELSE max(booked_today, ?3) END,
                    booked_day = max(booked_day, ?2)
                 WHERE entity = ?1",
            )?
            .execute(params![entity.as_str(), int(*day), i64::from(*count)])?;
        }
        SideOp::SeenSeq { entity, seq } => {
            touch(tx, entity, now_ms)?;
            tx.prepare_cached(
                "UPDATE sup_entity SET seen_seq = max(seen_seq, ?2) WHERE entity = ?1",
            )?
            .execute(params![entity.as_str(), int(*seq)])?;
        }
        SideOp::DropEntity { entity } => {
            for table in ["sup_entity", "sup_booking", "sup_plan", "sup_rule_count"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE entity = ?1"),
                    params![entity.as_str()],
                )?;
            }
        }
    }
    Ok(())
}

fn meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT value FROM sup_meta WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .optional()
}

/// Loads every side row.
///
/// # Errors
/// SQLite fails or a plan's lines do not decode.
pub(crate) fn load_sql(conn: &Connection) -> Result<SideState, StoreError> {
    let mut side = SideState {
        epoch: meta(conn, "epoch")?.map_or(0, unbits),
        boot: meta(conn, "boot")?.map_or(0, uint),
        next_booking_id: meta(conn, MetaKey::NextBookingId.name())?.map_or(0, uint),
        ..SideState::default()
    };
    load_entities(conn, &mut side)?;
    load_bookings(conn, &mut side)?;
    load_plans(conn, &mut side)?;
    load_rule_counts(conn, &mut side)?;
    Ok(side)
}

fn load_entities(conn: &Connection, side: &mut SideState) -> Result<(), StoreError> {
    let mut q = conn.prepare(
        "SELECT entity, next_macro_ms, macro_queued, macro_cleared, macro_ticks, seen_seq,
                budget_day, local_tokens, local_cost, local_calls, lg_tokens, lg_cost, lg_calls,
                harvest_tokens_seen, booked_day, booked_today, saved_ms
         FROM sup_entity ORDER BY entity",
    )?;
    let rows = q.query_map([], |r| {
        Ok((
            EntityId::new(r.get::<_, String>(0)?),
            EntityRow {
                next_macro_ms: r.get::<_, Option<i64>>(1)?.map(uint),
                macro_row: MacroRow {
                    queued: uint32(r.get(2)?),
                    cleared: uint32(r.get(3)?),
                    ticks: uint32(r.get(4)?),
                },
                seen_seq: uint(r.get(5)?),
                budget_day: uint(r.get(6)?),
                local: Tally {
                    tokens: uint(r.get(7)?),
                    cost_usd_micros: uint(r.get(8)?),
                    calls: uint(r.get(9)?),
                },
                lg: Tally {
                    tokens: uint(r.get(10)?),
                    cost_usd_micros: uint(r.get(11)?),
                    calls: uint(r.get(12)?),
                },
                harvest_tokens_seen: r.get::<_, Option<i64>>(13)?.map(uint),
                booked_day: uint(r.get(14)?),
                booked_today: uint32(r.get(15)?),
                saved_ms: uint(r.get(16)?),
            },
        ))
    })?;
    for row in rows {
        let (id, row) = row?;
        side.entities.insert(id, row);
    }
    Ok(())
}

fn load_bookings(conn: &Connection, side: &mut SideState) -> Result<(), StoreError> {
    let mut q =
        conn.prepare("SELECT id, entity, due_ms, note, booked_ms FROM sup_booking ORDER BY id")?;
    let rows = q.query_map([], |r| {
        Ok(BookingRow {
            id: uint(r.get(0)?),
            entity: EntityId::new(r.get::<_, String>(1)?),
            due_ms: uint(r.get(2)?),
            note: r.get(3)?,
            booked_ms: uint(r.get(4)?),
        })
    })?;
    for row in rows {
        let row = row?;
        side.bookings.insert(row.id, row);
    }
    Ok(())
}

fn load_plans(conn: &Connection, side: &mut SideState) -> Result<(), StoreError> {
    let mut q = conn.prepare(
        "SELECT entity, wake_id, lines, cursor, errand, started_ms FROM sup_plan ORDER BY entity",
    )?;
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    for row in rows {
        let (entity, wake_id, lines, cursor, errand, started_ms) = row?;
        side.plans.insert(
            EntityId::new(entity),
            PlanRow {
                wake_id,
                lines: serde_json::from_str(&lines)?,
                cursor: uint32(cursor),
                errand: errand != 0,
                started_ms: uint(started_ms),
            },
        );
    }
    Ok(())
}

/// The plan of one entity, if it has one.
///
/// # Errors
/// SQLite fails or the plan's lines do not decode.
pub(crate) fn load_plan(
    conn: &Connection,
    entity: &EntityId,
) -> Result<Option<PlanRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT wake_id, lines, cursor, errand, started_ms FROM sup_plan WHERE entity = ?1",
            [entity.as_str()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((wake_id, lines, cursor, errand, started_ms)) = row else {
        return Ok(None);
    };
    Ok(Some(PlanRow {
        wake_id,
        lines: serde_json::from_str(&lines)?,
        cursor: uint32(cursor),
        errand: errand != 0,
        started_ms: uint(started_ms),
    }))
}

fn load_rule_counts(conn: &Connection, side: &mut SideState) -> Result<(), StoreError> {
    let mut q = conn.prepare(
        "SELECT entity, digest, dup, count FROM sup_rule_count ORDER BY entity, digest, dup",
    )?;
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            RuleCountRow {
                digest: unbits(r.get(1)?),
                dup: uint32(r.get(2)?),
                count: uint32(r.get(3)?),
            },
        ))
    })?;
    for row in rows {
        let (entity, row) = row?;
        side.rule_counts
            .entry(EntityId::new(entity))
            .or_default()
            .push(row);
    }
    // Digests are stored bit for bit; order them as numbers.
    for list in side.rule_counts.values_mut() {
        list.sort();
    }
    Ok(())
}
