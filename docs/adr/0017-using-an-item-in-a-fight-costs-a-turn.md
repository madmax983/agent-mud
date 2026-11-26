# ADR 0017: Using an item in a fight costs a turn

Status: accepted

## Context

A balance study of crypt-v01 ran every fight through `World::execute` on
the real engine. It found that `use bandage` and `use draught` draw no mob
round. `attack` and `defend` each draw one: every hostile in the room
strikes the actor once. A heal did not, so a player could drink a draught
in the middle of a fight, at full price in coins but at no price in the
fight. Healing was then not a choice. It was always right, and a fight
with a draught in the pack was a fight with more HP. The owner chose:
make a heal cost a turn, then balance the pack against that rule.

## Decision

- **`[combat] use_draws_strike` in `world.toml`.** With it, a `use` that
  acts takes the user's turn. The item works first. Then the user draws
  the same mob round as `attack` and `defend` (`World::mob_round`): each
  hostile in the user's room that can act strikes it once, and a mob
  never strikes a body an earlier mob of the round dropped. The strike
  lines follow the item's line in the narrative, and the resolution gains
  `attacks`, as `defend` has. Without a hostile in the room the round is
  empty and nothing changes. crypt-v01 sets it.
- **Which uses act.** A heal (on yourself or on someone else), a coin
  pouch, a key and a scripted `on_use` that succeeds each act. Looking
  into a container with `use` is a look, and `use` on gear is `equip`, so
  both stay free. A refused `use` (no such item, no such target, a script
  `refuse`) changes nothing and takes no turn.
- **A heal on someone unhurt is refused.** At full HP and awake, a heal
  would waste the item and, with the key, draw a strike for nothing. The
  engine refuses it before it rolls: the item stays and no turn passes.
- **A use that kills its user ends the turn.** A scripted `on_use` may
  `hurt()` its user to death. The user then wakes at the respawn point,
  and no hostile there strikes the body that just woke. The engine reads
  a death the way the script runner does: a `dead` status or a move of
  the user in the command. The narrative is then the same with the key
  on and off.
- **The resolution's `hp` is the HP after the turn.** A heal reports the
  HP of the one it healed after the round, not before it, so an agent
  that reads `hp` sees its own body as it is.
- **`[combat] foe_memory_minutes` closes the hidden way back.** Walking
  back out is free with `retreat_free`, a heal with no hostile in the
  room is free, `attempt stealth` there is free, and a hidden mover draws
  no strike on entering. Together they made a loop: `e`, `use draught`,
  hide, `w`, and the fight goes on at full HP for no strike at all. With
  the key, a hostile remembers a body it fought (one it struck, or one
  that struck it) for that many minutes. A remembered body that walks in
  hidden is seen: it enters as if it were not hidden, the room sees it
  come in, and each hostile there strikes it once. A stranger who walks
  in hidden is still unseen, and the memory fades after the window. The
  engine reads the memory from `last_attacked_by`, which each strike
  already records, so no new state field is needed. crypt-v01 sets 5
  minutes, the window in which a pacifist counts as provoked. `help` and
  the agent prompt say it in one sentence. Unset means none.
- **A pack key, not an item flag.** The rule is about the turn, not the
  item. One line covers every usable item, a scripted one included, and
  `help` and an agent's prompt can state it in one sentence. A flag per
  item would need every heal marked, and a new item would be free by
  default without anyone choosing it. If a pack later wants a quick item,
  a per-item exception can sit on top of the pack rule.
- **Unset means false.** The `[combat]` table keeps its precedent: a pack
  without the key keeps the rule it was written for, a free `use`.
- **`help` and the agent prompt say it.** `CombatRules::rule` adds one
  sentence when the key is set: "With a hostile in the room, use takes
  your turn as attack and defend do: the item works, then each hostile
  strikes you once. Looking into a container or putting on gear stays
  free." `help` shows it under `fighting:`, and the agent system prompt
  carries it through `WorldPack::body_rules`. Without the key the text
  says nothing about `use`, which stays true.

## Every action, and whether it draws a strike

The table holds with the key set. Only `use` changes with the key.

| Action | Draws a strike |
|---|---|
| `look`, `exits`, `examine`, `inventory`, `sheet`, `who`, `help`, `list`, `board read` | No. They are read-only. |
| `say`, `emote`, `tell`, `board post` | No. Talk is free. |
| `take`, `drop`, `give`, `buy`, `sell` | No. |
| `equip`, `unequip`, and `use` on gear | No. |
| `use` that acts (heal, pouch, key, scripted use) | Yes, with `use_draws_strike`; no without it. |
| `use` on a container | No. It is a look. |
| `defend` | Yes, always. |
| `attempt <skill>` | Yes when the skill is a combat skill or the target is hostile; no otherwise. |
| `move` | Entering a room: each hostile there strikes once, unless the mover is hidden and, with `foe_memory_minutes`, no hostile there fought it in the window. Leaving: refused with a hostile in the room, except the way back with `retreat_free` (ADR 0013), which draws no strike. |
| `flee` | A failed roll draws the round where you stand. A success draws none there, and entering the next room draws that room's strikes. |
| `attack` | The target answers if it is a mob that is not hostile, and the round follows. |
| A custom verb | No mob round. Its script may hurt the actor itself. |

## Consequences

- A heal in a fight is a real choice: it trades a turn of the rats' teeth
  for the HP it gives back. A heal between fights, with no hostile in the
  room, costs what it did.
- A retreat to heal stays open, and it has a price. `e`, any number of
  heals outside, then `w` costs the one strike on walking back in, hidden
  or not, where the same heals inside cost one strike each. A test pins
  that cost. The balance step after this one measures retreat, heal and
  return as a strategy beside healing in the room.
- The balance study measured healing only between fights, so its rows
  stand. The balance step after this one measures mid-fight healing under
  the new rule.
- In a party, each member's `use` draws its own round, as each member's
  `attack` does.
- `take`, `give`, `equip` and `unequip` stay free beside a hostile. Picking
  up a fallen blade and wielding it mid-fight costs no turn. A later
  balance step may put them under the same kind of rule. This one does
  not, because the owner asked for heals, and no study has measured gear
  swaps yet.
- The engine names no item: the key is pack data, and the branch that
  decides is the kind of use (heal, pouch, key, script), not an item id.
- No state field, no new kernel and no config key: the round is the one
  `attack` and `defend` already draw, and the foe memory reads the
  `last_attacked_by` that each strike already writes.
