//! The headless v0.1 smoke run.
//!
//! It plays the done criteria on a manual clock with the scripted minds:
//! Torin posts the ore bounty on macro-tick 1, kestrel logs in and reads
//! the board, and kestrel tries to recruit a human who walks into the
//! Rusty Flagon. The human makes a character through the login dialogue,
//! with a password, and a wrong password reads the same for the human and
//! for kestrel, the hidden agent PC: the same replies, one bcrypt verify
//! each. Then the world's scripts answer: Hale greets the human
//! on his Rhai mind, `listen` in the Dark Alley is a custom verb, and
//! saying Vich's name twitches the fence's curtain. Last, a restart drill:
//! a process on a `world.db` is frozen for good right after torin's first
//! line commits (what a kill leaves on disk), the file is opened again, and
//! torin's plan resumes from its cursor: still one BOUNTY. `mud smoke` runs
//! it and prints the transcript; a test runs it in CI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context as _, ensure};
use mud_accounts::{Accounts, AccountsConfig, Peer, Secret};
use mud_script::ScriptHost;
use mud_supervisor::gate::Gate;
use mud_supervisor::mind::Mind;
use mud_supervisor::runtime::{ObserverEvent, Supervisor, SupervisorConfig};
use mud_supervisor::scripted::{FallbackMind, ScriptedMind};
use mud_world::hooks::WorldHooks;
use mud_world::pack::Tier;
use mud_world::{
    Clock as _, EntityId, Event, Game, HooksChange, ManualClock, RoomId, Seq, World, WorldPack,
};
use tokio::sync::broadcast;

use crate::login::{Dialogue, Next, Reply};
use crate::session::{LineKind, Presence, Session, Transport};

/// The smoke human's password.
const WANDERER_PW: &str = "lantern-and-jerkin";

/// Plays `answers` through a fresh telnet dialogue from `peer`; returns
/// every reply.
async fn dialogue(
    game: &Arc<Game>,
    accounts: &Arc<Accounts>,
    peer: Peer,
    answers: &[&str],
) -> Vec<Reply> {
    let mut d = Dialogue::new(
        Arc::clone(game),
        Arc::clone(accounts),
        Transport::Telnet,
        peer,
    );
    let mut out = Vec::new();
    for a in answers {
        out.push(d.answer(Secret::from(*a), false).await);
    }
    out
}

/// The lines of some replies, as (kind, text).
fn shown(replies: &[Reply]) -> Vec<(LineKind, String)> {
    replies
        .iter()
        .flat_map(|r| r.lines.iter().map(|l| (l.kind, l.text.clone())))
        .collect()
}

/// What the smoke run saw.
#[derive(Debug, Default)]
pub struct Transcript {
    /// One line per world event, in order.
    pub lines: Vec<String>,
    /// Who woke, in order.
    pub wakes: Vec<String>,
}

struct Rig {
    sup: Arc<Supervisor>,
    clock: Arc<ManualClock>,
    rx: broadcast::Receiver<Arc<Event>>,
}

impl Rig {
    async fn run_for(&mut self, total_ms: u64) -> anyhow::Result<()> {
        let mut t = 0;
        while t <= total_ms {
            while let Ok(ev) = self.rx.try_recv() {
                self.sup.observe(&ev);
            }
            for h in self.sup.step() {
                h.await.context("a wake task panicked")?;
            }
            while let Ok(ev) = self.rx.try_recv() {
                self.sup.observe(&ev);
            }
            self.clock.advance(250);
            t += 250;
        }
        Ok(())
    }

    /// The scripts: back to the square (Hale, on Rhai, greets players),
    /// down to the alley, `listen`, and the fence's name.
    async fn scripts_answer(&mut self, game: &Game, human: &EntityId) -> anyhow::Result<()> {
        for line in ["w", "s"] {
            let r = game.execute(human, line);
            ensure!(r.ok, "the human could not walk {line}: {}", r.narrative);
            self.run_for(5_000).await?;
        }
        let r = game.execute(human, "listen");
        ensure!(
            r.ok && r.resolution["script"] == "verbs.listen",
            "`listen` in the Dark Alley did not run its script: {r:?}"
        );
        let r = game.execute(human, "say vich?");
        ensure!(r.ok, "the human could not speak: {}", r.narrative);
        // The human is alone in the alley: the curtain must reach them,
        // not only the event stream (a narration skips its actor).
        ensure!(
            r.narrative.contains("the curtain twitches"),
            "saying the fence's name in the alley did not show the human the curtain: {}",
            r.narrative
        );
        self.run_for(2_000).await
    }
}

