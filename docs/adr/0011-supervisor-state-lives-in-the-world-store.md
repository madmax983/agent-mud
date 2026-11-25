# ADR 0011: Supervisor state lives in the world store

Status: accepted. Built in four phases, one pull request each; this ADR
covers all four and marks what each adds. **Phase 1 (store v1) is built:**
the versioned schema and its migration runner, the epoch and boot numbers,
the side rows and their proved merges, the turn lanes in `Game`, the capped
backlog of a failing store, the full reload of the event log, events
retention with an archive, and `truncated` in `mud_events`. **Phase 2
(kernels and resumes) is built:** persisted bookings, the macro schedule
and count, wake ids from `(epoch, boot, n)`, settle before close, plans
that resume at their cursor, rule counters by digest, the boot reconcile,
the proved kernels in `crates/mud-supervisor/src/persist.rs`, and the
kill-and-restart test. Phases 3 (budgets) and 4 (boot derivations) are
not built yet.

## Context

- **What a restart loses today.** The world survives a crash: one writer
  (the world mutex), one transaction per command, dice that resume from the
  snapshot, an append-only `events` table. The supervisor does not. Its
  bookings (ADR 0008), macro-tick schedule and count, the progress of a
  scripted plan, the scripted rule counters and the daily budgets live in
  memory. A restart therefore posts torin's BOUNTY again, fires every
  agent's first macro-tick at once, resets budgets and drops bookings.
- **Wake ids.** `wake-{entity}-{last seq}-{now_ms}` never repeats while the
  clock moves forward, but it says nothing useful after a crash, and a world
  reset with a kept `lg.db` or `harvest.db` can repeat one: littleguy
  answers `duplicate` for a seen id, and harvest dedupes a key across the
  chain of runs of a workflow.
- **The owner's idea, and warning.** "Run the whole MUD as a harvest
  workflow" was prototyped. History would bloat unless the workflow
  continues as new and purges, and with both it still costs 1.6 to 5 ms per
  drive (against about 80 µs for `Game::execute` plus a save), serializes
  every command, tick and read behind one owner, and re-runs an in-flight
  turn after a crash in a room that has moved on.

## Decision

### The world stays on `Store`, and the supervisor's rows join it

`world.db`, through the one `Store` connection behind the same lock, holds
the supervisor's durable state next to the world. That is the only place
where a wake's obligations and each line of a deterministic plan can commit
**in the same transaction as the world effect they caused**: WAL is not
atomic across attached files. With a separate `supervisor.db` or JSON
files one must consume before the act (a crash loses the obligation) or
after it (a crash repeats the act: a second BOUNTY). `accounts.db` stays
separate (ADR 0009): it has another secret and backup profile.

### Schema versions (phase 1)

- The file carries `PRAGMA user_version`. `Store::open` runs the missing
  steps of `MIGRATIONS` (step `i` takes version `i` to `i + 1`), in one
  `BEGIN IMMEDIATE` transaction; a step, once shipped, never changes. A
  file newer than the code is refused (`StoreError::Schema { found }`),
  before anything is written to it, and the server does not start.
- Version 0 is the file before versions. Step 1 re-creates every table it
  had (`snapshot`, `events`, `mints`, `script_memory`, their indexes; all
  `IF NOT EXISTS`, so a version 0 file upgrades in place) and adds the side
  tables: `sup_meta`, `sup_entity`, `sup_booking`, `sup_plan`,
  `sup_rule_count`. The Steward of the autonomy work adds its table as
  step 2.
- `busy_timeout` is 1 s, so a second process (the account CLI) waits for
  the server's transactions instead of failing.

### Epoch and boot (phase 1)

In the same opening transaction, run with `synchronous = FULL`:

- `epoch` is minted once per `world.db`, as `mix(seed, ["epoch", now_ms])`
  with `now_ms` passed in (no wall clock in the engine);
- `boot` is raised by one.

An in-memory world mints its epoch per process and has boot 0. From phase
2, wake ids are `wake-{epoch:016x}-{boot}-{entity}-{n}` with `n` a per-boot
counter: no id repeats across restarts (boot grows), across a world reset
(a new epoch), or across in-memory runs, and no write per wake is needed.

### Side ops: plain data, idempotent, monotone (phase 1)

