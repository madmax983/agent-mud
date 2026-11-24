//! One player connection, transport-agnostic: the web terminal and telnet
//! both drive a [`Session`] once the login dialogue ([`crate::login`]) has
//! proved who it is.
//!
//! The session words (`password`, `logout`, `quit`) are read here, before
//! the engine: `World::execute` never sees them, so a password never
//! reaches the world, the command log or a script.

use std::sync::Arc;

use mud_accounts::{Accounts, AuthError, Authenticated, IssuedToken, Peer, Secret};
use mud_world::{EntityId, Event, Game};

/// Which answer a secret prompt wants, for the browser's password manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secretness {
    /// The current password.
    Current,
    /// A new password.
    New,
}

impl Secretness {
    /// The `autocomplete` word the terminal uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current-password",
            Self::New => "new-password",
        }
    }
}

/// What a client sees, line by line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// The answer to the player's own command.
    Narrative,
    /// Something that happened around them.
    Event,
    /// A refusal.
    Error,
    /// A server notice.
    System,
    /// A question whose answer may echo.
    Prompt,
    /// A question whose answer must not echo.
    Secret(Secretness),
}

/// One line for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The kind.
    pub kind: LineKind,
    /// The text. Player-written parts are not escaped here; each
    /// transport escapes for its own medium.
    pub text: String,
}

impl Line {
    /// A line.
    pub fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// Which connection a session or a login runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The browser terminal.
    Web,
    /// Telnet and plain-text clients.
    Telnet,
}

/// A message to the client that is not a line.
#[derive(Debug)]
pub enum Control {
    /// The login is done.
    Authed {
        /// The character's name.
        name: String,
    },
    /// Keep this resume token.
    Remember(IssuedToken),
    /// Forget any resume token.
    Forget,
}

/// How a connection ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseKind {
    /// The player left, or a timeout: WebSocket 1000.
    Normal,
    /// The server refused to go on: WebSocket 1008.
    Policy,
}

/// What one input produced.
#[derive(Debug, Default)]
pub struct Output {
    /// Lines to show.
    pub lines: Vec<Line>,
    /// Controls (web only).
    pub controls: Vec<Control>,
    /// Close after sending, if set.
    pub close: Option<CloseKind>,
}

impl Output {
    fn lines(lines: Vec<Line>) -> Self {
        Self {
            lines,
            ..Self::default()
        }
    }

    fn one(kind: LineKind, text: impl Into<String>) -> Self {
        Self::lines(vec![Line::new(kind, text)])
    }

    const fn closing(mut self, kind: CloseKind) -> Self {
        self.close = Some(kind);
        self
    }
}

/// The `board read` narrative followed by one `  [n] author: text` line
/// per post in `resolution["posts"]`. The posts are player text, kept out
/// of the engine's narrative and rendered here, at the transport; each
/// transport then escapes for its medium. A line the narrative already
/// holds is not repeated.
fn with_board_posts(narrative: &str, resolution: &serde_json::Value) -> String {
    let mut text = narrative.to_owned();
    let posts = resolution["posts"].as_array().into_iter().flatten();
    for (i, p) in posts.enumerate() {
        let line = format!(
            "  [{}] {}: {}",
            i + 1,
            p["author"]["name"].as_str().unwrap_or("someone"),
            p["untrusted_text"].as_str().unwrap_or_default()
        );
        if !narrative.lines().any(|l| l == line) {
            text.push('\n');
            text.push_str(&line);
        }
    }
    text
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

    /// Brings a proved login into the world and counts one more
    /// connection for it. Only an [`Authenticated`] opens a character: the
    /// world sees the name alone, never a secret.
    ///
    /// # Errors
    /// The name changed hands since the login was proved.
    pub fn login(
        &self,
        game: &Game,
        auth: &Authenticated,
    ) -> Result<EntityId, mud_world::LoginError> {
        let mut open = self.lock();
        let id = game.login(auth.name())?;
        if &id != auth.entity() {
            drop(open);
            return Err(mud_world::LoginError::Taken);
        }
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

/// A password change in progress.
enum Change {
    Current,
    New { current: Secret },
    Repeat { current: Secret, new: Secret },
}

impl std::fmt::Debug for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Current => "Change::Current",
            Self::New { .. } => "Change::New",
            Self::Repeat { .. } => "Change::Repeat",
        })
    }
}

/// The help line for the session words.
pub const ACCOUNT_HELP: &str =
    "Account: password (change it) · logout (forget this browser) · quit";

/// A logged-in player.
#[derive(Debug)]
pub struct Session {
    game: Arc<Game>,
    presence: Arc<Presence>,
    accounts: Arc<Accounts>,
    auth: Authenticated,
    peer: Peer,
    transport: Transport,
    entity: EntityId,
    pending: Option<Change>,
    wrong: u32,
}

impl Session {
    /// Opens a session for a proved login. Returns it and its greeting.
    ///
    /// # Errors
    /// The name changed hands since the login was proved.
    pub fn login(
        game: Arc<Game>,
        presence: Arc<Presence>,
        accounts: Arc<Accounts>,
        auth: Authenticated,
        peer: Peer,
        transport: Transport,
    ) -> Result<(Self, Vec<Line>), mud_world::LoginError> {
        let entity = presence.login(&game, &auth)?;
        let motd = game.read(|w| w.pack().world.motd.clone());
        let mut lines = vec![Line::new(LineKind::System, motd)];
        let look = game.execute(&entity, "look");
        lines.push(Line::new(LineKind::Narrative, look.narrative));
        Ok((
            Self {
                game,
                presence,
                accounts,
                auth,
                peer,
                transport,
                entity,
                pending: None,
                wrong: 0,
            },
            lines,
        ))
    }

