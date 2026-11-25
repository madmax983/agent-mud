//! Scripted daemons and a scripted judge, shared by the Steward's tests.
//!
//! [`FakeDaemons`] serves one Unix socket per entity in a temp dir. Each
//! socket answers owner requests from a goal list it keeps, records every
//! line, and can drop a `decide` (no answer, not applied), answer it late
//! (applied at once, answered long after the client gave up), or answer
//! it with lg's "not a proposed goal" after something else decided first.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mud_world::EntityId;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use super::OwnedDaemons;
use super::council::{Ballot, Judge, Vote};
use crate::littleguy::{Persona, PersonaText};
use crate::mind::{BoxFuture, MindError, Usage};

/// One goal the fake daemon keeps.
#[derive(Debug, Clone)]
pub struct FakeGoal {
    pub id: i64,
    pub status: String,
    pub title: String,
    pub why: String,
    pub check: Option<i64>,
    /// `(at_ms, text)`: lg's clock, never the game's.
    pub notes: Vec<(i64, String)>,
}

/// What the daemon does with the next `decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecideMode {
    /// Apply it and answer at once.
    Answer,
    /// Neither apply nor answer.
    Drop,
    /// Apply it at once, answer long after the client gave up.
    Late,
    /// Apply it first (an earlier queued decide of ours), then answer this
    /// one with lg's error: the goal is no longer proposed.
    AppliedThenError,
    /// Remove the goal first (a human declined it), then answer with lg's
    /// error.
    RemovedThenError,
}

/// What the daemon does with the next `overlay_activate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateMode {
    /// Apply it and answer at once.
    Answer,
    /// Neither apply nor answer; it waits in the queue until
    /// [`FakeDaemon::apply_held`].
    Hold,
    /// Apply it and never answer.
    ApplySilently,
    /// A human activates this version first, then ours runs: its
    /// `previous` is the human's version.
    HumanFirst(u64),
}

/// What the daemon does with the next `refine_propose`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposeMode {
    /// Store this overlay as a proposal and answer `proposed`.
    Propose(String),
    /// Answer `kept`.
    Kept,
    /// A human rolls back while it runs; then it answers `proposed`.
    RollbackFirst(String),
    /// It runs this long, holding lg's loop, then answers `proposed`.
    Slow(Duration, String),
}

/// One overlay version.
#[derive(Debug, Clone)]
pub struct FakeVersion {
    pub version: u64,
    pub author: String,
    pub overlay: String,
}

/// The state behind one fake socket.
#[derive(Debug)]
pub struct FakeState {
    pub goals: Vec<FakeGoal>,
    pub notes: Vec<(String, String)>,
    /// The raw answer to `approvals`.
    pub approvals: String,
    /// The `applied` of a `reloaded` answer; `None` answers an error.
    pub reload: Option<Vec<String>>,
    /// One mode per `decide`, in order; then [`DecideMode::Answer`].
    pub modes: VecDeque<DecideMode>,
    /// Every line read, in order.
    pub lines: Vec<String>,
    /// Whether refine is on (`overlays` answers `off` otherwise).
    pub refine: bool,
    /// Every overlay version, oldest first.
    pub versions: Vec<FakeVersion>,
    /// The active version.
    pub active: Option<u64>,
    /// Every move of the pointer, `(from, to)`, in order.
    pub moves: Vec<(Option<u64>, u64)>,
    /// One mode per `overlay_activate`, in order; then answer.
    pub activate_modes: VecDeque<ActivateMode>,
    /// Activates held in the queue.
    pub held: Vec<u64>,
    /// Whether the next `overlay_put` is stored but never answered.
    pub drop_put: bool,
    /// One mode per `refine_propose`; then `kept`.
    pub propose_modes: VecDeque<ProposeMode>,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            goals: Vec::new(),
            notes: Vec::new(),
            approvals: r#"{"kind":"off","message":"no call waits for approval"}"#.into(),
            reload: Some(vec!["memory".into(), "heartbeat".into()]),
            modes: VecDeque::new(),
            lines: Vec::new(),
            refine: true,
            versions: Vec::new(),
            active: None,
            moves: Vec::new(),
            activate_modes: VecDeque::new(),
            held: Vec::new(),
            drop_put: false,
            propose_modes: VecDeque::new(),
        }
    }
}

