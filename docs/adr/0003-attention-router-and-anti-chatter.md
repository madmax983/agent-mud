# ADR 0003: Event-driven wakes, anti-chatter, and budget breakers

Status: accepted

## Context

Tokens cost money. Agents must not free-spin in empty rooms, and two
agents must not talk to each other all night.

## Decision

The attention router (`mud_supervisor::router`) reads every world event
and decides, per recipient agent, whether and when to wake it.

| Priority  | Triggers | Delay |
|-----------|----------|-------|
| Immediate | attacked, a tell, a self-control nudge, speech naming it, a trade or attempt aimed at it | 0–500 ms, no cooldown, no AFK hold |
| RoomBatch | any player action in the room | rolling 2 s window, capped at 4 s from the first |
| Ambient   | NPC or world activity | 5–10 s, plus the entity's cooldown |
| MacroTick | the slow timer (2–6 h, jittered) | only when no player is in the room |

A **player** is any player character (`kind = player`), whoever plays it:
a human, or an agent PC such as kestrel. Routing, anti-chatter and
scripts (`who = "human"`, or its synonym `"player"`) read
`Event::player_actor` and `World::player_present`, never `human_actor`.
Otherwise NPCs would fall silent around an agent PC after two exchanges,
which a human player never sees: a tell that gives the agent away.

Wakes for one entity coalesce; the highest priority wins. One wake runs
per entity at a time.

**Anti-chatter.** Each room counts NPC events with no player present.
A player event or a macro-tick resets it. An agent-only event may wake
other agents only while the count before it is under
`max_agent_only_turns` (2). Past that, every cross-agent wake in the room
is dropped, even if the agents name each other. The kernel
`chatter_step` is proved in Verus: from any start, at most `max`
agent-only events are allowed between resets, for every `max`. The proof
found a hole in an earlier rule (`next <= max` let everything through at
`max = 255`).

**Budgets.** Each agent has a daily token budget (UTC days):
`[supervisor] daily_token_budget`, or its own entry in
`[supervisor.budgets]` by entity id; `0` means no limit in both. At 80% it
goes soft: cooldowns double and ambient wakes stop. At 100% it goes hard:
until midnight the deterministic fallback answers direct interactions
with canned flavor ("Torin grunts and keeps hammering.") and no model is
called. `budget_level` is proved in Verus.

```mermaid
stateDiagram-v2
  [*] --> Normal
  Normal --> Soft: spent >= 80%
  Soft --> Hard: spent >= 100%
  Normal --> Hard: spent >= 100%
  Soft --> Normal: UTC midnight
  Hard --> Normal: UTC midnight
```

## Consequences

- An empty world costs nothing but macro-ticks.
- The observer dashboard shows every routing decision and why a wake was
  dropped.
