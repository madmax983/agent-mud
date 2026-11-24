//! The login dialogue, one for both transports.
//!
//! Every player name gets the same `Password:` prompt, whether a human or
//! an agent PC holds it (the route kernel's masquerade, proved in
//! `formal/verus/src/login_route.rs`), and every answer at it costs one
//! bcrypt verify. Nothing binds a session to an agent PC or an NPC: who
//! holds the name is read again before every account call and before the
//! login is done.
//!
//! At a password prompt every input is the answer. It is never parsed,
//! executed, echoed or logged; it lives in a [`Secret`] and goes only to
//! [`Accounts`].

use std::sync::Arc;

use mud_accounts::{
    Accounts, AuthError, AuthVia, Authenticated, Holder, NameKey, NewPurpose, PasswordRejection,
    Peer, ResetTicket, Route, Secret, SignIn, may_bind,
};
use mud_world::Game;

use crate::session::{CloseKind, Control, Line, LineKind, Secretness, Transport};

/// The most answers one connection may give before it is closed.
pub const MAX_ANSWERS: u32 = 20;

/// The name prompt.
pub const NAME_PROMPT: &str = "What is your name?";

/// What the dialogue does next.
#[derive(Debug)]
pub enum Next {
    /// Wait for the next answer.
    Ask,
    /// The login is proved.
    Done(Box<Authenticated>),
    /// Close the connection.
    Close(CloseKind),
}

/// The dialogue's answer to one input.
#[derive(Debug)]
pub struct Reply {
    /// Lines to show.
    pub lines: Vec<Line>,
    /// Controls (web only).
    pub controls: Vec<Control>,
    /// What happens next.
    pub next: Next,
}

impl Reply {
    const fn ask(lines: Vec<Line>) -> Self {
        Self {
            lines,
            controls: Vec::new(),
            next: Next::Ask,
        }
    }

    const fn close(lines: Vec<Line>, kind: CloseKind) -> Self {
        Self {
            lines,
            controls: Vec::new(),
            next: Next::Close(kind),
        }
    }
}

/// Why a new password is being chosen.
enum Purpose {
    Create,
    Claim,
    Reset(ResetTicket),
}

enum State {
    Name,
    Password {
        name: String,
        key: NameKey,
        route: Route,
    },
    NewPassword {
        name: String,
        key: NameKey,
        purpose: Purpose,
    },
    Repeat {
        name: String,
        key: NameKey,
        purpose: Purpose,
        first: Secret,
    },
    Over,
}

/// One connection's login.
pub struct Dialogue {
    game: Arc<Game>,
    accounts: Arc<Accounts>,
    transport: Transport,
    peer: Peer,
    state: State,
    wrong: u32,
    answers: u32,
}

impl std::fmt::Debug for Dialogue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialogue")
            .field("transport", &self.transport)
            .field("peer", &self.peer)
            .field("wrong", &self.wrong)
            .field("answers", &self.answers)
            .finish_non_exhaustive()
    }
}

fn error(text: impl Into<String>) -> Line {
    Line::new(LineKind::Error, text)
}

fn name_prompt() -> Line {
    Line::new(LineKind::Prompt, NAME_PROMPT)
}

fn password_prompt() -> Line {
    Line::new(LineKind::Secret(Secretness::Current), "Password:")
}

impl Dialogue {
    /// A dialogue for a client at `peer`.
    #[must_use]
    pub const fn new(
        game: Arc<Game>,
        accounts: Arc<Accounts>,
        transport: Transport,
        peer: Peer,
    ) -> Self {
        Self {
            game,
            accounts,
            transport,
            peer,
            state: State::Name,
            wrong: 0,
            answers: 0,
        }
    }

    /// The first question.
    #[must_use]
    pub fn greeting(&self) -> Reply {
        Reply::ask(vec![name_prompt()])
    }

    fn now(&self) -> u64 {
        self.game.now_ms()
    }

    fn min(&self) -> usize {
        self.accounts.config().min_password_chars
    }

    /// Who holds `name` now. A name that no longer parses holds nothing a
    /// session may bind to.
    fn holder(&self, name: &str) -> Holder {
        self.game
            .name_owner(name)
            .map_or(Holder::NonPlayer, |o| Holder::from(&o))
    }

    fn new_prompt(&self, name: &str, purpose: &Purpose) -> Line {
        let min = self.min();
        let text = match purpose {
            Purpose::Create => format!(
                "Nobody here is called {name}. Choose a password to make a new character ({min} or more characters):"
            ),
            Purpose::Claim => {
                format!("{name} has no password yet. Choose one ({min} or more characters):")
            }
            Purpose::Reset(_) => format!(
                "Your password was reset by the keeper of this world. Choose a new one ({min} or more characters):"
            ),
        };
        Line::new(LineKind::Secret(Secretness::New), text)
    }

    /// Back to the name, with `lines` first.
    fn back_to_name(&mut self, mut lines: Vec<Line>) -> Reply {
        self.state = State::Name;
        lines.push(name_prompt());
        Reply::ask(lines)
    }

