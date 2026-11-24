//! The command gate: where a hidden agent PC types.
//!
//! Every command from a mind passes through the gate. For most entities it
//! runs at once. A Turing PC's command is held first, as long as a person
//! would take to type it: speech by length and words per minute, moves and
//! attacks for 600 to 1400 ms. If combat breaks out around the PC while a
//! line is held, the line is dropped with `INTERRUPTED`, and the
//! supervisor wakes the PC at once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mud_world::engine::{Fail, refusal};
use mud_world::pack::Tier;
use mud_world::{Command, CommandResult, EntityId, ErrorCode, Game, parse_with};
use tokio::sync::Notify;

use crate::turing::{action_delay_ms, mix, speech_delay_ms};

/// The command gate.
#[derive(Debug)]
pub struct Gate {
    game: Arc<Game>,
    seed: u64,
    counter: AtomicU64,
    /// Typing holds can be shortened for tests and demos (1.0 is real time).
    scale: f64,
    /// The lines being typed, per entity, by the nonce of their run. One
    /// entity can type several at once (parallel MCP calls).
    held: Mutex<Held>,
}

type Held = HashMap<EntityId, HashMap<u64, Arc<Notify>>>;

/// Removes one run's held entry when the run ends, however it ends: done,
/// interrupted, or dropped mid-hold by a timeout or a cancelled request.
struct HeldGuard<'a> {
    held: &'a Mutex<Held>,
    entity: &'a EntityId,
    nonce: u64,
}

impl Drop for HeldGuard<'_> {
    fn drop(&mut self) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(lines) = held.get_mut(self.entity) {
            lines.remove(&self.nonce);
            if lines.is_empty() {
                held.remove(self.entity);
            }
        }
    }
}

impl Gate {
    /// A gate over `game`. `scale` multiplies every typing hold.
    #[must_use]
    pub fn new(game: Arc<Game>, seed: u64, scale: f64) -> Arc<Self> {
        Arc::new(Self {
            game,
            seed,
            counter: AtomicU64::new(0),
            scale: scale.max(0.0),
            held: Mutex::new(HashMap::new()),
        })
    }