`mud_world::side::SideOp` is plain data. The store applies each op as an
SQL upsert whose merge is idempotent and, where the row means a count or a
position, monotone: a retried save, two ops landing in either order, or the
newest ops lost in an OS crash all leave a row the supervisor can trust.
The merges are proved kernels (`macro_queue`, `macro_consume`, `dated_max`;
ADR 0006); `SideState::apply` folds them in memory and a property test
checks that the SQL agrees with it, in any order and applied twice. Side
rows never enter `WorldState`: they are not physics, and no view shows them.

| Op | Merge |
|---|---|
| `MacroSchedule` | sets the next macro tick |
| `MacroQueued` | `queued = max`; sets the next tick |
| `Consume` | `macro_ticks = max(.., macro_no)`, `macro_queued` and `macro_cleared = max(.., cover)`, deletes the bookings it carried (only the entity's own), `seen_seq = max` |
| `PlanSet` | starts a plan; a plan of the same wake keeps its cursor |
| `PlanCursor` | `cursor = max`, only for the plan's own wake id |
| `PlanDone` | deletes the plan, only of that wake id |
| `RuleCounts` | `count = max` |
| `Spend`, `Booked` | the dated join: the later UTC day wins; the same day takes the larger value; `harvest_tokens_seen = max` |
| `BookingInsert` / `BookingDelete` | insert once by id / delete |
| `MetaMax` | raises `next_booking_id` (never `epoch` or `boot`) |
| `SeenSeq` | `max` |
| `DropEntity` | deletes every row of an entity the pack dropped |

### Turns and lanes (phase 1 in `Game`; the supervisor uses them from phase 2)

`Game`'s store is a `StoreCell` behind one leaf lock: the store, the open
turns (`entity → { wake_id, staged ops }`) and the backlog of a failed save.

1. `open_turn(entity, wake_id, ops)` records a wake's turn with its
   `Consume` staged. One turn per entity (the router's `busy` rule); a new
   turn replaces an old one, whose obligations then stay unconsumed.
2. `stage(entity, wake_id, ops)` adds to the turn only if the wake id
   matches, else `StaleTurn` and nothing is staged. Minds stage
   `PlanSet`, `PlanCursor` and `RuleCounts`.
3. **The effectful-drain rule.** `Game::execute(actor, line)` drains
   `actor`'s staged ops into the command's own transaction only when the
   command is effectful: it parsed, is not read-only, and worked. A look, a
   typo or a refusal leaves them staged. `login`, `tick`, `bring_online`,
   `take_offline` and every `Game::write` drain nothing: they pass no
   actor. Humans have no turn.
4. `act(ops)` takes the world lock, then the cell, and writes in one
   transaction with `synchronous = FULL` (with a snapshot and the backlog
   when one waits). On failure it returns `Err` and keeps nothing of `ops`,
   now or later: `mud_wake_me` answers ok only after `Ok`.
5. `close_turn(entity, wake_id, ops)` is an `act` that also drains what the
   turn still has staged (a mind that never acted consumes at close) and
   forgets the turn. Another wake's id is refused (`StaleTurn`) and writes
   nothing.
6. `persist(ops)` takes the cell only. It writes at once when no world
   change waits, else it defers behind the backlog (`Durability::Deferred`).
   Only ops whose loss is a safe regression use it: a macro schedule, a
   late spend, boot-time cleanups. Nothing that precedes an external effect
   uses it.

**A failing store.** A failed save keeps its events, ledger rows and ops in
the backlog, and the next commit (or `act`) retries them all, with a fresh
snapshot, in one transaction. The backlog keeps at most `EVENT_LOG_CAP`
events: past that the oldest are dropped with `Audit::StoreBacklog`
(`pending_overflow`, proved). Ledger rows and ops are never dropped. While
a backlog waits, `act` fails as long as the store does, so no booking is
acknowledged and no wake opens; `persist` never writes ahead of a waiting
snapshot. Before this, a failed save lost its events.

**Lock order** (amends ADR 0002): `world → (router | state | wakebook, one
at a time) → store cell`. `open_turn`, `stage`, `persist` and `retain` take
only the leaf; `act` and `close_turn` take the world first, so they are
never called while a supervisor lock is held.

### Wake lifecycle (phase 2)

```text
take:   WakeBook::take_due marks booking ids in flight (rows stay)
open:   readiness and budget first; at Hard the autonomy requeue runs before
        Consume is built, so `booked` is what the chosen mind will see
        macro_no = (wake.macro_tick && queued > cleared) ? macro_ticks + 1 : 0
        open_turn(e, wake_id, [Consume{macro_no, cover = queued, booked, seen_seq}])
run:    effectful commands by e drain the staged ops
        Scripted / Rhai: stage(PlanSet), then for each line i:
        stage(PlanCursor{i + 1}), then tools.command(line)
settle: the turn ends when the mind is really done (littleguy: the charged
        op resolved or its grace over; harvest: the job finished)
close:  close_turn(e, wake_id, [Spend, PlanDone]); router.finish(e)
```

As built in phase 2 (`Supervisor::run_wake`; `Spend` joins the close in
phase 3):

- **Open.** `macro_open(row, wake.macro_tick)` gives the tick number and
  the queued ticks it covers; the in-memory row moves at once, so no other
  wake of this process takes the same tick, while the store's row moves
  only with the commit. The bookings the chosen mind sees are forgotten in
  memory at open; their rows go with the consume.
- **The plan.** The scripted mind (TOML or Rhai) stages
  `PlanSet { errand }` (`errand` when the wake carried a macro tick, a
  booking or a login) and, before each line, `PlanCursor { i + 1 }`.
  `plan_step` mirrors what is durable: the cursor passes a line only with
  its effect. Only the TOML plan stages `RuleCounts`.
- **Close.** `PlanDone` is in every close, whether the mind finished or
  timed out: in one process a plan runs once, and only a crash leaves a
  plan to resume. A close that fails is logged; what it kept comes back at
  the next boot.
- **Not ready.** An agent at its hard budget cap is not ready for its
  bookings (`take_due` skips it), so its bookings wait in the table; a
  booking taken just before the cap was reached is given back
  (`requeue`), never consumed by the fallback. A Turing PC that logs out
  gives back the bookings of the wake it had waiting.
- **Feed.** `WakeStarted` carries the wake id; a booking dropped at boot is
  `booking_dropped` (its note stays out of the feed) and an abandoned plan
  is `plan_abandoned`.

The entity stays `busy` until it settles, so a late littleguy command
lands in its own turn and never consumes the next wake. A wake is never
redelivered under an old id: littleguy answers a seen id with its latest
answer, so obligations that were not consumed come back as new wakes with
fresh ids through the normal, ready-gated paths.

### Load and reconcile (phase 1 loads; phases 2 to 4 reconcile)

- **Phase 1.** `Store::load(RELOAD_EVENTS)` returns the snapshot, the event
  tail and every side row (`SideState`). `RELOAD_EVENTS = EVENT_LOG_CAP`,
  so after a restart `mud_events` reads exactly what it read before. Rows of
  entities the pack no longer has are dropped at open (`DropEntity`).
  `Game::take_restored()` hands the rows over once.
- **Phase 2 (built).** `Supervisor::open(.., restored)` runs after
  `Game::open` (`Supervisor::new` restores whatever the game still holds).
  Bookings return to the `WakeBook` and are kept, fired
  through `take_due` when the entity is ready, or dropped (with an event)
  when overdue past `booking_max_late_ms`; the next booking id is
  `max(meta, max id) + 1`. The macro schedule is restored: a future time
  kept, one too far clamped, a past one fired once within
  `catchup_spread_ms`, an owed tick (`queued > cleared`) delivered with its
  number, also within `catchup_spread_ms` of boot rather than at a stored
  time hours away, and without queuing another. Rule counts return to the
  scripted mind by digest. Plans resume:
  a plan whose cursor reached its end is done; an **errand** (a wake that
  carried a macro tick, a booking or a login) resumes at its cursor
  **however long the server was down**; a reactive plan resumes only within
  `replay_max_age_ms` (120 s), else it is abandoned with an observer event.
  A resume is a fresh wake running `lines[cursor..]` through the gate; it
  waits for the entity to be online, runs before any other wake of that
  entity, and costs no budget. Its turn is keyed by the plan's own wake
  id, so its cursors land on the stored plan. The owner's decision (an
  errand resumes after any outage) supersedes the design draft, which
  bounded every resumed plan by `replay_max_age_ms`; only reactive plans
  keep that window.
- **Phase 3.** Budgets split into `local` and `lg` tallies joined by UTC
  day; `roll` becomes forward-only; littleguy meters are primed from the
  ledger (`lg = max(persisted, ledger today)`); HarvestMind's
  `harvest_tokens_seen` re-charges tokens unflushed at a crash once, to the
  boot day.
- **Phase 4.** Addressed events (a tell, an attack, a mention by name)
  newer than `replay_max_age_ms` and after the recipient's `seen_seq` are
  replayed as Immediate wakes only; older ones are skipped. The chatter
  counters are folded from the event tail through `chatter_step`; MCP
  cursors start at `seen_seq`.

### Retention and continue-as-new

- **`world.db` events (phase 1): archive before trim.** Retention never
  deletes history outright. Every `retention_every_ms` of game time the
  supervisor's step runs `Game::retain` off the async workers. Each batch is
  one transaction under the cell lock only: it takes at most
  `retention_batch` rows below the newest `events_keep_min` rows (never
  fewer than `EVENT_LOG_CAP`), oldest seq first, and stops before the
  first row stamped at or after `now - events_keep_days`. So a batch is
  always a seq prefix of the table, even when the wall clock stepped back
  and `at_ms` does not rise with seq: a row too young to go holds back
  every row after it until it ages (`lemma_prefix_leaves_only_higher_rows`).
  A row stamped ahead by a clock that later stepped back holds retention
  until its stamp ages; the table grows meanwhile, and nothing is lost.
  It appends the batch in seq order to
  `<archive_dir>/events-YYYY-MM-<epoch>.jsonl` (the UTC month of each
  event's own `at_ms` and the `world.db` epoch in 16 hex digits, one JSON
  line per event, the stored event as it is), fsyncs each file it
  touched, the directory and its parent (every batch, so a file or
  directory an earlier crashed run made is durable too), and only then
  deletes exactly those rows and commits.
  Each row gets an `archive_verdict`: above its file's last line it is
  written; at or below it, the file must already hold the row's exact
  line (a re-run after a crash between the append and the delete) or the
  whole batch is refused and nothing is deleted. A torn last line is cut
  off first. So no event is written twice
  (`lemma_crash_rerun_is_idempotent`), and every deleted row is in the
  archive, the exact line, not merely at or below a file's last seq
  (`lemma_every_trimmed_row_is_archived`), proved. The epoch in the file
  name keeps worlds apart: deleting `world.db` (a world reset) mints a new
  epoch, so the new world's seqs, which start again at 1, go to new files
  beside the old world's. A restored backup keeps its epoch; once it
  reuses a seq the archive holds with other text, retention refuses
  (`StoreError::Archive`, counted in `store_errors`) until the operator
  moves the file aside or gives the file a new epoch. The owner's decision
  named the files `events-YYYY-MM.jsonl`; the epoch suffix is the
  smallest change that keeps a reset or restored world from losing
  history to the old world's lines. If the archive cannot be written or
  refuses the batch, nothing is deleted.
  `events_keep_days = 0` keeps every event in the table. Freed pages are
  reused; there is no runtime `VACUUM`.
- **`mud_events` (phase 1)** reads the in-memory log, not the table, so
  retention does not touch it. It now answers `truncated: true` when some
  event after `since_seq` has left the in-memory log (`events_truncated`,
  proved exact); that loss happened before and was not reported. The
  proof needs the log to hold `oldest..=last` with no hole. A reload keeps
  only the run of stored rows that ends at the snapshot's last seq without
  a hole, so a gap in the table (events a version 0 store lost with a
  failed save) sits below the log and is reported as `truncated`. The
  backlog cap (`Audit::StoreBacklog`) keeps `EVENT_LOG_CAP` events after
  the gap it makes, so its gap is never inside the reloaded log either.
- **HarvestMind's `harvest.db` (phase 3)** continues as new every
  `ops_per_run` wakes (32) and purges sealed runs older than `keep_days`
  (3) with the public `purge_with`, on harvest's real clock (this
  exception applies only to the minds' own files).
- **Any future harvest workflow** (a Steward saga) must continue as new
  every 100 signals or fewer, purge, and drive on the game clock.

### Configuration

`[world]` (phase 1): `events_keep_days` (7; 0 to 36500), `events_keep_min`
(10000; below 10000 acts as 10000), `retention_every_ms` (3600000),
`retention_batch` (5000), `archive_dir` (unset: `archive/` beside `db`).
`[supervisor]` (phase 2): `replay_max_age_ms` (120000; 0 to one day),
`booking_max_late_ms` (86400000; 0 to 30 days), `catchup_spread_ms`
(120000; 0 to one hour). Phase 3 adds `[minds.harvest] ops_per_run`,
`keep_days`, `purge_every_ms`. Every section denies unknown keys and checks its bounds.

## Guarantees and crash model

**Deterministic plans (Scripted runtime, Rhai minds; phase 2).** Every
effectful line commits with `PlanCursor` past it, in the same transaction.
A line that changed nothing (a refusal, a look) may run again after a
crash, which is harmless. An unfinished plan resumes from its cursor, so
**each effectful line of the plan takes effect exactly once**: torin's
BOUNTY is posted once whether the crash comes before `move e`, after it,
after `move n` or after the post.

**LLM minds (littleguy, harvest, plugin-agent, ambient; phase 2).** A
wake's obligations (the booking, the macro tick number, the addressed
events) are consumed **with the mind's first effectful command, or at
close**. A crash before either leaves them, and they come back as new
wakes with fresh ids. A crash after the first effectful command loses the
rest of the turn: **effect at most once beyond the first act.** torin under
`runtime = "littleguy"` has no script: its bounty is the model's choice.

