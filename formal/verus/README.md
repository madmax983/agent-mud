# Verus proofs

Verus checks the crate root `src/lib.rs`. From the repository root:

```sh
VERUS=/home/user/tools/verus-x86-linux/verus   # any Verus 0.2026.10.04.426d8b0 binary
"$VERUS" formal/verus/src/lib.rs --crate-type=lib
```

Expect `verification results:: 18 verified, 0 errors`. The release links
against rustup toolchain `1.98.1-x86_64-unknown-linux-gnu`, which must be
installed.

| Proof | Mirrors | Property |
|---|---|---|
| `src/classify.rs` `classify` | `crates/mud-rules/src/roll.rs` | 3 and 4 always crit; 18 always fumbles; 17 never succeeds; for 7..=16 success iff `total <= effective`; raising `effective` never lowers the outcome rank; clamping `effective` to ±100 never changes the outcome of any `u8` total |
| `src/injury.rs` `injury` | `crates/mud-rules/src/damage.rs` | Result `>= 0`; `== 0` iff `raw <= dr`; non-decreasing in `raw`; for `raw > dr`: impaling is `2*(raw-dr)`, cutting is `(raw-dr)*3/2` floored (always `>= 1`, so the code's floor at 1 never decides), crushing and burn are `raw-dr` |
| `src/wounds.rs` `wound_state` | `crates/mud-rules/src/stats.rs` | Thresholds exactly as coded (`max` is `hp_max`, or 1 when below 1): Dead at `hp <= -5*max`, Dying at `<= -max`, Reeling at `<= 0`, Staggered when `3*hp < max`, else Healthy; lower HP never gives a better state |
| `src/schedule.rs` `shift_minute` | `crates/mud-world/src/schedule.rs` | Result always in `0..1440`; shifting by `k` then `-k` returns any minute `< 1440` |

Each executable function also proves it never overflows.

## Bounds

| Kernel | `requires` | How the Rust callers keep it |
|---|---|---|
| `classify` | `-100 <= effective <= 100`; `total` any `u8` | `success_roll` clamps `effective` to ±100 (`EFFECTIVE_LIMIT`); proved not to change any outcome |
| `injury` | `raw`, `dr` in `-10_000..=10_000` | `resolve_attack` clamps both to `INJURY_LIMIT`; honest dice roll at most 1657, and DR above the limit stops such a roll either way |
| `wound_state` | `hp <= i32::MAX / 3`, `hp_max <= i32::MAX / 5` | `Pools::wound_state` clamps to `HP_LIMIT` / `HP_MAX_LIMIT`; only inputs that overflowed before change |
| `shift_minute` | none | the sum runs in `i64` |

## Mirror guard

`cargo test -p mud-rules --test verus_mirror` compares the text between
`// mirror-begin <name>` and `// mirror-end <name>` in each Rust file and its
Verus module, white space removed. On the Verus side it drops only the
`requires`/`ensures` clauses and the return name in `-> (r: T)`. Doc
comments, signatures and the enums (`Outcome`, `DamageType`, `WoundState`)
are mirrored too.
