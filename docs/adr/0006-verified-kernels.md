# ADR 0006: Verus proves the kernels; a mirror test keeps them honest

Status: accepted

## Decision

Small pure functions that carry invariants are written once in Rust,
between `// mirror-begin <name>` and `// mirror-end <name>`, and mirrored
verbatim in `formal/verus/src/`, where Verus proves their properties:

| Kernel | Crate | Proved |
|---|---|---|
| `classify` | mud-rules | criticals as specified, 17 never succeeds, monotonic in skill |
| `injury` | mud-rules | never negative, zero iff no penetration, multipliers exact, monotonic |
| `wound_state` | mud-rules | thresholds exact, less HP is never better |
| `shift_minute` | mud-world | stays in 0..1440, shifting back restores |
| `chatter_step` | mud-supervisor | at most `max` agent-only wakes between resets, for every `max`; the boot fold over the reloaded event log, where a missing macro-tick reset reads as no change and an unseen player as none, a roomless event counts in the rooms its parties stood in when it was emitted (recorded on the event), and a room of unknown history starts at the bound, holds at least as much as the live counter did and lets through only what it let through (`lemma_fold_holds_at_least`, ADR 0011 phase 4) |
| `budget_level` | mud-supervisor | Hard/Soft/Normal exactly at the thresholds, monotonic |
| `typing_delay_ms` | mud-supervisor | no overflow, at least the reaction time, monotonic |
| `conn_step` | mud-server | an open is admitted only under both caps; no underflow; caps hold for every sequence |
| `hook_step` | mud-world | a world hook runs at most `max` consecutive faulting times, for every `max`; tripped is absorbing until a reload; a decline (a spent pool, a full mark or flag table) never moves the count, so declines never trip a hook; no overflow |
| `tally_admissible` | mud-world | admits exactly the effect lists of at most `max` effects with at most one heal/hurt, at most one reward, and a refuse only alone in a command hook, so a refused command never mints (ADR 0010) |
| `login_route`, `first_ask_of`, `may_bind` | mud-accounts | an agent PC is asked exactly what any human account is asked; only free names create; only open legacy humans claim; refused iff an NPC; nothing binds an agent PC or an NPC (ADR 0009) |
| `throttle_step`, `throttle_allows` | mud-accounts | no overflow; never locked past the cap; free failures never delay; past free at least `min(base, max)`; a quiet spell forgets; checked failures are spaced (`lemma_rate_bound`) (ADR 0009) |
| `macro_queue`, `macro_consume`, `dated_max` | mud-world | the side-row merges are inflationary, idempotent and commute (any two macro ops, in either order, give the same row; an op applied twice is applied once); `cleared <= queued` survives every run; the dated join of `Spend` and `Booked` is a join-semilattice (commutative, associative, idempotent), so upserts in any order converge (ADR 0011) |
| `schema_verdict`, `retention_cutoff`, `archive_verdict`, `pending_overflow`, `events_truncated` | mud-world | a file newer than the code is refused and only an equal one is used as is; `keep_days = 0` trims nothing, an event younger than the window is never trimmed, the cutoff never passes now and never moves back; the archive stays strictly ascending, an accepted batch leaves every row's exact line in the file, so a trim deletes only archived rows (`lemma_every_trimmed_row_is_archived`), a seq-prefix batch above the file is written whole, and a crash after any prefix of a batch then a re-run gives the file of one clean run (`lemma_crash_rerun_is_idempotent`); the backlog keeps at most its cap and drops nothing within it; `truncated` is set exactly when an event after `since` left the log, which holds `oldest..=last` with no hole (ADR 0011) |
| `macro_open` | mud-supervisor | a wake numbers a macro tick (`ticks + 1`) only when it carries one and one is owed; consuming it grows `ticks` by at most one and keeps `cleared <= queued`; any other wake consumes nothing; along committed consumes the tick numbers strictly grow, so each number is carried by at most one (`lemma_committed_tick_numbers_are_distinct`), and torin's `nth = 1` rule runs for one tick only (ADR 0011) |
| `next_booking_id` | mud-supervisor | the first id after a restart is above every id the meta row or a stored row holds, so no booking id is issued twice (`lemma_restart_never_reissues_a_booking_id`) (ADR 0011) |
| `booking_disposition` | mud-supervisor | total: `Wait` exactly before the due time, `Fire` from it to `max_late` after it, `Drop` past that (ADR 0011) |
| `restore_macro` | mud-supervisor | the restored next tick is never before now; a time inside the window stands, a later one is clamped, a past one fires once at `now + pick` however long the outage; a second restore at the same time changes nothing (`lemma_restore_is_stable`) (ADR 0011) |
| `plan_step`, `plan_resume` | mud-supervisor | the durable cursor moves past a line only with that line's effect: over every history of runs, crashes and resumes the cursor never moves back, each line's effect commits at most once, a line that took effect never runs again, and a plan run to its end ran every line, so each effectful line takes effect exactly once (`lemma_plan_exactly_once`, `lemma_no_run_after_effect`) (ADR 0011) |
| `wake_id_key` | mud-supervisor | injective; with the boot raised at every open and a counter within the boot, no wake key repeats (`lemma_wake_keys_never_repeat`); the supervisor keeps one such counter per entity, and the entity is in the id, so no wake id repeats (ADR 0011) |
| `budget_join`, `roll_day`, `budget_spend`, `budget_used` | mud-supervisor | a day's spend split into the supervisor's and littleguy's tallies: the join is a join-semilattice and the least upper bound of the dated order, and agrees field by field with the store's `dated_max` join; a spend never goes down; `roll_day` never lowers the day and zeroes only on an increase; so the stored budget, the join of any of the in-memory budgets written in any order, repeated or lost, never passes the one in memory (`lemma_stored_never_passes_memory`), and on one day the breaker's count grows with it; no overflow (ADR 0011) |
| `restore_lg`, `restore_budget`, `prime_budget` | mud-supervisor | the ledger tally after a restart is the componentwise max of the stored tally and the ledger's own today: with both at most the truth it is at most the truth and at least each (`lemma_restore_lg_stays_below_the_truth`), so no ledger row is billed twice and none is forgotten; a stored row of today or tomorrow stands, an earlier one is zeroed, a later one is reset; priming only raises today's ledger tally (ADR 0011) |
| `replay_floor`, `replay_due` | mud-supervisor | the boot replays an addressed event iff it is after the recipient's floor (the stored `seen_seq`, never above the last seq; the last seq without a row) and at most `replay_max_age_ms` old; over every history of answers, silent closes and crashes with a boot at any time, a tell is answered at most once (`lemma_a_tell_is_answered_at_most_once`), and a tell no committed wake consumed is queued again by every boot within the window (`lemma_a_fresh_tell_is_not_lost`); a consumed event is never replayed, one too old stays too old, and the MCP cursor issued at the floor never skips an event after the boot (ADR 0011 phase 4) |
| `pool_admit`, `pool_roll`, `pool_period` | mud-world | a reward ask is admitted whole iff it fits in what is left of its pool; no run of asks takes a pool past its budget, and the count is exactly the admitted asks; a later period starts at zero and the clock set back never refills a pool; no overflow (ADR 0010) |

The `verus_mirror` tests in `mud-rules` (which also guards the mud-world
kernels), `mud-supervisor`, `mud-server` and `mud-accounts` fail when the
Rust and Verus bodies drift. CI downloads the pinned Verus release, checks
its SHA-256, and requires `0 errors`.

Proofs prevent forbidden states; tests (unit, property and end to end)
prevent forgotten behavior.
