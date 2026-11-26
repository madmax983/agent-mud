# agent-mud: agent instructions

A text MUD where humans and littleguy agents share one world. Read
`docs/design-v0.1.md` for the design and `docs/adr/` for decisions.

## Workflow

- One issue or milestone per PR, against `main`, ready for review.
- SPEC, PROOF, RED, GREEN, REFACTOR: write the Verus spec and the failing
  test first, then the code.
- Before pushing: `cargo fmt --all`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace`, and `cargo run -p mud-server
  --bin mud -- smoke`.
- **Config changes are loud.** A new key, in `mud.toml` or in the `lg.toml`
  the MUD writes, goes into `mud.toml.example` with a comment, and into the
  README where a user meets it. The PR description lists each one under a
  "Config changes" heading. The `config::example_tests` fail when the
  example misses a key.
- **A new `lg.toml` part needs a littleguy that reads it.** Name the first
  littleguy commit that reads it, bump the pin (`LG_MIN_COMMIT` in
  `littleguy/compat.rs`, `ci.yml`, the README and the example together), and
  add it to `compat::Optional` when an older lg may run without it.

## Rules that bind this repository

- **The server owns physics.** Every state change goes through
  `mud_world::World::execute` with a parsed line. Never add a side door
  that mutates `WorldState` for a mind.
- **Untrusted text stays in `untrusted_text`.** Never interpolate player or
  agent text into a narrative, a cue, a prompt's instruction part, or an
  HTML page without escaping (maud escapes; the terminal uses
  `textContent`).
- **Views never leak tiers.** `look_view`, `sheet_view`, `Event::view_for`
  and `who` must not reveal `tier`, `human` or a mind. A test checks it.
- **Setting-agnostic engine.** New content goes in a pack. A new engine
  behavior is driven by pack data (see `SkillEffect`), not by a skill or
  item id in Rust.
- **Determinism.** All randomness that touches the world goes through
  `DiceRng` or the supervisor's `mix(seed, …)`. No global RNG, no wall
  clock in the engine (`now_ms` is passed in).
- **Secrets stay out of the world.** Passwords, hashes and tokens never
  reach `WorldState`, events, `Audit`, the observer, logs or
  `World::execute`; they live in `mud-accounts` and `accounts.db` only.
  Tests check it (`session::passwords_never_reach_the_world`,
  `tests/trace_log.rs`). `mud-script` never holds an `Accounts` handle,
  and `password`, `logout` and `quit` are never a script verb.
- **Secrets come from the OS CSPRNG** (`getrandom`), never `DiceRng` or
  `mix(seed, …)`.
- **Mirrored kernels.** Code between `// mirror-begin` and `// mirror-end`
  is proved in `formal/verus`. Change both sides in one commit; the
  `verus_mirror` tests fail otherwise.

## Rust rules

- clippy pedantic + nursery, warnings denied. No `#[allow]`; use
  `#[expect(lint, reason = "…")]` only for a real reason.
- No `unwrap()`/`expect()` outside tests.
- `thiserror` in libraries, `anyhow` in the binary.
- Newtype ids (`EntityId`, `RoomId`, `ItemId`, `Seq`, …) across crates.
- Tokio for async. Line coverage stays at 85% or more.
- One SQLite build: `rusqlite 0.42` (`bundled`), the same as autumn-harvest.

## Dependencies

autumn-web 0.11 (`maud`, `htmx`, `ws`; no default features), autumn-harvest
0.11, autumn-plugin-agent 0.5, rmcp 3.x. littleguy is a binary the
supervisor runs; it is not a crate dependency.