    /// A failed account call: the text, then the name prompt or a close.
    fn refused(&mut self, e: &AuthError) -> Reply {
        match e {
            AuthError::AddressHeld { .. } | AuthError::WorkHeld { .. } | AuthError::Busy => {
                self.state = State::Over;
                Reply::close(vec![error(e.to_string())], CloseKind::Policy)
            }
            AuthError::Unavailable(source) => {
                tracing::error!(target: "mud::auth", error = %source, "the account service failed during a login");
                self.state = State::Over;
                Reply::close(vec![error(e.to_string())], CloseKind::Policy)
            }
            _ => self.back_to_name(vec![error(e.to_string())]),
        }
    }

    /// One answer.
    pub async fn answer(&mut self, input: Secret, remember: bool) -> Reply {
        self.answers += 1;
        if self.answers > MAX_ANSWERS {
            self.state = State::Over;
            return Reply::close(vec![error("Too many tries. Farewell.")], CloseKind::Policy);
        }
        match std::mem::replace(&mut self.state, State::Over) {
            State::Name => self.name(input.expose()).await,
            State::Over => Reply::close(Vec::new(), CloseKind::Policy),
            _ if input.is_empty() => self.back_to_name(Vec::new()),
            State::Password { name, key, route } => {
                if route == Route::Claim {
                    self.claim(name, key, &input).await
                } else {
                    self.password(name, key, &input, remember).await
                }
            }
            State::NewPassword { name, key, purpose } => {
                self.new_password(name, key, purpose, input).await
            }
            State::Repeat {
                name,
                key,
                purpose,
                first,
            } => {
                self.repeat(name, key, purpose, &first, &input, remember)
                    .await
            }
        }
    }

    /// An input over the line limit, at any state. It counts as an answer
    /// and costs no verify.
    pub fn too_long(&mut self) -> Reply {
        const TOO_LONG: &str = "That line is too long; nothing was done.";
        self.answers += 1;
        if self.answers > MAX_ANSWERS {
            self.state = State::Over;
            return Reply::close(vec![error("Too many tries. Farewell.")], CloseKind::Policy);
        }
        match std::mem::replace(&mut self.state, State::Over) {
            State::Name => {
                self.state = State::Name;
                Reply::ask(vec![error(format!("{TOO_LONG} {NAME_PROMPT}"))])
            }
            State::Password { .. } => self.back_to_name(vec![error(TOO_LONG)]),
            State::NewPassword { name, key, purpose }
            | State::Repeat {
                name, key, purpose, ..
            } => {
                let prompt = self.new_prompt(&name, &purpose);
                self.state = State::NewPassword { name, key, purpose };
                Reply::ask(vec![error(PasswordRejection::TooLong.to_string()), prompt])
            }
            State::Over => Reply::close(Vec::new(), CloseKind::Policy),
        }
    }

    /// A web resume token, in place of the name.
    pub async fn resume(&mut self, credential: Secret) -> Reply {
        if !matches!(self.state, State::Name) {
            return Reply::ask(Vec::new());
        }
        let now = self.now();
        match self.accounts.resume(&credential, self.peer, now).await {
            Ok(auth) => self.done(auth, false, Vec::new()).await,
            Err(e @ (AuthError::ResumeRejected | AuthError::Locked)) => {
                let mut r = self.back_to_name(vec![error(e.to_string())]);
                r.controls.push(Control::Forget);
                r
            }
            Err(e) => self.refused(&e),
        }
    }

    async fn name(&mut self, typed: &str) -> Reply {
        let owner = match self.game.name_owner(typed) {
            Ok(o) => o,
            Err(e) => return self.name_again_inline(&format!("{e}.")),
        };
        let name = typed.trim().to_owned();
        let Some(key) = NameKey::parse(&name) else {
            return self.name_again_inline(&format!("{}.", mud_world::LoginError::Invalid));
        };
        let holder = Holder::from(&owner);
        let now = self.now();
        if let Err(e) = self.accounts.check_name(&key, self.peer, now).await {
            return self.refused(&e);
        }
        let route = match self.accounts.route(&key, holder, now).await {
            Ok(r) => r,
            Err(e) => return self.refused(&AuthError::Unavailable(e)),
        };
        match route {
            Route::Refuse => self.name_again_inline("that name is taken."),
            Route::Create => {
                if let Err(e) = self.accounts.may_create(self.peer, now) {
                    return self.back_to_name(vec![error(e.to_string())]);
                }
                let prompt = self.new_prompt(&name, &Purpose::Create);
                self.state = State::NewPassword {
                    name,
                    key,
                    purpose: Purpose::Create,
                };
                Reply::ask(vec![prompt])
            }
            Route::Password | Route::Claim => {
                self.state = State::Password { name, key, route };
                Reply::ask(vec![password_prompt()])
            }
        }
    }

    /// An error that carries the name question in its own line.
    fn name_again_inline(&mut self, text: &str) -> Reply {
        self.state = State::Name;
        Reply::ask(vec![error(format!("{text} {NAME_PROMPT}"))])
    }