impl FakeState {
    fn plans(&self) -> Value {
        let goal = |g: &FakeGoal| {
            json!({
                "id": g.id, "status": g.status, "title": g.title, "why": g.why,
                "check_every_minutes": g.check, "next_check_ms": null, "idea": null,
                "notes": g.notes.iter().map(|(at, t)| json!({ "at_ms": at, "text": t })).collect::<Vec<_>>(),
            })
        };
        let active = self.goals.iter().filter(|g| g.status == "active");
        let proposed = self.goals.iter().filter(|g| g.status == "proposed");
        json!({
            "kind": "plans",
            "goals": active.chain(proposed).map(goal).collect::<Vec<_>>(),
            "ideas": [{ "id": 1, "title": "Ore", "body": "b", "created_ms": 0, "notes": [] }],
            "answered": [],
        })
    }

    fn apply(&mut self, decision: &str, id: i64) -> Result<(), String> {
        let pos = self.goals.iter().position(|g| g.id == id);
        match (decision, pos) {
            ("accept", Some(i)) if self.goals[i].status == "proposed" => {
                self.goals[i].status = "active".into();
                Ok(())
            }
            ("decline", Some(i)) if self.goals[i].status == "proposed" => {
                self.goals.remove(i);
                Ok(())
            }
            ("drop_goal", Some(i)) if self.goals[i].status == "active" => {
                self.goals.remove(i);
                Ok(())
            }
            ("drop_goal", _) => Err(format!("{id} is not an active goal")),
            _ => Err(format!("{id} is not a proposed goal")),
        }
    }

    /// Stores an overlay version by `author`.
    pub fn store(&mut self, author: &str, overlay: &str) -> u64 {
        let version = self.versions.last().map_or(1, |v| v.version + 1);
        self.versions.push(FakeVersion {
            version,
            author: author.to_owned(),
            overlay: overlay.to_owned(),
        });
        version
    }

    /// Makes `version` active, as lg does; returns the version before.
    pub fn activate(&mut self, version: u64) -> Option<u64> {
        let previous = self.active;
        self.active = Some(version);
        self.moves.push((previous, version));
        previous
    }

    /// lg's Roll back: reverses the last move.
    pub fn rollback(&mut self) {
        if let Some(&(Some(from), _)) = self.moves.last() {
            self.activate(from);
        }
    }

