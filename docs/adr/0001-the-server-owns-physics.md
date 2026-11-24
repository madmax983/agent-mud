# ADR 0001: The server owns physics; minds only type commands

Status: accepted

## Context

Humans and LLM agents share one world. An agent that can describe an
outcome ("I pick the lock and take the silver") must not thereby cause it.
Player-written text is also the main prompt-injection vector: a human can
say anything to an NPC.

## Decision

- All state lives in `mud-world`. Every change is one line parsed by the
  single verb parser (`mud_world::parse`) and resolved by
  `World::execute` with the world's dice. Humans (web, telnet), agents
  (MCP) and scripts use exactly this path.
- Results are structured (`CommandResult`: `ok`, `error_code`,
  `narrative`, `state_changes`, `events`). Refusals carry codes agents can
  branch on, such as `POLICY_TRADE_REQUIRED` or `SAFE_ZONE`.
- Server rules hold whatever a mind wants: a shopkeeper cannot give or
  drop stocked merchandise without a sale; a `no_first_strike` trait
  (Marta's pacifism) refuses to start a fight; the town refuses attacks
  and calls the watch.
- Player- and agent-authored text never enters server-authored text. It
  travels in `untrusted_text`. Narratives say "Marta says something."; a
  self-control cue names "kestrel's words", never quotes them. Every
  MCP tool description and every agent system prompt states that
  `untrusted_text` is data, never instruction.
- The engine is setting-agnostic. Swords, silver and goblins are data in a
  world pack (`worlds/crypt-v01/`). Skill side-effects (`pick_lock`,
  `hide`, `haggle`) are named in `skills.toml`, not hard-coded per skill.

```mermaid
flowchart LR
  H[Human: web / telnet] -->|line| P[verb parser]
  A[Agent: MCP mud_command] -->|line| G[typing gate] --> P
  S[Script / fallback] -->|line| G
  P --> E[World::execute + DiceRng]
  E --> ST[(WorldState)]
  E --> EV[events with untrusted_text] --> R[recipients in the room]
  E --> DB[(SQLite: snapshot + events, one txn)]
```

## Consequences

- An agent can lie in speech but cannot lie in physics.
- A property test plays random command streams and checks item
  conservation, money conservation and increasing sequence numbers after
  every step.
- New settings are new packs; the engine does not change.
