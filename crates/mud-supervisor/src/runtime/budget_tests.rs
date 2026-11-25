//! What a restart keeps of the budgets (ADR 0011, phase 3): the day's
//! tallies by source, the ledger priming and the harvest token mark, on a
//! `world.db` in a temporary folder.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};

use autumn_harvest_agent::{
    AgentError, AgentModel, BoxFuture as HBoxFuture, ChatRequest, ChatResponse, ContentPart,
    StopReason, TokenUsage,
};
use mud_world::EntityId;

use super::persist_tests::{DAY, Disk, Run, T0, quiet, start, torin};
use super::*;
use crate::harvest::{HarvestMind, HarvestPersona};
use crate::ledger::LedgerMeter;
use crate::ledger::fake::{call, ledger};
use crate::mind::{BoxFuture, InFlight, InFlightGuard, MindError};

/// A mind that spends on every wake: a fixed amount it metered itself, or
/// what a littleguy ledger holds that it has not charged yet.
#[derive(Debug)]
struct Spender {
    per_wake: u64,
    ledger: Option<LedgerMeter>,
}

impl Spender {
    fn local(per_wake: u64) -> Arc<Self> {
        Arc::new(Self {
            per_wake,
            ledger: None,
        })
    }

    fn ledger(path: &Path) -> Arc<Self> {
        Arc::new(Self {
            per_wake: 0,
            ledger: Some(LedgerMeter::new(path)),
        })
    }
}

impl Mind for Spender {
    fn kind(&self) -> &'static str {
        "spender"
    }

    fn spend_source(&self) -> SpendSource {
        if self.ledger.is_some() {
            SpendSource::Ledger
        } else {
            SpendSource::Local
        }
    }

    fn prime(&self, _: &EntityId, now_ms: u64) -> Option<Usage> {
        self.ledger.as_ref()?.prime(now_ms).unwrap()
    }

    fn wake(&self, ctx: WakeContext, _: MudTools) -> BoxFuture<'_, Result<MindReply, MindError>> {
        let usage = match &self.ledger {
            Some(m) => m.charge(ctx.now_ms).unwrap().unwrap_or_default(),
            None => Usage {
                calls: 1,
                input_tokens: self.per_wake,
                ..Usage::default()
            },
        };
        Box::pin(async move {
            Ok(MindReply {
                usage,
                ..MindReply::default()
            })
        })
    }
}

fn budget(limit: u64) -> SupervisorConfig {
    SupervisorConfig {
        daily_token_budget: Some(limit),
        ..quiet()
    }
}

fn torin_on(m: Arc<dyn Mind>) -> HashMap<EntityId, Arc<dyn Mind>> {
    HashMap::from([(torin(), m)])
}

/// One macro-tick wake of torin, played to its close.
async fn one_wake(r: &mut Run) {
    r.sup.fire_macro_tick(&torin());
    r.run_for(10_000, 500).await;
}

fn level_and_spend(r: &Run) -> (BudgetLevel, DaySpend) {
    r.sup.budget_of(&torin()).unwrap()
}

/// The stored `sup_entity` spend columns of torin: day, local tokens, lg
/// tokens, harvest mark.
fn stored(disk: &Disk) -> (u64, u64, u64, Option<u64>) {
    let rows = disk.sql(
        "SELECT budget_day, local_tokens, lg_tokens, harvest_tokens_seen FROM sup_entity WHERE entity = 'torin'",
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        },
    );
    let u = |v: i64| u64::try_from(v).unwrap();
    rows.first()
        .map(|&(d, l, g, h)| (u(d), u(l), u(g), h.map(u)))
        .unwrap()
}