**Process crash.** The guarantees above hold.
`crates/mud-supervisor/tests/kill_restart.rs` (unix) checks them on a real
process: the test runs itself again as a child that stops for good at an
exact point, holding the world, and the parent SIGKILLs it and opens the
files one second later. The points: after a booking's "ok" (the row is
there, the next id is higher); after the first macro tick and a due
booking were taken but before any line took effect (nothing consumed; the
tick comes back as number 1 with a fresh id and one BOUNTY follows; the
booking fires once); after torin's first, third and last line (the cursor
is 1, 3 or 5; the resume runs only the rest, or nothing; one BOUNTY; the
next tick is number 2 and posts nothing; the dice match a run that was
never killed); and in the middle of retention (the table and the archive
are contiguous, with no line twice, and a re-run finishes). Each case
also checks `user_version = 1`, a bumped boot and that no wake id of the
child is used again. The budget case waits for phase 3.

**OS crash.** With WAL and `synchronous = NORMAL` the last world commits
may be lost, and their `Consume` and `PlanCursor` ops with them: durable
state is a prefix of commits, and world and bookkeeping roll back together.
The only promises made outside the process follow a `synchronous = FULL`
transaction: a booking's "ok" (`act`) and the boot number in wake ids.
Not covered: littleguy's own memory may hold a turn whose world effects an
OS crash rolled back.

