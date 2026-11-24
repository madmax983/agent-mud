# World scripts

A pack can make rooms, items, verbs and simple NPC minds behave with
[Rhai](https://rhai.rs) scripts, without a recompile. Scripts are **data in,
data out**: a world hook returns effects the engine checks and applies; a
mind script returns command lines. No script can change the world any
other way. A world hook may pay a quest reward in money and items, but
only from a reward pool the pack declares, within its budget, and every
mint is recorded in a ledger (see [Rewards](#rewards-ledgered-mints)). The design and its reasons are in
[ADR 0010](adr/0010-sandboxed-rhai-scripts.md).

## Layout

```
worlds/<pack>/scripts.toml        the manifest: what runs where
worlds/<pack>/scripts/lib.rhai    optional helpers merged into every script
worlds/<pack>/scripts/**/*.rhai   the scripts
```

A pack without `scripts.toml` behaves exactly as before (`mud check` warns
if a `scripts/` folder exists without one).

## `scripts.toml`

```toml
[scripts]
lib = "lib.rhai"                    # optional; a file's own functions win

[limits]                            # optional; pack data, since they change outcomes
max_operations = 10000              # per call, 1000..=10000 (also the work budget)
max_effects = 8                     # per hook call, 1..=16
max_lines_per_wake = 4              # 1..=8
max_consecutive_faults = 3          # 1..=10: then the hook's breaker trips

[minds.guard_01]                    # a cast id: ambient, persistent or turing
file = "minds/hale.rhai"            # defines fn on_wake(ctx)
runtime = "rhai"                    # optional: run on this script whatever [minds] says
memory_schema = 1                   # a change starts this mind's memory fresh
config = { night_from = 20, night_to = 6 }

[rooms.ossuary_crypt]
file = "rooms/ossuary.rhai"
on_enter = true                     # fn on_enter(ctx)
on_speech = false                   # fn on_speech(ctx)

[items.iron_ore]
file = "items/iron_ore.rhai"
on_take = true                      # fn on_take(ctx)
on_use = false                      # fn on_use(ctx): only items with no engine use

[[verbs]]
name = "pray"                       # 2 to 16 lowercase letters or underscores
usage = "pray"                      # shown by help
rooms = ["ossuary_crypt"]           # empty: anywhere
file = "rooms/ossuary.rhai"
fn = "on_pray"                      # default on_<name>
config = { modifier = -2 }
```

Rules the loader checks, all reported at once: known cast ids, rooms and
items; a binding declares at least one hook; `on_use` only for items that
are not a key, container, coin pouch, healing item or gear; a verb never
shadows a built-in verb or alias, never takes a session word (`password`,
`logout`, `quit`: the server reads those before the engine, ADR 0009), is
declared once and names known rooms;
files are relative paths of `[a-z0-9_/-]` ending in `.rhai`, inside the
`scripts/` folder (no symlinks), at most 64 KiB each, 512 KiB and 64 files
in all; `config` holds no floats or dates; every hook function exists and
takes one parameter, `ctx`. **A script file holds functions only**: top-level
statements, `let` or `const` are refused, so constants go in `config`.

## What a hook sees (`ctx`)

| Field | What |
|---|---|
| `ctx.hook` | e.g. `rooms.ossuary_crypt.on_enter`, `items.iron_ore.on_take`, `verbs.pray` |
| `ctx.now_ms` | the game clock |
| `ctx.actor` | `id, name, kind, room, hp, hp_max, condition, attrs{st,dx,iq,ht}, skills{every pack skill: effective level}, inventory[proto], equipped[proto], currency, marks{}` |
| `ctx.room` | `id, title, dark, lit, safe, exits[{dir,to,locked,lockable}], present[{id,name,kind,condition}] (at most 32, not the actor, not hidden), items[proto] (at most 32)` |
| `ctx.from` | the room the actor came from (`on_enter`), else `()` |
| `ctx.text` | the player's words (`on_speech`, a verb's arguments) as `Untrusted`; empty elsewhere |
| `ctx.target` | a verb's arguments, or `use .. on X`, resolved to someone here, else `()` |
| `ctx.item` | `id, proto, name` (`on_take`, `on_use`), else `()` |
| `ctx.flags` | the world flags |
| `ctx.cast` | every NPC and mob: `name, room, alive, present, deaths` (`room` is `()` when hidden, dead or away; `deaths` counts its deaths, so a script tells one kill of a respawning mob from the next) |
| `ctx.pools` | every reward pool of `economy.toml`, with what is left of it in this period: `per` (`day` or `ever`), `currency`, `items{proto: n}` |
| `ctx.config` | the binding's `config` |

`kind` is `player`, `npc` or `mob`: a human and an agent PC are both
`player`, and nothing tells them apart. Views are read-only, and reading a
key that does not exist faults (`no property "x"`), so a dry run catches
typos. Test with `"key" in view`.

## What a hook returns

`()` or an array of effects:

| Effect | Does |
|---|---|
| `msg(text)` | a line for the actor |
| `narrate(text)` | a line for everyone else in the room (a System event; it wakes no one) |
| `refuse(text)` | fail the command with this text: verb and `on_use` hooks only, and alone |
| `unlock(dir)`, `lock(dir)` | an exit of this room with a lock, both sides |
| `set_flag(name, v)`, `clear_flag(name)` | world flags (bool, int or text of at most 64 chars; at most 256 flags: a new flag past that declines the list) |
| `mark(name, v)`, `unmark(name)` | marks on the actor (at most 32: a new mark past that declines the list): per-player state goes here |
| `heal(dice)`, `hurt(dice)` | the actor; `1d` to `6d` with adds; at most one per hook; no `hurt` in a safe room |
| `reward(pool, #{ currency: n, items: #{ proto: n } })`, `reward(pool, n)` | money and items for the actor from a pack reward pool, recorded in the ledger; at most one per hook, never with `refuse` (see [Rewards](#rewards-ledgered-mints)) |

Names are lowercase words of `[a-z0-9_]` joined by `.`, `:` or `-`. Texts
are 1 to 500 printable characters. The list is admitted all-or-nothing: one
bad effect and **nothing** applies, the dice are untouched, and the hook
counts a fault.

A list that is sound but that the world cannot take **now** is
**declined**, not faulted: a reward that fits its pool but not what is left
of this period, a new mark on an actor who already holds 32, or a new flag
past 256 (counted through the list, so an `unmark` or `clear_flag` before
it makes room). Nothing applies, the dice are untouched, the feed shows a
`declined: …` log row, and the breaker does **not** move (proved:
`lemma_declines_never_trip`), so no player can take a hook down by asking
an empty pool again and again. A declined verb or `use` fails with
`Refused` and `Nothing comes of that just now.`; a declined event hook says
nothing and never fails its command. A mark or flag that guards a reward
is admitted with the reward or not at all, so a lost guard never pays.

## Rewards: ledgered mints

A script cannot make money or items out of nothing. A **reward pool** in
the world pack's `economy.toml` is the only source:

```toml
[rewards.warden_bounty]          # an id of 1 to 32 of a-z, 0-9 and _
currency = 60                    # per period, 0 to 1000000
items = { healing_draught = 3 }  # per period, each 1 to 1000; known items
per = "day"                      # "day": refills every UTC day of the game clock; "ever": never
```

A hook asks with `reward(pool, #{ currency, items })` (or `reward(pool, n)`
for money only). The ask is admitted **whole or not at all**:

- at most one `reward` per hook, never with `refuse` (admission, proved in
  Verus);
- the pool exists, offers every item asked for, the ask is not empty,
  makes at most 16 items, never passes a whole period's budget, and the
  actor is present (`check_effects`); anything else is a fault like any
  bad effect;
- it fits in what is left of the pool **for the current period**; if not,
  the hook declines (above): nothing applies, and no fault counts.

So read `ctx.pools.<pool>` first and ask only for what is left; when the
pool is spent, say so with a `msg` instead of leaving the player the
engine's bare decline:

```rhai
let purse = ctx.pools.warden_bounty;
if purse.currency < 20 || purse.items.healing_draught < 1 {
    return [msg("The watch's purse is empty till tomorrow.")];
}
[mark("bounty_day", day_utc(ctx.now_ms)),
 reward("warden_bounty", #{ currency: 20, items: #{ healing_draught: 1 } })]
```

A pool limits everyone together; a once-per-player rule is the script's
job (a `mark`, as above), and a once-per-kill rule too (a flag holding
`ctx.cast.<mob>.deaths`, as crypt-v01's `report` does). Write the guard in
the same list as the reward: they are admitted together, and a guard that
could not be kept (marks or flags full) declines the reward with it. Periods roll on the game clock (`now_ms`), never
the wall clock; setting the clock back never refills a pool.

What a mint does, in the same command: the actor's money goes up, the new
items are made and put in the actor's inventory, the actor reads `You
receive 20s and a healing draught.` (in the command's narrative and as a
`system` event only the actor perceives, with `pool`, `currency` and
`items` in its data), and the state records it:

- the ledger, the store's `mints` table: `seq` of that event, `pool`,
  `period`, `hook`, `entity`, `currency`, every item made (`id`, `proto`),
  `at_ms`, appended in the same transaction as the command's events (a
  mint costs one row there, as its event costs one in `events`);
- `WorldState.pool_spent`: what each pool has given in its period, and
  `WorldState.minted`: each pool's running totals (mints, currency,
  items). The snapshot, saved after every command, holds only these
  bounded counts, never the ledger's rows;
- `state_changes`: `currency`, an `item` change from nowhere, and `minted`.

The observer's feed shows a `minted` row, and the Scripts panel lists each
pool with what is left and its running totals. Money in the world is
always the start plus the ledger's currency, and every item made after the
start is a new player's kit or named by the ledger. The property tests
check exactly that. A reward is not a sale: shops and `POLICY_TRADE_REQUIRED` are
unchanged.

## Natives

| | |
|---|---|
| arrays | `len`, `is_empty`, `push`, `pop`, `+`, `+=`, `x in a`, `for x in a`, `a[i]` |
| maps | `len`, `is_empty`, `"k" in m`, `keys`, `remove`, `m.k`, `m["k"]` |
| strings | `len`, `is_empty`, `to_lower`, `to_upper`, `trim`, `starts_with`, `ends_with`, `contains`, `sub_string(start, len)`, `+`, interpolation `` `${x}` `` |
| dice | `d6()`, `roll("2d+1")`, `below(n)` (1..=1000000), `chance(pct)`, `check(level)` → `#{success, critical, margin, total}` |
| time (UTC) | `hour_utc(ms)`, `day_utc(ms)`, `minute_of_day_utc(ms)` |
| other | `log(text)` (16 lines of 200 chars, observer only), `type_of(x)` |

There are no floats, no `eval`, `print`, `debug` or `this`, no imports, no
closures, no `sleep` or clock, and none of Rhai's standard library beyond
this list (`map`, `filter`, `sort`, `split`, `index_of`, `==` on arrays or
maps are "function not found"). Each call has `max_operations` operations
and as many work units; natives whose cost grows with their input charge
work. Arrays and maps a script builds hold at most 64 entries, strings
1024 bytes, and calls nest 16 deep. A map or array grown past those caps
in place (`m[k] = v` in a loop) is not refused, but every read of a
variable over the caps costs one work unit per entry and per 64 bytes of
string, so keep tables small. A mind's `ctx` with many events is over the
caps: read what you need from it once (`let mem = ctx.memory;`) rather
than copying `ctx` in a loop.

## You can test player words; you can never quote them

`ctx.text`, a mind's `ev.text` and a booked note's `note` are `Untrusted`:

- `t.has_word("vich")`, `t.has_any(["vich", "fence"])`: whole words,
  case-insensitive; search words are 2 to 32 of `[a-z0-9']`.
- `t.pick(["vich", "fence"])`: the first of **your** words that appears, or `()`.
- `t.is_empty()`.
- At most 16 search words per text per call.
- `to_string` gives `[player text]`. `+`, `==`, `!=` with a string fault;
  `msg`, `log`, lines and memory take only your own strings.

## Mind scripts

`fn on_wake(ctx)` sees `ctx.hook`, `ctx.priority` (`immediate`,
`room_batch`, `ambient`, `macro_tick`, `booked`), `ctx.macro_tick`,
`ctx.macro_ticks`, `ctx.login`, `ctx.now_ms`, `ctx.me` (the actor facts,
without marks), `ctx.here` (what `look` shows), `ctx.events` (at most 32:
`seq, kind, verb, tags, room, actor{id,name,kind}|(), target|(), is_self,
at_me, text`), `ctx.booked` (`id, due_ms, note`), `ctx.toml_plan` (the
lines the TOML rules would run), `ctx.memory` (a copy you may change) and
`ctx.config`.

It returns `()`, an array of command lines, or
`#{ lines, memory, wake_me: #{ minutes, note } }`. Lines must parse (custom
verbs included), at most `max_lines_per_wake`; they go through the typing
gate like any mind's. `memory` holds plain data only (bools, integers,
strings of at most 200 chars, lists and maps of at most 64 entries, three
levels, 8 KiB as JSON) and persists across restarts. `wake_me` books a
wake through `mud_wake_me` (1 to 10080 minutes, a note of at most 280
chars).

A mind script runs for an entity whose tier's runtime is `scripted`, or
for any entity whose binding sets `runtime = "rhai"` (unless mud.toml sets
`[scripts] allow_runtime_override = false`). Its TOML rules stay the
fallback: on a fault, or once the breaker trips, the TOML plan runs, and
its `nth` counters move only when it does.

## Determinism

Hooks roll the world's dice (a clone, kept only if the effects apply);
minds roll dice seeded by the entity and the wake. Time is `ctx.now_ms`.
Same scripts, seed, commands and clock: same world, same log.

## Faults, breakers and reload

A runtime error, a limit, a wrong return shape or an inadmissible list is
a fault: nothing applies, and the observer's feed shows a `script` row. A
decline (a pool spent for the period, marks or flags full) is not a fault
and never moves the breaker.
After `max_consecutive_faults` in a row the hook's breaker trips (TRIPPED
on the observer's Scripts panel) until the scripts change or are reloaded.
Reload from the panel, or:

```sh
curl -X POST "http://127.0.0.1:3000/observer/scripts/reload?token=$MUD_ADMIN_TOKEN"
```

200 gives the new version; 422 lists every problem and keeps the old
scripts running (the feed shows a `scripts reload refused` row and the
server logs a warning); with scripts off it is 409. The bundled pack
(`[world] pack` unset) has nothing to read again: its reload re-arms the
same scripts. Every reload reopens every breaker, hook and mind; the
panel shows each mind's faults and TRIPPED too. A reload cannot change which
entities run on Rhai (restart for that). Old memory can fault new code:
bump `memory_schema`, or press **Reset memory** on the panel
(`POST /observer/scripts/memory/{entity}/reset?token=`). A wake already
running when you reset does not save its memory over the reset.

## `mud check`

```sh
cargo run -p mud-server --bin mud -- check worlds/crypt-v01
# crypt-v01: 11 rooms, 19 items, 14 skills, 8 cast, 3 shops, 1 reward pool — ok
# scripts rhai-… (7 files: 1 mind, 2 rooms, 2 items, 3 verbs) — ok
```

It compiles, validates and dry-runs every hook once and every mind twice on
probe inputs, and lists every problem. `mud serve` does the same and will
not start on a problem.

## crypt-v01, walked through

- `lib.rhai`: `warden_here(ctx)`, `done_today(marks, key, now_ms)` and
  `carries(actor, proto)`.
- `rooms/ossuary.rhai`: the first visitor is remembered in a flag and the
  room narrates dust falling; the warden turns its head (or scrapes in the
  dark). `pray` heals once a day (a mark holds the day), never while the
  warden stands (`refuse`), and a critical failure hurts.
- `rooms/alley.rhai`: saying "vich" or "fence" in the Dark Alley twitches
  the fence's curtain, without repeating a word; `listen` on the alley or
  the cellar stair hears the rats and the warden until they die.
- `rooms/square.rhai`: the watch's bounty. Put the skeletal warden down,
  bring its notched grave-blade to Hale in the square, and `report`: while
  the ossuary is quiet he pays 20s and a healing draught from the
  `warden_bounty` pool (60s and 3 draughts a day), once a day per player.
  When the pool is spent he says so; when he is away, `report` is refused.
- `items/iron_ore.rhai`: taking ore under the warden's eye can cost blood.
- `items/bone_charm.rhai`: the charm, loot with no use until now, hums near
  the warden and answers `use charm on warden`.
- `minds/hale.rhai`: Hale (on Rhai through `runtime = "rhai"`) warns
  players off the alley, by night by name, remembers who he warned today,
  and answers talk of the grate. He greets kestrel exactly as he greets a
  human.
