# ADR 0013: Shops keep their stock, and a retreat is a walk

Status: accepted

## Context

ADR 0012 stopped the limbo of a downed keeper, but not its cause. In the
same crypt-v01 log the town's only sellers of healing, Marta and Vich,
walked into the cellar, and their shops went with them: the stock lived on
the keeper's body, and a sale needed the keeper present. When they
collapsed in the wine vault, the Flagon's ale and draught lay on the
floor of the vault with them. The cellar itself was a loop: every
arrival draws a strike from each hostile, walking out was refused, and a
bare `flee` took the first exit in `BTreeMap` order, which from the wine
vault is south, to the warden. Torin went in unarmed, and Hale said the
grate was locked without looking.

The owner decided (Q3): no engine leash, since a scripted keeper already is
one. Keepers may roam; the stock lives in the shop; a soft post and more
goals pull them home. And (Q4): walking back out the way you came in is a
normal move; any other exit needs `flee`.

## Decision

- **A shop fixture holds the stock (`[shops.x] fixture`).** The fixture is
  an item prototype that is a fixed, unlocked container. The world makes
  one in the shop room (`room`, by default the keeper's starting room)
  and puts the shop's stock in it, marked `stock`. `WorldState.shop_fixtures`
  maps each shop to its fixture item (serde default). Nobody, the keeper
  included, takes stock out of a fixture (`POLICY_TRADE_REQUIRED`): it
  leaves only through `buy`. Goods sold to the shop go into the fixture as
  stock, with the price paid, so the keeper never resells for less.
- **A sale needs the keeper there.** `list`, `buy` and `sell` find the shop
  by room: a fixture shop trades in its room while its keeper can act
  there; with `unattended = true` it also sells with nobody minding it (an
  honesty box), and the coins still go to the keeper. Buying from
  customers always needs the keeper. A shop is named by its keeper or its
  fixture (`buy ale from shelf`). A shop with no fixture keeps the old
  rule: stock on the keeper's body, trading wherever the keeper stands.
- **Older saves migrate on load.** `WorldState::reconcile` builds each
  missing fixture and moves the stock a keeper carries into it, so no
  item is made or lost; a keeper new to the save gets the pack's stock in
  its fixture. A fixture the pack no longer names goes: its stock moves to
  the new fixture or back onto the keeper's body, and the stock of a shop
  the pack dropped stays on the floor as plain goods.
- **A soft post (`post = { rooms = [...] }` on a cast entry).** Nothing
  holds the entity there. Its own sheet shows `post` (the rooms, whether it
  is at its post, and, away from it, the room to go back to and the way,
  by `World::way_to`, a breadth-first search over the pack's exits in
  display order). An agent's macro-tick cue then reads "You are away from
  your post; return to The Rusty Flagon (the way: e, u, n, e). Then
  advance your goals." A Rhai mind reads the same as `ctx.me.post`.
- **A retreat is a walk (`[combat] retreat_free`).** Each entity records
  `came_from`, the room it last walked in from (a room, not a direction,
  because exits are not always symmetric; serde default; no view shows
  it). With `retreat_free`, walking back to `came_from` is a normal move
  with no flee roll and no strike, even with a hostile in the room; any
  other exit still needs `flee`. The strike on entering a lair stays. A
  bare `flee` runs back to `came_from`, else to a room with no hostile,
  else to the first open exit; it never takes a locked exit.
- **Mobs leave a body alone.** In a mob round, each mob strikes only a
  victim that can still act, so a second mob never strikes the body the
  first one dropped.
- **A lock can lock itself again (`relock_minutes` on a room lock).** The
  tick locks the exit, on both sides, that many minutes after it was
  opened (`RoomState.unlocked_at`, serde default; an exit open in an older
  save starts its clock at the first tick), and tells the room.
- **Content.** crypt-v01 gives Marta, Torin and Vich fixtures (the shelf
  behind the bar, the weapon rack, the crate of oddments), posts, and
  goals that keep them in their shops. Torin wields a broadsword and wears
  a jerkin. The player template and kestrel carry a lit torch. Hale's
  mind says "locked" only when he stands at the grate and `ctx.here`
  shows it locked, and he steps back toward his post. The grate re-locks
  after 30 minutes; from below, the way up is never locked.

## Consequences

- A keeper who adventures is still fun, and the shop no longer vanishes
  with her: when Marta collapses in the wine vault, the Flagon's stock is
  on its shelf, and it sells again once she walks home.
- The engine names no shop, item or room: fixtures, posts, retreat and
  relocking are all pack data. Every new state field has a serde default,
  so an older snapshot loads.
- `help` gains a `fighting:` line, and the agent system prompt carries the
  same rule through `WorldPack::body_rules`.
- No new verified kernel: the rules are lookups and a walk, not arithmetic
  a proof would add to.
