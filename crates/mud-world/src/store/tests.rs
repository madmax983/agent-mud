use std::sync::Arc;

use proptest::prelude::*;
use rusqlite::Connection;

use super::*;
use crate::World;
use crate::pack::WorldPack;
use crate::side::{BookingRow, MacroRow, MetaKey, PlanRow, RuleCountRow, Tally};

/// The DDL of the store before versions (the base code's `SCHEMA`),
/// verbatim: a file it made has `user_version = 0`.
const V0_DDL: &str = "
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

fn pack() -> Arc<WorldPack> {
    Arc::new(WorldPack::crypt_v01().unwrap())
}

/// A world with ada, who said `n` things at times 10, 20, ...
fn chatty(n: u64) -> (World, Vec<Event>) {
    let mut w = World::new(pack(), 3);
    let ada = w.login("ada", 5).unwrap();
    for i in 1..=n {
        assert!(w.execute(&ada, &format!("say line {i}"), i * 10).ok);
    }
    let fresh = w.drain_fresh();
    (w, fresh)
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut q = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    q.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn v0_file_migrates_to_user_version_1_keeping_snapshot_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let (w, fresh) = chatty(3);
    {
        // A file the base code made: its DDL, its rows, no version.
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(V0_DDL).unwrap();
        conn.execute(
            "INSERT INTO snapshot (id, pack, state, saved_ms) VALUES (1, 'crypt-v01', ?1, 9)",
            [serde_json::to_string(&w.snapshot()).unwrap()],
        )
        .unwrap();
        for e in &fresh {
            conn.execute(
                "INSERT INTO events (seq, at_ms, room, kind, body) VALUES (?1, ?2, NULL, ?3, ?4)",
                params![
                    int(e.seq.0),
                    int(e.at_ms),
                    e.kind.name(),
                    serde_json::to_string(e).unwrap()
                ],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO mints (seq, pool, period, entity, body) VALUES (2, 'p', 0, 'pc_ada', 'x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO script_memory (entity, schema, body, saved_ms) VALUES ('guard_01', 1, '{\"day\":3}', 1)",
            [],
        )
        .unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0);
    }
    let s = Store::open(&db, 1, 1_000).unwrap();
    assert_eq!(s.user_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 2);
    assert_eq!(s.boot(), 1, "the first open of a v0 file is boot 1");
    let loaded = s.load(100).unwrap().unwrap();
    assert_eq!(loaded.pack, "crypt-v01");
    assert_eq!(loaded.state, w.snapshot());
    assert_eq!(loaded.recent, fresh, "every event kept, in order");
    assert_eq!(s.event_count().unwrap(), 4);
    let memory = s.load_script_memory().unwrap();
    assert_eq!(memory[&EntityId::from("guard_01")].1["day"], 3);
    let conn = Connection::open(&db).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM mints", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "the ledger is kept");
    for t in [
        "sup_booking",
        "sup_entity",
        "sup_meta",
        "sup_plan",
        "sup_rule_count",
        "sup_steward",
    ] {
        assert!(tables(&conn).contains(&t.to_owned()), "{t} is made");
    }
    assert_eq!(loaded.side.epoch, crate::hash::epoch(1, 1_000));
    drop(s);
    // Opening again runs no step and keeps everything.
    let again = Store::open(&db, 1, 2_000).unwrap();
    assert_eq!(again.user_version().unwrap(), 2);
    assert_eq!(again.event_count().unwrap(), 4);
    assert_eq!(again.boot(), 2);
}

#[test]
fn v1_file_migrates_to_v2_keeping_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    {
        // A version 1 file: the V1 step alone, with a side row.
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO sup_booking (id, entity, due_ms, note, booked_ms) VALUES (7, 'pc_ada', 50, 'n', 1)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO sup_meta (key, value) VALUES ('boot', 4)", [])
            .unwrap();
        assert!(!tables(&conn).contains(&"sup_steward".to_owned()));
    }
    let mut s = Store::open(&db, 1, 1_000).unwrap();
    assert_eq!(s.user_version().unwrap(), 2);
    assert_eq!(s.boot(), 5, "the boot count goes on");
    let side = s.load_side().unwrap();
    assert_eq!(side.bookings[&7].note, "n", "version 1 rows are kept");
    assert!(side.steward.is_empty());
    s.apply_side(
        &[SideOp::Steward {
            entity: ada(),
            rev: 1,
            schema: 1,
            body: serde_json::json!({ "goals": {} }),
        }],
        5,
        Fsync::Full,
    )
    .unwrap();
    drop(s);
    let again = Store::open(&db, 1, 2_000).unwrap();
    let side = again.load_side().unwrap();
    assert_eq!(side.steward[&ada()].rev, 1);
    assert_eq!(side.steward[&ada()].body["goals"], serde_json::json!({}));
}

#[test]
fn a_version_3_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    drop(Store::open(&db, 1, 1).unwrap());
    {
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
    }
    let err = Store::open(&db, 1, 1).unwrap_err();
    assert!(matches!(err, StoreError::Schema { found: 3 }), "{err:?}");
    assert!(err.to_string().contains("schema version 3"), "{err}");
    // Nothing was written: the boot stayed where it was.
    let conn = Connection::open(&db).unwrap();
    let boot: i64 = conn
        .query_row("SELECT value FROM sup_meta WHERE key = 'boot'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(boot, 1);
    // A version no code wrote is refused too.
    conn.pragma_update(None, "user_version", -1).unwrap();
    drop(conn);
    assert!(matches!(
        Store::open(&db, 1, 1),
        Err(StoreError::Schema { found: u32::MAX })
    ));
}

#[test]
fn drop_entity_drops_the_steward_row() {
    let mut s = Store::open_in_memory(1, 0).unwrap();
    let bob = EntityId::from("pc_bob");
    let row = |entity: &EntityId, rev: u64| SideOp::Steward {
        entity: entity.clone(),
        rev,
        schema: 1,
        body: serde_json::json!({ "rev": rev }),
    };
    s.apply_side(&[row(&ada(), 2), row(&bob, 1)], 1, Fsync::Normal)
        .unwrap();
    // A lower rev, late, never moves the row back.
    s.apply_side(&[row(&ada(), 1)], 2, Fsync::Normal).unwrap();
    let side = s.load_side().unwrap();
    assert_eq!(side.steward[&ada()].body["rev"], 2);
    assert_eq!(side.entity_ids(), [ada(), bob.clone()]);
    s.apply_side(&[SideOp::DropEntity { entity: ada() }], 3, Fsync::Normal)
        .unwrap();
    let side = s.load_side().unwrap();
    assert!(!side.steward.contains_key(&ada()));
    assert!(side.steward.contains_key(&bob), "only the dropped entity's");
    let mut model = SideState::default();
    model.apply(&row(&ada(), 2), 0);
    model.apply(&SideOp::DropEntity { entity: ada() }, 0);
    assert!(model.steward.is_empty());
}

#[test]
fn a_steward_body_that_is_not_json_loads_as_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    drop(Store::open(&db, 1, 1).unwrap());
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO sup_steward (entity, rev, schema, body, saved_ms) VALUES ('pc_ada', 3, 1, 'not json', 1)",
            [],
        )
        .unwrap();
    }
    let s = Store::open(&db, 1, 2).unwrap();
    let side = s.load_side().unwrap();
    assert_eq!(side.steward[&ada()].rev, 3);
    assert!(side.steward[&ada()].body.is_null());
}

