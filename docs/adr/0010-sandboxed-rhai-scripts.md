# ADR 0010: Sandboxed Rhai scripts: data in, data out

Status: accepted (v0.2). Ledgered rewards: proposed, on the roadmap
(see "Future: ledgered rewards").

## Context

- ADR 0001: the server owns physics. Every state change is a parsed line
  resolved by `World::execute`.
- ADR 0004: tiers never leak. Nothing a player, an agent or a view sees may
  tell a human from an agent PC.
- ADR 0008: agents book wakes, and their notes may quote players.
- Builders want rooms, items, verbs and simple NPCs to behave without a
  recompile. TOML `script` rules (`cast.toml`) and `SkillEffect` cover a
  little of that; anything conditional needs code.

## Decision

Packs may ship Rhai scripts (`worlds/<pack>/scripts.toml` and
`scripts/**/*.rhai`). Scripts are **data in, data out**: no script holds
the world, the state or a writable view.

```mermaid
flowchart LR
  L[line] --> P[parse_with: built-ins win, then custom verbs]
  P --> X[World::execute]
  X -->|custom verb / use / enter / say / take| H[WorldHooks::call]
  H -->|read-only HookInput + cloned world dice| R[mud-script: Rhai sandbox]
  R -->|Vec of ScriptEffect + advanced dice| A{tally_admissible<br/>Verus-proved}
  A -->|no| F[fault: nothing applied, no dice used, breaker steps]
  A -->|yes| C{check_effects}
  C -->|no| F
  C -->|yes| E[engine helpers apply effects in the same command]
  W[mind wake] --> M[Rhai on_wake: lines, memory, wake_me]
  M --> G[typing gate] --> P
```

### Two hook families

- **World hooks** (a room's `on_enter`/`on_speech`, an item's
  `on_take`/`on_use`, a custom verb) return a closed list of effects:
  `msg`, `narrate`, `refuse`, `unlock`, `lock`, `set_flag`, `clear_flag`,
  `mark`, `unmark`, `heal`, `hurt`. The engine admits the list
  all-or-nothing through `tally_step`/`tally_admissible` (at most
  `max_effects`, at most one heal/hurt, a `refuse` only alone and only in
  a verb or `on_use` hook), then `check_effects` validates each effect
  against the world (texts 1 to 500 printable characters, a lock on that
  exit, valid flag names, no `hurt` in a safe room, the actor present).
  Only then does it apply them with its own helpers, in the same `Ctx`,
  `CommandResult`, events and commit.
- **Mind scripts** return command lines (and optionally memory and a
  `wake_me`). The lines go through the typing gate, `parse_with` and
  `World::execute` like any mind's. A mind script that calls an effect
  constructor faults.

### Hook points

At most one hook per command, and nothing a hook does sets off another.
Custom verbs and `use` of an item with no engine use decide the command
(`REFUSED`, `SCRIPT_FAILED`, `NOT_HERE`). Enter, say and take react after a
command that worked; their faults never fail it. A hidden mover sets off
no `on_enter`, as it emits no arrival. Login, respawn, ticks and effects
set off nothing. A scripted death skips every remaining effect but `msg`.

### The sandbox (`mud-script`, the only crate with rhai)

- `rhai = "=1.27.0"` with `std, sync, no_float, no_time, no_module,
  no_custom_syntax, only_i64`. Never `unchecked`: it compiles out the
  `set_max_*` calls, so the build fails instead of running unbounded.
- `Engine::new_raw` with only the arithmetic (checked), logic and range
  iterator packages. Never `Engine::new` or `StandardPackage`:
  `LanguageCorePackage` carries `sleep`, and the std collection packages
  hold natives that cost far more than one operation each (deep map
  equality, `map` over a closure). A curated set replaces them: array,
  map and string basics, effect constructors, dice, `log`, UTC time.
  `eval`, `print`, `debug` are disabled; closures are off.
- Fixed limits: 16 call levels, expression depth 64/32, strings 1024
  bytes, arrays and maps 64 (aggregate when built), 128 variables, 256
  functions. Pack limits: `max_operations` (1 000 to 10 000), which is
  also the budget of a deterministic **work counter** that natives whose
  cost grows with their input charge. No wall clock, no `on_progress`.
