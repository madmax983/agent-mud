//! The Steward: the MUD as every littleguy daemon's owner (ADR 0015).
//!
//! lg keeps some things for its owner to decide: the goals an agent
//! proposes for itself (and, with refine, its overlays). The supervisor
//! starts every `lg serve` as its own user, so it is that owner, and the
//! Steward acts as it over the socket ([`crate::littleguy::owner`]). Every
//! `every_secs`, for each daemon that runs (it never starts one), one pass:
//!
//! 1. Spend: a heartbeat tick's ledger rows, settled under a claim and
//!    owed to the budget as late `lg` spend. A wake in flight owns its
//!    rows, so a failed claim skips the settle.
//! 2. Quiet at `Hard`: the config is rewritten without `[heartbeat]` and
//!    with memory off, and lg reloads it, so no tick, reflection or
//!    consolidation spends; loud again when the budget allows.
//! 3. Approvals: checked once per daemon start; `off` is expected, and the
//!    Steward never approves or denies.
//! 4. Plans: an open intent is settled from the listing first. Then each
//!    proposed goal, oldest first, goes through the text guard
//!    ([`vet::goal_text`]), the proved [`vet::goal_verdict`], and, when
//!    admitted, the council ([`council`]). The decision is written to the
//!    store as an intent before `decide` is sent, so a crash or a lost
//!    answer ends in exactly one owner action. Stale active goals are
//!    dropped.
//! 5. Refine ([`refine`], with `[littleguy] refine` on): the pointer
//!    check of lg's active overlay, every pass; the empty baseline at first
//!    contact; and, when due, lg's proposer under a claim, a menu ballot
//!    (or, with `refine = "free"` and not for a Turing-tier entity, the
//!    free-text guard and a unanimous council, ADR 0016), and a put and an
//!    activate one move above the baseline. A human's move pauses refine;
//!    a free text that comes to name an entity is revoked.
//! 6. Notes: counted, for the observer.
//!
//! Every decision is published on the observer feed and noted for the
//! agent's next wake. The Steward holds no supervisor lock across a `Game`
//! call or a socket call; it reads the game clock only.

pub mod council;
#[cfg(test)]
pub(crate) mod fake;
mod refine;
pub mod state;
pub mod vet;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mud_world::side::StewardStored;
use mud_world::{EntityId, SideOp};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::budget::{BudgetLevel, DAY_MS};
use crate::held::assert_none_held;
use crate::littleguy::owner::{
    self, GoalEntry, OwnerRequest, OwnerResponse, PlanDecision, READ_TIMEOUT,
};
use crate::littleguy::{PersonaText, RefineMode};
use crate::mind::{BoxFuture, MindError, Usage};
use crate::runtime::{ObserverEvent, Supervisor};
use council::{Ballot, Judge};
use state::{IntentAction, ROW_SCHEMA, StewardRow, StuckAccept};
use vet::{GoalAsk, GoalCaps, GoalVerdict, Snippet, TextLimits, TextVerdict};

/// The daemons the Steward owns: the seam between it and littleguy.
/// [`crate::littleguy::LittleguyMind`] implements it; tests implement it
/// with scripted sockets.
pub trait OwnedDaemons: Send + Sync + std::fmt::Debug {
    /// The entities this mind serves, in id order.
    fn entities(&self) -> Vec<EntityId>;
    /// The socket of a live daemon; never starts one.
    fn running_socket<'a>(&'a self, entity: &'a EntityId) -> BoxFuture<'a, Option<PathBuf>>;
    /// A key that changes on every daemon start (the child pid).
    fn start_key<'a>(&'a self, entity: &'a EntityId) -> BoxFuture<'a, Option<u32>>;
    /// The author's persona text, token-free.
    fn brief(&self, entity: &EntityId) -> Option<PersonaText>;
    /// Ledger rows not yet charged, now marked charged.
    fn settle<'a>(&'a self, entity: &'a EntityId, now_ms: u64) -> BoxFuture<'a, Usage>;
    /// Rewrites the daemon's config quiet or loud; the Steward then sends
    /// `reload`.
    fn set_quiet<'a>(
        &'a self,
        entity: &'a EntityId,
        quiet: bool,
    ) -> BoxFuture<'a, Result<(), MindError>>;
    /// Whether its config is quiet now.
    fn is_quiet(&self, entity: &EntityId) -> bool;
}