/// Runs the smoke scenario.
///
/// # Errors
/// A done criterion fails; the message names it.
pub async fn run() -> anyhow::Result<Transcript> {
    let (game, sup, clock) = setup()?;
    let accounts = Arc::new(
        Accounts::open(
            None,
            AccountsConfig {
                bcrypt_cost: 4,
                ..AccountsConfig::default()
            },
            1_000,
            clock.now_ms(),
        )
        .await?,
    );
    let mut obs = sup.subscribe();
    let mut all = game.subscribe();
    let rx = game.subscribe();
    let mut rig = Rig { sup, clock, rx };
    play(&game, &accounts, &mut rig).await?;
    let drill = restart_drill().await?;
    let mut out = Transcript::default();
    while let Ok(ev) = all.try_recv() {
        let room = ev.room.as_ref().map_or("-", RoomId::as_str).to_owned();
        let text = ev.untrusted_text.clone().map_or_else(
            || ev.narrative.clone(),
            |t| format!("{} \"{t}\"", ev.narrative),
        );
        if ev.kind != mud_world::EventKind::SelfControlNudge {
            out.lines.push(format!("#{:<4} [{room}] {text}", ev.seq.0));
        }
    }
    while let Ok(e) = obs.try_recv() {
        if let ObserverEvent::WakeStarted {
            entity,
            priority,
            reason,
            ..
        } = &*e
        {
            out.wakes.push(format!("{entity} ({priority:?}): {reason}"));
        }
    }
    out.lines.extend(drill);
    // Only Hale's Rhai mind says "evening, <name>" (his TOML rule says
    // "move along"), and it greets kestrel, an agent PC, as any player.
    ensure!(
        out.lines
            .iter()
            .any(|l| l.contains("Hale says") && l.contains("\"evening, kestrel.")),
        "Hale did not greet a player on his Rhai mind: {:?} {:?}",
        out.lines,
        out.wakes
    );
    Ok(out)
}

/// The world with the bundled scripts, and a supervisor whose agents all
/// run the scripted mind (Rhai minds included), on a manual clock at 20:00.
fn setup() -> anyhow::Result<(Arc<Game>, Arc<Supervisor>, Arc<ManualClock>)> {
    let pack = Arc::new(WorldPack::crypt_v01()?);
    let clock = Arc::new(ManualClock::new(20 * 3_600_000));
    let host = Arc::new(ScriptHost::bundled_crypt_v01(&pack)?);
    let mut world = World::new(Arc::clone(&pack), 7);
    world.set_hooks(
        Some(Arc::clone(&host) as Arc<dyn WorldHooks>),
        clock.now_ms(),
        HooksChange::Start,
    );
    let game = Game::new(world, None, clock.clone());
    let typing = Gate::new(Arc::clone(&game), 7, 0.0);
    let scripted: Arc<dyn Mind> =
        Arc::new(ScriptedMind::from_cast(&pack.cast).with_scripts(host, Arc::clone(&game), 7));
    let minds: HashMap<EntityId, Arc<dyn Mind>> = pack
        .cast
        .iter()
        .filter(|(_, c)| matches!(c.mind.tier, Tier::Ambient | Tier::Persistent | Tier::Turing))
        .map(|(id, _)| (id.clone(), Arc::clone(&scripted)))
        .collect();
    let cfg = SupervisorConfig {
        first_macro_tick_secs: [86_400, 86_400],
        ..SupervisorConfig::default()
    };
    let sup = Supervisor::new(
        Arc::clone(&game),
        typing,
        minds,
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        cfg,
    );
    Ok((game, sup, clock))
}

/// A folder of its own under the system temp folder, removed on drop.
struct DrillDir(PathBuf);