#[tokio::test]
async fn todays_spend_survives_restart_and_the_hard_stop_stands() {
    let disk = Disk::new();
    let mut r = start(&disk, T0, budget(1_000), torin_on(Spender::local(300)));
    for _ in 0..3 {
        one_wake(&mut r).await;
    }
    let before = level_and_spend(&r);
    assert_eq!(before.0, BudgetLevel::Soft, "{before:?}");
    assert_eq!(before.1.local.tokens, 900);
    assert_eq!(before.1.local.calls, 3);
    drop(r);

    let mut r = start(
        &disk,
        T0 + 60_000,
        budget(1_000),
        torin_on(Spender::local(300)),
    );
    assert_eq!(level_and_spend(&r), before, "the same level and tallies");
    one_wake(&mut r).await;
    let hard = level_and_spend(&r);
    assert_eq!(hard.0, BudgetLevel::Hard);
    assert_eq!(hard.1.local.tokens, 1_200);
    drop(r);

    let r = start(
        &disk,
        T0 + 120_000,
        budget(1_000),
        torin_on(Spender::local(300)),
    );
    assert_eq!(
        level_and_spend(&r),
        hard,
        "the hard stop survives a restart"
    );
    assert_eq!(stored(&disk), (T0 / DAY, 1_200, 0, None));
    // Spend from outside a wake is stored at once.
    r.sup.record_spend(
        &torin(),
        Usage {
            input_tokens: 10,
            ..Usage::default()
        },
    );
    assert_eq!(stored(&disk).1, 1_210);
}

#[tokio::test]
async fn spend_rolls_when_the_outage_crosses_utc_midnight() {
    let disk = Disk::new();
    let mut r = start(&disk, T0, budget(1_000), torin_on(Spender::local(1_000)));
    one_wake(&mut r).await;
    assert_eq!(level_and_spend(&r).0, BudgetLevel::Hard);
    drop(r);
    let r = start(
        &disk,
        DAY + 3_600_000,
        budget(1_000),
        torin_on(Spender::local(1_000)),
    );
    let (level, spend) = level_and_spend(&r);
    assert_eq!(level, BudgetLevel::Normal, "a new day");
    assert_eq!(spend.day, 1);
    assert_eq!(spend.local.tokens, 0);
}

#[tokio::test]
async fn a_clock_that_goes_back_does_not_reset_todays_spend() {
    let disk = Disk::new();
    let day1 = DAY + 3_600_000;
    let mut r = start(&disk, day1, budget(1_000), torin_on(Spender::local(1_000)));
    one_wake(&mut r).await;
    assert_eq!(level_and_spend(&r).0, BudgetLevel::Hard);
    drop(r);
    // The clock went back across midnight: the later day's counters stand.
    let r = start(&disk, T0, budget(1_000), torin_on(Spender::local(1_000)));
    assert_eq!(level_and_spend(&r).0, BudgetLevel::Hard, "not reset");
    assert_eq!(level_and_spend(&r).1.day, 1);
    // They still apply on the real next day, and roll the day after it.
    r.clock.set(day1 + 60_000);
    assert_eq!(level_and_spend(&r).0, BudgetLevel::Hard);
    r.clock.set(2 * DAY + 1);
    assert_eq!(level_and_spend(&r).0, BudgetLevel::Normal);
    drop(r);
    // More than a day back is a reset.
    let r = start(
        &disk,
        5 * DAY,
        budget(1_000),
        torin_on(Spender::local(1_000)),
    );
    drop(r);
    let r = start(
        &disk,
        2 * DAY + 5,
        budget(1_000),
        torin_on(Spender::local(1_000)),
    );
    assert_eq!(level_and_spend(&r).0, BudgetLevel::Normal);
}

