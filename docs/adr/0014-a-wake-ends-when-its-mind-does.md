# ADR 0014: A wake ends when its mind does, and owes only what it did not read

Status: accepted

## Context

In the crypt-v01 log that ADR 0012 answers, every wake of kestrel, the
hidden PC, ended `FAILED · 0 tok · timed out` exactly two minutes after it
started. The wake timeout (120 s) counted the time kestrel spent typing:
the gate holds a Turing PC's line as long as a person would take to type
it (ADR 0004), and a long notice holds for most of a minute. The mind
was not hung; it was typing. Each timed-out wake then showed no spend in
the feed, because littleguy's ledger is read when the op ends, after the
wake's line. The next wake replayed, as Immediate, the tells that the
op had already read with `mud_events` and answered. Three smaller faults
sat in the same log: kestrel's own macro-tick was always "deferred: a
player is present", the player being kestrel; a collapse or a death,
which the engine emits with the body as its actor, counted as that
body's agent turn in the anti-chatter counter (or, for a PC, reset the
room as a human act would); and combat that interrupted a held line
queued a second Immediate wake on top of the one still running.

The supervisor already kept an entity busy until its mind settled
(`Mind::settled`, ADR 0011): the next chat waited for the op to be read.
That stays, and is now tested against the littleguy socket protocol.

## Decision

- **A wake's bound leaves out typing (`wake_expired`).** The gate keeps a
  hold clock per entity: the time it has spent with at least one line
  held, overlapping lines counted once, by the tokio clock. A wake is past
  its bound only when the time it ran outside holds reaches it. The
  proved kernel `wake_expired(elapsed, held, timeout)` is
  `elapsed - held >= timeout` (saturating); holds only ever extend the
  bound, and a wake past it stays past it.
- **The bound is per tier.** `[supervisor] wake_timeout_ms` (default 120
  s) and `[supervisor.wake_timeout_ms_by_tier]` with `ambient`,
  `persistent` and `turing` (1 s to 1 h each; a tier left out uses the
  default). The example config gives `turing` 180 s.
- **The next wake owes only what the mind did not read.** `Game::events`
  (`mud_events`, in process and over MCP) records the newest event it
  handed each entity: its read mark, in memory. When a wake's mind has
  settled, the router drops from the entity's waiting wake every event at
  or below the mark (`Router::finish_seen`); its priority and reason fall
  back to what is left, and a wake left with no event and no cause outside
  the event stream (a login, a come-to, a booking, a macro tick, an
  interrupt, a wake given back) is dropped. An event that came after the
  last read still wakes the mind. The close of the turn also stores the
  mark as `seen_seq`, so a boot replay (ADR 0011, phase 4) and the MCP
  cursor after a restart do not hand the read events out again. A crash
  before the close loses the mark, and the boot replays them once: the
  safe direction.
- **An interrupt does not stack a wake.** Combat still drops a held line
  with `INTERRUPTED` and publishes it; it forces an Immediate wake only
  when the PC has no wake running. A running wake reads the result and
  goes on.
- **A PC's macro-tick counts the other players.** The tick is deferred
  only when a player other than the entity itself is in its room
  (`World::player_present_except`).
- **A body's line is the world's.** The engine names its lines about a
  body by verb: `collapse`, `stagger`, `die`, `respawn` (`BODY_VERBS`,
  `Event::is_body_line`). The proved kernel `chatter_input_for` classifies
  every room event for the anti-chatter counter: a body line is `World`
  whoever's body it is (`lemma_a_body_line_never_counts`: it never moves a
  room's count and never stops a wake), a player's act is `Human`, an
  agent-played NPC's act counts by whether a player is there, anything
  else is `World`. The `ChatterInput` enum and `chatter_step` do not
  change.
- **Late spend has a feed line (`ObserverEvent::LateSpend`).** Spend
  charged after its wake's `WakeFinished` line (an op read after a
  timeout, a model call that ended late) is published with the entity,
  the usage and the budget level, and the observer shows
  `late    pc_kestrel  +41900 tok after its wake · budget normal`. The
  budget charged it before too; now the feed shows it.

## Consequences

- A Turing PC that types a 45 s notice under a 30 s bound ends its wake
  ok. A mind that hangs still times out on its tier's bound.
- A littleguy op that reads its tells and answers them is not woken again
  for them, after the wake or after a restart. A mind that never calls
  `mud_events` (a scripted mind reads only its wake's events) is not
  affected.
- The read mark is per process and not a view: no entity sees it, and it
  never reaches `WorldState`.
- Two kernels join the proof (`chatter_input_for`, `wake_expired`); the
  Verus count is 195.
