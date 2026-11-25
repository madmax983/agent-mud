//! Player accounts of the littleguy MUD (ADR 0009).
//!
//! Accounts live outside the world. This crate keeps bcrypt hashes, web
//! resume tokens and each account's known addresses (hashed) in its own
//! SQLite file, `accounts.db`. None of it reaches `WorldState`, the world
//! snapshot, events, `Audit`, a view or `World::execute`: the server asks
//! [`Accounts`] whether a login may bind, then calls `Game::login` with the
//! name alone.
//!
//! Every secret this crate makes comes from the OS CSPRNG (`getrandom`),
//! never from the world's dice. Every password answer costs exactly one
//! bcrypt verify, and an agent PC answers exactly like a human account: the
//! route kernel ([`route`]) and the backoff kernel ([`throttle`]) are
//! proved in `formal/verus`.

pub mod accounts;
pub mod device;
pub mod error;
pub mod hasher;
pub mod name;
pub mod peer;
pub mod policy;
pub mod route;
pub mod secret;
pub mod store;
pub mod throttle;

pub use accounts::{
    AccountStatsView, AccountSummary, Accounts, AccountsConfig, AuthVia, Authenticated, Legacy,
    NewPurpose, ResetTicket, SignIn, ThrottleConfig,
};
pub use device::{IssuedToken, Series};
pub use error::{AccountError, AuthError, wait_text};
pub use hasher::Hasher;
pub use name::NameKey;
pub use peer::Peer;
pub use policy::{PasswordRejection, check_new_password};
pub use route::{FirstAsk, Holder, Route, first_ask_of, login_route, may_bind};
pub use secret::Secret;
pub use store::{AccountStore, Origin};
pub use throttle::{CreateRefusal, Held, Throttle, ThrottleOp, ThrottlePolicy, Throttles};