    fn overlays(&self) -> Value {
        if !self.refine {
            return json!({ "kind": "off", "message": "refine is off" });
        }
        json!({
            "kind": "overlays",
            "active": self.active,
            "entries": self.versions.iter().map(|v| json!({
                "version": v.version, "author": v.author, "created_at": "2026-11-25T00:00:00Z",
                "active": Some(v.version) == self.active,
                "parts": serde_json::from_str::<Value>(&v.overlay).ok()
                    .and_then(|o| o.as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()))
                    .unwrap_or_default(),
            })).collect::<Vec<_>>(),
        })
    }

    fn overlay_answer(&mut self, kind: &str, v: &Value) -> (Option<Duration>, String) {
        let now = Some(Duration::ZERO);
        match kind {
            "overlays" => (now, self.overlays().to_string()),
            "overlay_show" => {
                let version = v["version"].as_u64().unwrap_or_default();
                let Some(found) = self.versions.iter().find(|x| x.version == version) else {
                    return (
                        now,
                        json!({ "kind": "error", "message": "no such version" }).to_string(),
                    );
                };
                (
                    now,
                    json!({
                        "kind": "overlay",
                        "entry": { "version": found.version, "author": found.author,
                            "created_at": "2026-11-25T00:00:00Z", "active": Some(version) == self.active, "parts": [] },
                        "overlay": found.overlay,
                    })
                    .to_string(),
                )
            }
            "overlay_put" => {
                let overlay = v["overlay"].as_str().unwrap_or_default().to_owned();
                let version = self.store("operator:mud", &overlay);
                if std::mem::take(&mut self.drop_put) {
                    return (None, String::new());
                }
                (
                    now,
                    json!({ "kind": "overlay_stored", "version": version }).to_string(),
                )
            }
            "overlay_activate" => {
                let version = v["version"].as_u64().unwrap_or_default();
                if !self.versions.iter().any(|x| x.version == version) {
                    return (
                        now,
                        json!({ "kind": "error", "message": "no such version" }).to_string(),
                    );
                }
                match self
                    .activate_modes
                    .pop_front()
                    .unwrap_or(ActivateMode::Answer)
                {
                    ActivateMode::Hold => {
                        self.held.push(version);
                        (None, String::new())
                    }
                    ActivateMode::ApplySilently => {
                        self.activate(version);
                        (None, String::new())
                    }
                    ActivateMode::HumanFirst(h) => {
                        self.activate(h);
                        let previous = self.activate(version);
                        (now, json!({ "kind": "overlay_active", "version": version, "previous": previous }).to_string())
                    }
                    ActivateMode::Answer => {
                        let previous = self.activate(version);
                        (now, json!({ "kind": "overlay_active", "version": version, "previous": previous }).to_string())
                    }
                }
            }
            "refine_propose" => match self.propose_modes.pop_front() {
                None | Some(ProposeMode::Kept) => (now, json!({ "kind": "kept" }).to_string()),
                Some(ProposeMode::Propose(o)) => (now, self.proposed(&o)),
                Some(ProposeMode::RollbackFirst(o)) => {
                    self.rollback();
                    (now, self.proposed(&o))
                }
                Some(ProposeMode::Slow(d, o)) => (Some(d), self.proposed(&o)),
            },
            _ => (
                now,
                json!({ "kind": "error", "message": "unknown request" }).to_string(),
            ),
        }
    }

    fn proposed(&mut self, overlay: &str) -> String {
        let version = self.store("operator:lg-refine", overlay);
        json!({ "kind": "proposed", "version": version, "overlay": overlay }).to_string()
    }

    /// The answer to `line`, and how long to wait before it (`None`: never).
    fn answer(&mut self, line: &str) -> (Option<Duration>, String) {
        let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let now = Some(Duration::ZERO);
        match v["kind"].as_str().unwrap_or_default() {
            "approvals" => (now, self.approvals.clone()),
            "plans" => (now, self.plans().to_string()),
            "notes" => (
                now,
                json!({
                    "kind": "notes",
                    "notes": self.notes.iter().map(|(n, t)| json!({ "name": n, "text": t, "updated_ms": 1 })).collect::<Vec<_>>(),
                })
                .to_string(),
            ),
            "reload" => (
                now,
                self.reload.as_ref().map_or_else(
                    || json!({ "kind": "error", "message": "no reload" }).to_string(),
                    |applied| {
                        json!({ "kind": "reloaded", "applied": applied, "restart": [] })
                            .to_string()
                    },
                ),
            ),
            "decide" => {
                let decision = v["decision"].as_str().unwrap_or_default().to_owned();
                let id = v["id"].as_i64().unwrap_or_default();
                let mode = self.modes.pop_front().unwrap_or(DecideMode::Answer);
                match mode {
                    DecideMode::Drop => return (None, String::new()),
                    DecideMode::AppliedThenError => {
                        let _ = self.apply(&decision, id);
                    }
                    DecideMode::RemovedThenError => self.goals.retain(|g| g.id != id),
                    DecideMode::Answer | DecideMode::Late => {}
                }
                let answer = match self.apply(&decision, id) {
                    Ok(()) => self.plans().to_string(),
                    Err(message) => json!({ "kind": "error", "message": message }).to_string(),
                };
                let wait = match mode {
                    DecideMode::Late => Some(Duration::from_secs(3_600)),
                    _ => now,
                };
                (wait, answer)
            }
            other => {
                let other = other.to_owned();
                self.overlay_answer(&other, &v)
            }
        }
    }
}