/// `[littleguy.steward]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StewardConfig {
    /// Whether the Steward runs.
    pub enabled: bool,
    /// Seconds between passes.
    pub every_secs: u64,
    /// How long `decide` and `reload` may wait behind lg's loop.
    pub owner_timeout_secs: u64,
    /// An intent still open after this is sent again.
    pub intent_retry_secs: u64,
    /// Sends of one intent before it is given up as stuck.
    pub max_intent_tries: u32,
    /// After a stuck intent, the entity's actions wait this long.
    pub stuck_backoff_hours: u64,
    /// The `[models]` alias of the council. `None`: `council`, if that
    /// alias exists; with no alias the Steward has no judge, and goals
    /// wait until their TTL declines them.
    pub council_model: Option<String>,
    /// More aliases; members cycle `council_model`, then these.
    pub council_models: Vec<String>,
    /// The temperature of every vote.
    pub council_temperature: f32,
    /// Votes per ballot: odd, 1 to 7.
    pub council_members: u32,
    /// Ballots per entity per game-clock day.
    pub max_council_votes_per_day: u32,
    /// Decisions (a decide sent, or an intent written) per entity per pass.
    pub max_decisions_per_pass: u32,
    /// Active goals at most.
    pub max_active_goals: u32,
    /// Accepts per entity per game-clock day.
    pub max_goal_accepts_per_day: u32,
    /// The shortest check interval a goal may ask, in minutes.
    pub min_check_minutes: u32,
    /// A proposed goal still waiting after this is declined.
    pub proposal_ttl_hours: u64,
    /// An active goal with no new note for this long is dropped.
    pub goal_stale_days: u64,
    /// The longest goal title, in characters.
    pub title_chars: usize,
    /// The longest goal reason, in characters.
    pub why_chars: usize,
    /// Phrases refused in a goal, added to [`vet::DENY_PHRASES`].
    pub deny_phrases: Vec<String>,
    /// Whether a daemon goes quiet at the hard budget cap.
    pub quiet_on_hard: bool,
    /// The refine mode, from `[littleguy] refine` (not a key of this
    /// section): the server copies it in. `Off` leaves overlays alone.
    #[serde(skip)]
    pub refine: RefineMode,
    /// How long the Steward waits on one `overlay_activate` answer. A lost
    /// answer is never resent.
    pub activate_wait_secs: u64,
    /// How long `refine_propose` may take.
    pub refine_timeout_secs: u64,
    /// After a propose timed out, how long the claim drains lg's loop.
    pub refine_drain_secs: u64,
    /// Hours between two proposes of one entity (at least 24).
    pub refine_every_hours: u64,
    /// Wakes that end well between two proposes (at least 1).
    pub refine_min_wakes: u32,
    /// Tokens charged before each propose, as local spend: lg's proposer
    /// writes no ledger row.
    pub refine_charge_tokens: u64,
    /// Menu items active at once at most.
    pub max_guardrails: u32,
    /// Free mode: the whole candidate, lines joined by `\n`, in
    /// characters at most (ADR 0016).
    pub refine_max_chars: usize,
    /// Free mode: lines at most.
    pub refine_max_lines: usize,
    /// Free mode: characters of one line at most.
    pub refine_line_chars: usize,
    /// Free mode: words refused, added to [`vet::DENY_WORDS`].
    pub refine_deny_words: Vec<String>,
    /// Free mode: stems refused, added to [`vet::DENY_STEMS`].
    pub refine_deny_stems: Vec<String>,
    /// Entities whose refine pause, recorded in an earlier boot, ends at
    /// this boot. Remove an entity once it is resumed.
    pub resume_refine: Vec<String>,
    /// The operator menu (`[[littleguy.steward.guardrail]]`); empty uses
    /// [`vet::default_menu`].
    pub guardrail: Vec<Snippet>,
}

impl Default for StewardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            every_secs: 60,
            owner_timeout_secs: 30,
            intent_retry_secs: 600,
            max_intent_tries: 5,
            stuck_backoff_hours: 6,
            council_model: None,
            council_models: Vec::new(),
            council_temperature: 0.7,
            council_members: 3,
            max_council_votes_per_day: 12,
            max_decisions_per_pass: 2,
            max_active_goals: 3,
            max_goal_accepts_per_day: 2,
            min_check_minutes: 60,
            proposal_ttl_hours: 48,
            goal_stale_days: 7,
            title_chars: 120,
            why_chars: 800,
            deny_phrases: Vec::new(),
            quiet_on_hard: true,
            refine: RefineMode::Off,
            activate_wait_secs: 600,
            refine_timeout_secs: 120,
            refine_drain_secs: 600,
            refine_every_hours: 168,
            refine_min_wakes: 20,
            refine_charge_tokens: 30_000,
            max_guardrails: 3,
            refine_max_chars: 600,
            refine_max_lines: 6,
            refine_line_chars: 160,
            refine_deny_words: Vec::new(),
            refine_deny_stems: Vec::new(),
            resume_refine: Vec::new(),
            guardrail: Vec::new(),
        }
    }
}

impl StewardConfig {
    /// The goal limits of the kernel.
    #[must_use]
    pub const fn caps(&self) -> GoalCaps {
        GoalCaps {
            min_check: self.min_check_minutes,
            max_active: self.max_active_goals,
            max_per_day: self.max_goal_accepts_per_day,
            ttl_ms: self.proposal_ttl_hours.saturating_mul(3_600_000),
        }
    }

    /// The text limits of the guard.
    #[must_use]
    pub const fn limits(&self) -> TextLimits {
        TextLimits {
            title_chars: self.title_chars,
            why_chars: self.why_chars,
        }
    }

    /// The free-text guard: the limits, and the deny lists config adds to
    /// the built-in ones.
    #[must_use]
    pub fn free_guard(&self) -> vet::FreeGuard {
        vet::FreeGuard {
            max_lines: self.refine_max_lines,
            line_chars: self.refine_line_chars,
            max_chars: self.refine_max_chars,
            extra_words: self.refine_deny_words.clone(),
            extra_stems: self.refine_deny_stems.clone(),
        }
    }

    /// The operator menu: `guardrail`, or the code default.
    #[must_use]
    pub fn menu(&self) -> Vec<Snippet> {
        if self.guardrail.is_empty() {
            vet::default_menu()
        } else {
            self.guardrail.clone()
        }
    }
}

/// The longest title or reason the observer and the agent's goal line
/// show, in characters.
pub const VIEW_CHARS: usize = 200;

fn cut(s: &str) -> String {
    s.chars().take(VIEW_CHARS).collect()
}

/// One goal as the observer shows it: the agent's text, data only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalView {
    /// lg's id.
    pub id: i64,
    /// The title, cut to [`VIEW_CHARS`].
    pub title: String,
    /// The reason, cut to [`VIEW_CHARS`].
    pub why: String,
}

/// The agent's notes, counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesView {
    /// How many.
    pub count: usize,
    /// Their text, in bytes.
    pub bytes: usize,
}

/// What the Steward last read of one agent, for the observer and the
/// agent's goal line. Admin eyes only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomyView {
    /// Active goals.
    pub active: Vec<GoalView>,
    /// Proposed goals.
    pub proposed: Vec<GoalView>,
    /// Open ideas.
    pub ideas: usize,
    /// The notes, once counted.
    pub notes: Option<NotesView>,
    /// When the pass ran, by the game clock.
    pub pass_ms: u64,
    /// What lg answered `off` to.
    pub off: Vec<String>,
    /// The overlays, when refine is on.
    pub overlay: Option<OverlayView>,
    /// Whether the daemon is quiet.
    pub quiet: bool,
    /// The open intent's action, if any.
    pub intent: Option<String>,
}

