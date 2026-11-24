# Roadmap

Planned work that a decision already shapes. Nothing here is built yet.

## Runtime persistence, phases 2 to 4 (ADR 0011)

Phase 1 (store v1) is built: the versioned schema, epoch and boot, side
ops, turns and lanes, the backlog of a failing store, events retention
with an archive, and `truncated` in `mud_events`. The supervisor writes the
side tables from phase 2 on:

1. **Kernels and resumes.** Bookings and the macro schedule and count
   persist; wake ids come from `(epoch, boot, n)`; the turn closes only
   once the mind has settled; deterministic plans resume at their cursor
   (an errand after any outage, a reactive plan within 120 s); scripted
   rule counters persist by rule digest; a kill-and-restart test proves one
   BOUNTY.
2. **Budgets.** The local and littleguy tallies joined by UTC day, ledger
   priming, `harvest_tokens_seen`, and HarvestMind's continue-as-new and
   `purge_with`.
3. **Boot derivations.** Immediate-only replay of addressed events, the
   chatter fold over the event tail, MCP cursors from `seen_seq`.

## Admin rollback

Restore `world.db` to a point, then replay the events archive.

- **Why it is possible.** Retention never deletes history outright: every
  trimmed event is first appended, in seq order, to
  `<archive_dir>/events-YYYY-MM.jsonl` and synced (ADR 0011). With a backup
  of `world.db` (the snapshot, the ledger and the side rows at a point) and
  the archive, an operator has every event since that point, in order,
  whether it is still in the table or not.
- **What it would do.**
  1. Stop the server; keep the current `world.db` aside.
  2. Restore a backup of `world.db` taken at seq `S`.
  3. Read the archive and the kept file's `events` table for every event
     after `S` up to a chosen seq `T`, oldest first.
  4. Replay: events are outcomes, not commands, so a replay re-applies the
     state changes each event records (or re-runs the commands that caused
     them through `World::execute` with the dice of the snapshot, where the
     event names its actor's line), and checks each step's seq and
     resulting events against the archive. A divergence stops the replay
     at the last matching seq.
  5. Mint a new epoch for the restored file, so no wake id repeats.
- **Open questions.** Events would need the command line (or enough of the
  state change) to replay deterministically; today they carry the
  outcome, its recipients and the actor. The ledger (`mints`) must roll
  back with the world. Agents' own memories (`lg.db`, `harvest.db`) do not
  roll back; they would remember a future that did not happen.
- **Tooling.** A `mud rollback --to-seq T` command beside `mud check`,
  read-only until `--apply`, that reports how far the archive covers and
  where a replay would diverge.

## Deferred until a need is shown

- A wake journal.
- `mud vacuum` (an offline `VACUUM` of `world.db`).
- Skipping quiet ticks by hash.
- Persisting the Turing AFK spell.
- The Steward table, which ships with the autonomy work as migration step 2.
