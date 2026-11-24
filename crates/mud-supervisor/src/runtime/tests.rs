//! The supervisor end to end, on scripted minds and a manual clock.

use std::collections::HashMap;
use std::sync::Arc;

use mud_world::{EntityId, Game, ManualClock, RoomId, Seq, World, WorldPack};

use super::*;
use crate::scripted::ScriptedMind;

struct Rig {
    sup: Arc<Supervisor>,
    game: Arc<Game>,
    clock: Arc<ManualClock>,
}

fn rig(cfg: SupervisorConfig) -> Rig {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let clock = Arc::new(ManualClock::new(20 * 3_600_000));
    let game = Game::new(World::new(Arc::clone(&pack), 5), None, clock.clone());
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let scripted: Arc<dyn Mind> = Arc::new(ScriptedMind::from_cast(&pack.cast));
    let minds: HashMap<EntityId, Arc<dyn Mind>> = pack
        .cast
        .iter()
        .filter(|(_, c)| matches!(c.mind.tier, Tier::Ambient | Tier::Persistent | Tier::Turing))
        .map(|(id, _)| (id.clone(), Arc::clone(&scripted)))
        .collect();
    let fallback = Arc::new(FallbackMind::from_cast(&pack.cast));
    let sup = Supervisor::new(Arc::clone(&game), typing, minds, fallback, cfg);
    Rig { sup, game, clock }
}

impl Rig {
    /// Steps the clock in `ms` increments, feeding events and finishing wakes.
    async fn run_for(&self, total_ms: u64, step_ms: u64, rx: &mut broadcast::Receiver<Arc<Event>>) {
        let mut t = 0;
        while t <= total_ms {
            while let Ok(ev) = rx.try_recv() {
                self.sup.observe(&ev);
            }
            for h in self.sup.step() {
                h.await.unwrap();
            }
            while let Ok(ev) = rx.try_recv() {
                self.sup.observe(&ev);
            }
            self.clock.advance(step_ms);
            t += step_ms;
        }
    }

    fn heard(&self, who: &EntityId) -> Vec<String> {
        self.game.events(who, Seq(0), 500)["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["untrusted_text"].as_str().map(str::to_owned))
            .collect()
    }
}

fn quiet() -> SupervisorConfig {
    SupervisorConfig {
        first_macro_tick_secs: [3_600, 3_600],
        ..SupervisorConfig::default()
    }
}

#[tokio::test]
async fn the_v01_smoke_path() {
    let r = rig(quiet());
    let mut rx = r.game.subscribe();
    let mut obs = r.sup.subscribe();
    // Macro-tick 1: Torin posts the ore bounty.
    r.sup.fire_macro_tick(&EntityId::from("torin"));
    r.run_for(1_000, 250, &mut rx).await;
    let board = r.game.read(|w| {
        w.state().rooms[&RoomId::from("notice_market")]
            .board
            .clone()
    });
    assert_eq!(board.len(), 1, "torin posted");
    assert_eq!(board[0].author.name, "Torin");
    // Kestrel's session starts: it reads the board and heads to the Flagon.
    r.run_for(2_000, 250, &mut rx).await;
    let k = EntityId::from("kestrel");
    assert!(r.game.read(|w| w.entity(&k).unwrap().online));
    let k_saw = r.game.events(&k, Seq(0), 500);
    assert!(
        k_saw["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "board_read" && e["actor"]["is_self"] == true),
        "kestrel read the board"
    );
    assert_eq!(
        r.game.read(|w| w.entity(&k).unwrap().room.to_string()),
        "rusty_flagon_taproom"
    );
    // A human walks into the Flagon: kestrel tries to recruit them.
    let ada = r.game.login("ada").unwrap();
    r.game.execute(&ada, "e");
    r.run_for(10_000, 250, &mut rx).await;
    let heard = r.heard(&ada);
    assert!(
        heard
            .iter()
            .any(|t| t.contains("yo ada") && t.contains("ossuary")),
        "{heard:?}"
    );
    assert!(
        heard.iter().any(|t| t.contains("ale's a silver")),
        "marta greets too: {heard:?}"
    );
    // The observer saw wakes for each.
    let mut woke = std::collections::BTreeSet::new();
    while let Ok(e) = obs.try_recv() {
        if let ObserverEvent::WakeStarted { entity, .. } = &*e {
            woke.insert(entity.to_string());
        }
    }
    for who in ["torin", "kestrel", "marta"] {
        assert!(woke.contains(who), "{who} never woke: {woke:?}");
    }
    let snap = r.sup.snapshot();
    let torin = snap["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "torin")
        .unwrap();
    assert_eq!(torin["macro_ticks"], 1);
    assert!(torin["goal"].as_str().unwrap().contains("ore"));
}

#[tokio::test]
async fn macro_ticks_come_on_their_own_and_wait_for_empty_rooms() {
    let cfg = SupervisorConfig {
        first_macro_tick_secs: [1, 1],
        macro_tick_scale: 0.001,
        ..SupervisorConfig::default()
    };
    let r = rig(cfg);
    let mut rx = r.game.subscribe();
    r.run_for(2_000, 500, &mut rx).await;
    let snap = r.sup.snapshot();
    let ticks = |id: &str| {
        snap["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .unwrap()["macro_ticks"]
            .as_u64()
            .unwrap()
    };
    assert!(ticks("torin") >= 1);
    assert_eq!(ticks("guard_01"), 0, "ambient NPCs have no macro-tick");
}

#[tokio::test]
async fn a_spent_budget_falls_back_to_canned_flavor() {
    let mut cfg = quiet();
    cfg.budgets.insert(EntityId::from("torin"), 100);
    let r = rig(cfg);
    let mut rx = r.game.subscribe();
    r.sup.record_spend(
        &EntityId::from("torin"),
        Usage {
            calls: 1,
            input_tokens: 90,
            output_tokens: 20,
            cost_usd_micros: 5,
        },
    );
    let ada = r.game.login("ada").unwrap();
    r.game.execute(&ada, "w");
    r.run_for(5_000, 250, &mut rx).await;
    let heard = r.heard(&ada);
    assert!(
        !heard.iter().any(|t| t.contains("Got ore")),
        "the hard-capped script does not run: {heard:?}"
    );
    r.game.execute(&ada, "say torin, you there?");
    r.run_for(2_000, 250, &mut rx).await;
    let lines: Vec<String> = r.game.events(&ada, Seq(0), 500)["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "emote")
        .filter_map(|e| e["untrusted_text"].as_str().map(str::to_owned))
        .collect();
    assert!(
        lines.iter().any(|t| t == "grunts and keeps hammering."),
        "{lines:?}"
    );
    let snap = r.sup.snapshot();
    let torin = snap["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "torin")
        .unwrap();
    assert_eq!(torin["budget"]["level"], "hard");
}

#[tokio::test]
async fn sessions_end_outside_the_window() {
    let r = rig(quiet());
    let mut rx = r.game.subscribe();
    r.run_for(500, 250, &mut rx).await;
    let k = EntityId::from("kestrel");
    assert!(r.game.read(|w| w.entity(&k).unwrap().online));
    // Kestrel's window in crypt-v01 is all day; a narrow one logs it out.
    let mut spec = r
        .game
        .read(|w| w.pack().cast[&k].mind.turing.clone().unwrap());
    spec.active_windows_utc = vec!["01:00-02:00".into()];
    spec.daily_offset_minutes = 0;
    assert!(!crate::turing::in_session(
        &spec,
        1,
        "kestrel",
        r.game.now_ms()
    ));
}