/// The overlays of one agent as the observer shows them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayView {
    /// `menu` or `free`.
    pub mode: String,
    /// lg's active version, as the Steward last confirmed it.
    pub pointer: Option<u64>,
    /// The empty baseline: one Roll back on lg's page returns to it.
    pub baseline: Option<u64>,
    /// What the Steward's active text came from (menu ids, or `free`), if
    /// any.
    pub source: Option<String>,
    /// The active free-text lines, without the preamble: model text a
    /// unanimous council approved (ADR 0016), escaped on render.
    #[serde(default)]
    pub lines: Vec<String>,
    /// Why refine is paused, if it is.
    pub paused: Option<String>,
    /// The version lg's Roll back button would activate, as far as the
    /// Steward knows (`None`: unknown).
    #[serde(default)]
    pub rollback_to: Option<u64>,
    /// The Steward's own versions, newest first, with their full text.
    pub chain: Vec<ChainEntry>,
}

/// One version the Steward put.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainEntry {
    /// lg's version.
    pub version: u64,
    /// `baseline`, the menu ids, or `free`.
    pub label: String,
    /// Its guardrails text as put (`""` for the baseline): operator text
    /// in menu mode, the preamble and model lines in free mode; escaped on
    /// render.
    pub text: String,
    /// Whether lg has it active.
    pub active: bool,
}

/// One of the agent's goals in its wake message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalLine {
    /// lg's id.
    pub id: i64,
    /// The title: the agent's text, data only.
    pub title: String,
}

/// What the Steward tells an agent at its next wake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StewardNote {
    /// What happened: `accepted`, `declined_text`, `declined_repeat`,
    /// `declined_check_too_frequent`, `declined_council`,
    /// `declined_stale`, `dropped_stale` or `waiting_cap`; for refine,
    /// `overlay_on` or `overlay_off` (with no goal and no text).
    pub code: String,
    /// The goal.
    pub goal: Option<i64>,
    /// Its title: the agent's own text, data only.
    pub title: Option<String>,
}

/// One line of the Steward on the observer feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StewardEvent {
    /// Whose.
    pub entity: EntityId,
    /// `goal`, `overlay`, `approval`, `budget` or `council`.
    pub item: String,
    /// The goal, or the overlay version, if any.
    pub id: Option<i64>,
    /// What happened, such as `accepted`, `declined` or `quiet`.
    pub action: String,
    /// Why.
    pub code: String,
    /// Yes and no votes, when the council voted.
    pub votes: Option<[u32; 2]>,
    /// The goal title (untrusted, cut to [`VIEW_CHARS`]), the menu ids and
    /// text of an overlay, or the full free-text candidate (untrusted, at
    /// most `refine_max_chars`, never cut).
    pub text: Option<String>,
    /// The council's reasons, each cut to 200 characters: here only,
    /// never for lg or the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

/// A point in a pass, for fault drills ([`Steward::set_probe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StewardPoint {
    /// An intent was written durably; its owner request is not sent yet.
    IntentWritten,
    /// lg stored an overlay put; the row does not know its version yet.
    OverlayStored,
}

/// Called at each [`StewardPoint`].
pub type StewardProbe = Arc<dyn Fn(StewardPoint, &EntityId) + Send + Sync>;

/// What the Steward keeps of one entity in memory.
#[derive(Debug, Clone, Default)]
struct Run {
    row: StewardRow,
    /// The rev of the last write (or of the stored row).
    rev: u64,
    /// The start key of the daemon whose approvals were checked.
    checked: Option<u32>,
    /// A quiet or loud rewrite whose `reload` got no answer yet.
    reload_owed: bool,
    /// The row changed since it was last written.
    dirty: bool,
}

/// A goal listing, as `plans` answered it.
#[derive(Debug, Clone, Default)]
struct Listing {
    goals: Vec<GoalEntry>,
    ideas: usize,
}

impl Listing {
    fn status(&self, id: i64) -> Option<&str> {
        self.goals
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.status.as_str())
    }

    fn title(&self, id: i64) -> Option<String> {
        self.goals
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.title.clone())
    }

    fn active(&self) -> u32 {
        u32::try_from(self.goals.iter().filter(|g| g.status == "active").count())
            .unwrap_or(u32::MAX)
    }
}

/// The Steward.
pub struct Steward {
    sup: Arc<Supervisor>,
    daemons: Vec<Arc<dyn OwnedDaemons>>,
    judge: Option<Arc<dyn Judge>>,
    cfg: StewardConfig,
    boot: u64,
    owner_timeout: Duration,
    /// How long a read beside lg's loop may take.
    read_timeout: Duration,
    /// How long one `overlay_activate` answer is awaited.
    activate_wait: Duration,
    /// How long `refine_propose` may take.
    refine_timeout: Duration,
    /// How long a drain after a propose timeout may take.
    refine_drain: Duration,
    /// How often a drain probes lg's loop.
    drain_every: Duration,
    /// The operator menu.
    menu: Vec<Snippet>,
    runs: tokio::sync::Mutex<BTreeMap<EntityId, Run>>,
    /// Wakes that ended well, per entity, since the last pass.
    wakes_done: Mutex<BTreeMap<EntityId, u32>>,
    probe: Mutex<Option<StewardProbe>>,
}

