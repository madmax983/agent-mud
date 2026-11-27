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

An `on_use` that succeeds is a `use` that acts. With `[combat]
use_draws_strike` in `world.toml`, it takes the user's turn beside a
hostile, after the script's lines (ADR 0017). A `refuse` takes no turn.
An `on_use` that kills its user with `hurt()` also ends the turn: no
hostile strikes the body where it wakes.

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
typos. Test with `"key" in view`. `for x in view` walks a list's
elements, or a map's keys (`for k in ctx.flags`); a view has no `keys()`.

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
| `heal(dice)`, `hurt(dice)` | the actor; `1d` to `6d` with adds; at most one per hook; no `hurt` in a safe room. A hook runs only for an actor that can act, so a heal never meets one out cold; the effect still brings to any actor it lifts above 0 HP, with the room line and the wake of its mind that any coming to has (ADR 0012) |
| `reward(pool, #{ currency: n, items: #{ proto: n } })`, `reward(pool, n)` | money and items for the actor from a pack reward pool, recorded in the ledger; at most one per hook, never with `refuse` (see [Rewards](#rewards-ledgered-mints)) |
| `use_up(proto)`, `use_up(proto, n)` | use up n (1 to 16) items of a prototype the actor carries: they leave the world. Never money, contents, merchandise or a hidden item. Before any `reward`. Too few to give declines the whole list (see [Using up items](#using-up-items)) |

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
pack pool with what is left and its running totals. Money in the world is
always the start plus the ledger's currency, and every item made after the
start is a new player's kit or named by the ledger. Items leave the world
only when used up: by the engine (a heal, a purse, a pool sale) or by
`use_up`. The property tests
check exactly that. A reward is not a sale: shops and `POLICY_TRADE_REQUIRED` are
unchanged. A shop can buy with a pool, though (below).

The engine writes to the same ledger without a script: a respawned mob's
kit (`[cast.x] respawn_kit`, ADR 0012) is one row in the pool `@respawn`
(`rewards::RESPAWN_POOL`), with hook `respawn`, period 0, currency 0 and
no budget. No pack can declare that pool, because `@` is not in a pool
id. Its rows reach the `mints` table, `WorldState.minted`, `state_changes`
and the feed as a script's do, and `check_rewards` checks them. The
Scripts panel shows pack pools only, so `@respawn` is not in it.

A shop can buy with a pack pool. `buy_pool = "<pool>"` on a shop of
`economy.toml` pays for what the shop buys from the pool, while the pool
has the price left in its period: the seller gets the price as one ledger
row (hook `shops.<shop>`, currency only, `seq` of the sale's `trade`
event, whose data names the `pool`), and the keeper's purse does not
move. The shop uses up what the pool pays for: the item leaves the world
and never goes on sale, so no item is paid from the pool twice and
selling it back and forth makes no money. Goods that hold something (a
container with contents, a purse with coins) never go to the pool. When
the pool cannot pay, the
keeper's purse pays as it always does, the goods go on sale, and a
keeper short of the price buys nothing. crypt-v01's Torin
buys ore with `ore_bounty`, 60s a day, so the drift's daily ore never
empties his purse.

## Using up items

A hook can take what the actor carries out of the world with
`use_up(proto)` or `use_up(proto, n)`: a hand-in, a price in goods, the
ingredients of a recipe. crypt-v01's `report` uses up the warden's blade,
so each blade proves one kill, once.

What can be used up, checked in `check_effects` before anything applies:

- `n` is 1 to 16; anything else faults (in the script, and again in the
  engine);
- the prototype is in `items.toml`, is not `hidden` (a natural weapon,
  which a script never sees) and has no `currency` (a purse is emptied
  with `use`, not used up); anything else faults;
- `use_up` comes before any `reward` in the list, or the hook faults. So
  a reward's items can never be the price;
- the actor is present, or the hook faults;
- the actor carries at least `n` items of the prototype that are not
  shop merchandise, hold nothing (a container with contents) and hold no
  coins of their own (a dropped purse). Too few is a **decline**, not a
  fault: the world cannot take the list now, so nothing applies, the
  dice are untouched, the breaker does not move, and a verb answers with
  the engine's `Nothing comes of that just now.` Two calls of one
  prototype count together, against what the actor carries before the
  list.

The engine takes spares first and an item in use last, each in inventory
order. An item the actor wields or wears is put away and then used up, at
once and with no turn of its own; the command's own rules decide turns.

The whole list is admitted together, so a hand-in pays only with its
proof, and the proof goes only with its pay: when a guard mark or flag
cannot be kept, or the pool is spent, the item stays. The actor reads
`You give up a notched grave-blade.` (or `3 x chunk of dwarven iron ore`)
at the effect's place in the list, and a `system` event only the actor
perceives carries `proto` and the `items` used up (`id`, `proto`) in its
data. `state_changes` holds an `equipped` change for an item in use, then
an `item` change to nowhere. The observer's effects row lists
`use_up(warden_blade)`.

Using up is not ledgered. Like a drunk draught, an emptied purse or ore
a shop's pool buys, a used-up item leaves the world and makes nothing, so
money stays the start plus the ledger, and only making things is
ledgered.

Check first, and say why with a `msg`: a hand-in that meets a player with
nothing to give should not leave them the bare decline. The dry run's
probe carries only the template kit, so `mud check` reports a bare
`use_up` of anything else.

```rhai
// Torin forges a pick from three chunks of ore and his own stock.
fn on_forge(ctx) {
    let ore = 0;
    for p in ctx.actor.inventory {
        if p == "iron_ore" { ore += 1; }
    }
    if ore < 3 {
        return [msg("Torin shakes his head. \"Three chunks, and I make you a pick.\"")];
    }
    [msg("Torin takes the ore to the forge."),
     use_up("iron_ore", 3),
     reward("forge", #{ items: #{ miners_pick: 1 } })]
}
```

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
without marks, plus `ctx.me.post`: `()`, or `#{ rooms, at_post }` with
`return_to`, `return_title` and `way`, a list of exits such as `["n"]`,
when away from a soft post), `ctx.here` (what `look` shows, with each
exit's `locked`), `ctx.events` (at most 32:
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
chars, at most 5 waiting and 24 a day), for a Persistent or Turing entity
only: an ambient NPC's booking is refused (`no_wakes`, logged), and its
lines still run (ADR 0008). The memory, the booking and the lines are one decision: the memory
and the booking are stored with the first line that takes effect (or when
the wake ends), never without it. If the server dies before that, the wake
comes back and your script decides again from the memory it had before
(ADR 0011).

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
# crypt-v01: 13 rooms, 27 items, 15 skills, 11 cast, 3 shops, 6 reward pools — ok
# scripts rhai-… (11 files: 1 mind, 2 rooms, 2 items, 11 verbs) — ok
```

It compiles, validates and dry-runs every hook once and every mind twice on
probe inputs, and lists every problem. `mud serve` does the same and will
not start on a problem.

## Patterns

These come from crypt-v01's scripts, and each one keeps a rule above.

- **Once per kill.** A world flag holds the death a reward paid for
  (`ctx.cast.<mob>.deaths`), and the reward checks it. A respawned mob
  that dies again has a new count, so it pays again. `report` and
  `search` do this.
- **One try each.** A mark on the player holds the death, the day or
  the count it already used. `search` marks the body a player searched,
  so a failed roll is not tried again, and the next player can try.
- **A count a day.** A mark that holds `day * 100 + n` counts up to `n`
  today and starts at 0 the next day, in one mark (`count_today` and
  `count_mark` in crypt-v01's `lib.rhai`).
- **State that others read.** A hook writes marks on its actor only, and
  `ctx.room.present` shows no marks. So state that a later hook reads for
  someone else goes in a world flag named for that player, such as
  `bones.<id>`. Ids are lowercase words, so the name is valid. Give such a
  flag a day in its value (or a round that holds the day, as `bones`
  does), and sweep stale ones as you go, so the 256 flags last. Put the
  sweep's `clear_flag`s before the new `set_flag`: admission counts the
  list in order, so a full table still takes the new flag.
- **Content of the day.** `day_utc(ctx.now_ms)` through a small hash picks
  the day's room and spot. Every player and every mind sees the same pick
  all day, it changes at midnight of the game clock, and no wall clock
  or new dice are used.
- **Tables in config.** A loot table is data: nested tables under
  `[verbs.config]` in `scripts.toml`. When several bindings share one
  table, it is a function in one file instead, because each binding has
  its own config.
- **A hand-in.** Check `carries(ctx.actor, proto)` (or `ctx.item`)
  first and say why with a `msg` when the player has nothing to give.
  Then put the guards, the `use_up` and the `reward` in one list, with
  `use_up` before `reward`: the proof goes only with the pay. `report`
  does this.
- **Pay what the pool can.** Read `ctx.pools.<pool>` before you ask, and
  say so in a `msg` when the day's budget is spent, as `search`, `dig` and
  `mine` do. The guard flags and marks go in the same list as the reward.

## crypt-v01, walked through

- `lib.rhai`: `warden_here(ctx)`, `done_today(marks, key, now_ms)`,
  `carries(actor, proto)`, `hostile_here(ctx)` (a mob that can fight is
  here), `dark_mod(ctx, dark)` (the darkness modifier of a roll) and the
  counting marks `count_today` and `count_mark`.
- `rooms/ossuary.rhai`: the first visitor is remembered in a flag and the
  room narrates dust falling; the warden turns its head (or scrapes in the
  dark). `pray` heals once a day (a mark holds the day), never while the
  warden stands (`refuse`), and a critical failure hurts.
- `rooms/alley.rhai`: saying "vich" or "fence" in the Dark Alley twitches
  the fence's curtain, without repeating a word; `listen` on the alley or
  the cellar stair hears the rats, the pups, the warden and the crawler
  until they die.
- `rooms/square.rhai`: the watch's bounty. Put the skeletal warden down,
  bring its notched grave-blade to Hale in the square, and `report`: while
  the ossuary is quiet he pays 20s and a healing draught from the
  `warden_bounty` pool (60s and 3 draughts a day), once a day per player.
  Hale keeps the blade: each blade pays once.
  When the pool is spent he says so; when he is away, `report` is refused.
- `items/iron_ore.rhai`: taking ore under the warden's eye can cost blood.
- `items/bone_charm.rhai`: the charm, loot with no use until now, hums near
  the warden and answers `use charm on warden`.
- `rooms/loot.rhai`: `search` in the vault, the warren, the root cellar,
  the ossuary and the drift. The first good searcher of each death (an
  IQ roll, -3 in the dark) rolls once on the mob's loot table and is paid
  from a per-day pool: `vermin_loot` for the rats, the pups and the
  crawler, `warden_hoard` for the warden. A failed roll leaves the body
  for the next player. When the pool is spent, the body yields nothing.
- `rooms/flagon.rhai`: liar's bones in the taproom. `bones` shakes five
  dice that only the shaker sees, eight rounds a day. A cup down stays as
  it fell until the call: a second shake is refused. `call` shows every
  cup of the round at the table, with a tally of each face, and ends the
  round (the flag `bones_round`), so every cup of it goes stale at once.
  A cup whose owner left the table stays out of the count, and the call
  says how many. Alone, `call` tips the caller's dice back. Bids are
  `say` and stakes are `give`.
- `rooms/rumor.rhai`: the rumor of the day. The day picks one of five
  rooms and one of three spots. `eavesdrop` in the taproom (IQ, once a
  day) hints at the room, and a critical failure hints at a wrong one.
  `case` in the backroom (Streetwise, or IQ-1; once a day, with Vich
  there) names the spot. `dig <corner|floor|wall>`, two tries a day, pays
  the first finder 15s and a draught from `rumor_cache`.
- `rooms/drift.rhai`: the old dwarf drift. `clear` in the ossuary (ST,
  with nothing standing there) unlocks the rubble on the stair, which
  locks itself again an hour later. The rubble's lock has
  `pickable = false` in `world.toml`, so `attempt lockpicking` on it is
  refused before it rolls, and `clear` is the only way down. `mine` in
  the drift needs a miner's pick and the crawler down: one HT roll a day
  per player (-3 in the dark), and a success mints one chunk of ore from
  `drift_ore`, four a day in all. Torin's `ore_bounty` buys them.
- `minds/hale.rhai`: Hale (on Rhai through `runtime = "rhai"`) warns
  players off the alley, by night by name, remembers who he warned today,
  and answers talk of the grate. He says it is locked only when he stands
  at it (`config.grate_room`, `config.grate_dir`) and `ctx.here` shows it
  locked, and off his post (`ctx.me.post`) he takes a step back toward it.
  He greets kestrel exactly as he greets a human.
