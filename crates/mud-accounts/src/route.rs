//! The login route table, proved in `formal/verus/src/login_route.rs`.
//!
//! - R1 (masquerade): an agent PC and every human account are asked the
//!   same first question, `Password:`, whatever their account and legacy
//!   state.
//! - R2: only a free name with no account creates a character.
//! - R3: only a human with no account claims, and only in the window.
//! - R4: a login is refused iff an NPC or a mob holds the name.
//! - R5: a session binds only with a proved secret, and only to a human or
//!   a free name.

use mud_world::NameOwner;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// mirror-begin holder
/// Who holds a login name, as the route kernel sees it.
pub enum Holder {
    /// Nobody: a new character may take it.
    Free,
    /// A human character.
    Human,
    /// A player character no human plays.
    OtherPlayer,
    /// An NPC or a mob.
    NonPlayer,
}
// mirror-end holder

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// mirror-begin route
/// Where a login goes.
pub enum Route {
    /// Make a new character with a new password.
    Create,
    /// Give a legacy character its first password.
    Claim,
    /// Check a password.
    Password,
    /// Refuse the name.
    Refuse,
}
// mirror-end route

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// mirror-begin first_ask
/// The first thing a login asks after the name.
pub enum FirstAsk {
    /// A new password.
    NewPassword,
    /// The password.
    Password,
    /// Nothing: the name is taken.
    Taken,
}
// mirror-end first_ask

// mirror-begin login_route
/// The route of a login: who holds the name, whether it has an account, and whether legacy characters may still be claimed.
#[must_use]
pub const fn login_route(holder: Holder, has_account: bool, legacy_open: bool) -> Route {
    match holder {
        Holder::NonPlayer => Route::Refuse,
        Holder::OtherPlayer => Route::Password,
        Holder::Human => {
            if has_account || !legacy_open {
                Route::Password
            } else {
                Route::Claim
            }
        }
        Holder::Free => {
            if has_account {
                Route::Password
            } else {
                Route::Create
            }
        }
    }
}
// mirror-end login_route

// mirror-begin first_ask_of
/// What a route asks first.
#[must_use]
pub const fn first_ask_of(route: Route) -> FirstAsk {
    match route {
        Route::Create => FirstAsk::NewPassword,
        Route::Claim | Route::Password => FirstAsk::Password,
        Route::Refuse => FirstAsk::Taken,
    }
}
// mirror-end first_ask_of

// mirror-begin may_bind
/// Whether a session may bind to the holder of a name once its secret is `proved`.
#[must_use]
pub const fn may_bind(holder: Holder, proved: bool) -> bool {
    match holder {
        Holder::Human | Holder::Free => proved,
        Holder::OtherPlayer | Holder::NonPlayer => false,
    }
}
// mirror-end may_bind

impl From<&NameOwner> for Holder {
    fn from(o: &NameOwner) -> Self {
        match o {
            NameOwner::Free(_) => Self::Free,
            NameOwner::Human(_) => Self::Human,
            NameOwner::OtherPlayer => Self::OtherPlayer,
            NameOwner::NonPlayer => Self::NonPlayer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLDERS: [Holder; 4] = [
        Holder::Free,
        Holder::Human,
        Holder::OtherPlayer,
        Holder::NonPlayer,
    ];
    const BOOLS: [bool; 2] = [false, true];

    #[test]
    fn an_agent_pc_asks_exactly_like_any_human() {
        for a in BOOLS {
            for a2 in BOOLS {
                for l in BOOLS {
                    for l2 in BOOLS {
                        let agent = first_ask_of(login_route(Holder::OtherPlayer, a, l));
                        let human = first_ask_of(login_route(Holder::Human, a2, l2));
                        assert_eq!(agent, FirstAsk::Password);
                        assert_eq!(agent, human);
                    }
                }
            }
        }
    }

    #[test]
    fn only_free_names_create_and_only_open_legacy_humans_claim() {
        use Route::{Claim, Create, Password, Refuse};
        // (holder, has_account, legacy_open) -> route, all 16 rows.
        let table = [
            (Holder::Free, false, false, Create),
            (Holder::Free, false, true, Create),
            (Holder::Free, true, false, Password),
            (Holder::Free, true, true, Password),
            (Holder::Human, false, false, Password),
            (Holder::Human, false, true, Claim),
            (Holder::Human, true, false, Password),
            (Holder::Human, true, true, Password),
            (Holder::OtherPlayer, false, false, Password),
            (Holder::OtherPlayer, false, true, Password),
            (Holder::OtherPlayer, true, false, Password),
            (Holder::OtherPlayer, true, true, Password),
            (Holder::NonPlayer, false, false, Refuse),
            (Holder::NonPlayer, false, true, Refuse),
            (Holder::NonPlayer, true, false, Refuse),
            (Holder::NonPlayer, true, true, Refuse),
        ];
        for (h, a, l, want) in table {
            assert_eq!(login_route(h, a, l), want, "{h:?} {a} {l}");
        }
        assert_eq!(first_ask_of(Create), FirstAsk::NewPassword);
        assert_eq!(first_ask_of(Refuse), FirstAsk::Taken);
    }

    #[test]
    fn nothing_binds_an_agent_or_an_npc() {
        for h in HOLDERS {
            for p in BOOLS {
                let ok = may_bind(h, p);
                assert_eq!(
                    ok,
                    p && matches!(h, Holder::Human | Holder::Free),
                    "{h:?} {p}"
                );
            }
        }
        let owners = [
            (NameOwner::Free("pc_a".into()), Holder::Free),
            (NameOwner::Human("pc_a".into()), Holder::Human),
            (NameOwner::OtherPlayer, Holder::OtherPlayer),
            (NameOwner::NonPlayer, Holder::NonPlayer),
        ];
        for (o, h) in owners {
            assert_eq!(Holder::from(&o), h);
        }
    }
}