impl std::fmt::Debug for Steward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Steward")
            .field("daemons", &self.daemons)
            .field("judge", &self.judge)
            .field("cfg", &self.cfg)
            .field("boot", &self.boot)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Steward {
    /// A Steward over the daemons of `daemons`, restoring `restored`, the
    /// rows split off the side state before the supervisor took the rest.
    /// A row of another schema, or one that does not parse, is logged and
    /// replaced by a fresh one.
    #[must_use]
    pub fn new(
        sup: Arc<Supervisor>,
        daemons: Vec<Arc<dyn OwnedDaemons>>,
        judge: Option<Arc<dyn Judge>>,
        cfg: StewardConfig,
        restored: BTreeMap<EntityId, StewardStored>,
        boot: u64,
    ) -> Arc<Self> {
        let mut runs: BTreeMap<EntityId, Run> = restored
            .into_iter()
            .map(|(entity, stored)| {
                let row = StewardRow::from_body(stored.schema, &stored.body).unwrap_or_else(|| {
                    tracing::warn!(
                        %entity,
                        schema = stored.schema,
                        "a steward row of an unknown schema was replaced by a fresh one"
                    );
                    StewardRow::default()
                });
                (
                    entity,
                    Run {
                        row,
                        rev: stored.rev,
                        ..Run::default()
                    },
                )
            })
            .collect();
        resume_refine(&sup, &cfg, boot, &mut runs);
        Arc::new(Self {
            sup,
            daemons,
            judge,
            owner_timeout: Duration::from_secs(cfg.owner_timeout_secs.max(1)),
            read_timeout: READ_TIMEOUT,
            activate_wait: Duration::from_secs(cfg.activate_wait_secs.max(1)),
            refine_timeout: Duration::from_secs(cfg.refine_timeout_secs.max(1)),
            refine_drain: Duration::from_secs(cfg.refine_drain_secs).min(MAX_WAIT),
            drain_every: DRAIN_EVERY,
            menu: cfg.menu(),
            cfg,
            boot,
            runs: tokio::sync::Mutex::new(runs),
            wakes_done: Mutex::new(BTreeMap::new()),
            probe: Mutex::new(None),
        })
    }

    /// The boot this Steward runs in.
    #[must_use]
    pub const fn boot(&self) -> u64 {
        self.boot
    }

    /// Whether a judge votes. Without one, goals wait for their TTL.
    #[must_use]
    pub fn has_judge(&self) -> bool {
        self.judge.is_some()
    }

    /// Calls `probe` at each [`StewardPoint`] (fault drills and tests).
    pub fn set_probe(&self, probe: Option<StewardProbe>) {
        *lock(&self.probe) = probe;
    }

    /// Shortens the wait for an owner answer (tests).
    #[cfg(test)]
    pub(crate) fn set_owner_timeout(self: &mut Arc<Self>, t: Duration) {
        if let Some(me) = Arc::get_mut(self) {
            me.owner_timeout = t;
        }
    }

    /// Shortens the refine waits (tests): one activate answer, a propose,
    /// its drain, and the drain's probe.
    #[cfg(test)]
    pub(crate) fn set_refine_waits(
        self: &mut Arc<Self>,
        activate: Duration,
        propose: Duration,
        drain: Duration,
        drain_every: Duration,
    ) {
        if let Some(me) = Arc::get_mut(self) {
            me.activate_wait = activate;
            me.refine_timeout = propose;
            me.refine_drain = drain;
            me.drain_every = drain_every;
            me.read_timeout = activate;
        }
    }

    fn point(&self, p: StewardPoint, entity: &EntityId) {
        let probe = lock(&self.probe).clone();
        if let Some(f) = probe {
            f(p, entity);
        }
    }

    /// The row of `entity` as the Steward holds it.
    pub async fn row(&self, entity: &EntityId) -> Option<StewardRow> {
        self.runs.lock().await.get(entity).map(|r| r.row.clone())
    }

    /// Runs the Steward: a pass every `every_secs`, and a counter of the
    /// wakes that end well. Abort the handles to stop it. Without a judge
    /// it says so once, in the log and on the feed.
    #[must_use]
    pub fn spawn(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        if self.judge.is_none() {
            tracing::warn!(
                "the Steward has no council model: proposed goals wait, and their TTL declines them; set [littleguy.steward] council_model to a [models] alias"
            );
            for e in self.daemons.iter().flat_map(|d| d.entities()) {
                self.sup.report_steward(StewardEvent {
                    entity: e,
                    item: "council".into(),
                    id: None,
                    action: "no_judge".into(),
                    code: "no_judge".into(),
                    votes: None,
                    text: None,
                    reasons: Vec::new(),
                });
            }
        }
        let me = Arc::clone(self);
        let mut rx = self.sup.subscribe();
        let counter = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) => {
                        if let ObserverEvent::WakeFinished {
                            entity, ok: true, ..
                        } = &*e
                        {
                            *lock(&me.wakes_done).entry(entity.clone()).or_default() += 1;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let me = Arc::clone(self);
        let every = Duration::from_secs(self.cfg.every_secs.max(1));
        let clock = tokio::spawn(async move {
            let mut t = tokio::time::interval(every);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                t.tick().await;
                me.pass().await;
            }
        });
        vec![counter, clock]
    }

    /// One pass over every running daemon, in entity id order.
    pub async fn pass(&self) {
        let mut todo: Vec<(EntityId, Arc<dyn OwnedDaemons>)> = self
            .daemons
            .iter()
            .flat_map(|d| d.entities().into_iter().map(|e| (e, Arc::clone(d))))
            .collect();
        todo.sort_by(|a, b| a.0.cmp(&b.0));
        let mut runs = self.runs.lock().await;
        for (entity, d) in todo {
            let Some(socket) = d.running_socket(&entity).await else {
                continue;
            };
            let run = runs.entry(entity.clone()).or_default();
            self.visit(&*d, &entity, &socket, run).await;
        }
        drop(runs);
    }

    async fn visit(&self, d: &dyn OwnedDaemons, e: &EntityId, socket: &Path, run: &mut Run) {
        let now = self.sup.game().now_ms();
        let done = lock(&self.wakes_done).remove(e);
        if let Some(n) = done {
            run.row.wakes_since_refine = run.row.wakes_since_refine.saturating_add(n);
            run.dirty = true;
        }
        self.settle_spend(d, e, now).await;
        let level = self
            .sup
            .budget_of(e)
            .map_or(BudgetLevel::Normal, |(level, _)| level);
        self.quiet(d, e, socket, level, run).await;
        let key = d.start_key(e).await;
        if key.is_some() && key != run.checked {
            if run.checked.is_some() && !run.row.overlay.late.is_empty() {
                // A restarted lg holds no queued activate of ours.
                run.row.overlay.late.clear();
                run.dirty = true;
            }
            self.check_approvals(e, socket).await;
            run.checked = key;
        }
        if !self.cfg.refine.is_on() {
            self.refine_off(e, run, now);
        }
        let mut view = AutonomyView {
            pass_ms: now,
            quiet: d.is_quiet(e),
            ..AutonomyView::default()
        };
        if let Some(mut listing) = self.read_plans(socket, &mut view).await {
            Self::seen(run, &listing, now);
            if run.row.stuck_until_ms.is_some_and(|u| now >= u) {
                run.row.stuck_until_ms = None;
                run.dirty = true;
            }
            self.settle_stuck(e, run, &listing);
            self.reconcile(e, socket, run, &mut listing, now, level)
                .await;
            let stuck = run.row.stuck_until_ms.is_some();
            if !stuck && run.row.intent.is_none() && level != BudgetLevel::Hard {
                self.decide(d, e, socket, run, &mut listing, now, level)
                    .await;
            }
            view.active = listing
                .goals
                .iter()
                .filter(|g| g.status == "active")
                .map(goal_view)
                .collect();
            view.proposed = listing
                .goals
                .iter()
                .filter(|g| g.status == "proposed")
                .map(goal_view)
                .collect();
            view.ideas = listing.ideas;
        } else if let Some(old) = self.sup.autonomy(e) {
            view.active = old.active;
            view.proposed = old.proposed;
            view.ideas = old.ideas;
        }
        if self.cfg.refine.is_on() {
            self.refine(d, e, socket, run, now, level).await;
            view.overlay = Some(self.overlay_view(run));
        }
        view.notes = self.read_notes(socket, &mut view).await;
        view.intent = run.row.intent.as_ref().map(|i| i.action.name().to_owned());
        self.sup.set_autonomy(e, view);
        if run.dirty {
            self.write(e, run, false);
        }
    }

    /// Settles the ledger rows a tick wrote, under a claim, and owes them
    /// to the budget as the mind's own (`lg`) spend. A wake in flight or
    /// waiting owns its rows: the claim fails and nothing is settled.
    async fn settle_spend(&self, d: &dyn OwnedDaemons, e: &EntityId, now: u64) {
        let Some(claim) = Supervisor::claim(&self.sup, e) else {
            return;
        };
        let usage = d.settle(e, now).await;
        self.sup.owe(e, usage);
        drop(claim);
    }

    /// Quiet exactly when the budget is `Hard` (and `quiet_on_hard`).
    async fn quiet(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        socket: &Path,
        level: BudgetLevel,
        run: &mut Run,
    ) {
        let want = self.cfg.quiet_on_hard && level == BudgetLevel::Hard;
        if want != d.is_quiet(e) {
            if let Err(err) = d.set_quiet(e, want).await {
                tracing::warn!(entity = %e, error = %err, "rewriting a daemon's config failed");
                return;
            }
            run.reload_owed = true;
        }
        if !run.reload_owed {
            return;
        }
        // Any `reloaded` is success: lg's own watcher may have applied the
        // file first. No answer: the next pass sends it again.
        match owner::request(socket, &OwnerRequest::Reload, self.owner_timeout).await {
            Ok(OwnerResponse::Reloaded { .. }) => {
                run.reload_owed = false;
                let action = if want { "quiet" } else { "loud" };
                self.publish(e, "budget", None, action, "hard_budget", None, None);
            }
            other => {
                tracing::warn!(entity = %e, answer = ?other, "a daemon did not reload its config");
            }
        }
    }

    /// The approvals check after a daemon start: `off` is expected.
    async fn check_approvals(&self, e: &EntityId, socket: &Path) {
        match owner::request(socket, &OwnerRequest::Approvals, READ_TIMEOUT).await {
            Ok(OwnerResponse::Off { .. }) => {}
            other => {
                tracing::warn!(entity = %e, answer = ?other, "lg serve did not answer the approvals check with off");
                self.publish(e, "approval", None, "not_off", "not_off", None, None);
            }
        }
    }

    async fn read_plans(&self, socket: &Path, view: &mut AutonomyView) -> Option<Listing> {
        match owner::request(socket, &OwnerRequest::Plans, READ_TIMEOUT).await {
            Ok(OwnerResponse::Plans { goals, ideas, .. }) => Some(Listing {
                goals,
                ideas: ideas.len(),
            }),
            Ok(OwnerResponse::Off { message }) => {
                view.off.push(message);
                None
            }
            _ => None,
        }
    }

    async fn read_notes(&self, socket: &Path, view: &mut AutonomyView) -> Option<NotesView> {
        match owner::request(socket, &OwnerRequest::Notes, READ_TIMEOUT).await {
            Ok(OwnerResponse::Notes { notes }) => Some(NotesView {
                count: notes.len(),
                bytes: notes.iter().map(|n| n.name.len() + n.text.len()).sum(),
            }),
            Ok(OwnerResponse::Off { message }) => {
                view.off.push(message);
                None
            }
            _ => None,
        }
    }

    /// Updates what the row remembers of each listed goal, by the game
    /// clock: a new goal is first seen now, a new note marks a change now.
    fn seen(run: &mut Run, listing: &Listing, now: u64) {
        let old = std::mem::take(&mut run.row.goals);
        for g in &listing.goals {
            let mark = g.notes.last().map_or(0, |n| n.at_ms);
            let seen = old.get(&g.id).map_or_else(
                || state::GoalSeen {
                    first_seen_ms: now,
                    note_count: g.notes.len(),
                    note_mark: mark,
                    note_change_ms: now,
                    waiting: None,
                    cap_noted: false,
                },
                |s| {
                    let mut s = s.clone();
                    if s.note_count != g.notes.len() || s.note_mark != mark {
                        s.note_count = g.notes.len();
                        s.note_mark = mark;
                        s.note_change_ms = now;
                    }
                    s
                },
            );
            run.row.goals.insert(g.id, seen);
        }
        if run.row.goals != old {
            run.dirty = true;
        }
    }

    /// Settles an open intent from `listing` (step 0): done, still owed,
    /// owed again after the retry window, or stuck.
    async fn reconcile(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        listing: &mut Listing,
        now: u64,
        level: BudgetLevel,
    ) {
        if self.settle_intent(e, run, listing) {
            return;
        }
        let Some(intent) = run.row.intent.clone() else {
            return;
        };
        // An overlay intent is refine's to settle (step 5).
        let Some(goal) = intent.action.goal() else {
            return;
        };
        let retry_ms = self.cfg.intent_retry_secs.saturating_mul(1_000);
        if now.saturating_sub(intent.sent_ms) < retry_ms || level == BudgetLevel::Hard {
            return;
        }
        if intent.tries >= self.cfg.max_intent_tries {
            tracing::warn!(entity = %e, goal, tries = intent.tries, "an owner action got no answer; the Steward backs off");
            run.row.intent = None;
            if let IntentAction::Accept { goal } = intent.action {
                // A queued decide may still run: the reservation stays, and
                // a later listing settles it (`settle_stuck`).
                run.row.stuck_accept = Some(StuckAccept {
                    goal,
                    day: run.row.accepts.day,
                });
            }
            run.row.stuck_until_ms =
                Some(now.saturating_add(self.cfg.stuck_backoff_hours.saturating_mul(3_600_000)));
            run.dirty = true;
            self.publish(
                e,
                "goal",
                Some(goal),
                "stuck",
                intent.action.name(),
                None,
                None,
            );
            return;
        }
        if let Some(i) = run.row.intent.as_mut() {
            i.tries = i.tries.saturating_add(1);
            i.sent_ms = now;
        }
        if !self.write(e, run, true) {
            return;
        }
        self.send(e, socket, run, listing, intent.action, now).await;
    }

    /// Settles the intent against `listing` if the listing shows it done.
    /// Returns whether it was settled.
    fn settle_intent(&self, e: &EntityId, run: &mut Run, listing: &Listing) -> bool {
        let Some(intent) = run.row.intent.clone() else {
            return false;
        };
        let Some(g) = intent.action.goal() else {
            return false;
        };
        let status = listing.status(g);
        let done = match intent.action {
            IntentAction::Accept { .. } => match status {
                Some("active") => {
                    let title = listing.title(g);
                    self.publish(
                        e,
                        "goal",
                        Some(g),
                        "accepted",
                        "accepted",
                        None,
                        title.as_deref(),
                    );
                    self.note(e, "accepted", g, title);
                    true
                }
                Some("proposed") => false,
                _ => {
                    // Gone: a human declined it, or our accept applied and
                    // the agent finished it (lg lists neither). The
                    // reservation stays: the cap errs on the safe side.
                    self.publish(e, "goal", Some(g), "gone", "gone", None, None);
                    true
                }
            },
            IntentAction::Decline { .. } => status != Some("proposed"),
            IntentAction::DropGoal { .. } => status != Some("active"),
            IntentAction::Put { .. } | IntentAction::Activate { .. } | IntentAction::Propose => {
                false
            }
        };
        if done {
            run.row.intent = None;
            run.dirty = true;
        }
        done
    }

    /// Settles a stuck accept from `listing`: active, it is accepted after
    /// all (published and noted once, on the place it kept); gone, it is
    /// over; still proposed, it waits, and a new vote on it reuses its place.
    fn settle_stuck(&self, e: &EntityId, run: &mut Run, listing: &Listing) {
        let Some(stuck) = run.row.stuck_accept else {
            return;
        };
        let g = stuck.goal;
        match listing.status(g) {
            Some("proposed") => return,
            Some("active") => {
                let title = listing.title(g);
                self.publish(
                    e,
                    "goal",
                    Some(g),
                    "accepted",
                    "accepted",
                    None,
                    title.as_deref(),
                );
                self.note(e, "accepted", g, title);
            }
            _ => self.publish(e, "goal", Some(g), "gone", "gone", None, None),
        }
        run.row.stuck_accept = None;
        run.dirty = true;
    }

    /// Sends the `decide` of `action` (its intent is written) and settles
    /// the intent from the answer. A lost answer keeps the intent for the
    /// next pass.
    async fn send(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        listing: &mut Listing,
        action: IntentAction,
        now: u64,
    ) {
        let (decision, id) = match action {
            IntentAction::Accept { goal } => (PlanDecision::Accept, goal),
            IntentAction::Decline { goal } => (PlanDecision::Decline, goal),
            IntentAction::DropGoal { goal } => (PlanDecision::DropGoal, goal),
            IntentAction::Put { .. } | IntentAction::Activate { .. } | IntentAction::Propose => {
                return;
            }
        };
        let req = OwnerRequest::Decide { decision, id };
        let answer = match owner::request(socket, &req, self.owner_timeout).await {
            Ok(OwnerResponse::Plans { goals, ideas, .. }) => Some(Listing {
                goals,
                ideas: ideas.len(),
            }),
            // "Not a proposed goal": our own late accept, or a human's
            // decision. The listing tells which.
            Ok(_) => {
                let mut view = AutonomyView::default();
                self.read_plans(socket, &mut view).await
            }
            Err(err) => {
                tracing::warn!(entity = %e, error = %err, "a decide got no answer; its intent stays open");
                None
            }
        };
        if let Some(l) = answer {
            *listing = l;
            Self::seen(run, listing, now);
            self.settle_intent(e, run, listing);
        }
    }

    /// Writes the row with a higher rev: durably (`act`, before an owner
    /// action) or not (`persist`). Returns whether the write took.
    fn write(&self, e: &EntityId, run: &mut Run, durable: bool) -> bool {
        run.rev = run.rev.saturating_add(1);
        let op = SideOp::Steward {
            entity: e.clone(),
            rev: run.rev,
            schema: ROW_SCHEMA,
            body: run.row.body(),
        };
        assert_none_held("a steward write");
        if durable {
            if let Err(err) = self.sup.game().act(&[op]) {
                tracing::warn!(entity = %e, error = %err, "a steward intent was not written; nothing is sent");
                return false;
            }
        } else {
            self.sup.game().persist(vec![op]);
        }
        run.dirty = false;
        true
    }

    /// Step 4: decides proposed goals, oldest first, then drops stale
    /// active ones, at most `max_decisions_per_pass` in all.
    #[expect(
        clippy::too_many_arguments,
        reason = "one pass's context, passed down rather than stored"
    )]
    async fn decide(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        listing: &mut Listing,
        now: u64,
        level: BudgetLevel,
    ) {
        let mut decisions = 0;
        let mut proposed: Vec<GoalEntry> = listing
            .goals
            .iter()
            .filter(|g| g.status == "proposed")
            .cloned()
            .collect();
        proposed.sort_by_key(|g| {
            (
                run.row.goals.get(&g.id).map_or(now, |s| s.first_seen_ms),
                g.id,
            )
        });
        for g in proposed {
            if decisions >= self.cfg.max_decisions_per_pass || run.row.intent.is_some() {
                return;
            }
            if listing.status(g.id) != Some("proposed") {
                continue;
            }
            if self
                .one_goal(d, e, socket, run, listing, &g, now, level)
                .await
            {
                decisions += 1;
            }
        }
        let stale_ms = self.cfg.goal_stale_days.saturating_mul(DAY_MS);
        let stale: Vec<(i64, String)> = listing
            .goals
            .iter()
            .filter(|g| g.status == "active")
            .filter(|g| {
                run.row
                    .goals
                    .get(&g.id)
                    .is_some_and(|s| now.saturating_sub(s.note_change_ms) >= stale_ms)
            })
            .map(|g| (g.id, g.title.clone()))
            .collect();
        for (id, title) in stale {
            if decisions >= self.cfg.max_decisions_per_pass || run.row.intent.is_some() {
                return;
            }
            let action = IntentAction::DropGoal { goal: id };
            run.row.open(action.clone(), now);
            if !self.write(e, run, true) {
                run.row.intent = None;
                return;
            }
            self.point(StewardPoint::IntentWritten, e);
            decisions += 1;
            self.publish(
                e,
                "goal",
                Some(id),
                "dropped",
                "dropped_stale",
                None,
                Some(&title),
            );
            self.note(e, "dropped_stale", id, Some(title));
            self.send(e, socket, run, listing, action, now).await;
        }
    }

    /// Decides one proposed goal. Returns whether that was a decision (a
    /// decide sent or an intent written; a wait is none).
    #[expect(
        clippy::too_many_arguments,
        reason = "one pass's context, passed down rather than stored"
    )]
    async fn one_goal(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        listing: &mut Listing,
        g: &GoalEntry,
        now: u64,
        level: BudgetLevel,
    ) -> bool {
        let text = vet::goal_text(
            &g.title,
            &g.why,
            self.cfg.limits(),
            &self.cfg.deny_phrases,
            |digest| run.row.was_declined(digest),
        );
        let cap = self.cfg.max_council_votes_per_day;
        let ballots = run.row.ballots.today(now);
        // A stuck accept of this goal, reserved today, keeps its place: the
        // goal is not counted against itself, and a new accept reuses it.
        let reuse = run.row.stuck_accept.is_some_and(|s| {
            s.goal == g.id && s.day == now / DAY_MS && run.row.accepts.day == s.day
        });
        let can_vote = self.judge.is_some() && level == BudgetLevel::Normal && ballots < cap;
        let first = run.row.goals.get(&g.id).map_or(now, |s| s.first_seen_ms);
        let ask = GoalAsk {
            text_ok: text == TextVerdict::Ok,
            check_minutes: g
                .check_every_minutes
                .map_or(0, |m| u32::try_from(m.max(0)).unwrap_or(u32::MAX)),
            active: listing.active(),
            accepted_today: run.row.accepts.today(now).saturating_sub(u32::from(reuse)),
            can_vote,
            age_ms: now.saturating_sub(first),
        };
        let caps = self.cfg.caps();
        match vet::goal_verdict(ask, caps) {
            GoalVerdict::Wait => {
                let reason = self.wait_reason(ask, caps, level, ballots >= cap);
                self.waiting(e, run, g, reason);
                false
            }
            GoalVerdict::Decline => {
                let code = decline_code(text, ask, caps);
                if text == TextVerdict::Text {
                    run.row.decline(vet::title_digest(&g.title));
                }
                self.decline(e, socket, run, listing, g, code, None, Vec::new(), now)
                    .await
            }
            GoalVerdict::Admit => {
                let (yes, no, reasons) = self.council(d, e, g).await;
                run.row.ballots.cast(now);
                run.dirty = true;
                let votes = Some([yes, no]);
                if vet::council_majority(yes, self.cfg.council_members) {
                    let action = IntentAction::Accept { goal: g.id };
                    let saved = (run.row.accepts, run.row.stuck_accept);
                    if !reuse {
                        run.row.accepts.reserve(now);
                    }
                    if run.row.stuck_accept.is_some_and(|s| s.goal == g.id) {
                        // The new intent settles this goal from now on.
                        run.row.stuck_accept = None;
                    }
                    run.row.open(action.clone(), now);
                    if !self.write(e, run, true) {
                        run.row.intent = None;
                        (run.row.accepts, run.row.stuck_accept) = saved;
                        return false;
                    }
                    self.point(StewardPoint::IntentWritten, e);
                    // The votes and reasons; `accepted` follows once lg
                    // lists the goal as active.
                    self.publish_full(StewardEvent {
                        entity: e.clone(),
                        item: "council".into(),
                        id: Some(g.id),
                        action: "voted".into(),
                        code: "accept".into(),
                        votes,
                        text: Some(cut(&g.title)),
                        reasons,
                    });
                    self.send(e, socket, run, listing, action, now).await;
                    true
                } else {
                    run.row.decline(vet::title_digest(&g.title));
                    self.decline(
                        e,
                        socket,
                        run,
                        listing,
                        g,
                        "declined_council",
                        votes,
                        reasons,
                        now,
                    )
                    .await
                }
            }
        }
    }

    /// Why a goal waits: no judge, the budget, the ballot cap (when a vote
    /// would admit it), or a cap on goals.
    fn wait_reason(
        &self,
        ask: GoalAsk,
        caps: GoalCaps,
        level: BudgetLevel,
        ballots_spent: bool,
    ) -> &'static str {
        let admit_with_a_vote = vet::goal_verdict(
            GoalAsk {
                can_vote: true,
                ..ask
            },
            caps,
        ) == GoalVerdict::Admit;
        if self.judge.is_none() {
            "no_judge"
        } else if level != BudgetLevel::Normal {
            "budget"
        } else if ballots_spent && admit_with_a_vote {
            "ballot_cap"
        } else {
            "caps"
        }
    }

    /// Publishes, once per reason, that a goal waits; on the ballot cap,
    /// also tells the agent, once per goal.
    fn waiting(&self, e: &EntityId, run: &mut Run, g: &GoalEntry, reason: &str) {
        let Some(seen) = run.row.goals.get_mut(&g.id) else {
            return;
        };
        if seen.waiting.as_deref() != Some(reason) {
            seen.waiting = Some(reason.to_owned());
            run.dirty = true;
            self.publish(
                e,
                "goal",
                Some(g.id),
                "waiting",
                reason,
                None,
                Some(&g.title),
            );
        }
        if reason == "ballot_cap" && !seen.cap_noted {
            seen.cap_noted = true;
            run.dirty = true;
            self.note(e, "waiting_cap", g.id, Some(g.title.clone()));
        }
    }

    /// Writes a decline intent and sends it. Returns whether it was a
    /// decision.
    #[expect(
        clippy::too_many_arguments,
        reason = "one decision's context, passed down rather than stored"
    )]
    async fn decline(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        listing: &mut Listing,
        g: &GoalEntry,
        code: &str,
        votes: Option<[u32; 2]>,
        reasons: Vec<String>,
        now: u64,
    ) -> bool {
        let action = IntentAction::Decline { goal: g.id };
        run.row.open(action.clone(), now);
        if !self.write(e, run, true) {
            run.row.intent = None;
            return false;
        }
        self.point(StewardPoint::IntentWritten, e);
        self.publish_full(StewardEvent {
            entity: e.clone(),
            item: "goal".into(),
            id: Some(g.id),
            action: "declined".into(),
            code: code.into(),
            votes,
            text: Some(cut(&g.title)),
            reasons,
        });
        self.note(e, code, g.id, Some(g.title.clone()));
        self.send(e, socket, run, listing, action, now).await;
        true
    }

    /// The council's votes on `g`: yes, no, and the reasons. A vote that
    /// fails is a no; each vote's spend is the supervisor's own (`local`).
    async fn council(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        g: &GoalEntry,
    ) -> (u32, u32, Vec<String>) {
        let Some(judge) = &self.judge else {
            return (0, self.cfg.council_members, Vec::new());
        };
        let brief = d.brief(e).unwrap_or_else(|| PersonaText {
            name: String::new(),
            persona: String::new(),
            goal: String::new(),
            body: String::new(),
        });
        let (mut yes, mut no, mut reasons) = (0, 0, Vec::new());
        for member in 0..self.cfg.council_members {
            let ballot = Ballot::goal(member, &brief, &g.title, &g.why, g.check_every_minutes);
            match judge.vote(&ballot).await {
                Ok((vote, usage)) => {
                    self.sup.record_spend(e, usage);
                    if vote.yes {
                        yes += 1;
                    } else {
                        no += 1;
                    }
                    reasons.extend(vote.reason);
                }
                Err(err) => {
                    tracing::warn!(entity = %e, error = %err, "a council vote failed; it counts as a no");
                    no += 1;
                }
            }
        }
        (yes, no, reasons)
    }

    fn note(&self, e: &EntityId, code: &str, goal: i64, title: Option<String>) {
        self.sup.steward_note(
            e,
            StewardNote {
                code: code.to_owned(),
                goal: Some(goal),
                title: title.map(|t| cut(&t)),
            },
        );
    }

    #[expect(clippy::too_many_arguments, reason = "the fields of one observer line")]
    fn publish(
        &self,
        e: &EntityId,
        item: &str,
        id: Option<i64>,
        action: &str,
        code: &str,
        votes: Option<[u32; 2]>,
        text: Option<&str>,
    ) {
        self.publish_full(StewardEvent {
            entity: e.clone(),
            item: item.to_owned(),
            id,
            action: action.to_owned(),
            code: code.to_owned(),
            votes,
            text: text.map(cut),
            reasons: Vec::new(),
        });
    }

    fn publish_full(&self, event: StewardEvent) {
        self.sup.publish_steward(event);
    }
}