    async fn password(&mut self, name: String, key: NameKey, pw: &Secret, remember: bool) -> Reply {
        let holder = self.holder(&name);
        let now = self.now();
        match self
            .accounts
            .sign_in(&key, holder, pw, self.peer, now)
            .await
        {
            Ok(SignIn::Ok(auth)) => self.done(auth, remember, Vec::new()).await,
            Ok(SignIn::MustChange(ticket)) => {
                let purpose = Purpose::Reset(ticket);
                let prompt = self.new_prompt(&name, &purpose);
                self.state = State::NewPassword { name, key, purpose };
                Reply::ask(vec![prompt])
            }
            Err(AuthError::WrongPassword) => {
                self.wrong += 1;
                if self.wrong >= self.accounts.config().max_password_attempts {
                    self.state = State::Over;
                    return Reply::close(
                        vec![error("Wrong password. Farewell.")],
                        CloseKind::Policy,
                    );
                }
                self.state = State::Password {
                    name,
                    key,
                    route: Route::Password,
                };
                Reply::ask(vec![error("Wrong password."), password_prompt()])
            }
            Err(e) => self.refused(&e),
        }
    }

    async fn claim(&mut self, name: String, key: NameKey, pw: &Secret) -> Reply {
        let now = self.now();
        match self
            .accounts
            .answer_claim(&key, &name, pw, self.peer, now)
            .await
        {
            Ok(Ok(())) => {
                let text = format!(
                    "{name} has no password yet; the one you typed will become it. Type it again to confirm:"
                );
                self.state = State::Repeat {
                    name,
                    key,
                    purpose: Purpose::Claim,
                    first: Secret::new(pw.expose().to_owned()),
                };
                Reply::ask(vec![Line::new(LineKind::Secret(Secretness::New), text)])
            }
            Ok(Err(reason)) => {
                let min = self.min();
                let text = format!(
                    "{name} has no password yet. {reason} Choose one ({min} or more characters):"
                );
                self.state = State::NewPassword {
                    name,
                    key,
                    purpose: Purpose::Claim,
                };
                Reply::ask(vec![Line::new(LineKind::Secret(Secretness::New), text)])
            }
            Err(e) => self.refused(&e),
        }
    }

    async fn new_password(
        &mut self,
        name: String,
        key: NameKey,
        purpose: Purpose,
        pw: Secret,
    ) -> Reply {
        if let Err(reason) = self.accounts.check_new_password(&pw, &name).await {
            let prompt = self.new_prompt(&name, &purpose);
            self.state = State::NewPassword { name, key, purpose };
            return Reply::ask(vec![error(reason.to_string()), prompt]);
        }
        self.state = State::Repeat {
            name,
            key,
            purpose,
            first: pw,
        };
        Reply::ask(vec![Line::new(
            LineKind::Secret(Secretness::New),
            "Type it again:",
        )])
    }

    async fn repeat(
        &mut self,
        name: String,
        key: NameKey,
        purpose: Purpose,
        first: &Secret,
        again: &Secret,
        remember: bool,
    ) -> Reply {
        if !autumn_web::auth::constant_time_eq(first.expose().as_bytes(), again.expose().as_bytes())
        {
            let prompt = self.new_prompt(&name, &purpose);
            self.state = State::NewPassword { name, key, purpose };
            return Reply::ask(vec![error("The passwords did not match."), prompt]);
        }
        let now = self.now();
        let (made, welcome) = match purpose {
            Purpose::Create | Purpose::Claim => {
                let (p, welcome) = if matches!(purpose, Purpose::Create) {
                    (
                        NewPurpose::Create,
                        format!(
                            "Welcome, {name}. Your password is the only way back to this character; keep it."
                        ),
                    )
                } else {
                    (
                        NewPurpose::Claim,
                        format!("{name} is yours now. Use this password next time."),
                    )
                };
                let holder = self.holder(&name);
                let made = self
                    .accounts
                    .create(&name, &key, holder, first, p, self.peer, now)
                    .await;
                (made, welcome)
            }
            Purpose::Reset(ticket) => (
                self.accounts
                    .finish_reset(ticket, first, self.peer, now)
                    .await,
                "Password changed.".to_owned(),
            ),
        };
        match made {
            Ok(auth) => {
                self.done(auth, remember, vec![Line::new(LineKind::System, welcome)])
                    .await
            }
            Err(e) => self.refused(&e),
        }
    }

    /// Binds the login if the name's holder still allows it.
    async fn done(&mut self, mut auth: Authenticated, remember: bool, lines: Vec<Line>) -> Reply {
        if !may_bind(self.holder(auth.name()), true) {
            return self.name_again_inline("that name is taken.");
        }
        self.state = State::Over;
        let mut controls = vec![Control::Authed {
            name: auth.name().to_owned(),
        }];
        if self.transport == Transport::Web && auth.via() != AuthVia::Resumed {
            let now = self.now();
            match self.accounts.remember(&mut auth, remember, now).await {
                Ok(Some(token)) => controls.push(Control::Remember(token)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(target: "mud::auth", error = %e, "a browser could not be remembered");
                }
            }
        }
        Reply {
            lines,
            controls,
            next: Next::Done(Box::new(auth)),
        }
    }
}

#[cfg(test)]
mod tests;
