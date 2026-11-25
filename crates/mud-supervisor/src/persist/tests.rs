use proptest::prelude::*;

use super::*;

fn row(queued: u32, cleared: u32, ticks: u32) -> MacroRow {
    MacroRow {
        queued,
        cleared,
        ticks,
    }
}

#[test]
fn a_macro_wake_with_a_tick_owed_carries_the_next_number() {
    let open = macro_open(row(1, 0, 0), true);
    assert_eq!(
        open,
        MacroOpen {
            macro_no: 1,
            cover: 1
        }
    );
    let after = mud_world::side::macro_consume(row(1, 0, 0), open.macro_no, open.cover);
    assert_eq!(after, row(1, 1, 1));
    assert_eq!(
        macro_open(after, true),
        MacroOpen {
            macro_no: 0,
            cover: 1
        },
        "nothing owed: no number"
    );
}

#[test]
fn a_wake_without_a_tick_consumes_nothing_of_the_macro_row() {
    let r = row(3, 1, 1);
    let open = macro_open(r, false);
    assert_eq!(open.macro_no, 0);
    assert_eq!(
        mud_world::side::macro_consume(r, open.macro_no, open.cover),
        r
    );
}

#[test]
fn the_last_tick_number_is_never_wrapped() {
    assert_eq!(
        macro_open(row(5, 4, u32::MAX), true),
        MacroOpen {
            macro_no: 0,
            cover: 5
        }
    );
}

#[test]
fn booking_ids_after_a_restart_pass_both_sources() {
    assert_eq!(next_booking_id(0, 0), 1);
    assert_eq!(next_booking_id(7, 3), 8);
    assert_eq!(next_booking_id(3, 7), 8);
    assert_eq!(next_booking_id(u64::MAX, 0), u64::MAX);
}

#[test]
fn bookings_wait_fire_or_drop() {
    assert_eq!(booking_disposition(100, 99, 10), BookingDisposition::Wait);
    assert_eq!(booking_disposition(100, 100, 10), BookingDisposition::Fire);
    assert_eq!(booking_disposition(100, 110, 10), BookingDisposition::Fire);
    assert_eq!(booking_disposition(100, 111, 10), BookingDisposition::Drop);
    assert_eq!(
        booking_disposition(100, u64::MAX, u64::MAX),
        BookingDisposition::Fire
    );
}

#[test]
fn a_restored_macro_tick_stands_is_clamped_or_fires_once() {
    assert_eq!(restore_macro(1_500, 1_000, 1_000, 7), 1_500, "stands");
    assert_eq!(restore_macro(1_000, 1_000, 1_000, 7), 1_000, "now stands");
    assert_eq!(restore_macro(9_000, 1_000, 1_000, 7), 2_000, "clamped");
    assert_eq!(restore_macro(10, 1_000, 1_000, 7), 1_007, "catch-up");
    assert_eq!(restore_macro(10, u64::MAX - 1, 0, 7), u64::MAX);
}

#[test]
fn a_plan_cursor_moves_only_with_an_effect() {
    assert_eq!(plan_step(0, 0, true), 1);
    assert_eq!(plan_step(1, 1, false), 1);
    assert_eq!(plan_step(1, 2, true), 3);
    assert_eq!(plan_step(3, 1, true), 3, "never back");
    assert_eq!(plan_step(0, u32::MAX, true), 0);
    assert_eq!(plan_resume(2, 5), 2);
    assert_eq!(plan_resume(5, 5), 5);
    assert_eq!(plan_resume(9, 5), 5);
}

#[test]
fn wake_ids_read_back_whatever_the_entity_name() {
    let k = wake_id_key(0xdead_beef, 3, 17);
    let id = k.id(&EntityId::from("pc-kestrel-2"));
    assert_eq!(id, "wake-00000000deadbeef-3-pc-kestrel-2-17");
    assert_eq!(
        WakeKey::parse(&id),
        Some((k, EntityId::from("pc-kestrel-2")))
    );
    assert_eq!(WakeKey::parse("wake-1-2-x-3"), None);
    assert_eq!(WakeKey::parse("wake-00000000deadbeef-3--17"), None);
    assert_eq!(WakeKey::parse("wake-00000000deadbeef-x-e-17"), None);
}

