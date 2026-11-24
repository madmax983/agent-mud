//! The GURPS-like resolution core of the littleguy MUD.
//!
//! One mechanic resolves everything: roll 3d6, and succeed when the total
//! is at most the effective skill. Combat, stealth, lockpicking, haggling
//! and self-control checks all use it. The crate knows nothing about a
//! setting. Swords and goblins live in a world pack, not here.
//!
//! Every random number flows through [`DiceRng`], so a test or a replay
//! fixes the dice. The kernels ([`classify`], [`injury`], [`wound_state`])
//! are pure and are mirrored by the Verus proofs in `formal/verus`.

mod combat;
mod damage;
mod dice;
mod roll;
mod self_control;
mod stats;

pub use combat::{AttackInput, AttackResolution, DefenseKind, resolve_attack};
pub use damage::{DamageDice, DamageType, ParseDamageError, injury, swing, thrust};
pub use dice::{DiceRng, FixedDice, SeededDice};
pub use roll::{Outcome, SuccessRoll, classify, success_roll};
pub use self_control::{SelfControlRoll, self_control_roll};
pub use stats::{Attributes, Pools, WoundState, basic_dodge, defense_from_skill, wound_state};