#[tokio::test]
async fn a_budget_reset_far_ahead_is_stored_and_the_new_day_survives_restarts() {
    let disk = Disk::new();
    // The clock ran three days fast, and torin spent to Hard on day 5.
    let mut r = start(
        &disk,
        5 * DAY + T0,
        budget(1_000),
        torin_on(Spender::local(1_000)),
    );
    one_wake(&mut r).await;
    assert_eq!(level_and_spend(&r).0, BudgetLevel::Hard);
    drop(r);
    // The clock is fixed: day 2. The reset is stored, not only in memory.
    let mut r = start(
        &disk,
        2 * DAY + T0,
        budget(1_000),
        torin_on(Spender::local(900)),
    );
    assert_eq!(level_and_spend(&r).0, BudgetLevel::Normal, "reset");
    assert_eq!(stored(&disk), (2, 0, 0, None), "the reset is stored");
    one_wake(&mut r).await;
    let soft = level_and_spend(&r);
    assert_eq!(soft.0, BudgetLevel::Soft);
    assert_eq!(stored(&disk), (2, 900, 0, None), "day 2's spend is stored");
    drop(r);
    // A restart on day 2 keeps day 2's spend and gives no new allowance.
    let r = start(
        &disk,
        2 * DAY + T0 + 60_000,
        budget(1_000),
        torin_on(Spender::local(900)),
    );
    assert_eq!(
        level_and_spend(&r),
        soft,
        "the spend after the reset stands"
    );
    // The same process runs on to the real day 5, spending nothing.
    r.clock.set(5 * DAY + T0 + 1_000);
    let before = level_and_spend(&r);
    assert_eq!(before.0, BudgetLevel::Normal);
    drop(r);
    // The stale day-5 counters never come back.
    let r = start(
        &disk,
        5 * DAY + T0 + 2_000,
        budget(1_000),
        torin_on(Spender::local(900)),
    );
    assert_eq!(level_and_spend(&r), before, "a restart keeps the level");
}

#[tokio::test]
async fn late_spend_outside_a_wake_is_stored_by_the_next_step() {
    let disk = Disk::new();
    let mut r = start(&disk, T0, budget(1_000), torin_on(Spender::local(100)));
    one_wake(&mut r).await;
    assert_eq!(stored(&disk).1, 100);
    // A model that kept going after its wake closed reports late.
    r.sup.usage.add(
        &torin(),
        Usage {
            calls: 1,
            input_tokens: 7,
            ..Usage::default()
        },
    );
    for h in r.sup.step() {
        h.await.unwrap();
    }
    assert_eq!(stored(&disk).1, 107, "the step stores the late spend");
    drop(r);
    let r = start(
        &disk,
        T0 + 60_000,
        budget(1_000),
        torin_on(Spender::local(100)),
    );
    assert_eq!(level_and_spend(&r).1.local.tokens, 107);
    assert_eq!(level_and_spend(&r).1.local.calls, 2);
}

#[tokio::test]
async fn littleguy_spend_is_not_counted_twice() {
    // Day 1, so the ledger has a yesterday.
    let t0 = DAY + T0;
    let disk = Disk::new();
    let lg = tempfile::tempdir().unwrap();
    let (path, conn) = ledger(lg.path());
    call(&conn, 1, 100, 0, Some(7), t0 - 1_000);
    call(&conn, 2, 900, 0, None, t0 - DAY);
    let mut r = start(&disk, t0, budget(10_000), torin_on(Spender::ledger(&path)));
    let (_, spend) = level_and_spend(&r);
    assert_eq!(
        (spend.lg.tokens, spend.lg.cost_usd_micros, spend.lg.calls),
        (100, 7, 1),
        "primed at boot with today's rows only"
    );
    assert_eq!(spend.local.tokens, 0);
    call(&conn, 3, 50, 0, None, t0 + 10);
    one_wake(&mut r).await;
    assert_eq!(level_and_spend(&r).1.lg.tokens, 150);
    assert_eq!(stored(&disk).2, 150);
    drop(r);

    // A restart neither bills the ledger again nor forgets it.
    let r = start(
        &disk,
        t0 + 30_000,
        budget(10_000),
        torin_on(Spender::ledger(&path)),
    );
    assert_eq!(level_and_spend(&r).1.lg.tokens, 150, "not 300");
    drop(r);
    // A call the store never charged (a crash before the close) counts.
    call(&conn, 4, 30, 0, None, t0 + 31_000);
    let mut r = start(
        &disk,
        t0 + 40_000,
        budget(10_000),
        torin_on(Spender::ledger(&path)),
    );
    assert_eq!(level_and_spend(&r).1.lg.tokens, 180);
    one_wake(&mut r).await;
    assert_eq!(level_and_spend(&r).1.lg.tokens, 180, "nothing new");
}