/// One history of a plan of `len` lines: runs from the durable cursor,
/// each line effectful or not, with a crash after any of the first
/// `crashes.len()` lines run (then resume runs to the end).
fn replay(len: u32, effects: &[bool], crashes: &[bool]) -> (Vec<u32>, Vec<u32>, u32) {
    let mut taken = vec![0u32; len as usize];
    let mut ran = vec![0u32; len as usize];
    let mut durable = 0u32;
    let mut k = 0usize;
    loop {
        let mut line = plan_resume(durable, len);
        if line == len {
            break;
        }
        while line < len {
            let e = effects[k % effects.len()];
            let crash = crashes.get(k).copied().unwrap_or(false);
            k += 1;
            ran[line as usize] += 1;
            if e {
                taken[line as usize] += 1;
            }
            durable = plan_step(durable, line, e);
            line += 1;
            if crash {
                break;
            }
        }
        if line == len {
            break;
        }
    }
    (taken, ran, durable)
}

proptest! {
    /// The proved exactly-once property, run: whatever the crashes, no
    /// line takes effect twice, and every line runs.
    #[test]
    fn plans_take_each_effect_at_most_once_and_run_every_line(
        len in 1u32..8,
        effects in proptest::collection::vec(any::<bool>(), 1..16),
        crashes in proptest::collection::vec(any::<bool>(), 1..16),
    ) {
        let (taken, ran, durable) = replay(len, &effects, &crashes);
        prop_assert!(taken.iter().all(|&t| t <= 1), "{taken:?}");
        prop_assert!(ran.iter().all(|&r| r >= 1), "{ran:?}");
        prop_assert!(durable <= len);
        for (i, (&t, &r)) in taken.iter().zip(&ran).enumerate() {
            // A line that took effect ran its last time then.
            prop_assert!(t == 0 || r >= 1, "line {i}");
        }
    }

    /// Wake ids never collide across boots and counters.
    #[test]
    fn wake_ids_are_injective(
        a in (any::<u64>(), 0u64..1_000, 0u64..1_000),
        b in (any::<u64>(), 0u64..1_000, 0u64..1_000),
    ) {
        let e = EntityId::from("torin");
        let (ka, kb) = (wake_id_key(a.0, a.1, a.2), wake_id_key(b.0, b.1, b.2));
        prop_assert_eq!(ka.id(&e) == kb.id(&e), ka == kb);
        prop_assert_eq!(WakeKey::parse(&ka.id(&e)), Some((ka, e)));
    }
}

// ---- budgets (phase 3) ----

const fn t(tokens: u64, cost_usd_micros: u64, calls: u64) -> Tally {
    Tally {
        tokens,
        cost_usd_micros,
        calls,
    }
}

const fn ds(day: u64, local: Tally, lg: Tally) -> DaySpend {
    DaySpend { day, local, lg }
}

/// The dated order of the proofs.
fn le(a: DaySpend, b: DaySpend) -> bool {
    let tle = |x: Tally, y: Tally| {
        x.tokens <= y.tokens && x.cost_usd_micros <= y.cost_usd_micros && x.calls <= y.calls
    };
    a.day < b.day || (a.day == b.day && tle(a.local, b.local) && tle(a.lg, b.lg))
}