    /// The hold of `line` for `entity`, in ms. Zero unless it is a Turing PC.
    #[must_use]
    pub fn hold_ms(&self, entity: &EntityId, line: &str, nonce: u64) -> u64 {
        let spec = self.game.read(|w| {
            let e = w.entity(entity)?;
            (e.tier == Tier::Turing)
                .then(|| {
                    w.pack()
                        .cast
                        .get(entity)
                        .and_then(|c| c.mind.turing.clone())
                })
                .flatten()
        });
        let Some(spec) = spec else { return 0 };
        // A custom verb is typed like any action: no delay would be a tell.
        let Ok(cmd) = self.parse(line) else {
            return 0;
        };
        let h = mix(
            self.seed,
            &[entity.as_str().as_bytes(), &nonce.to_le_bytes(), b"typing"],
        );
        let raw = if cmd.is_speech() {
            speech_delay_ms(&spec, line.chars().count(), h)
        } else if cmd.is_read_only() {
            0
        } else {
            action_delay_ms(h)
        };
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a few seconds of ms"
        )]
        let scaled = (raw as f64 * self.scale).round() as u64;
        scaled
    }

    /// Parses `line` as the world will, custom verbs included.
    fn parse(&self, line: &str) -> Result<Command, mud_world::parser::ParseError> {
        self.game
            .read(|w| parse_with(line, &|v| w.is_custom_verb(v)))
    }

    /// Whether `entity` has a line held right now.
    #[must_use]
    pub fn is_holding(&self, entity: &EntityId) -> bool {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(entity)
            .is_some_and(|lines| !lines.is_empty())
    }

    /// Drops every held line of `entity`. Returns whether one was held.
    pub fn interrupt(&self, entity: &EntityId) -> bool {
        let lines = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(entity)
            .unwrap_or_default();
        for n in lines.values() {
            n.notify_one();
        }
        !lines.is_empty()
    }

    /// Runs one line for `entity`, after its hold.
    pub async fn run(&self, entity: &EntityId, line: &str) -> CommandResult {
        let nonce = self.counter.fetch_add(1, Ordering::Relaxed);
        let hold = self.hold_ms(entity, line, nonce);
        if hold > 0 {
            let notify = Arc::new(Notify::new());
            self.held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(entity.clone())
                .or_default()
                .insert(nonce, Arc::clone(&notify));
            let guard = HeldGuard {
                held: &self.held,
                entity,
                nonce,
            };
            let interrupted = tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(hold)) => false,
                () = notify.notified() => true,
            };
            drop(guard);
            if interrupted {
                let cmd = self.parse(line).ok();
                return refusal(
                    cmd.as_ref().map(Command::display_verb),
                    &Fail::new(
                        ErrorCode::Interrupted,
                        "combat broke out before you finished typing",
                    ),
                );
            }
        }
        self.game.execute(entity, line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mud_world::{ManualClock, World, WorldPack};

    fn game() -> Arc<Game> {
        let pack = Arc::new(WorldPack::crypt_v01().unwrap());
        Game::new(World::new(pack, 1), None, Arc::new(ManualClock::new(1_000)))
    }

    #[tokio::test(start_paused = true)]
    async fn a_turing_pc_types_and_others_do_not() {
        let g = game();
        let k = EntityId::from("pc_kestrel");
        g.bring_online(&k);
        let gate = Gate::new(Arc::clone(&g), 9, 1.0);
        let say = gate.hold_ms(&k, "say anyone up for the cellar?", 1);
        assert!((800 + 4_000..=2_200 + 7_000).contains(&say), "{say}");
        let mv = gate.hold_ms(&k, "move w", 2);
        assert!((600..=1_400).contains(&mv));
        assert_eq!(gate.hold_ms(&k, "look", 3), 0);
        assert_eq!(gate.hold_ms(&EntityId::from("marta"), "say hi", 4), 0);
        let started = tokio::time::Instant::now();
        let r = gate.run(&k, "say hi").await;
        assert!(r.ok);
        assert!(started.elapsed() >= Duration::from_millis(800));
        let fast = Gate::new(Arc::clone(&g), 9, 0.0);
        assert_eq!(fast.hold_ms(&k, "say a long line here", 5), 0);
    }

    fn game_with_scripts() -> Arc<Game> {
        let pack = Arc::new(WorldPack::crypt_v01().unwrap());
        let host = Arc::new(mud_script::ScriptHost::bundled_crypt_v01(&pack).unwrap());
        let mut w = World::new(pack, 1);
        w.set_hooks(Some(host), 0, mud_world::HooksChange::Start);
        Game::new(w, None, Arc::new(ManualClock::new(1_000)))
    }

    #[tokio::test(start_paused = true)]
    async fn a_custom_verb_is_held_like_any_action() {
        let g = game_with_scripts();
        let k = EntityId::from("pc_kestrel");
        g.bring_online(&k);
        let gate = Gate::new(Arc::clone(&g), 9, 1.0);
        let held = gate.hold_ms(&k, "listen", 1);
        assert!((600..=1_400).contains(&held), "{held}");
        assert_eq!(gate.hold_ms(&k, "dance", 2), 0, "a typo is not typed out");
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupted_custom_verb_names_the_verb() {
        let g = game_with_scripts();
        let k = EntityId::from("pc_kestrel");
        g.bring_online(&k);
        let gate = Gate::new(Arc::clone(&g), 9, 1.0);
        let (g2, k2) = (Arc::clone(&gate), k.clone());
        let task = tokio::spawn(async move { g2.run(&k2, "listen").await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(gate.interrupt(&k));
        let r = task.await.unwrap();
        assert_eq!(r.error_code, Some(ErrorCode::Interrupted));
        assert_eq!(r.verb.as_deref(), Some("listen"));
    }

    #[tokio::test(start_paused = true)]
    async fn combat_interrupts_a_held_line() {
        let g = game();
        let k = EntityId::from("pc_kestrel");
        g.bring_online(&k);
        let gate = Gate::new(Arc::clone(&g), 9, 1.0);
        let g2 = Arc::clone(&gate);
        let k2 = k.clone();
        let task =
            tokio::spawn(async move { g2.run(&k2, "say this is a long sentence to type").await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(gate.is_holding(&k));
        assert!(gate.interrupt(&k));
        let r = task.await.unwrap();
        assert!(!r.ok);
        assert_eq!(r.error_code, Some(ErrorCode::Interrupted));
        assert!(!gate.interrupt(&k), "nothing left to interrupt");
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_line_leaves_another_held_line_interruptible() {
        // Regression: the first line to finish removed the entity's entry,
        // so a second line still being typed could not be interrupted.
        let g = game();
        let k = EntityId::from("pc_kestrel");
        g.bring_online(&k);
        let gate = Gate::new(Arc::clone(&g), 9, 1.0);
        let (g1, k1) = (Arc::clone(&gate), k.clone());
        let say = tokio::spawn(async move {
            g1.run(&k1, "say this is a long sentence to type, really long")
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        let (g2, k2) = (Arc::clone(&gate), k.clone());
        let mv = tokio::spawn(async move { g2.run(&k2, "look").await });
        assert!(mv.await.unwrap().ok);
        let (g3, k3) = (Arc::clone(&gate), k.clone());
        let mv = tokio::spawn(async move { g3.run(&k3, "move w").await });
        assert!(mv.await.unwrap().ok, "the move commits first");
        assert!(gate.is_holding(&k), "the say is still being typed");
        assert!(gate.interrupt(&k));
        let r = say.await.unwrap();
        assert_eq!(r.error_code, Some(ErrorCode::Interrupted));
        assert!(!gate.is_holding(&k));
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_drops_every_held_line() {
        let g = game();
        let k = EntityId::from("pc_kestrel");
        g.bring_online(&k);
        let gate = Gate::new(Arc::clone(&g), 9, 1.0);
        let lines = [
            "say first long line to type",
            "say second long line to type",
        ];
        let tasks: Vec<_> = lines
            .iter()
            .map(|l| {
                let (g, k, l) = (Arc::clone(&gate), k.clone(), (*l).to_owned());
                tokio::spawn(async move { g.run(&k, &l).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(gate.interrupt(&k));
        for t in tasks {
            assert_eq!(t.await.unwrap().error_code, Some(ErrorCode::Interrupted));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_line_leaves_no_held_entry() {
        let g = game();
        let k = EntityId::from("pc_kestrel");
        g.bring_online(&k);
        let gate = Gate::new(Arc::clone(&g), 9, 1.0);
        let cut = tokio::time::timeout(
            Duration::from_millis(200),
            gate.run(&k, "say a line that never gets finished"),
        )
        .await;
        assert!(cut.is_err(), "the wake timed out mid-hold");
        assert!(!gate.is_holding(&k), "no phantom typing");
        assert!(!gate.interrupt(&k), "nothing to interrupt");
    }
}
