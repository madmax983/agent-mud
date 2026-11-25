# Verus proofs

Verus checks the crate root `src/lib.rs`. From the repository root:

```sh
VERUS=/home/user/tools/verus-x86-linux/verus   # any Verus 0.2026.10.04.426d8b0 binary
"$VERUS" formal/verus/src/lib.rs --crate-type=lib
```

Expect `verification results:: 72 verified, 0 errors`. The release links
against rustup toolchain `1.98.1-x86_64-unknown-linux-gnu`, which must be
installed.

| Proof | Mirrors | Property |
|---|---|---|
| `src/classify.rs` `classify` | `crates/mud-rules/src/roll.rs` | 3 and 4 always crit; 18 always fumbles; 17 never succeeds; for 7..=16 success iff `total <= effective`; raising `effective` never lowers the outcome rank; clamping `effective` to ±100 never changes the outcome of any `u8` total |
| `src/injury.rs` `injury` | `crates/mud-rules/src/damage.rs` | Result `>= 0`; `== 0` iff `raw <= dr`; non-decreasing in `raw`; for `raw > dr`: impaling is `2*(raw-dr)`, cutting is `(raw-dr)*3/2` floored (always `>= 1`, so the code's floor at 1 never decides), crushing and burn are `raw-dr` |
| `src/wounds.rs` `wound_state` | `crates/mud-rules/src/stats.rs` | Thresholds exactly as coded (`max` is `hp_max`, or 1 when below 1): Dead at `hp <= -5*max`, Dying at `<= -max`, Reeling at `<= 0`, Staggered when `3*hp < max`, else Healthy; lower HP never gives a better state |
| `src/schedule.rs` `shift_minute` | `crates/mud-world/src/schedule.rs` | Result always in `0..1440`; shifting by `k` then `-k` returns any minute `< 1440` |
| `src/chatter.rs` `chatter_step` | `crates/mud-supervisor/src/chatter.rs` | Human/MacroTick reset to 0 and allow; AgentWithHuman/World keep the count and allow; AgentAlone adds 1 (saturating) and allows iff the count before it is `< max`. Safety (`lemma_run_allows_at_most_max`): for every input sequence from a fresh counter and every `max`, the AgentAlone events allowed since the last reset number exactly `min(turns, max)`, so at most `max` |
| `src/budget.rs` `budget_level` | `crates/mud-supervisor/src/budget.rs` | Hard iff `limit == 0 \|\| used >= limit`; Soft iff not Hard and `used*5 >= limit*4` (exact, `u128`); otherwise Normal; non-decreasing in `used` |
| `src/connections.rs` `conn_step` | `crates/mud-server/src/limits.rs` | An open is admitted iff `total < max_total && here < max_here` and adds one to each; a close takes effect iff `here > 0 && total > 0` and takes one from each; a refused step changes nothing; the step never overflows; `here <= total <= max_total && here <= max_here` survives every step. Safety (`lemma_run_keeps_caps`): every sequence of opens and closes from a fresh limiter keeps that invariant |
| `src/typing.rs` `typing_delay_ms` | `crates/mud-supervisor/src/turing.rs` | No overflow; equals `reaction + (chars*12000/max(wpm,1))*jitter/1000`; `>= reaction`; non-decreasing in `chars` |
| `src/hooks.rs` `hook_step`, `hook_allowed` | `crates/mud-world/src/hooks.rs` | The flag is exactly `count < max`; a reload resets to 0; a good run under the limit clears the count; a tripped hook (`faults >= max`) stays tripped until a reload; a fault adds 1, saturating. Safety (`lemma_at_most_max_consecutive_faults`): after `n` consecutive faults the hook may run iff `n < max`, for every `max`, 255 included |
| `src/login_route.rs` `login_route`, `first_ask_of`, `may_bind` | `crates/mud-accounts/src/route.rs` | R1 (`lemma_agent_pc_asks_like_a_human`): an agent PC (`OtherPlayer`) and a human, for every account and legacy state of either, are first asked `Password`; R2: `Create` only for a free name with no account; R3: `Claim` only for a human with no account while legacy is open; R4: `Refuse` iff an NPC or a mob holds the name; R5: `may_bind` iff the secret is proved and the holder is a human or a free name |
| `src/throttle.rs` `throttle_step`, `throttle_allows` | `crates/mud-accounts/src/throttle.rs` | S1 no overflow; S2 a success clears the key (allows at every time); S3 a failure never delays past `max_ms`, even from a larger stored delay; S4 a failure records `now` and locks until `>= now`, exactly `now + delay <= now + max` with room below `u64::MAX`; S5 free failures never delay; S6 past free, at least `min(base, max)`; S7 the count restarts at 1 after `forget_ms` of quiet, else adds one saturating; S8 allows iff `now >= locked_until_ms`. `lemma_lock_ends_by_cap`; `lemma_rate_bound`: along admitted failures in rising times with no quiet spell, the count after failure `j` is `j`, and failure `j` comes at least `min(base, max)` after failure `j - 1` once `j - 1 > free` |
| `src/hooks.rs` `tally_step`, `tally_admissible` | `crates/mud-world/src/hooks.rs` | The saturating tally of a list counts its effects, refuses and heal/hurts exactly up to 255 (`lemma_tally_counts`); for `max <= 16` it admits a list iff the list has at most `max` effects, at most one heal/hurt, and a refuse only alone in a command hook (`lemma_admission_is_exact`); `tally_exec` folds a list to `tally_of` |

Each executable function also proves it never overflows.

## Bounds

| Kernel | `requires` | How the Rust callers keep it |
|---|---|---|
| `classify` | `-100 <= effective <= 100`; `total` any `u8` | `success_roll` clamps `effective` to ±100 (`EFFECTIVE_LIMIT`); proved not to change any outcome |
| `injury` | `raw`, `dr` in `-10_000..=10_000` | `resolve_attack` clamps both to `INJURY_LIMIT`; honest dice roll at most 1657, and DR above the limit stops such a roll either way |
| `wound_state` | `hp <= i32::MAX / 3`, `hp_max <= i32::MAX / 5` | `Pools::wound_state` clamps to `HP_LIMIT` / `HP_MAX_LIMIT`; only inputs that overflowed before change |
| `shift_minute` | none | the sum runs in `i64` |
| `chatter_step` | none | an earlier `next <= max` let every event through at `max == 255` once the count saturated; the proof found it, and the rule is now `turns < max` |
| `budget_level` | none | the 80% test runs in `u128` |
| `typing_delay_ms` | `chars <= 100_000`, `reaction_ms <= 10_000`, `jitter_permille <= 2_000`, any `wpm` | `speech_delay_ms` clamps `chars` to `MAX_TYPED_CHARS` (the parser already rejects lines over 500 characters) and passes reaction 800..=2200, jitter 850..=1150. Only the `chars` bound is needed for no overflow |
| `hook_step` | none | the count saturates at 255 |
| `tally_admissible` | `max <= 16` (the exactness lemma) | `scripts.toml` `limits.max_effects` is checked to `1..=16` (`MAX_EFFECTS_CEILING`) |
| `login_route`, `may_bind` | none | the dialogue reads `Game::name_owner` again before every account call and before it binds |
| `throttle_step` | none (`lemma_rate_bound` needs `ts[n] + max <= u64::MAX`) | `[accounts.throttle.*]` is checked to `1 <= base <= max <= 86_400_000 < forget`; a key past free inside `forget_ms` is never evicted |

## Mirror guard

`cargo test -p mud-rules --test verus_mirror`,
`cargo test -p mud-supervisor --test verus_mirror`,
`cargo test -p mud-server --test verus_mirror` and
`cargo test -p mud-accounts --test verus_mirror` (shared logic in
`crates/mud-rules/tests/mirror/mod.rs`) compare the text between
`// mirror-begin <name>` and `// mirror-end <name>` in each Rust file and its
Verus module, white space removed. On the Verus side it drops only the
`requires`/`ensures` clauses and the return name in `-> (r: T)`. Doc
comments, signatures and the enums (`Outcome`, `DamageType`, `WoundState`,
`ChatterInput`, `BudgetLevel`, `HookOutcome`, `EffectClass`, `Tally`,
`Holder`, `Route`, `FirstAsk`, `ThrottleOp`, `ThrottlePolicy`, `Throttle`) are
mirrored too. Proof steps a body needs
live outside it (`typing.rs` broadcasts its arithmetic lemmas), so no
`proof { }` block sits inside a mirrored region. Every module in `src/` must
be listed in the guard's `PAIRS`.