#[test]
fn a_spend_lands_in_its_tally_and_a_later_day_rolls_both() {
    let s = budget_spend(DaySpend::default(), 7, false, t(10, 2, 1));
    let s = budget_spend(s, 7, true, t(5, 1, 1));
    assert_eq!(s, ds(7, t(10, 2, 1), t(5, 1, 1)));
    assert_eq!(budget_used(s), 15);
    let next = budget_spend(s, 8, true, t(1, 0, 1));
    assert_eq!(
        next,
        ds(8, tally_zero(), t(1, 0, 1)),
        "a new day starts at 0"
    );
    let back = budget_spend(next, 7, false, t(3, 0, 1));
    assert_eq!(back.day, 8, "the day never rolls back");
    assert_eq!(back.local, t(3, 0, 1));
    assert_eq!(
        budget_used(ds(0, t(u64::MAX, 0, 0), t(1, 0, 0))),
        u64::MAX,
        "saturating"
    );
    assert_eq!(
        tally_add(t(u64::MAX, u64::MAX, u64::MAX), t(1, 1, 1)),
        t(u64::MAX, u64::MAX, u64::MAX)
    );
}

#[test]
fn a_restore_keeps_today_and_tomorrow_and_zeroes_the_rest() {
    let row = ds(10, t(5, 1, 1), t(7, 2, 1));
    assert_eq!(restore_budget(row, 10), row, "the same day stands");
    assert_eq!(restore_budget(row, 9), row, "one day ahead stands");
    assert_eq!(restore_budget(row, 11), ds(11, tally_zero(), tally_zero()));
    assert_eq!(restore_budget(row, 8), ds(8, tally_zero(), tally_zero()));
    let primed = prime_budget(row, 10, t(9, 1, 3));
    assert_eq!(primed, ds(10, t(5, 1, 1), t(9, 2, 3)), "each field rises");
    assert_eq!(prime_budget(row, 9, t(99, 9, 9)), row, "another day: kept");
    assert_eq!(restore_lg(t(4, 0, 2), t(3, 5, 1)), t(4, 5, 2));
}

fn tally() -> impl Strategy<Value = Tally> {
    (0u64..50, 0u64..50, 0u64..5).prop_map(|(a, b, c)| t(a, b, c))
}

fn day_spend() -> impl Strategy<Value = DaySpend> {
    (0u64..4, tally(), tally()).prop_map(|(d, l, g)| ds(d, l, g))
}

proptest! {
    #[test]
    fn the_budget_join_is_a_semilattice(a in day_spend(), b in day_spend(), c in day_spend()) {
        prop_assert_eq!(budget_join(a, b), budget_join(b, a));
        prop_assert_eq!(budget_join(budget_join(a, b), c), budget_join(a, budget_join(b, c)));
        prop_assert_eq!(budget_join(a, a), a);
        prop_assert!(le(a, budget_join(a, b)) && le(b, budget_join(a, b)));
    }

    /// The store's `Spend` merge, folded in memory, is the kernel's join.
    #[test]
    fn the_store_merge_is_the_kernel_join(rows in proptest::collection::vec(day_spend(), 1..6)) {
        let e = EntityId::from("torin");
        let mut side = mud_world::side::SideState::default();
        let mut want = DaySpend::default();
        for r in &rows {
            side.apply(
                &mud_world::SideOp::Spend {
                    entity: e.clone(),
                    day: r.day,
                    local: r.local,
                    lg: r.lg,
                    harvest_tokens_seen: None,
                },
                0,
            );
            want = budget_join(want, *r);
        }
        let got = &side.entities[&e];
        prop_assert_eq!(ds(got.budget_day, got.local, got.lg), want);
    }

    /// Writes of memory snapshots, in any order, never pass memory.
    #[test]
    fn the_stored_budget_never_passes_memory(
        spends in proptest::collection::vec((0u64..3, any::<bool>(), tally()), 1..12),
        order in proptest::collection::vec(any::<prop::sample::Index>(), 0..12),
    ) {
        let mut mem = DaySpend::default();
        let mut hist = vec![mem];
        for (day, ledger, add) in spends {
            let next = budget_spend(mem, day, ledger, add);
            prop_assert!(le(mem, next), "a spend never goes down");
            mem = next;
            hist.push(mem);
        }
        let mut stored = DaySpend::default();
        for i in order {
            stored = budget_join(stored, *i.get(&hist));
        }
        prop_assert!(le(stored, mem));
        if stored.day == mem.day {
            prop_assert!(budget_used(stored) <= budget_used(mem));
        }
    }
}
