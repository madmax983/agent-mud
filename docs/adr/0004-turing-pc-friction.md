# ADR 0004: Hidden agent PCs pass by friction, not prose

Status: accepted

## Decision

A Turing PC (`tier = "turing"`, `pc_kestrel` in crypt-v01) is an ordinary
`player` to every view: `look`, `who` and `mud_events` never expose a
tier, a controller or a human flag. Its id has the same shape as a human
login's (`pc_<name>`); the pack check refuses a player entry with any
other id, and `who` sorts by name so its order reveals nothing. The
supervisor adds friction:

- **Sessions.** `active_windows_utc` shift each UTC day by a deterministic
  offset of up to `daily_offset_minutes`. Outside a window the PC logs
  out cleanly; on login it gets an Immediate "login" wake.
- **Selective attention.** On RoomBatch and Ambient events where it is not
  named and no combat or loot happened, it wakes with `wake_chance` (35%),
  always on a goal keyword, and always when someone arrives (people
  notice arrivals; the v0.1 recruiting scene depends on it).
- **Distracted spells.** When idle in a safe room it steps away for 2–6
  minutes every 20–60 minutes. Non-urgent wakes wait for it to return.
- **Typing delay.** Each `mud_command` passes the typing gate. Speech is
  held `reaction(800..2200) + chars/5 words at wpm, ×jitter(0.85..1.15)`
  ms; moves and attacks 600–1400 ms. If combat breaks out around it while
  a line is held, the line is dropped with `INTERRUPTED` and the PC is
  woken at once. `typing_delay_ms` is proved in Verus (no overflow,
  monotonic in length).
- **Style.** The persona asks for lowercase fragments and MUD idioms and
  forbids assistant-isms; the goal is concrete and mildly selfish
  ("never split loot less than 50/50").
- **Blind.** Nothing announces how many hidden PCs exist or whether any
  is online. Only the observer dashboard knows.

All randomness derives from a hash of (seed, entity, moment), so a replay
makes the same choices.
