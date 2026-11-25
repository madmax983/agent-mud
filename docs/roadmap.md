# Roadmap

Planned work that a decision already shapes. Nothing here is built yet.

## Runtime persistence, phases 3 and 4 (ADR 0011)

Phases 1 (store v1) and 2 (kernels and resumes) are built: the versioned
schema, epoch and boot, side ops, turns and lanes, the backlog of a failing
store, events retention with an archive, `truncated` in `mud_events`;
persisted bookings, macro schedule and count, wake ids from
`(epoch, boot, n)`, settle before close, plans that resume at their cursor
(an errand after any outage, a reactive plan within 120 s), rule counters
by digest, and the kill-and-restart test. Next:

1. **Budgets.** The local and littleguy tallies joined by UTC day, ledger
   priming, `harvest_tokens_seen`, and HarvestMind's continue-as-new and
   `purge_with`. A kill with local spend flushed then keeps the budget
   level (the kill test gains that case).
2. **Boot derivations.** Immediate-only replay of addressed events, the
   chatter fold over the event tail, MCP cursors from `seen_seq`.

## Admin rollback

Restore `world.db` to a point, then replay the events archive.

- **Why it is possible.** Retention never deletes history outright: every
  trimmed event is first appended, in seq order, to
  `<archive_dir>/events-YYYY-MM-<epoch>.jsonl` and synced (ADR 0011). The
  epoch names the `world.db` the events came from. With a backup
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
  5. Mint a new epoch for the restored file, so no wake id repeats and
     its events after `S`, which reuse seqs the archive already holds
     under the old epoch, go to new archive files. Without a new epoch,
     retention refuses to trim at the first reused seq whose line differs
     (ADR 0011); it never deletes such an event unarchived.
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
