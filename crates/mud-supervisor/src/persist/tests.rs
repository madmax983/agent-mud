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