    /// The player's entity.
    #[must_use]
    pub const fn entity(&self) -> &EntityId {
        &self.entity
    }

    /// The proved login behind the session.
    #[must_use]
    pub const fn auth(&self) -> &Authenticated {
        &self.auth
    }

    /// Runs one typed line through the engine.
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
        let text = if r.ok && r.verb.as_deref() == Some("board_read") {
            with_board_posts(&r.narrative, &r.resolution)
        } else {
            r.narrative
        };
        vec![Line::new(kind, text)]
    }

    fn min_chars(&self) -> usize {
        self.accounts.config().min_password_chars
    }

    /// Takes one line from the player: the answer to a pending password
    /// question, a session word, or a command for the engine.
    pub async fn input(&mut self, line: Secret) -> Output {
        if let Some(change) = self.pending.take() {
            return self.answer_change(change, line).await;
        }
        let text = line.expose();
        let mut words = text.split_whitespace();
        let first = words.next().unwrap_or("").to_ascii_lowercase();
        let alone = words.next().is_none();
        match first.as_str() {
            "password" if !alone => Output::one(
                LineKind::Error,
                "Type password alone; you will be asked for it privately.",
            ),
            "password" => {
                self.pending = Some(Change::Current);
                Output::one(LineKind::Secret(Secretness::Current), "Current password:")
            }
            "logout" if self.transport == Transport::Web => {
                if let Some(s) = self.auth.series()
                    && let Err(e) = self.accounts.forget(s).await
                {
                    tracing::warn!(target: "mud::auth", error = %e, "forgetting a browser failed");
                }
                Output {
                    lines: vec![Line::new(
                        LineKind::System,
                        "Farewell. This browser will ask for your password next time.",
                    )],
                    controls: vec![Control::Forget],
                    close: Some(CloseKind::Normal),
                }
            }
            "quit" | "logout" => {
                Output::one(LineKind::System, "Farewell.").closing(CloseKind::Normal)
            }
            "help" => {
                let mut lines = self.command(text);
                lines.push(Line::new(LineKind::System, ACCOUNT_HELP));
                Output::lines(lines)
            }
            _ => Output::lines(self.command(text)),
        }
    }

    fn new_prompt(&self) -> Line {
        Line::new(
            LineKind::Secret(Secretness::New),
            format!("New password ({} or more characters):", self.min_chars()),
        )
    }

    async fn answer_change(&mut self, change: Change, answer: Secret) -> Output {
        if answer.is_empty() {
            return Output::one(LineKind::System, "Password change cancelled.");
        }
        match change {
            Change::Current => {
                self.pending = Some(Change::New { current: answer });
                Output::lines(vec![self.new_prompt()])
            }
            Change::New { current } => {
                if let Err(r) = self
                    .accounts
                    .check_new_password(&answer, self.auth.name())
                    .await
                {
                    self.pending = Some(Change::New { current });
                    return Output::lines(vec![
                        Line::new(LineKind::Error, r.to_string()),
                        self.new_prompt(),
                    ]);
                }
                self.pending = Some(Change::Repeat {
                    current,
                    new: answer,
                });
                Output::one(LineKind::Secret(Secretness::New), "Type it again:")
            }
            Change::Repeat { current, new } => {
                if !autumn_web::auth::constant_time_eq(
                    new.expose().as_bytes(),
                    answer.expose().as_bytes(),
                ) {
                    self.pending = Some(Change::New { current });
                    return Output::lines(vec![
                        Line::new(LineKind::Error, "The passwords did not match."),
                        self.new_prompt(),
                    ]);
                }
                self.change(&current, &new).await
            }
        }
    }

    async fn change(&mut self, current: &Secret, new: &Secret) -> Output {
        let now = self.game.now_ms();
        match self
            .accounts
            .change_password(&mut self.auth, current, new, self.peer, now)
            .await
        {
            Ok(()) => Output::one(
                LineKind::System,
                "Password changed. Your other sessions and remembered browsers are signed out.",
            ),
            Err(AuthError::WrongPassword) => {
                self.wrong += 1;
                if self.wrong >= self.accounts.config().max_password_attempts {
                    Output::one(LineKind::Error, "Wrong password. Farewell.")
                        .closing(CloseKind::Policy)
                } else {
                    Output::one(LineKind::Error, "Wrong password.")
                }
            }
            Err(e @ AuthError::Locked) => {
                Output::one(LineKind::Error, e.to_string()).closing(CloseKind::Policy)
            }
            Err(e) => {
                if let AuthError::Unavailable(source) = &e {
                    tracing::error!(target: "mud::auth", error = %source, "a password change failed");
                }
                Output::one(LineKind::Error, e.to_string())
            }
        }
    }

    /// Whether the account still allows this session: not locked, and no
    /// password change, reset or sign-out since it began.
    ///
    /// # Errors
    /// The account store failed.
    pub async fn revalidate(&self) -> Result<bool, mud_accounts::AccountError> {
        self.accounts
            .revalidate(&self.auth, self.game.now_ms())
            .await
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
pub(crate) mod tests;
