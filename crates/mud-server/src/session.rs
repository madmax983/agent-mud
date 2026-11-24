//! One player connection, transport-agnostic: the web terminal and telnet
//! both drive a [`Session`].

use std::sync::Arc;

use mud_world::{EntityId, Event, Game};

/// What a client sees, line by line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    /// The answer to the player's own command.
    Narrative,
    /// Something that happened around them.
    Event,
    /// A refusal.
    Error,
    /// A server notice.
    System,
}

/// One line for a client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Line {
    /// The kind.
    pub kind: LineKind,
    /// The text. Player-written parts are not escaped here; each
    /// transport escapes for its own medium.
    pub text: String,
}

impl Line {
    fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// Counts open connections per entity, so a second tab does not log a
/// player out when the first one closes.
///
/// The count and the world's online flag change together, under this
/// mutex: a login racing the last logout of the same player runs either
/// before it (and the logout then keeps the player online) or after it
/// (and brings the player back online).
#[derive(Debug, Default)]
pub struct Presence {
    open: std::sync::Mutex<std::collections::HashMap<EntityId, usize>>,
}

impl Presence {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<EntityId, usize>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Logs `name` in and counts one more connection for it.
    ///
    /// # Errors
    /// The name is invalid or taken.
    pub fn login(&self, game: &Game, name: &str) -> Result<EntityId, mud_world::LoginError> {
        let mut open = self.lock();
        let id = game.login(name)?;
        *open.entry(id.clone()).or_default() += 1;
        drop(open);
        Ok(id)
    }

    /// Counts one fewer connection for `id` and, if that was the last,
    /// takes it offline. Returns whether it was the last.
    pub fn logout(&self, game: &Game, id: &EntityId) -> bool {
        let mut open = self.lock();
        let n = open.entry(id.clone()).or_default();
        *n = n.saturating_sub(1);
        let last = *n == 0;
        if last {
            open.remove(id);
            game.take_offline(id);
        }
        drop(open);
        last
    }
}

/// A logged-in player.
#[derive(Debug)]
pub struct Session {
    game: Arc<Game>,
    presence: Arc<Presence>,
    entity: EntityId,
}

impl Session {
    /// Logs in `name`. Returns the session and its greeting.
    ///
    /// # Errors
    /// The name is invalid or taken.
    pub fn login(
        game: Arc<Game>,
        presence: Arc<Presence>,
        name: &str,
    ) -> Result<(Self, Vec<Line>), mud_world::LoginError> {
        let entity = presence.login(&game, name)?;
        let motd = game.read(|w| w.pack().world.motd.clone());
        let mut lines = vec![Line::new(LineKind::System, motd)];
        let look = game.execute(&entity, "look");
        lines.push(Line::new(LineKind::Narrative, look.narrative));
        Ok((
            Self {
                game,
                presence,
                entity,
            },
            lines,
        ))
    }

    /// The player's entity.
    #[must_use]
    pub const fn entity(&self) -> &EntityId {
        &self.entity
    }

    /// Runs one typed line.
    #[must_use]
    pub fn command(&self, line: &str) -> Vec<Line> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        let r = self.game.execute(&self.entity, line);
        let kind = if r.ok {
            LineKind::Narrative
        } else {
            LineKind::Error
        };
        vec![Line::new(kind, r.narrative)]
    }

    /// The line an event shows this player, if it reaches them and is not
    /// their own doing (their own command already answered).
    #[must_use]
    pub fn render(&self, ev: &Event) -> Option<Line> {
        (ev.reaches(&self.entity) && !ev.is_by(&self.entity))
            .then(|| Line::new(LineKind::Event, ev.render_line(&self.entity)))
            .filter(|l| !l.text.is_empty())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.presence.logout(&self.game, &self.entity);
    }
}

#[cfg(test)]
mod tests {
    use mud_world::{ManualClock, World, WorldPack};

    use super::*;

    fn game() -> Arc<Game> {
        Game::new(
            World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1),
            None,
            Arc::new(ManualClock::new(1)),
        )
    }

    #[test]
    fn two_players_hear_each_other_and_logout_needs_the_last_tab() {
        let g = game();
        let p = Arc::new(Presence::default());
        let (ada, greet) = Session::login(Arc::clone(&g), Arc::clone(&p), "ada").unwrap();
        assert!(greet[1].text.contains("Town Square"));
        let (bo, _) = Session::login(Arc::clone(&g), Arc::clone(&p), "bo").unwrap();
        let mut rx = g.subscribe();
        let out = ada.command("say hi bo");
        assert_eq!(out[0].text, "You say, \"hi bo\"");
        let ev = rx.try_recv().unwrap();
        assert_eq!(bo.render(&ev).unwrap().text, "ada says, \"hi bo\"");
        assert!(ada.render(&ev).is_none(), "no echo of your own line");
        assert_eq!(ada.command("dance")[0].kind, LineKind::Error);
        let (ada2, _) = Session::login(Arc::clone(&g), Arc::clone(&p), "ada").unwrap();
        drop(ada);
        assert!(
            g.read(|w| w.entity(&EntityId::from("pc_ada")).unwrap().online),
            "a tab is still open"
        );
        drop(ada2);
        assert!(!g.read(|w| w.entity(&EntityId::from("pc_ada")).unwrap().online));
        assert!(Session::login(g, p, "kestrel").is_err());
    }

    /// A tab closing while another tab of the same player logs in must
    /// never leave the new session attached to an offline entity.
    #[test]
    fn a_login_racing_the_last_logout_stays_online() {
        let g = game();
        let p = Arc::new(Presence::default());
        let id = EntityId::from("pc_ada");
        let stale = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..2_000 {
                        let (sess, _) =
                            Session::login(Arc::clone(&g), Arc::clone(&p), "ada").unwrap();
                        if !g.read(|w| w.entity(&id).unwrap().online) {
                            stale.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        drop(sess);
                    }
                });
            }
        });
        assert_eq!(
            stale.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a live session saw its entity offline"
        );
        assert!(!g.read(|w| w.entity(&id).unwrap().online));
    }
}