#[test]
fn each_open_bumps_boot_and_keeps_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let first = Store::open(&db, 7, 100).unwrap();
    let epoch = first.epoch();
    assert_eq!(epoch, crate::hash::epoch(7, 100));
    assert_eq!(first.boot(), 1);
    drop(first);
    for boot in 2..=5 {
        // Another seed and another time do not change the epoch.
        let s = Store::open(&db, 99, boot * 1_000).unwrap();
        assert_eq!(s.epoch(), epoch);
        assert_eq!(s.boot(), boot);
        let side = s.load_side().unwrap();
        assert_eq!((side.epoch, side.boot), (epoch, boot));
    }
}

#[test]
fn a_new_world_db_mints_a_new_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let a = Store::open(&dir.path().join("a.db"), 7, 100).unwrap();
    let b = Store::open(&dir.path().join("b.db"), 7, 101).unwrap();
    assert_ne!(a.epoch(), b.epoch(), "a reset at another moment");
    assert_eq!((a.boot(), b.boot()), (1, 1));
    let m = Store::open_in_memory(7, 100).unwrap();
    assert_eq!(m.epoch(), a.epoch(), "the same seed and moment");
}

fn ada() -> EntityId {
    EntityId::from("pc_ada")
}

fn booking(id: u64, entity: &EntityId) -> BookingRow {
    BookingRow {
        id,
        entity: entity.clone(),
        due_ms: 5_000 + id,
        note: format!("check the board <{id}>"),
        booked_ms: 1_000,
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one walk through every side op reads best"
)]
fn every_side_op_round_trips_with_its_merge() {
    let mut s = Store::open_in_memory(1, 0).unwrap();
    let e = ada();
    let bob = EntityId::from("pc_bob");
    let lines = vec!["move e".to_owned(), "board post BOUNTY".to_owned()];
    let ops = vec![
        SideOp::MacroSchedule {
            entity: e.clone(),
            next_ms: 900,
        },
        SideOp::MacroQueued {
            entity: e.clone(),
            queued: 2,
            next_ms: 1_900,
        },
        SideOp::BookingInsert {
            row: booking(1, &e),
        },
        SideOp::BookingInsert {
            row: booking(2, &e),
        },
        SideOp::BookingInsert {
            row: booking(3, &bob),
        },
        SideOp::MetaMax {
            key: MetaKey::NextBookingId,
            value: 4,
        },
        SideOp::Consume {
            entity: e.clone(),
            macro_no: 1,
            macro_cover: 1,
            // Booking 3 is bob's: ada's consume cannot delete it.
            booked: vec![1, 3],
            seen_seq: 40,
        },
        SideOp::PlanSet {
            entity: e.clone(),
            wake_id: "w1".into(),
            lines: lines.clone(),
            errand: true,
            started_ms: 50,
        },
        SideOp::PlanCursor {
            entity: e.clone(),
            wake_id: "w1".into(),
            cursor: 1,
        },
        // A replayed PlanSet of the same wake keeps the cursor.
        SideOp::PlanSet {
            entity: e.clone(),
            wake_id: "w1".into(),
            lines: lines.clone(),
            errand: true,
            started_ms: 50,
        },
        // Another wake's cursor does not move it.
        SideOp::PlanCursor {
            entity: e.clone(),
            wake_id: "w0".into(),
            cursor: 2,
        },
        SideOp::RuleCounts {
            entity: e.clone(),
            rows: vec![
                RuleCountRow {
                    digest: u64::MAX - 3,
                    dup: 0,
                    count: 2,
                },
                RuleCountRow {
                    digest: 7,
                    dup: 1,
                    count: 1,
                },
            ],
        },
        SideOp::RuleCounts {
            entity: e.clone(),
            rows: vec![RuleCountRow {
                digest: u64::MAX - 3,
                dup: 0,
                count: 1,
            }],
        },
        SideOp::Spend {
            entity: e.clone(),
            day: 3,
            local: Tally {
                tokens: 10,
                cost_usd_micros: 5,
                calls: 1,
            },
            lg: Tally::default(),
            harvest_tokens_seen: None,
        },
        SideOp::Spend {
            entity: e.clone(),
            day: 3,
            local: Tally {
                tokens: 8,
                cost_usd_micros: 9,
                calls: 1,
            },
            lg: Tally {
                tokens: 4,
                cost_usd_micros: 2,
                calls: 1,
            },
            harvest_tokens_seen: Some(70),
        },
        // An older day's spend is ignored.
        SideOp::Spend {
            entity: e.clone(),
            day: 2,
            local: Tally {
                tokens: 999,
                cost_usd_micros: 999,
                calls: 9,
            },
            lg: Tally::default(),
            harvest_tokens_seen: Some(60),
        },
        SideOp::Booked {
            entity: e.clone(),
            day: 3,
            count: 2,
        },
        SideOp::Booked {
            entity: e.clone(),
            day: 3,
            count: 1,
        },
        SideOp::SeenSeq {
            entity: e.clone(),
            seq: 30,
        },
    ];
    s.apply_side(&ops, 77, Fsync::Full).unwrap();
    // Applied twice: the same rows.
    s.apply_side(&ops, 77, Fsync::Normal).unwrap();
    let side = s.load_side().unwrap();
    let mut model = SideState {
        epoch: side.epoch,
        boot: side.boot,
        ..SideState::default()
    };
    for op in ops.iter().chain(&ops) {
        model.apply(op, 77);
    }
    assert_eq!(side, model, "the SQL agrees with the kernels");
    let row = &side.entities[&e];
    assert_eq!(
        row.macro_row,
        MacroRow {
            queued: 2,
            cleared: 1,
            ticks: 1
        }
    );
    assert_eq!(row.next_macro_ms, Some(1_900));
    assert_eq!(row.seen_seq, 40);
    assert_eq!(row.budget_day, 3);
    assert_eq!(
        row.local,
        Tally {
            tokens: 10,
            cost_usd_micros: 9,
            calls: 1
        }
    );
    assert_eq!(row.lg.tokens, 4);
    assert_eq!(row.harvest_tokens_seen, Some(70));
    assert_eq!((row.booked_day, row.booked_today), (3, 2));
    assert_eq!(row.saved_ms, 77);
    assert_eq!(
        side.bookings.keys().copied().collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(side.bookings[&2].note, "check the board <2>", "data only");
    assert_eq!(side.next_booking_id, 4);
    assert_eq!(
        side.plans[&e],
        PlanRow {
            wake_id: "w1".into(),
            lines,
            cursor: 1,
            errand: true,
            started_ms: 50
        }
    );
    assert_eq!(
        side.rule_counts[&e],
        vec![
            RuleCountRow {
                digest: 7,
                dup: 1,
                count: 1
            },
            RuleCountRow {
                digest: u64::MAX - 3,
                dup: 0,
                count: 2
            },
        ]
    );
    // Another wake's plan replaces it; PlanDone of a stale wake is a no-op.
    s.apply_side(
        &[
            SideOp::PlanSet {
                entity: e.clone(),
                wake_id: "w2".into(),
                lines: vec!["look".into()],
                errand: false,
                started_ms: 60,
            },
            SideOp::PlanDone {
                entity: e.clone(),
                wake_id: "w1".into(),
            },
            SideOp::BookingDelete { ids: vec![3] },
        ],
        80,
        Fsync::Normal,
    )
    .unwrap();
    let side = s.load_side().unwrap();
    assert_eq!(side.plans[&e].wake_id, "w2");
    assert_eq!(side.plans[&e].cursor, 0);
    assert!(!side.bookings.contains_key(&3));
    s.apply_side(
        &[
            SideOp::PlanDone {
                entity: e.clone(),
                wake_id: "w2".into(),
            },
            SideOp::DropEntity { entity: e.clone() },
        ],
        90,
        Fsync::Normal,
    )
    .unwrap();
    let side = s.load_side().unwrap();
    assert!(side.plans.is_empty());
    assert!(side.entities.is_empty() && side.bookings.is_empty() && side.rule_counts.is_empty());
    assert_eq!(side.next_booking_id, 4, "meta is not an entity's");
}

#[test]
fn a_side_op_commits_with_the_events_or_not_at_all() {
    let mut s = Store::open_in_memory(1, 0).unwrap();
    let (w, fresh) = chatty(2);
    let op = SideOp::SeenSeq {
        entity: ada(),
        seq: 9,
    };
    // A bad op (a plan's JSON cannot fail, so: a failing table) rolls
    // the whole commit back.
    s.conn()
        .unwrap()
        .execute_batch(
            "CREATE TEMP TRIGGER no_seen BEFORE UPDATE OF seen_seq ON sup_entity
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let snapshot = w.snapshot();
    let c = Commit {
        snapshot: Some(("crypt-v01", &snapshot)),
        events: &fresh,
        ops: std::slice::from_ref(&op),
        now_ms: 5,
        ..Commit::default()
    };
    assert!(s.commit(&c).is_err());
    assert_eq!(s.event_count().unwrap(), 0, "no event without its op");
    assert!(s.load(10).unwrap().is_none(), "no snapshot either");
    s.conn()
        .unwrap()
        .execute_batch("DROP TRIGGER no_seen;")
        .unwrap();
    s.commit(&c).unwrap();
    assert_eq!(s.event_count().unwrap(), 3);
    assert_eq!(s.load_side().unwrap().entities[&ada()].seen_seq, 9);
}

#[test]
fn a_spend_reset_moves_the_day_back_and_later_spend_joins_it() {
    let mut s = Store::open_in_memory(1, 0).unwrap();
    let t = |tokens: u64| Tally {
        tokens,
        cost_usd_micros: tokens / 2,
        calls: 1,
    };
    let ops = [
        // A day stored ahead of the clock, with a harvest mark.
        SideOp::Spend {
            entity: ada(),
            day: 9,
            local: t(800),
            lg: t(200),
            harvest_tokens_seen: Some(40),
        },
        // A plain Spend of an earlier day cannot move it back.
        SideOp::Spend {
            entity: ada(),
            day: 4,
            local: t(5),
            lg: Tally::default(),
            harvest_tokens_seen: None,
        },
        SideOp::SpendReset {
            entity: ada(),
            day: 4,
            local: Tally::default(),
            lg: Tally::default(),
        },
        SideOp::Spend {
            entity: ada(),
            day: 4,
            local: t(30),
            lg: t(6),
            harvest_tokens_seen: None,
        },
    ];
    let mut model = SideState::default();
    for (i, op) in ops.iter().enumerate() {
        s.apply_side(std::slice::from_ref(op), i as u64, Fsync::Normal)
            .unwrap();
        model.apply(op, i as u64);
    }
    let side = s.load_side().unwrap();
    let row = &side.entities[&ada()];
    assert_eq!(row.budget_day, 4, "the day moved back");
    assert_eq!((row.local, row.lg), (t(30), t(6)), "and the new day joins");
    assert_eq!(row.harvest_tokens_seen, Some(40), "the mark is kept");
    model.epoch = side.epoch;
    model.boot = side.boot;
    assert_eq!(side, model, "SQL and the model agree");
    // A second reset is the same as one.
    s.apply_side(&ops[2..3], 9, Fsync::Normal).unwrap();
    s.apply_side(&ops[2..3], 10, Fsync::Normal).unwrap();
    let row = &s.load_side().unwrap().entities[&ada()];
    assert_eq!((row.budget_day, row.local), (4, Tally::default()));
}

fn arb_tally() -> impl Strategy<Value = Tally> {
    (0u64..50, 0u64..50, 0u64..5).prop_map(|(tokens, cost_usd_micros, calls)| Tally {
        tokens,
        cost_usd_micros,
        calls,
    })
}

/// The ops whose merges commute: every row converges whatever the order.
fn arb_op() -> impl Strategy<Value = SideOp> {
    let who = prop_oneof![Just(ada()), Just(EntityId::from("pc_bob"))];
    prop_oneof![
        (who.clone(), 0u32..6).prop_map(|(entity, queued)| SideOp::MacroQueued {
            entity,
            queued,
            next_ms: 0,
        }),
        (who.clone(), 0u32..6, 0u32..6, 0u64..50).prop_map(|(entity, n, c, seq)| {
            SideOp::Consume {
                entity,
                macro_no: n,
                macro_cover: c,
                booked: Vec::new(),
                seen_seq: seq,
            }
        }),
        (
            who.clone(),
            0u64..4,
            arb_tally(),
            arb_tally(),
            proptest::option::of(0u64..90)
        )
            .prop_map(|(entity, day, local, lg, h)| SideOp::Spend {
                entity,
                day,
                local,
                lg,
                harvest_tokens_seen: h,
            }),
        (who.clone(), 0u64..4, 0u32..9).prop_map(|(entity, day, count)| SideOp::Booked {
            entity,
            day,
            count
        }),
        (who.clone(), 0u64..90).prop_map(|(entity, seq)| SideOp::SeenSeq { entity, seq }),
        (who.clone(), 0u64..90, 0u32..3).prop_map(|(entity, digest, count)| SideOp::RuleCounts {
            entity,
            rows: vec![RuleCountRow {
                digest,
                dup: 0,
                count,
            }],
        }),
        (0u64..90).prop_map(|value| SideOp::MetaMax {
            key: MetaKey::NextBookingId,
            value,
        }),
        // One writer per entity: an equal rev is the same write, so the
        // body is a function of the entity and the rev.
        (who, 0u64..6).prop_map(|(entity, rev)| SideOp::Steward {
            body: serde_json::json!({ "who": entity.as_str(), "rev": rev }),
            entity,
            rev,
            schema: 1,
        }),
    ]
}

/// The rows without the stamps: the stamp is the last writer's time.
fn unstamped(mut s: SideState) -> SideState {
    for row in s.entities.values_mut() {
        row.saved_ms = 0;
    }
    s
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sql_merge_matches_the_kernel(
        ops in proptest::collection::vec(arb_op(), 0..24),
        seed in any::<u64>(),
    ) {
        let mut s = Store::open_in_memory(1, 0).unwrap();
        // One op per transaction, in order.
        for (i, op) in ops.iter().enumerate() {
            s.apply_side(std::slice::from_ref(op), i as u64, Fsync::Normal).unwrap();
        }
        let side = s.load_side().unwrap();
        let mut model = SideState { epoch: side.epoch, boot: side.boot, ..SideState::default() };
        for (i, op) in ops.iter().enumerate() {
            model.apply(op, i as u64);
        }
        prop_assert_eq!(&side, &model);
        // Shuffled, with every op applied twice: the same rows.
        let mut shuffled: Vec<SideOp> = ops.iter().chain(&ops).cloned().collect();
        let n = shuffled.len();
        for i in (1..n).rev() {
            let j = usize::try_from(crate::hash::mix(seed, &[&i.to_le_bytes()]) % (i as u64 + 1)).unwrap();
            shuffled.swap(i, j);
        }
        let mut t = Store::open_in_memory(1, 0).unwrap();
        t.apply_side(&shuffled, 0, Fsync::Normal).unwrap();
        prop_assert_eq!(unstamped(t.load_side().unwrap()), unstamped(side));
    }
}

/// A store whose events 1..=n are `days` days apart, one per day from
/// day 1, and the archive dir.
fn aged(n: u64) -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(&dir.path().join("world.db"), 1, 0).unwrap();
    let mut w = World::new(pack(), 3);
    let ada = w.login("ada", crate::rewards::DAY_MS).unwrap();
    for i in 2..=n {
        assert!(
            w.execute(&ada, &format!("say {i}"), i * crate::rewards::DAY_MS)
                .ok
        );
    }
    let fresh = w.drain_fresh();
    s.save("crypt-v01", &w.snapshot(), &fresh, &[], 0).unwrap();
    (s, dir)
}

#[test]
fn retention_deletes_old_events_in_batches_and_never_the_newest_keep_min() {
    let (mut s, dir) = aged(20);
    let arch = dir.path().join("archive");
    assert_eq!(s.event_count().unwrap(), 20);
    let now = 21 * crate::rewards::DAY_MS;
    // Keep 7 days: events of days 1..=13 are older.
    let cutoff = crate::durability::retention_cutoff(now, 7);
    let t = s.trim_events(cutoff, 3, 5, &arch).unwrap();
    assert_eq!(
        t,
        Trimmed {
            rows: 5,
            archived: 5,
            through: Some(5)
        }
    );
    let t = s.trim_events(cutoff, 3, 5, &arch).unwrap();
    assert_eq!(t.through, Some(10));
    let t = s.trim_events(cutoff, 3, 5, &arch).unwrap();
    assert_eq!(t.rows, 3, "the last batch is short");
    assert_eq!(
        s.trim_events(cutoff, 3, 5, &arch).unwrap(),
        Trimmed::default()
    );
    assert_eq!(s.event_range().unwrap(), Some((14, 20)));
    assert_eq!(archive::seqs(&arch).unwrap(), (1..=13).collect::<Vec<_>>());
    // keep_min protects the newest rows however old they are.
    let far = 1_000 * crate::rewards::DAY_MS;
    let cutoff = crate::durability::retention_cutoff(far, 1);
    while s.trim_events(cutoff, 4, 100, &arch).unwrap().rows > 0 {}
    assert_eq!(s.event_range().unwrap(), Some((17, 20)));
    assert_eq!(archive::seqs(&arch).unwrap(), (1..=16).collect::<Vec<_>>());
    // The archive lines are the stored events.
    let back = s.events_after(16, 10).unwrap();
    assert_eq!(back.len(), 4);
}

#[test]
fn retention_with_keep_days_zero_deletes_nothing() {
    let (mut s, dir) = aged(5);
    let cutoff = crate::durability::retention_cutoff(u64::MAX / 2, 0);
    assert_eq!(cutoff, 0);
    let t = s
        .trim_events(cutoff, 0, 100, &dir.path().join("a"))
        .unwrap();
    assert_eq!(t, Trimmed::default());
    assert_eq!(s.event_count().unwrap(), 5);
    assert!(!dir.path().join("a").exists(), "no archive was made");
}

#[test]
fn a_crash_between_archive_and_delete_archives_each_event_once() {
    let (mut s, dir) = aged(10);
    let arch = dir.path().join("archive");
    let cutoff = 9 * crate::rewards::DAY_MS;
    // The crash: the first 4 rows reached the archive, the delete did not
    // commit.
    let rows: Vec<ArchiveRow> = {
        let mut q = s
            .conn()
            .unwrap()
            .prepare("SELECT seq, at_ms, body FROM events ORDER BY seq LIMIT 4")
            .unwrap();
        q.query_map([], |r| {
            Ok(ArchiveRow {
                seq: uint(r.get(0)?),
                at_ms: uint(r.get(1)?),
                body: r.get(2)?,
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    archive::append(&arch, s.epoch(), &rows).unwrap();
    assert_eq!(s.event_count().unwrap(), 10);
    let t = s.trim_events(cutoff, 0, 100, &arch).unwrap();
    assert_eq!(t.rows, 8, "days 1..=8 are older than the cutoff");
    assert_eq!(t.archived, 4, "the first 4 were archived before the crash");
    assert_eq!(archive::seqs(&arch).unwrap(), (1..=8).collect::<Vec<_>>());
    assert_eq!(s.event_range().unwrap(), Some((9, 10)));
}

#[test]
fn an_archive_that_cannot_be_written_trims_nothing() {
    let (mut s, dir) = aged(6);
    // A file where the archive directory should be.
    let arch = dir.path().join("archive");
    std::fs::write(&arch, b"in the way").unwrap();
    let err = s
        .trim_events(10 * crate::rewards::DAY_MS, 0, 100, &arch)
        .unwrap_err();
    assert!(matches!(err, StoreError::Archive(_)), "{err:?}");
    assert_eq!(s.event_count().unwrap(), 6, "nothing deleted");
}

#[test]
fn a_store_that_refuses_writes_fails_until_it_is_let_through() {
    let mut s = Store::open_in_memory(1, 0).unwrap();
    s.refuse_writes(true).unwrap();
    let op = SideOp::SeenSeq {
        entity: ada(),
        seq: 1,
    };
    assert!(
        s.apply_side(std::slice::from_ref(&op), 1, Fsync::Full)
            .is_err()
    );
    s.refuse_writes(false).unwrap();
    s.apply_side(&[op], 1, Fsync::Full).unwrap();
    assert_eq!(s.load_side().unwrap().entities.len(), 1);
}

/// A store at `db` (made at the first stamp, which picks its epoch) whose
/// events are stamped at `days` (in days), in seq order: a login, then one
/// `say` each.
fn stamped(db: &Path, days: &[u64]) -> Store {
    let day = crate::rewards::DAY_MS;
    let mut s = Store::open(db, 1, days[0] * day).unwrap();
    let mut w = World::new(pack(), 3);
    let ada = w.login("ada", days[0] * day).unwrap();
    for (i, d) in days.iter().enumerate().skip(1) {
        assert!(w.execute(&ada, &format!("say {i}"), d * day).ok);
    }
    let fresh = w.drain_fresh();
    let got: Vec<(u64, u64)> = fresh.iter().map(|e| (e.seq.0, e.at_ms / day)).collect();
    let want: Vec<(u64, u64)> = (1..).zip(days.iter().copied()).collect();
    assert_eq!(got, want, "one event per stamp");
    s.save("crypt-v01", &w.snapshot(), &fresh, &[], 0).unwrap();
    s
}

/// Every seq still in the table.
fn stored(s: &Store) -> Vec<u64> {
    s.events_after(0, usize::MAX)
        .unwrap()
        .iter()
        .map(|e| e.seq.0)
        .collect()
}

fn trim_all(s: &mut Store, cutoff_day: u64, arch: &Path) {
    let cutoff = cutoff_day * crate::rewards::DAY_MS;
    while s.trim_events(cutoff, 0, 100, arch).unwrap().rows > 0 {}
}

#[test]
fn a_clock_that_steps_back_never_loses_an_event_to_retention() {
    // Review findings 1, 4 and 6: the clock steps back between seq 2 and
    // seq 3, so at_ms does not rise with seq.
    let dir = tempfile::tempdir().unwrap();
    let arch = dir.path().join("archive");
    let mut s = stamped(&dir.path().join("world.db"), &[10, 10, 5, 20]);
    // Seq 3 is old enough, but seq 1 and 2 are not: retention trims only a
    // seq prefix, so it waits.
    trim_all(&mut s, 8, &arch);
    // Later the cutoff passes days 10.
    trim_all(&mut s, 15, &arch);
    let mut all = archive::seqs(&arch).unwrap();
    all.extend(stored(&s));
    assert_eq!(
        all,
        vec![1, 2, 3, 4],
        "each event is archived or stored, once"
    );
    assert_eq!(stored(&s), vec![4]);
    // The other shape: a later seq is younger than an earlier one.
    let dir = tempfile::tempdir().unwrap();
    let arch = dir.path().join("archive");
    let mut s = stamped(&dir.path().join("world.db"), &[1, 5, 3, 20]);
    trim_all(&mut s, 4, &arch);
    assert_eq!(
        stored(&s),
        vec![2, 3, 4],
        "seq 2 is young: nothing past it goes"
    );
    trim_all(&mut s, 6, &arch);
    let mut all = archive::seqs(&arch).unwrap();
    all.extend(stored(&s));
    assert_eq!(all, vec![1, 2, 3, 4]);
}

#[test]
fn a_world_reset_keeps_the_new_worlds_history_in_the_kept_archive() {
    // Review findings 2 and 5: world.db is deleted, the archive is kept,
    // and the new world's seqs start again at 1 in the same month.
    let dir = tempfile::tempdir().unwrap();
    let arch = dir.path().join("archive");
    let db = dir.path().join("world.db");
    let mut s = stamped(&db, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    trim_all(&mut s, 20, &arch);
    assert_eq!(stored(&s), Vec::<u64>::new());
    drop(s);
    std::fs::remove_file(&db).unwrap();
    let mut s = stamped(&db, &[11, 12, 13, 14, 15]);
    trim_all(&mut s, 20, &arch);
    let seqs = archive::seqs(&arch).unwrap();
    assert_eq!(seqs.len() + stored(&s).len(), 15, "archive {seqs:?}");
}

#[test]
fn a_restored_backup_that_reuses_archived_seqs_refuses_to_trim() {
    // The same epoch (a copy of world.db) with other events under seqs the
    // archive already holds: retention refuses rather than deleting them.
    let day = crate::rewards::DAY_MS;
    let dir = tempfile::tempdir().unwrap();
    let arch = dir.path().join("archive");
    let db = dir.path().join("world.db");
    let backup = dir.path().join("backup.db");
    drop(stamped(&db, &[1, 2, 3]));
    std::fs::copy(&db, &backup).unwrap();
    let go_on = |s: &mut Store, words: &str| {
        let mut w = World::from_state(pack(), s.load(0).unwrap().unwrap().state, Vec::new());
        let ada = w.login("ada", 4 * day).unwrap();
        assert!(w.execute(&ada, &format!("say {words}"), 5 * day).ok);
        let fresh = w.drain_fresh();
        s.save("crypt-v01", &w.snapshot(), &fresh, &[], 0).unwrap();
    };
    // The live world goes on, then archives through seq 5.
    let mut s = Store::open(&db, 1, 9 * day).unwrap();
    go_on(&mut s, "live");
    trim_all(&mut s, 20, &arch);
    assert_eq!(archive::seqs(&arch).unwrap(), vec![1, 2, 3, 4, 5]);
    drop(s);
    // The operator restores the backup (seqs 1..=3, the same epoch) and
    // the world goes on: seq 4 (the same login at the same time) is the
    // same line, seq 5 is another.
    std::fs::copy(&backup, &db).unwrap();
    let mut s = Store::open(&db, 1, 9 * day).unwrap();
    go_on(&mut s, "other");
    assert_eq!(stored(&s), vec![1, 2, 3, 4, 5]);
    let err = s.trim_events(20 * day, 0, 100, &arch).unwrap_err();
    assert!(matches!(err, StoreError::Archive(_)), "{err:?}");
    assert!(err.to_string().contains("seq 5"), "{err}");
    assert_eq!(stored(&s), vec![1, 2, 3, 4, 5], "nothing was deleted");
    assert_eq!(archive::seqs(&arch).unwrap(), vec![1, 2, 3, 4, 5]);
}

#[test]
fn dropped_rule_counts_and_script_memory_match_the_model() {
    let mut s = Store::open_in_memory(1, 0).unwrap();
    let row = |digest, dup| RuleCountRow {
        digest,
        dup,
        count: 2,
    };
    let ops = [
        SideOp::RuleCounts {
            entity: ada(),
            rows: vec![row(u64::MAX, 0), row(7, 0), row(7, 1)],
        },
        SideOp::RuleCountsDrop {
            entity: ada(),
            keys: vec![(u64::MAX, 0), (7, 1), (9, 0)],
        },
        SideOp::ScriptMemory {
            entity: ada(),
            schema: 2,
            body: serde_json::json!({ "seen": 3 }),
        },
    ];
    s.apply_side(&ops, 5, Fsync::Normal).unwrap();
    let side = s.load_side().unwrap();
    let mut model = SideState {
        epoch: side.epoch,
        boot: side.boot,
        ..SideState::default()
    };
    for op in &ops {
        model.apply(op, 5);
    }
    assert_eq!(side, model);
    assert_eq!(side.rule_counts[&ada()], [row(7, 0)]);
    let memory = s.load_script_memory().unwrap();
    assert_eq!(memory[&ada()], (2, serde_json::json!({ "seen": 3 })));
    // Dropping the last key forgets the entity's counts.
    s.apply_side(
        &[SideOp::RuleCountsDrop {
            entity: ada(),
            keys: vec![(7, 0)],
        }],
        6,
        Fsync::Normal,
    )
    .unwrap();
    assert!(s.load_side().unwrap().rule_counts.is_empty());
}