- Rhai checks the size of a map it builds or a function returns, not of
  one grown in place by index assignment, and a copy (`let b = m`, a
  function argument) costs one operation whatever its size. So the
  sandbox's `on_var` hook charges every read of a variable whose value is
  over the caps one work unit per entry and per 64 string bytes; reads
  within the caps are free. `this` is disabled, since a method call binds
  it without a variable read. A map of 300 arrays copied in a loop, which
  took 0.8 s per call before, now runs out of work in under 1 ms.
  `tests/cost.rs` runs the hostile loops at the `max_operations`
  ceiling; the worst, a copy loop over a 63-entry map, takes about 10 ms
  in a release build (CI asserts under 25 ms). The ceiling was 20 000
  until that loop measured about 30 ms there. The shipped hooks run in
  about 20 µs.
- Views (`View`) are O(1)-clone, read-only JSON views; reading a missing
  key faults, so a dry run finds typos. Top-level code is refused: a
  script file is functions only. Every call runs under `catch_unwind`.

### Player text: `Untrusted`

Player text (speech, a verb's arguments) and booked-wake notes reach a
script only as `Untrusted`. **A script cannot quote player text.** It can
learn only whether each of at most 16 dictionary words of its own choosing
(2 to 32 of `[a-z0-9']`, written in the pack) appears in one text, per
call (`has_word`, `has_any`, `pick`, which returns the script's own
word). `to_string` gives `[player text]`; `+`, `==` and `!=` with a string
fault; effects, `log`, lines and memory accept only strings. Substring
tests and a length were removed: with them a script could rebuild a text
character by character within its budget.

### Determinism

Hooks get a **clone** of the world dice; the world takes the advanced
clone back only when the effects are admitted and valid, so a faulted or
panicking hook consumes no dice. Mind dice are
`mix(seed, entity, wake_id, "rhai")`. Time comes in as `now_ms`. Budgets
are operation and work counts. Same pack, script versions per seq, seed,
commands and clock give the same snapshot and event log (tested).

### Views carry `kind` only

`HookInput`, actor and cast facts, mind events and the mind ctx carry
`kind` (`player`, `npc`, `mob`), never `tier`, `human`, a runtime or a
mind. `ctx.cast` lists only NPCs and mobs. Hale's shipped script greets
kestrel exactly as a human (tested).

### Breakers and admission, proved

`hook_step`/`hook_allowed` and `tally_step`/`tally_admissible` live in
`crates/mud-world/src/hooks.rs`, mirrored in `formal/verus/src/hooks.rs`
(ADR 0006): a hook runs at most `max` consecutive faulting times for every
`max`, stays tripped until the scripts change or are reloaded, and the
saturating tally admits exactly the lists the plain rule admits
(`max <= 16`). Hook breakers persist in `WorldState.hook_faults`; mind
breakers live in the supervisor and reset on any new version or reload.

### Reload, versions, records

The version is `rhai-` plus FNV-1a-64 over every script byte. `set_hooks`
records it in `WorldState.scripts_version` and a marker event no entity
perceives, at start (when it changed) and on every reload, so history says
which version ran from which seq. The store does not keep script sources;
an operator who keeps the pack in git matches the hash to a commit.
`POST /observer/scripts/reload` compiles, validates and dry-runs off the
async workers and swaps between two commands; every successful reload
reopens every hook breaker. A reload that would change which entities run
on Rhai is refused: that needs a restart.

### Runtime opt-in, and TOML as the net

A mind binding may set `runtime = "rhai"`; the server then runs that
entity on its script whatever `[minds]` maps its tier to
(`[scripts] allow_runtime_override = false` vetoes it). crypt-v01 does so
for Hale, so in the owner's live configuration he is a free, deterministic
guard instead of a plugin-agent call. TOML rules stay the fallback: the
TOML plan is computed dry, and its `nth` counters move only when it runs.

### Pack data, and the proposed repository rule

Limits are pack data (`[limits]` in `scripts.toml`) because they change
outcomes. Proposed `CLAUDE.md` rule, for the owner to adopt: "Scripts are
data in, data out: world hooks return `ScriptEffect`s the engine admits
and validates; mind scripts return lines. rhai lives only in mud-script,
on `Engine::new_raw` with the curated natives, never `Engine::new()` or
`StandardPackage`."

## Alternatives rejected

- **Lua or WASM.** Lua needs C and its own sandboxing story; WASM needs a
  toolchain per builder. Rhai is pure Rust, embeds in a few lines, and
  compiles the dangerous parts out.
- **Scripts that call `execute`.** A side door around physics, and a
  recursion hazard.
- **A raw substring test on player text.** It lets a script reconstruct
  the text.
- **`StandardPackage`.** `sleep`, and natives with unbounded per-operation
  cost.
- **Wall-clock timeouts.** Not deterministic.
- **A plain-map ctx.** Cloned per access: O(n) per operation.
- **Placeholder dice.** The world never hands its dice out; it lends a
  clone.
- **Effect trees, cascades and ticks.** One hook per command keeps cost and
  reasoning bounded.
- **Item and money creation (for v0.2).** See below.
- **A `by_human` flag.** It would leak tiers.
- **Storing sources for replay.** Deferred.

## Future: ledgered rewards (roadmap)

The owner asked for quest rewards as a future roadmap item. v0.2 scripts
mint nothing: random play tests that money and items are conserved and no
item id is ever made by a script. When rewards land, they follow this
design, in one PR with its proof:

- **A pack-declared source.** `economy.toml` gains `[rewards.<pool>]`:
  `currency = 50`, `items = { bone_charm = 2 }`, `per = "day"` (UTC day
  of `now_ms`) or `"ever"`. A pool is physics, so it lives in the world
  pack, not in `scripts.toml`; the pack validator checks the item ids.
- **One engine-validated effect.** `reward(pool, #{ currency, items })`
  builds `ScriptEffect::Reward { pool, currency, items }` of a new class
  `Mint`. Admission allows at most one mint per hook, never alongside
  `refuse`. `check_effects` refuses an unknown pool, an item the pool does
  not offer, an absent actor, and any ask past what is left of the pool's
  budget for the period: never past it, and never partly.
- **Every mint recorded.** `WorldState.mints` (a ledger: seq, pool,
  entity, currency, the item ids and prototypes made, `at_ms`) and
  `WorldState.pool_spent` (by pool and period), both `serde(default)`; a
  `StateChange::Minted`, an event with the reward in `data`, and an
  `Audit::Minted` for the observer. Items are made with `new_item` and
  attached to the actor; the ledger names every id.
- **Conservation, counted.** The property tests change from "conserved" to
  "conserved except for ledgered mints, which they count exactly": total
  money equals the start plus the ledger's currency; every item id above
  the start's `next_item` was made by a login, a shop restock or a ledger
  entry, and each ledger item exists once or was used up; a pool's
  `spent` never passes its budget.
- **The proof.** The admission tally gains a `mints` count
  (`tally_admissible` adds `t.mints <= 1 && (t.mints == 0 || t.refuses ==
  0)`), and a new mirrored kernel `pool_admit(spent, budget, ask) -> (bool,
  u64)` proves that an admitted ask never takes `spent` past `budget`, for
  every sequence of asks in a period, without overflow. Both sides change
  in one commit; `PAIRS`, ADR 0006 and `formal/verus/README.md` list it.
- **Untouched rules.** Rewards never take player text; the amounts are
  pack integers or script integers within the pool. A reward is not a
  sale: shops and the `PolicyTradeRequired` rule are unchanged.

## Consequences

- Per command at most one hook, bounded by the operation and work limits;
  mind and reload CPU runs on blocking threads.
- A faulting hook is visible in the observer (feed and Scripts panel) and
  trips after `max_consecutive_faults`; the Reload button reopens it.
- History is keyed by script version.
- Rhai minds run live where a binding opts in; without one, nothing
  changes for LLM-run entities.
- `mud check`, `mud serve` and every reload compile, validate and dry-run
  every script, and report every problem at once.