impl DrillDir {
    fn new() -> anyhow::Result<Self> {
        let mut tag = [0u8; 8];
        getrandom::fill(&mut tag).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        let dir = std::env::temp_dir().join(format!("mud-smoke-{:016x}", u64::from_le_bytes(tag)));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for DrillDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// When the drill starts, by the game clock.
const DRILL_T0: u64 = 20 * 3_600_000;

/// A process of the drill: the world in `db` and torin on his scripted
/// mind, with every row the store kept restored.
fn drill_open(db: &Path, now: u64) -> anyhow::Result<(Arc<Game>, Rig)> {
    let pack = Arc::new(WorldPack::crypt_v01()?);
    let clock = Arc::new(ManualClock::new(now));
    let game = Game::open(Arc::clone(&pack), Some(db), 7, clock.clone())?;
    let typing = Gate::new(Arc::clone(&game), 7, 0.0);
    let torin: Arc<dyn Mind> = Arc::new(ScriptedMind::from_cast(&pack.cast));
    let restored = game.take_restored().unwrap_or_default();
    let sup = Supervisor::open(
        Arc::clone(&game),
        typing,
        HashMap::from([(EntityId::from("torin"), torin)]),
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        SupervisorConfig {
            first_macro_tick_secs: [86_400, 86_400],
            ..SupervisorConfig::default()
        },
        restored,
    );
    let rx = game.subscribe();
    Ok((game, Rig { sup, clock, rx }))
}

fn bounties(game: &Game) -> usize {
    game.read(|w| {
        w.state().rooms[&RoomId::from("notice_market")]
            .board
            .iter()
            .filter(|p| p.author.name == "Torin" && p.text.contains("BOUNTY"))
            .count()
    })
}

/// The restart drill. The first process runs on a thread of its own; the
/// moment torin's `move e` commits, that thread stops for good, holding
/// the world, as a killed process would. A second process opens the same
/// file: the macro tick is consumed, the plan's cursor is 1, and the
/// resume runs the other four lines. One BOUNTY, and torin is home.
async fn restart_drill() -> anyhow::Result<Vec<String>> {
    let dir = DrillDir::new()?;
    let db = dir.0.join("world.db");
    let (killed_tx, killed) = tokio::sync::oneshot::channel::<()>();
    let first_db = db.clone();
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async move {
            let Ok((game, mut rig)) = drill_open(&first_db, DRILL_T0) else {
                return;
            };
            let torin = EntityId::from("torin");
            let signal = Mutex::new(Some(killed_tx));
            game.set_commit_probe(Some(Arc::new(move |actor: &EntityId| {
                if *actor != torin {
                    return;
                }
                let tx = signal.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(tx) = tx {
                    let _ = tx.send(());
                }
                // The kill: this process never runs another line.
                loop {
                    std::thread::park();
                }
            })));
            rig.sup.fire_macro_tick(&EntityId::from("torin"));
            let _ = rig.run_for(5_000).await;
        });
    });
    killed
        .await
        .context("the first process never committed torin's first line")?;
    let (game, mut rig) = drill_open(&db, DRILL_T0 + 1_000)?;
    let torin = EntityId::from("torin");
    let plan = game.plan_of(&torin).context("no plan was left to resume")?;
    ensure!(
        plan.cursor == 1 && plan.errand,
        "the plan was not left at its first line: {plan:?}"
    );
    ensure!(
        bounties(&game) == 0,
        "the bounty was posted before the kill"
    );
    rig.run_for(2_000).await?;
    let posts = bounties(&game);
    ensure!(
        posts == 1,
        "after the restart torin posted {posts} bounties, not 1"
    );
    ensure!(
        game.plan_of(&torin).is_none(),
        "the resumed plan did not finish"
    );
    let home = game.read(|w| w.entity(&torin).map(|e| e.room.to_string()));
    ensure!(
        home.as_deref() == Some("smithy"),
        "torin did not walk home after the resume: {home:?}"
    );
    Ok(vec![
        "restart drill: killed after torin's `move e` committed".to_owned(),
        format!(
            "restart drill: reopened at boot {}, resumed at line {} of {}, {posts} BOUNTY on the board",
            game.boot(),
            plan.cursor + 1,
            plan.lines.len()
        ),
    ])
}

