# Verus proofs

Verus checks the crate root `src/lib.rs`. From the repository root:

```sh
VERUS=/home/user/tools/verus-x86-linux/verus   # any Verus 0.2026.10.04.426d8b0 binary
"$VERUS" formal/verus/src/lib.rs --crate-type=lib
```

Expect `verification results:: 32 verified, 0 errors`. The release links
against rustup toolchain `1.98.1-x86_64-unknown-linux-gnu`, which must be
installed.

| Proof | Mirrors | Property |
|---|---|---|
| `src/classify.rs` `classify` | `crates/mud-rules/src/roll.rs` | 3 and 4 always crit; 18 always fumbles; 17 never succeeds; for 7..=16 success iff `total <= effective`; raising `effective` never lowers the outcome rank; clamping `effective` to ±100 never changes the outcome of any `u8` total |
| `src/injury.rs` `injury` | `crates/mud-rules/src/damage.rs` | Result `>= 0`; `== 0` iff `raw <= dr`; non-decreasing in `raw`; for `raw > dr`: impaling is `2*(raw-dr)`, cutting is `(raw-dr)*3/2` floored (always `>= 1`, so the code's floor at 1 never decides), crushing and burn are `raw-dr` |
| `src/wounds.rs` `wound_state` | `crates/mud-rules/src/stats.rs` | Thresholds exactly as coded (`max` is `hp_max`, or 1 when below 1): Dead at `hp <= -5*max`, Dying at `<= -max`, Reeling at `<= 0`, Staggered when `3*hp < max`, else Healthy; lower HP never gives a better state |
| `src/schedule.rs` `shift_minute` | `crates/mud-world/src/schedule.rs` | Result always in `0..1440`; shifting by `k` then `-k` returns any minute `< 1440` |
| `src/chatter.rs` `chatter_step` | `crates/mud-supervisor/src/chatter.rs` | Human/MacroTick reset to 0 and allow; AgentWithHuman/World keep the count and allow; AgentAlone adds 1 (saturating) and allows iff the new count `<= max`. Safety (`lemma_run_allows_at_most_max`): for every input sequence from a fresh counter, the AgentAlone events allowed since the last reset number exactly `min(turns, max)`, so at most `max`, provided `max < 255` |
| `src/budget.rs` `budget_level` | `crates/mud-supervisor/src/budget.rs` | Hard iff `limit == 0 \|\| used >= limit`; Soft iff not Hard and `used*5 >= limit*4` (exact, `u128`); otherwise Normal; non-decreasing in `used` |
| `src/typing.rs` `typing_delay_ms` | `crates/mud-supervisor/src/turing.rs` | No overflow; equals `reaction + (chars*12000/max(wpm,1))*jitter/1000`; `>= reaction`; non-decreasing in `chars` |

Each executable function also proves it never overflows.

## Bounds

| Kernel | `requires` | How the Rust callers keep it |
|---|---|---|
| `classify` | `-100 <= effective <= 100`; `total` any `u8` | `success_roll` clamps `effective` to ±100 (`EFFECTIVE_LIMIT`); proved not to change any outcome |
| `injury` | `raw`, `dr` in `-10_000..=10_000` | `resolve_attack` clamps both to `INJURY_LIMIT`; honest dice roll at most 1657, and DR above the limit stops such a roll either way |
| `wound_state` | `hp <= i32::MAX / 3`, `hp_max <= i32::MAX / 5` | `Pools::wound_state` clamps to `HP_LIMIT` / `HP_MAX_LIMIT`; only inputs that overflowed before change |
| `shift_minute` | none | the sum runs in `i64` |
| `chatter_step` | none (the safety theorem assumes `max < 255`) | default `MAX_AGENT_ONLY_TURNS` is 2. At `max == 255` the count sticks at 255 and every agent-only event is allowed; `RouterConfig::max_agent_only_turns` does not rule that out |
| `budget_level` | none | the 80% test runs in `u128` |
| `typing_delay_ms` | `chars <= 100_000`, `reaction_ms <= 10_000`, `jitter_permille <= 2_000`, any `wpm` | `speech_delay_ms` clamps `chars` to `MAX_TYPED_CHARS` (the parser already rejects lines over 500 characters) and passes reaction 800..=2200, jitter 850..=1150. Only the `chars` bound is needed for no overflow |

## Mirror guard

`cargo test -p mud-rules --test verus_mirror` and
`cargo test -p mud-supervisor --test verus_mirror` (shared logic in
`crates/mud-rules/tests/mirror/mod.rs`) compare the text between
`// mirror-begin <name>` and `// mirror-end <name>` in each Rust file and its
Verus module, white space removed. On the Verus side it drops only the
`requires`/`ensures` clauses and the return name in `-> (r: T)`. Doc
comments, signatures and the enums (`Outcome`, `DamageType`, `WoundState`,
`ChatterInput`, `BudgetLevel`) are mirrored too. Proof steps a body needs
live outside it (`typing.rs` broadcasts its arithmetic lemmas), so no
`proof { }` block sits inside a mirrored region. Every module in `src/` must
be listed in the guard's `PAIRS`.