/// How often a drain probes lg's loop after a propose timed out.
pub const DRAIN_EVERY: Duration = Duration::from_secs(10);

/// The longest drain after a propose timed out: one day.
pub const MAX_WAIT: Duration = Duration::from_hours(24);

/// Ends, at this boot, the refine pause of each entity `resume_refine`
/// names, if an earlier boot recorded it; warns about a name that is not
/// paused, so the operator removes it. The rows are written durably.
fn resume_refine(
    sup: &Supervisor,
    cfg: &StewardConfig,
    boot: u64,
    runs: &mut BTreeMap<EntityId, Run>,
) {
    let mut ops = Vec::new();
    for name in &cfg.resume_refine {
        let e = EntityId::from(name.as_str());
        let Some(run) = runs
            .get_mut(&e)
            .filter(|r| r.row.overlay.paused.as_ref().is_some_and(|p| p.boot < boot))
        else {
            tracing::warn!(entity = %e, "resume_refine names an entity whose refine is not paused from an earlier boot; remove it");
            continue;
        };
        let code = run
            .row
            .overlay
            .paused
            .take()
            .map(|p| p.code)
            .unwrap_or_default();
        run.rev = run.rev.saturating_add(1);
        ops.push(SideOp::Steward {
            entity: e.clone(),
            rev: run.rev,
            schema: ROW_SCHEMA,
            body: run.row.body(),
        });
        tracing::info!(entity = %e, %code, "refine resumed by the operator");
        sup.report_steward(StewardEvent {
            entity: e,
            item: "overlay".into(),
            id: None,
            action: "resumed".into(),
            code,
            votes: None,
            text: None,
            reasons: Vec::new(),
        });
    }
    if !ops.is_empty() {
        assert_none_held("a steward write");
        if let Err(err) = sup.game().act(&ops) {
            tracing::warn!(error = %err, "a refine resume was not written; it is retried at the next boot");
        }
    }
}

/// The feedback code of a declined goal.
const fn decline_code(text: TextVerdict, ask: GoalAsk, caps: GoalCaps) -> &'static str {
    match text {
        TextVerdict::Text => "declined_text",
        TextVerdict::Repeat => "declined_repeat",
        TextVerdict::Ok if ask.check_minutes != 0 && ask.check_minutes < caps.min_check => {
            "declined_check_too_frequent"
        }
        TextVerdict::Ok => "declined_stale",
    }
}

fn goal_view(g: &GoalEntry) -> GoalView {
    GoalView {
        id: g.id,
        title: cut(&g.title),
        why: cut(&g.why),
    }
}

#[cfg(test)]
mod free_tests;
#[cfg(test)]
mod refine_tests;
#[cfg(test)]
mod tests;
