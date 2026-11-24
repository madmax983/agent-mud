//! The login route table.
//!
//! Mirrors `crates/mud-accounts/src/route.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! Proved for all inputs:
//! - R1 (masquerade): an agent PC and any human account are asked the same
//!   first question, `Password:`, whatever their account and legacy state.
//! - R2: only a free name with no account creates a character.
//! - R3: only a human with no account claims, and only while legacy
//!   characters are open.
//! - R4: a login is refused iff the name belongs to an NPC or a mob.
//! - R5: a session binds only with a proved secret, and only to a human
//!   or a free name: never to an agent PC, an NPC or a mob.

use vstd::prelude::*;

verus! {

#[derive(Clone, Copy)]
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

#[derive(Clone, Copy)]
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

#[derive(Clone, Copy)]
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

/// The route table over plain values.
pub open spec fn spec_login_route(holder: Holder, has_account: bool, legacy_open: bool) -> Route {
    match holder {
        Holder::NonPlayer => Route::Refuse,
        Holder::OtherPlayer => Route::Password,
        Holder::Human => if has_account || !legacy_open { Route::Password } else { Route::Claim },
        Holder::Free => if has_account { Route::Password } else { Route::Create },
    }
}

/// The first question over plain values.
pub open spec fn spec_first_ask_of(route: Route) -> FirstAsk {
    match route {
        Route::Create => FirstAsk::NewPassword,
        Route::Claim | Route::Password => FirstAsk::Password,
        Route::Refuse => FirstAsk::Taken,
    }
}

// mirror-begin login_route
/// The route of a login: who holds the name, whether it has an account, and whether legacy characters may still be claimed.
#[must_use]
pub const fn login_route(holder: Holder, has_account: bool, legacy_open: bool) -> (r: Route)
    ensures
        r == spec_login_route(holder, has_account, legacy_open),
        // R2: only a free name with no account creates.
        r is Create ==> holder is Free && !has_account,
        // R3: only an open legacy human with no account claims.
        r is Claim ==> holder is Human && !has_account && legacy_open,
        // R4: refused iff an NPC or a mob holds the name.
        (r is Refuse) <==> (holder is NonPlayer),
{
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
pub const fn first_ask_of(route: Route) -> (r: FirstAsk)
    ensures
        r == spec_first_ask_of(route),
{
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
pub const fn may_bind(holder: Holder, proved: bool) -> (r: bool)
    ensures
        // R5: a proved secret, and a human or a free name.
        r ==> proved && (holder is Human || holder is Free),
        r <==> proved && (holder is Human || holder is Free),
{
    match holder {
        Holder::Human | Holder::Free => proved,
        Holder::OtherPlayer | Holder::NonPlayer => false,
    }
}
// mirror-end may_bind

/// R1, the masquerade: an agent PC is asked exactly what any human account
/// is asked, `Password:`, for every account and legacy state of either.
pub proof fn lemma_agent_pc_asks_like_a_human(a: bool, a2: bool, l: bool, l2: bool)
    ensures
        spec_first_ask_of(spec_login_route(Holder::OtherPlayer, a, l)) == FirstAsk::Password,
        spec_first_ask_of(spec_login_route(Holder::Human, a2, l2)) == FirstAsk::Password,
        spec_first_ask_of(spec_login_route(Holder::OtherPlayer, a, l)) == spec_first_ask_of(
            spec_login_route(Holder::Human, a2, l2),
        ),
{
}

/// R2 and R3 over the spec table, for every input.
pub proof fn lemma_create_and_claim_are_narrow(h: Holder, a: bool, l: bool)
    ensures
        spec_login_route(h, a, l) is Create ==> h is Free && !a,
        spec_login_route(h, a, l) is Claim ==> h is Human && !a && l,
        (spec_login_route(h, a, l) is Refuse) <==> (h is NonPlayer),
{
}

} // verus!
