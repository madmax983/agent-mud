//! The supervisor of the littleguy MUD: it decides when minds think.
//!
//! The MUD server owns the world and its lifecycle; minds (littleguy
//! agents, harvest agents, plugin-agent NPCs, or scripts) own only
//! decisions, and they act only through the same command surface as a
//! human. The supervisor sits between: the attention router decides who
//! wakes, the budgets decide whether a model may be called, and the
//! Turing gate makes hidden agent PCs behave like people at keyboards.

pub mod budget;
pub mod chatter;
pub mod gate;
pub mod mind;
pub mod router;
pub mod runtime;
pub mod scripted;
pub mod turing;
