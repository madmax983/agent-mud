//! The headless v0.1 smoke run.
//!
//! It plays the done criteria on a manual clock with the scripted minds:
//! Torin posts the ore bounty on macro-tick 1, kestrel logs in and reads
//! the board, and kestrel tries to recruit a human who walks into the
//! Rusty Flagon. `mud smoke` runs it and prints the transcript; a test
//! runs it in CI.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, ensure};
use mud_supervisor::gate::Gate;
use mud_supervisor::mind::Mind;
use mud_supervisor::runtime::{ObserverEvent, Supervisor, SupervisorConfig};
use mud_supervisor::scripted::{FallbackMind, ScriptedMind};
use mud_world::pack::Tier;
use mud_world::{EntityId, Event, Game, ManualClock, RoomId, Seq, World, WorldPack};
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
}

/// Runs the smoke scenario.
///
/// # Errors
/// A done criterion fails; the message names it.
pub async fn run() -> anyhow::Result<Transcript> {
    let pack = Arc::new(WorldPack::crypt_v01()?);
    let clock = Arc::new(ManualClock::new(20 * 3_600_000));
    let game = Game::new(World::new(Arc::clone(&pack), 7), None, clock.clone());
    let typing = Gate::new(Arc::clone(&game), 7, 0.0);
    let scripted: Arc<dyn Mind> = Arc::new(ScriptedMind::from_cast(&pack.cast));
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
    let mut obs = sup.subscribe();
    let mut all = game.subscribe();
    let rx = game.subscribe();
    let mut rig = Rig { sup, clock, rx };

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
    let kestrel = EntityId::from("kestrel");
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
                e["actor"]["id"] == "kestrel"
                    && e["untrusted_text"]
                        .as_str()
                        .is_some_and(|t| t.contains("wanderer") && t.contains("50/50"))
            })
        });
    ensure!(
        recruited,
        "kestrel did not try to recruit the human in the Rusty Flagon"
    );

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
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_v01_done_criteria_hold() {
        let t = super::run().await.unwrap();
        assert!(t.wakes.iter().any(|w| w.starts_with("torin")));
        assert!(t.wakes.iter().any(|w| w.starts_with("kestrel")));
        assert!(t.lines.iter().any(|l| l.contains("BOUNTY")));
    }
}