## Consequences

- `world.db` is one private backup unit: it holds the world, the ledger,
  agent notes (booking notes are untrusted text, data only, escaped on
  render) and plans. Back it up with `accounts.db` and `archive/`.
- Restoring an older `world.db` and replaying the archive is the planned
  admin rollback ([roadmap](../roadmap.md)); the archive is its basis.
- mud-world defines plain side rows; the supervisor gives them meaning.
- A redelivered turn may be paid twice when the first turn made no effect.
- HarvestMind retention uses the real clock; a crash's harvest spend is
  charged to the boot day.
- A process that opens `world.db` (the account CLI included) bumps the boot
  number; that only skips boot numbers.

## Rejected

- **The world as one harvest workflow**: a fixed 1.6 to 5 ms per drive, one
  owner serializing every command and read, a stale turn re-run after a
  crash.
- **A per-entity harvest chronicle**: consumes before the effect, so a crash
  loses the obligation.
- **`supervisor.db`**: consume-before (loss) or consume-after (a repeat),
  never consume-with.
- **JSON files**: the same, without transactions.
- **State in `WorldState`**: side rows are not physics and must not reach a
  view or a script.
- **Same-id redelivery**: littleguy swallows a seen id.
- **Trimming events without an archive**: history is never deleted
  outright.

Revisit harvest only for a multi-step external Steward saga, in its own
file, with continue-as-new every 100 signals or fewer, `purge_with`, and
game-clock drives.
