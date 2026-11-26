# ADR 0012: Downed, not limbo

Status: accepted

## Context

In an observer log of crypt-v01, nobody died, but the town stalled. Marta
and Vich, the only sellers of healing, went into the cellar and collapsed
at -3 HP in the wine vault. Mobs never strike a body that does not act,
so they lay there. Regeneration brought them to at about +4 minutes, but
coming to emitted nothing and woke no mind. Their minds did not know how
recovery works and had booked wakes 15 and 20 minutes out. Meanwhile the
router woke them 7 times while they were still unconscious, for about
190k tokens of "I can't lift a finger". The survivors quoted stale HP
figures, because a wake never showed an agent its own body. On top of
that, death was a free full heal at the square, cheaper than collapsing.

## Decision

- **Coming to is a state change, a room line and a wake.** One helper,
  `World::come_to`, clears `unconscious`, records
  `StateChange::Status { status: "unconscious", value: false }` and emits
  a room line ("Marta stirs and comes to.") tagged `came_to`. The line has
  no actor and targets the entity, so the router never skips the entity
  as the event's own actor. The world tick, a heal item (`heal_with`) and
  a script heal (`heal_actor`) all use it. `World::tick` returns a
  `TickOutcome { seqs, came_to, changes }`.
- **The supervisor wakes the mind once.** `Supervisor::step` reads
  `came_to` and calls `Router::came_to`, which forces one Immediate wake
  with the reason `came to`. A heal inside a command reaches the router
  through the line itself. Both name the line's seq, and the router wakes
  an entity once per line. Bystanders get an Ambient wake for the line
  ("so others can rescue, rob or finish the body"); a Gear event that
  targets an agent ("Vich uses the bandage on Marta") is a room batch.
- **Wakes respect consciousness.** While an entity cannot act
  (unconscious, or dead until a respawn), the router holds its waiting
  wake (`Router::due_where`, the proved `wake_admit` kernel): routed
  events, macro ticks and a wake handed back by `run_wake` (an entity that
  collapsed after the router took its wake) merge into it, and nothing is
  dropped. A booking that falls due stays in the wake book, stored, until
  the entity can act. Plan resumes wait too. When the entity comes to, the
  "came to" wake goes out with all of it in one wake: the held events, the
  due bookings with their notes, a merged macro tick. Across a restart the
  bookings come back from `sup_booking`, and the boot replay waives the
  `replay_max_age_ms` window for a recipient that is out cold at boot: its
  wake was held, not idle, so an addressed event after its floor is still
  owed. It waives the window too for an addressed event at or before a
  come-to line of the recipient that is after its floor: the process
  died inside the come-to wake, before its turn committed, and that wake
  carried the event. A room batch or an ambient wake is not replayed for anyone (ADR
  0011, phase 4).
- **Agents know their body.** Each wake message carries a `you:` block
  next to `room:`, cut from `sheet_view` only (HP, FP, wound state,
  unconscious), so it shows nothing the sheet does not and never a tier
  or a mind. The system prompt carries the pack's own rule for a body
  (`WorldPack::body_rules`: the same recovery and death sentences `help`
  gives, so a pack with another period or no come-to roll says so), that
  you are woken when you come to, and to read `mud_sheet` before quoting
  HP. A refused command from an entity out
  cold says what still works (`sheet`, `help`, `who`, `look`) and the
  pack's recovery rule; an unknown word typed out cold (`wake`) gets the
  same hint. `look` works out cold, read-only, as `mud_look` did. `help`
  gains a `recovery:` line with the recovery and death rules.
- **Recovery is pack data (`[recovery]` in `world.toml`).** Every
  `every_secs` (default 60, 1 to 3600) each entity in the world regains
  1 HP up to its maximum, and an unconscious one above 0 HP comes to. With
  `come_to_roll = "<attribute>"` (crypt-v01: `"ht"`), an unconscious one
  still at 0 HP or below but above -HP also rolls that attribute each step,
  with the world's `DiceRng` inside the tick, and comes to on a success.
  The step is the proved `recovery_step` kernel; the roll is its input, and
  the engine rolls only when `recovery_rolls` says the roll can decide the
  step. Each roll is audited (`Audit::ComeToRoll`). A pack without the
  table keeps a 60 second step and no roll.
- **Death costs more than collapse (`[death]`).** A dead player or NPC
  wakes at the respawn room with `respawn_hp_frac` of its maximum HP
  (rounded, at least 1; crypt-v01: 0.34) and full FP. With
  `drop_purse = true`, its purse stays where it fell: one `purse_item`
  (a prototype that carries `currency`) holds exactly what it carried,
  its own amount on the item instance (`Item.currency`, serde default, so
  an older snapshot loads). The coins move, so `total_currency` and the
  ledger checks hold; anyone can take the purse and `use` it. Mobs keep
  their own respawn. A pack without the table keeps the free full heal.