#[tokio::test]
async fn ledger_rows_purged_by_littleguy_keep_the_persisted_floor() {
    let disk = Disk::new();
    let lg = tempfile::tempdir().unwrap();
    let (path, conn) = ledger(lg.path());
    call(&conn, 1, 400, 0, None, T0 - 1_000);
    let r = start(&disk, T0, budget(500), torin_on(Spender::ledger(&path)));
    assert_eq!(level_and_spend(&r).1.lg.tokens, 400);
    drop(r);
    assert_eq!(stored(&disk).2, 400, "the boot stores the primed floor");
    conn.execute("DELETE FROM harvest_llm_ledger", []).unwrap();
    let mut r = start(
        &disk,
        T0 + 10_000,
        budget(500),
        torin_on(Spender::ledger(&path)),
    );
    assert_eq!(level_and_spend(&r).1.lg.tokens, 400, "the floor holds");
    call(&conn, 2, 100, 0, None, T0 + 11_000);
    one_wake(&mut r).await;
    let (level, spend) = level_and_spend(&r);
    assert_eq!(spend.lg.tokens, 500);
    assert_eq!(level, BudgetLevel::Hard);
}

/// Answers every call at once, with a fixed usage.
#[derive(Debug)]
struct Talker;

impl AgentModel for Talker {
    fn chat<'a>(
        &'a self,
        _request: &'a ChatRequest,
    ) -> HBoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            Ok(ChatResponse {
                content: vec![ContentPart::Text("ok".into())],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::new(10, 5),
            })
        })
    }
}

fn harvest_torin(dir: &Path) -> HashMap<EntityId, Arc<dyn Mind>> {
    let mind = HarvestMind::new(
        Arc::new(Talker),
        Some(dir.to_path_buf()),
        HashMap::from([(
            torin(),
            HarvestPersona {
                name: "Torin".into(),
                persona: "A smith.".into(),
                goal: "Ore.".into(),
            },
        )]),
    );
    torin_on(Arc::new(mind))
}

#[tokio::test(flavor = "multi_thread")]
async fn harvest_spend_unflushed_at_a_crash_is_charged_once() {
    let disk = Disk::new();
    let agents = tempfile::tempdir().unwrap();
    let mut r = start(&disk, T0, budget(1_000_000), harvest_torin(agents.path()));
    one_wake(&mut r).await;
    let one = level_and_spend(&r).1.local.tokens;
    assert!(one > 0, "a harvest job is metered locally");
    assert_eq!(
        stored(&disk).3,
        Some(one),
        "the mark is stored at the close"
    );
    // A crash after the second job, before its close stored anything.
    r.game.refuse_store_writes(true).unwrap();
    one_wake(&mut r).await;
    assert_eq!(level_and_spend(&r).1.local.tokens, 2 * one, "in memory");
    assert_eq!(stored(&disk).1, one, "the store missed it");
    drop(r);

    let mut r = start(
        &disk,
        T0 + 60_000,
        budget(1_000_000),
        harvest_torin(agents.path()),
    );
    assert_eq!(level_and_spend(&r).1.local.tokens, one);
    one_wake(&mut r).await;
    assert_eq!(
        level_and_spend(&r).1.local.tokens,
        3 * one,
        "the lost job is charged once, with the next"
    );
    assert_eq!(stored(&disk).3, Some(3 * one));
    one_wake(&mut r).await;
    assert_eq!(
        level_and_spend(&r).1.local.tokens,
        4 * one,
        "and never again"
    );
}

/// A mind that counts lifetime tokens as a harvest agent does. The count
/// lives in its "file" (`used`) and outlasts a restart; the mark (what the
/// budget was charged up to) is per process and comes back from the
/// store. A wake adds `per_wake` to the count and charges what the count
/// passed the mark. A held wake returns at once and leaves its job open:
/// the test ends the job late ([`Lifetime::finish_late`]) and then lets
/// the wake settle ([`Lifetime::release`]).
#[derive(Debug)]
struct Lifetime {
    used: Arc<AtomicU64>,
    per_wake: u64,
    mark: StdMutex<Option<u64>>,
    in_flight: InFlight,
    held: StdMutex<Option<InFlightGuard>>,
    hold_next: AtomicBool,
}

impl Lifetime {
    fn new(used: &Arc<AtomicU64>, per_wake: u64) -> Arc<Self> {
        Arc::new(Self {
            used: Arc::clone(used),
            per_wake,
            mark: StdMutex::new(None),
            in_flight: InFlight::default(),
            held: StdMutex::new(None),
            hold_next: AtomicBool::new(false),
        })
    }