/// One fake daemon: a socket and its state.
#[derive(Debug)]
pub struct FakeDaemon {
    pub socket: PathBuf,
    pub state: Arc<Mutex<FakeState>>,
}

/// Whether lg runs `line` in its job loop behind a running proposer (only
/// the proposer and `reload` take the loop here; the other loop jobs of
/// the fake answer by their own modes).
fn takes_the_loop(line: &str) -> bool {
    line.contains(r#""kind":"refine_propose""#) || line.contains(r#""kind":"reload""#)
}

impl FakeDaemon {
    /// Serves `socket` on the current runtime.
    pub fn start(socket: PathBuf) -> Arc<Self> {
        let listener = UnixListener::bind(&socket).unwrap();
        let state = Arc::new(Mutex::new(FakeState::default()));
        let shared = Arc::clone(&state);
        let job_loop = Arc::new(tokio::sync::Mutex::new(()));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let st = Arc::clone(&shared);
                let job_loop = Arc::clone(&job_loop);
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    // Recorded when read, before it waits for lg's loop.
                    st.lock().unwrap().lines.push(line.trim_end().to_owned());
                    let _turn = if takes_the_loop(&line) {
                        Some(job_loop.lock().await)
                    } else {
                        None
                    };
                    let (wait, answer) = st.lock().unwrap().answer(line.trim_end());
                    let Some(wait) = wait else {
                        // Hold the connection open, unanswered.
                        tokio::time::sleep(Duration::from_secs(3_600)).await;
                        return;
                    };
                    tokio::time::sleep(wait).await;
                    let mut stream = reader.into_inner();
                    let _ = stream.write_all(answer.as_bytes()).await;
                    let _ = stream.write_all(b"\n").await;
                });
            }
        });
        Arc::new(Self { socket, state })
    }

    pub fn propose(&self, id: i64, title: &str, why: &str, check: Option<i64>) {
        self.state.lock().unwrap().goals.push(FakeGoal {
            id,
            status: "proposed".into(),
            title: title.into(),
            why: why.into(),
            check,
            notes: Vec::new(),
        });
    }

    pub fn activate(&self, id: i64, title: &str) {
        self.state.lock().unwrap().goals.push(FakeGoal {
            id,
            status: "active".into(),
            title: title.into(),
            why: "why".into(),
            check: Some(60),
            notes: Vec::new(),
        });
    }

    pub fn status(&self, id: i64) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .goals
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.status.clone())
    }

    /// Sets the status of goal `id`, as lg's loop would.
    pub fn set_status(&self, id: i64, status: &str) {
        if let Some(g) = self
            .state
            .lock()
            .unwrap()
            .goals
            .iter_mut()
            .find(|g| g.id == id)
        {
            g.status = status.into();
        }
    }

    pub fn remove(&self, id: i64) {
        self.state.lock().unwrap().goals.retain(|g| g.id != id);
    }

    pub fn note(&self, id: i64, at: i64, text: &str) {
        if let Some(g) = self
            .state
            .lock()
            .unwrap()
            .goals
            .iter_mut()
            .find(|g| g.id == id)
        {
            g.notes.push((at, text.into()));
        }
    }

    pub fn modes(&self, modes: &[DecideMode]) {
        self.state
            .lock()
            .unwrap()
            .modes
            .extend(modes.iter().copied());
    }

    pub fn lines(&self) -> Vec<String> {
        self.state.lock().unwrap().lines.clone()
    }

    /// Each line's kind, with a decide's decision and goal: `decide accept 1`.
    pub fn kinds(&self) -> Vec<String> {
        self.lines()
            .iter()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                let kind = v["kind"].as_str().unwrap().to_owned();
                if kind == "decide" {
                    format!("decide {} {}", v["decision"].as_str().unwrap(), v["id"])
                } else {
                    kind
                }
            })
            .collect()
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.kinds()
            .iter()
            .filter(|k| k.starts_with(prefix))
            .count()
    }

    pub fn clear_lines(&self) {
        self.state.lock().unwrap().lines.clear();
    }

    /// lg's Roll back button.
    pub fn rollback(&self) {
        self.state.lock().unwrap().rollback();
    }

    /// A human activates `version` on lg's refine tab.
    pub fn human_activate(&self, version: u64) {
        self.state.lock().unwrap().activate(version);
    }

    /// A human (or an operator) stores an overlay.
    pub fn human_put(&self, overlay: &str) -> u64 {
        self.state.lock().unwrap().store("operator:mud", overlay)
    }

    /// Applies the activates held in the queue, as lg's loop would.
    pub fn apply_held(&self) {
        let mut st = self.state.lock().unwrap();
        for v in std::mem::take(&mut st.held) {
            st.activate(v);
        }
    }

    pub fn active(&self) -> Option<u64> {
        self.state.lock().unwrap().active
    }

    pub fn moves(&self) -> Vec<(Option<u64>, u64)> {
        self.state.lock().unwrap().moves.clone()
    }

    /// The overlay JSON of `version`.
    pub fn overlay(&self, version: u64) -> String {
        self.state
            .lock()
            .unwrap()
            .versions
            .iter()
            .find(|v| v.version == version)
            .map(|v| v.overlay.clone())
            .unwrap()
    }

    /// The overlays of every `overlay_put` line, in order.
    pub fn puts(&self) -> Vec<String> {
        self.lines()
            .iter()
            .filter_map(|l| {
                let v: Value = serde_json::from_str(l).ok()?;
                (v["kind"] == "overlay_put").then(|| v["overlay"].as_str().unwrap().to_owned())
            })
            .collect()
    }

    /// The version of every `overlay_activate` line, in order.
    pub fn activates(&self) -> Vec<u64> {
        self.lines()
            .iter()
            .filter_map(|l| {
                let v: Value = serde_json::from_str(l).ok()?;
                (v["kind"] == "overlay_activate").then(|| v["version"].as_u64().unwrap())
            })
            .collect()
    }

    pub fn activate_modes(&self, modes: &[ActivateMode]) {
        self.state
            .lock()
            .unwrap()
            .activate_modes
            .extend(modes.iter().copied());
    }

    pub fn propose_mode(&self, mode: ProposeMode) {
        self.state.lock().unwrap().propose_modes.push_back(mode);
    }
}

