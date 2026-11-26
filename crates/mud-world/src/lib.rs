//! The world of the littleguy MUD: the room graph, the world pack, the
//! verb parser and the deterministic command engine.
//!
//! Humans and agents share one command surface. Every state change is a
//! line parsed by [`parser::parse`] and resolved by [`World::execute`].
//! [`Game`] wraps a world for concurrent use: it persists each command to
//! SQLite and broadcasts its events.

pub mod archive;
pub mod durability;
pub mod engine;
pub mod event;
pub mod game;
pub mod hash;
pub mod hooks;
pub mod ids;
pub mod pack;
pub mod parser;
pub mod rewards;
pub mod schedule;
pub mod side;
pub mod state;
pub mod store;

pub use engine::{
    Audit, CAME_TO_TAG, CommandResult, ErrorCode, HookStatus, HooksChange, LoginError, StateChange,
    TickOutcome, World,
};
pub use event::{
    ActorRef, BODY_VERBS, Event, EventKind, VERB_COLLAPSE, VERB_DIE, VERB_RESPAWN, VERB_STAGGER,
};
pub use game::{
    Clock, CommitProbe, Durability, Game, ManualClock, RELOAD_EVENTS, Retained, Retention,
    SideError, SystemClock,
};
pub use ids::{EntityId, ItemId, PoolId, ProtoId, RoomId, Seq, ShopId, SkillId};
pub use pack::WorldPack;
pub use parser::{Command, SESSION_WORDS, parse, parse_with};
pub use side::{SideOp, SideState};
pub use state::NameOwner;
pub use store::StoreError;