## Consequences

- An unconscious agent spends no tokens; the budget relies on it, and
  `lemma_no_wake_while_held` proves the router never delivers a wake while
  the entity cannot act.
- Downed agents come to in 1 to 4 minutes in crypt-v01 (regeneration
  alone guarantees `1 - hp` minutes; `lemma_regen_brings_to`), and are
  woken within a second of it, not at their bookings.
- A dropped purse gives corpse runs and looting. It is an item in a room
  and is saved with the world.
- Shop stock lived on the keeper's body, and keepers roamed with it;
  ADR 0013 moves the stock into a shop fixture and gives keepers a soft
  post.

## Amended: a respawned mob comes back with its kit

### Context

A balance study of crypt-v01 on the real engine found that the skeletal
warden fights with its grave-blade only once per world. When a mob dies,
`kill` drops its non-hidden kit where it fell. The respawn in
`World::tick` restored HP and the home room, but not the kit. So every
later warden fought bare-handed (brawling at its DX default, thr-1
crushing), and every later warren rat came back without its tail. The
odds the study measured held for the first spawn only.

### Decision

- **A respawned mob gets its starting gear back, made new.** At respawn
  the engine makes each piece of the cast entry's `inventory` that the
  mob no longer carries, from the pack as it is now, and wields or wears
  what `equip` names in a free slot. A hidden natural weapon never falls,
  so it is kept, not made again. An item that holds money (a prototype
  with `currency`) is never made, so money stays conserved. Shop stock
  is not part of a kit: it lives in a fixture (ADR 0013).
- **It is the rule for every mob that respawns, with a pack switch.**
  `[cast.x] respawn_kit` is unset by default, which means yes. A mob
  that respawns is a new one of its kind, and a new one carries what its
  kind carries; a pack should not need a line per mob to say so.
  `respawn_kit = false` keeps a one-off piece one-off (a unique key, a
  named relic). The pack check refuses `respawn_kit` on an entry that is
  not a mob with `respawn_minutes`, because it would mean nothing there.
  The switch is pack data, so the engine names no mob and no item.
- **The saved mob decides whether it respawns; the pack decides what
  it gets.** `kill` and `World::tick` read the entity's own
  `respawn_minutes`, and reconcile copies it from the cast entry at each
  load, so a pack edit reaches a saved mob. A mob that is dead at load
  keeps the timer it died with, and its next death uses the new one. A
  mob that comes back always gets its kit, unless its entry sets
  `respawn_kit = false`, even when the pack has since dropped its
  `respawn_minutes`.
- **The old kit stays where it fell, as loot.** Each death drops one kit
  and each respawn makes one. Loot accrues once per kill, and the mob's
  respawn timer bounds how often that happens (the warden: one blade per
  two hours at most). Anyone may take it, sell it or wield it.
- **The kit is a ledgered mint.** It goes into the same ledger as a
  script's reward (ADR 0010), under the engine's own pool `@respawn`
  (`rewards::RESPAWN_POOL`). `@` is not in a pool id, so no pack can
  declare it. The pool has no budget and no period (period 0). One
  respawn with something to make is one `MintRecord`: its `seq` is the
  respawn line, its `hook` is `respawn`, its entity is the mob, and its
  currency is 0. The tick's changes carry an `item` change from nowhere
  for each piece, an `equipped` change for each slot filled and a
  `minted` change; the observer feed gets an `Audit::Minted` entry; the
  snapshot counts it in `WorldState.minted`. So "items are conserved
  except for ledgered mints" still holds, and `check_rewards` checks the
  respawn rows as it checks a script's.
- **Determinism.** The mint reads no dice and no clock: it makes the
  missing pieces in the order the entry lists them, with `next_item`
  ids from the state. A restart between the death and the respawn makes
  the same kit with the same ids.

### Consequences

- A second warden fights with its blade, so the study's warden odds hold
  for every spawn. Each kill of the warden leaves a blade to loot.
- This step does not fix Hale's bounty. Its proof (`report` in the
  square) is any warden blade, and the report does not take the blade.
  Before this step only one blade existed, so at most one player could
  claim a kill someone else made. Now every past killer holds a blade
  and can claim each later death, once a day, before the real killer
  walks back. The payouts do not grow: each death still pays once, and
  the pool caps the day. Making the proof single-use (the report takes
  the blade, or only the killer is paid) needs a script effect that
  crypt-v01 does not have yet. It belongs to a later balance or content
  step.
- giant_rat_2 comes back with a tail, so tails come back every 30
  minutes, one per kill.
- The ledger grows by one row per respawn that makes something, as it
  grows by one row per scripted reward.