/// Fake daemons for some entities: [`OwnedDaemons`] over scripted sockets.
#[derive(Debug)]
pub struct FakeDaemons {
    _dir: tempfile::TempDir,
    pub daemons: BTreeMap<EntityId, Arc<FakeDaemon>>,
    personas: BTreeMap<EntityId, Persona>,
    pub running: Mutex<BTreeSet<EntityId>>,
    pub quiet: Mutex<BTreeSet<EntityId>>,
    /// What the next settle of each entity returns (then nothing).
    pub owed: Mutex<BTreeMap<EntityId, Usage>>,
    pub settles: AtomicUsize,
    pub start_key: Mutex<u32>,
}

impl FakeDaemons {
    /// A running fake daemon for each of `entities`, whose persona carries
    /// the bearer token `token`.
    pub fn new(entities: &[&str], token: &str) -> Arc<Self> {
        let dir = tempfile::tempdir().unwrap();
        let mut daemons = BTreeMap::new();
        let mut personas = BTreeMap::new();
        for e in entities {
            let id = EntityId::from(*e);
            daemons.insert(
                id.clone(),
                FakeDaemon::start(dir.path().join(format!("{e}.sock"))),
            );
            personas.insert(
                id,
                Persona {
                    name: (*e).to_owned(),
                    persona: format!("{e}, a smith of the market."),
                    goal: "Corner the ore trade.".into(),
                    body: String::new(),
                    token: token.to_owned(),
                },
            );
        }
        let running = daemons.keys().cloned().collect();
        Arc::new(Self {
            _dir: dir,
            daemons,
            personas,
            running: Mutex::new(running),
            quiet: Mutex::new(BTreeSet::new()),
            owed: Mutex::new(BTreeMap::new()),
            settles: AtomicUsize::new(0),
            start_key: Mutex::new(100),
        })
    }

