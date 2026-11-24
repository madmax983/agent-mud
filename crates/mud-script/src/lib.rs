//! Sandboxed Rhai scripts for world packs (ADR 0010).
//!
//! Scripts are data in, data out:
//!
//! - A **world hook** (a room's `on_enter` or `on_speech`, an item's
//!   `on_take` or `on_use`, or a custom verb) reads a read-only view and
//!   returns a list of effects. The engine admits that list all-or-nothing
//!   and applies it inside the running command ([`ScriptHost`] implements
//!   [`mud_world::hooks::WorldHooks`]).
//! - A **mind script** reads a read-only view of a wake and returns command
//!   lines, which the supervisor types through the same gate as any mind.
//!
//! No script holds the world. Player text reaches a script only as an
//! opaque [`untrusted::Untrusted`] value it can test dictionary words in;
//! it can never quote it. This is the only crate that depends on Rhai, and
//! its engine starts from `Engine::new_raw` with a curated set of natives
//! ([`sandbox`]).

pub mod api;
pub mod convert;
pub mod crypt;
pub mod dry_run;
pub mod error;
pub mod host;
pub mod limits;
pub mod load;
pub mod manifest;
pub mod mind;
pub mod sandbox;
pub mod untrusted;
pub mod view;

pub use crypt::crypt_v01_sources;
pub use error::ScriptError;
pub use host::{Compiled, ScriptHost};
pub use limits::Limits;
pub use load::ScriptSources;
pub use manifest::{Manifest, MindRuntime};
pub use mind::{EventActor, MindPlan, MindReply, MindWake, ScriptEvent};
