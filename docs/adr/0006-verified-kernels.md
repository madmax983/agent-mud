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
| `chatter_step` | mud-supervisor | at most `max` agent-only wakes between resets, for every `max` |
| `budget_level` | mud-supervisor | Hard/Soft/Normal exactly at the thresholds, monotonic |
| `typing_delay_ms` | mud-supervisor | no overflow, at least the reaction time, monotonic |
| `conn_step` | mud-server | an open is admitted only under both caps; no underflow; caps hold for every sequence |
| `hook_step` | mud-world | a world hook runs at most `max` consecutive faulting times, for every `max`; tripped is absorbing until a reload; no overflow |
| `tally_admissible` | mud-world | admits exactly the effect lists of at most `max` effects with at most one heal/hurt, at most one reward, and a refuse only alone in a command hook, so a refused command never mints (ADR 0010) |
| `login_route`, `first_ask_of`, `may_bind` | mud-accounts | an agent PC is asked exactly what any human account is asked; only free names create; only open legacy humans claim; refused iff an NPC; nothing binds an agent PC or an NPC (ADR 0009) |
| `throttle_step`, `throttle_allows` | mud-accounts | no overflow; never locked past the cap; free failures never delay; past free at least `min(base, max)`; a quiet spell forgets; checked failures are spaced (`lemma_rate_bound`) (ADR 0009) |
| `pool_admit`, `pool_roll`, `pool_period` | mud-world | a reward ask is admitted whole iff it fits in what is left of its pool; no run of asks takes a pool past its budget, and the count is exactly the admitted asks; a later period starts at zero and the clock set back never refills a pool; no overflow (ADR 0010) |

The `verus_mirror` tests in `mud-rules` (which also guards the mud-world
kernels), `mud-supervisor`, `mud-server` and `mud-accounts` fail when the
Rust and Verus bodies drift. CI downloads the pinned Verus release, checks
its SHA-256, and requires `0 errors`.

Proofs prevent forbidden states; tests (unit, property and end to end)
prevent forgotten behavior.