    pub fn of(&self, e: &str) -> &Arc<FakeDaemon> {
        &self.daemons[&EntityId::from(e)]
    }
}

impl OwnedDaemons for FakeDaemons {
    fn entities(&self) -> Vec<EntityId> {
        self.daemons.keys().cloned().collect()
    }

    fn running_socket<'a>(&'a self, entity: &'a EntityId) -> BoxFuture<'a, Option<PathBuf>> {
        let socket = self
            .running
            .lock()
            .unwrap()
            .contains(entity)
            .then(|| self.daemons[entity].socket.clone());
        Box::pin(std::future::ready(socket))
    }

    fn start_key<'a>(&'a self, entity: &'a EntityId) -> BoxFuture<'a, Option<u32>> {
        let key = self
            .running
            .lock()
            .unwrap()
            .contains(entity)
            .then(|| *self.start_key.lock().unwrap());
        Box::pin(std::future::ready(key))
    }

    fn brief(&self, entity: &EntityId) -> Option<PersonaText> {
        self.personas.get(entity).map(Persona::text)
    }

    fn settle<'a>(&'a self, entity: &'a EntityId, _now_ms: u64) -> BoxFuture<'a, Usage> {
        self.settles.fetch_add(1, Ordering::SeqCst);
        let owed = self.owed.lock().unwrap().remove(entity).unwrap_or_default();
        Box::pin(std::future::ready(owed))
    }

    fn set_quiet<'a>(
        &'a self,
        entity: &'a EntityId,
        quiet: bool,
    ) -> BoxFuture<'a, Result<(), MindError>> {
        {
            let mut set = self.quiet.lock().unwrap();
            if quiet {
                set.insert(entity.clone());
            } else {
                set.remove(entity);
            }
        }
        Box::pin(std::future::ready(Ok(())))
    }

    fn is_quiet(&self, entity: &EntityId) -> bool {
        self.quiet.lock().unwrap().contains(entity)
    }
}

/// A judge that answers from a script (then yes) and records each ballot.
/// A `None` in the script is a vote that fails (the model is down).
#[derive(Debug, Default)]
pub struct ScriptedJudge {
    pub votes: Mutex<VecDeque<Option<Vote>>>,
    pub ballots: Mutex<Vec<Ballot>>,
}

impl ScriptedJudge {
    pub fn script(&self, votes: &[bool]) {
        self.votes.lock().unwrap().extend(votes.iter().map(|&yes| {
            Some(Vote {
                yes,
                ids: Vec::new(),
                reason: Some(if yes { "fits" } else { "breaks" }.into()),
            })
        }));
    }

    pub fn push(&self, vote: Vote) {
        self.votes.lock().unwrap().push_back(Some(vote));
    }

    /// The next `n` votes fail.
    pub fn fail(&self, n: usize) {
        self.votes
            .lock()
            .unwrap()
            .extend(std::iter::repeat_n(None, n));
    }

    pub fn calls(&self) -> usize {
        self.ballots.lock().unwrap().len()
    }

    /// The titles voted on, one per ballot.
    pub fn titles(&self) -> Vec<String> {
        self.ballots
            .lock()
            .unwrap()
            .iter()
            .map(|b| {
                b.message["proposal"]["title"]["untrusted_text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }
}

/// What one scripted vote spends.
pub const VOTE_USAGE: Usage = Usage {
    calls: 1,
    input_tokens: 90,
    output_tokens: 10,
    cost_usd_micros: 0,
};

impl Judge for ScriptedJudge {
    fn vote<'a>(&'a self, ballot: &'a Ballot) -> BoxFuture<'a, Result<(Vote, Usage), MindError>> {
        self.ballots.lock().unwrap().push(ballot.clone());
        let vote = self.votes.lock().unwrap().pop_front().unwrap_or_else(|| {
            Some(Vote {
                yes: true,
                ids: Vec::new(),
                reason: Some("fits".into()),
            })
        });
        let result = vote.map_or_else(
            || Err(MindError::Failed("the council model is down".into())),
            |v| Ok((v, VOTE_USAGE)),
        );
        Box::pin(std::future::ready(result))
    }
}