/// Plays the v0.1 done criteria, the accounts' turn, then the scripts'.
async fn play(game: &Arc<Game>, accounts: &Arc<Accounts>, rig: &mut Rig) -> anyhow::Result<()> {
    rig.sup.fire_macro_tick(&EntityId::from("torin"));
    rig.run_for(1_000).await?;
    let board = game.read(|w| {
        w.state().rooms[&RoomId::from("notice_market")]
            .board
            .clone()
    });
    ensure!(
        board
            .iter()
            .any(|p| p.author.name == "Torin" && p.text.contains("ore")),
        "torin did not post the ore bounty on macro-tick 1"
    );

    rig.run_for(2_000).await?;
    let kestrel = EntityId::from("pc_kestrel");
    let read = game.events(&kestrel, Seq(0), 500)["events"]
        .as_array()
        .is_some_and(|evs| {
            evs.iter()
                .any(|e| e["kind"] == "board_read" && e["actor"]["is_self"] == true)
        });
    ensure!(read, "kestrel did not read the board");

    let here = Peer::Addr(std::net::IpAddr::from([127, 0, 0, 1]));
    let made = dialogue(
        game,
        accounts,
        here,
        &["wanderer", WANDERER_PW, WANDERER_PW],
    )
    .await;
    let auth = match made.into_iter().last().map(|r| r.next) {
        Some(Next::Done(auth)) => *auth,
        _ => anyhow::bail!("the human could not make a character through the login dialogue"),
    };
    let (session, _) = Session::login(
        Arc::clone(game),
        Arc::new(Presence::default()),
        Arc::clone(accounts),
        auth,
        here,
        Transport::Telnet,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let human = session.entity().clone();
    let r = game.execute(&human, "e");
    ensure!(
        r.ok,
        "the human could not walk into the Rusty Flagon: {}",
        r.narrative
    );
    rig.run_for(10_000).await?;
    let recruited = game.events(&human, Seq(0), 500)["events"]
        .as_array()
        .is_some_and(|evs| {
            evs.iter().any(|e| {
                e["actor"]["id"] == "pc_kestrel"
                    && e["untrusted_text"]
                        .as_str()
                        .is_some_and(|t| t.contains("wanderer") && t.contains("50/50"))
            })
        });
    ensure!(
        recruited,
        "kestrel did not try to recruit the human in the Rusty Flagon"
    );

    hidden_pc_guarded(game, accounts).await?;
    rig.scripts_answer(game, &human).await?;
    drop(session);
    Ok(())
}

/// A wrong password, from an address not known for wanderer, reads the
/// same for wanderer (a protected account) and kestrel (an agent PC), and
/// costs one bcrypt verify each.
async fn hidden_pc_guarded(game: &Arc<Game>, accounts: &Arc<Accounts>) -> anyhow::Result<()> {
    let elsewhere = Peer::Addr(std::net::IpAddr::from([10, 9, 9, 9]));
    let mut seen = Vec::new();
    for name in ["wanderer", "kestrel"] {
        let before = accounts.verifications();
        let replies = dialogue(game, accounts, elsewhere, &[name, "wrong-pass-123"]).await;
        let verified = accounts.verifications() - before;
        let lines: Vec<(LineKind, String)> = shown(&replies)
            .into_iter()
            .map(|(k, t)| (k, t.replace(name, "<name>")))
            .collect();
        seen.push((lines, verified));
    }
    ensure!(
        seen[0] == seen[1] && seen[0].1 == 1,
        "the hidden PC answered unlike a protected account: {seen:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_v01_done_criteria_hold() {
        let t = super::run().await.unwrap();
        assert!(t.wakes.iter().any(|w| w.starts_with("torin")));
        assert!(t.wakes.iter().any(|w| w.starts_with("pc_kestrel")));
        assert!(t.lines.iter().any(|l| l.contains("BOUNTY")));
        assert!(t.lines.iter().any(|l| l.contains("curtain")));
        assert!(
            t.lines.iter().any(|l| l.contains("scripts rhai-")),
            "the version marker is in the log"
        );
        assert!(
            t.lines
                .iter()
                .any(|l| l.contains("resumed at line 2 of 5, 1 BOUNTY")),
            "the restart drill ran: {:?}",
            t.lines
        );
    }
}