    fn is_held(&self) -> bool {
        self.held.lock().unwrap().is_some()
    }

    /// The held job ends: the count rises by `n`, the mark is raised, and
    /// then the spend goes to the supervisor's sink, as the harvest
    /// worker does on a timed-out or failed job.
    fn finish_late(&self, sup: &Supervisor, entity: &EntityId, n: u64) {
        let now = self.used.fetch_add(n, Ordering::SeqCst) + n;
        *self.mark.lock().unwrap() = Some(now);
        sup.usage.add(
            entity,
            Usage {
                calls: 1,
                input_tokens: n,
                ..Usage::default()
            },
        );
    }

    fn release(&self) {
        self.held.lock().unwrap().take();
    }
}

impl Mind for Lifetime {
    fn kind(&self) -> &'static str {
        "lifetime"
    }

    fn restore_tokens_seen(&self, _: &EntityId, seen: u64) {
        *self.mark.lock().unwrap() = Some(seen);
    }

    fn tokens_seen(&self, _: &EntityId) -> Option<u64> {
        *self.mark.lock().unwrap()
    }

    fn settled(&self, entity: &EntityId) -> BoxFuture<'_, ()> {
        self.in_flight.settled(entity)
    }

    fn wake(&self, ctx: WakeContext, _: MudTools) -> BoxFuture<'_, Result<MindReply, MindError>> {
        let reply = if self.hold_next.swap(false, Ordering::SeqCst) {
            *self.held.lock().unwrap() = Some(self.in_flight.start(&ctx.entity));
            MindReply::default()
        } else {
            let before = self.used.load(Ordering::SeqCst);
            let from = self.mark.lock().unwrap().map_or(before, |m| m.min(before));
            let now = self.used.fetch_add(self.per_wake, Ordering::SeqCst) + self.per_wake;
            *self.mark.lock().unwrap() = Some(now);
            MindReply {
                usage: Usage {
                    calls: 1,
                    input_tokens: now - from,
                    ..Usage::default()
                },
                ..MindReply::default()
            }
        };
        Box::pin(async move { Ok(reply) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_spend_of_an_open_wake_is_stored_with_its_mark() {
    let disk = Disk::new();
    let used = Arc::new(AtomicU64::new(0));
    let m = Lifetime::new(&used, 10);
    let mut r = start(&disk, T0, budget(1_000_000), torin_on(m.clone()));
    one_wake(&mut r).await;
    assert_eq!(stored(&disk), (0, 10, 0, Some(10)));
    // A wake whose job outlives it: the wake returns, the close waits for
    // the job to settle.
    m.hold_next.store(true, Ordering::SeqCst);
    r.sup.fire_macro_tick(&torin());
    let mut open = Vec::new();
    for _ in 0..40 {
        open.extend(r.sup.step());
        if m.is_held() {
            break;
        }
        r.clock.advance(500);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(m.is_held(), "the held wake started");
    // The job ends 15 tokens later, and a clock step runs before the close.
    m.finish_late(&r.sup, &torin(), 15);
    for h in r.sup.step() {
        h.await.unwrap();
    }
    assert_eq!(
        stored(&disk),
        (0, 10, 0, Some(10)),
        "no tally is stored ahead of its mark"
    );
    // A kill before the close commits.
    r.game.refuse_store_writes(true).unwrap();
    m.release();
    for h in open {
        h.await.unwrap();
    }
    assert_eq!(level_and_spend(&r).1.local.tokens, 25, "in memory");
    drop(r);

    let m = Lifetime::new(&used, 10);
    let mut r = start(&disk, T0 + 60_000, budget(1_000_000), torin_on(m.clone()));
    assert_eq!(level_and_spend(&r).1.local.tokens, 10);
    one_wake(&mut r).await;
    assert_eq!(
        level_and_spend(&r).1.local.tokens,
        35,
        "every token the agent used is charged once"
    );
    assert_eq!(stored(&disk), (0, 35, 0, Some(35)));
}
