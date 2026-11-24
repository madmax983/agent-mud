//! The world of the littleguy MUD: the room graph, the world pack, the
//! verb parser and the deterministic command engine.
//!
//! Humans and agents share one command surface. Every state change is a
//! line parsed by [`parser::parse`] and resolved by [`World::execute`].
//! [`Game`] wraps a world for concurrent use: it persists each command to
//! SQLite and broadcasts its events.

pub mod engine;
pub mod event;
pub mod game;
pub mod hooks;
pub mod ids;
pub mod pack;
pub mod parser;
pub mod schedule;
pub mod state;
pub mod store;

pub use engine::{
    Audit, CommandResult, ErrorCode, HookStatus, HooksChange, LoginError, StateChange, World,
};
pub use event::{ActorRef, Event, EventKind};
pub use game::{Clock, Game, ManualClock, SystemClock};
pub use ids::{EntityId, ItemId, ProtoId, RoomId, Seq, ShopId, SkillId};
pub use pack::WorldPack;
pub use parser::{Command, SESSION_WORDS, parse, parse_with};
pub use state::NameOwner;
