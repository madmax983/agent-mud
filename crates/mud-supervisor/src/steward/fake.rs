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

    /// The answer to `line`, and how long to wait before it (`None`: never).
    fn answer(&mut self, line: &str) -> (Option<Duration>, String) {
        self.lines.push(line.to_owned());
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
            _ => (
                now,
                json!({ "kind": "error", "message": "unknown request" }).to_string(),
            ),
        }
    }
}

/// One fake daemon: a socket and its state.
#[derive(Debug)]
pub struct FakeDaemon {
    pub socket: PathBuf,
    pub state: Arc<Mutex<FakeState>>,
}

impl FakeDaemon {
    /// Serves `socket` on the current runtime.
    pub fn start(socket: PathBuf) -> Arc<Self> {
        let listener = UnixListener::bind(&socket).unwrap();
        let state = Arc::new(Mutex::new(FakeState::default()));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let st = Arc::clone(&shared);
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
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
