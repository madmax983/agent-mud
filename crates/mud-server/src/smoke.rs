//! The headless v0.1 smoke run.
//!
//! It plays the done criteria on a manual clock with the scripted minds:
//! Torin posts the ore bounty on macro-tick 1, kestrel logs in and reads
//! the board, and kestrel tries to recruit a human who walks into the
//! Rusty Flagon. Then the world's scripts answer: Hale greets the human
//! on his Rhai mind, `listen` in the Dark Alley is a custom verb, and
//! saying Vich's name twitches the fence's curtain. `mud smoke` runs it and
//! prints the transcript; a test runs it in CI.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, ensure};
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
        self.run_for(2_000).await
    }
}

/// Runs the smoke scenario.
///
/// # Errors
/// A done criterion fails; the message names it.
pub async fn run() -> anyhow::Result<Transcript> {
    let (game, sup, clock) = setup()?;
    let mut obs = sup.subscribe();
    let mut all = game.subscribe();
    let rx = game.subscribe();
    let mut rig = Rig { sup, clock, rx };
    play(&game, &mut rig).await?;
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
    ensure!(
        out.lines
            .iter()
            .any(|l| l.contains("The curtain over the east door twitches")),
        "saying the fence's name in the alley did not stir the curtain"
    );
    // Only Hale's Rhai mind says "evening, <name>" (his TOML rule says
    // "move along"), and it greets kestrel, an agent PC, as any player.
    ensure!(
        out.lines
            .iter()
            .any(|l| l.contains("Hale says") && l.contains("\"evening, kestrel.")),
        "Hale did not greet a player on his Rhai mind"
    );
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

/// Plays the v0.1 done criteria, then the scripts' turn.
async fn play(game: &Game, rig: &mut Rig) -> anyhow::Result<()> {
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

    let human = game.login("wanderer").map_err(|e| anyhow::anyhow!("{e}"))?;
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

    rig.scripts_answer(game, &human).await
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
    }
}
