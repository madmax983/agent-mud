//! World hooks: the engine side of world scripts.
//!
//! A world hook is a pure function from a read-only [`HookInput`] to a
//! closed list of [`ScriptEffect`]s. The engine admits that list
//! all-or-nothing (the Verus-proved [`tally_admissible`]), validates every
//! effect against the world, and applies it with its own helpers inside
//! the running command. No hook holds the world.
//!
//! This crate knows no scripting language. `mud-script` implements
//! [`WorldHooks`] with Rhai; tests implement it with plain Rust.
//!
//! Two kernels here are mirrored in `formal/verus/src/hooks.rs`:
//! [`hook_step`] (the breaker of a faulting hook) and
//! [`tally_step`]/[`tally_admissible`] (effect admission, at most one
//! reward per hook). The reward pools have their own kernels in
//! [`crate::rewards`].

use std::collections::BTreeMap;
use std::fmt;

use mud_rules::{DamageDice, DiceRng, FixedDice, SeededDice};
use serde::{Deserialize, Serialize};

use crate::ids::{EntityId, ItemId, PoolId, ProtoId, RoomId};
use crate::pack::Direction;
use crate::rewards::PoolFacts;

/// The most world flags a world keeps.
pub const FLAG_CAP: usize = 256;
/// The most marks one entity carries.
pub const MARK_CAP: usize = 32;
/// The longest flag or mark name, in characters.
pub const NAME_MAX: usize = 64;
/// The longest text value of a flag or mark, in characters.
pub const FLAG_TEXT_MAX: usize = 64;
/// The longest list a hook view holds.
pub const VIEW_LIST_CAP: usize = 32;
/// The longest effect text, in characters.
pub const EFFECT_TEXT_MAX: usize = 500;
/// The highest `max_effects` a pack may set; the admission proof assumes it.
pub const MAX_EFFECTS_CEILING: u8 = 16;
/// The longest fault message kept in the audit.
pub const FAULT_MESSAGE_MAX: usize = 300;

/// The dice of a world: seeded in play, fixed in tests.
///
/// The world never gives its dice away. A hook gets a clone, and the world
/// takes the advanced clone back only when the hook's effects are admitted.
#[derive(Debug, Clone)]
pub struct WorldDice(Dice);

#[derive(Debug, Clone)]
enum Dice {
    Seeded(SeededDice),
    Fixed(FixedDice),
}

impl WorldDice {
    /// Seeded dice at `state`.
    #[must_use]
    pub const fn seeded(state: u64) -> Self {
        Self(Dice::Seeded(SeededDice::new(state)))
    }

    /// Dice that roll `faces`, for tests and dry runs.
    pub fn fixed(faces: impl IntoIterator<Item = u8>) -> Self {
        Self(Dice::Fixed(FixedDice::new(faces)))
    }

    /// The seeded state, to store; `None` for fixed dice.
    #[must_use]
    pub const fn seeded_state(&self) -> Option<u64> {
        match &self.0 {
            Dice::Seeded(d) => Some(d.state()),
            Dice::Fixed(_) => None,
        }
    }
}

impl DiceRng for WorldDice {
    fn d6(&mut self) -> u8 {
        match &mut self.0 {
            Dice::Seeded(d) => d.d6(),
            Dice::Fixed(d) => d.d6(),
        }
    }

    fn below(&mut self, n: u32) -> u32 {
        match &mut self.0 {
            Dice::Seeded(d) => d.below(n),
            Dice::Fixed(d) => d.below(n),
        }
    }
}

/// One hook point.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookId {
    /// Someone walked into a room.
    Enter(RoomId),
    /// Someone spoke in a room.
    Speech(RoomId),
    /// Someone picked up an item of a prototype.
    Take(ProtoId),
    /// Someone used an item of a prototype that has no engine use.
    Use(ProtoId),
    /// Someone typed a custom verb.
    Verb(String),
}

impl HookId {
    /// The key the manifest, the audit and the breakers use, such as
    /// `rooms.ossuary_crypt.on_enter` or `verbs.pray`.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Enter(r) => format!("rooms.{r}.on_enter"),
            Self::Speech(r) => format!("rooms.{r}.on_speech"),
            Self::Take(p) => format!("items.{p}.on_take"),
            Self::Use(p) => format!("items.{p}.on_use"),
            Self::Verb(v) => format!("verbs.{v}"),
        }
    }

    /// Whether the hook decides its command (a verb or `use`), rather
    /// than reacting after it (enter, speech, take).
    #[must_use]
    pub const fn is_command(&self) -> bool {
        matches!(self, Self::Use(_) | Self::Verb(_))
    }
}

impl fmt::Display for HookId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// A custom verb a pack declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerbInfo {
    /// The verb, such as `pray`.
    pub name: String,
    /// How to type it, for `help`.
    pub usage: String,
    /// Where it works; empty means anywhere.
    pub rooms: Vec<RoomId>,
}

impl VerbInfo {
    /// Whether it works in `room`.
    #[must_use]
    pub fn works_in(&self, room: &RoomId) -> bool {
        self.rooms.is_empty() || self.rooms.contains(room)
    }
}

/// The limits that change how hooks run. They are pack data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookLimits {
    /// The most effects one hook call may return (at most 16).
    pub max_effects: u8,
    /// Consecutive faults that trip a hook's breaker.
    pub max_faults: u8,
}

impl Default for HookLimits {
    fn default() -> Self {
        Self {
            max_effects: 8,
            max_faults: 3,
        }
    }
}

/// What a hook call answered.
#[derive(Debug, Clone)]
pub struct HookReply {
    /// The effects and the dice advanced by exactly the rolls made, or why
    /// the call faulted.
    pub outcome: Result<(Vec<ScriptEffect>, WorldDice), String>,
    /// The script's own log lines, for the observer.
    pub log: Vec<String>,
}

/// The scripts of a world, as the engine sees them.
pub trait WorldHooks: Send + Sync + fmt::Debug {
    /// The content hash of the scripts, such as `rhai-0123456789abcdef`.
    fn version(&self) -> String;
    /// Whether a script handles `hook`.
    fn has(&self, hook: &HookId) -> bool;
    /// Whether `name` is a custom verb.
    fn is_verb(&self, name: &str) -> bool {
        self.verb(name).is_some()
    }
    /// The custom verb `name`.
    fn verb(&self, name: &str) -> Option<VerbInfo>;
    /// Every custom verb, by name.
    fn verbs(&self) -> Vec<VerbInfo>;
    /// The limits.
    fn limits(&self) -> HookLimits;
    /// Runs `hook` on `input`. Pure: it reads nothing else and changes
    /// nothing. On success it returns `dice` advanced by exactly the rolls
    /// it made.
    fn call(&self, hook: &HookId, input: &HookInput, dice: WorldDice) -> HookReply;
}

/// The value of a flag or a mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlagValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A short text, at most [`FLAG_TEXT_MAX`] characters.
    Text(String),
}

impl fmt::Display for FlagValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Text(t) => write!(f, "{t:?}"),
        }
    }
}

/// An entity as a hook sees it. It carries `kind`, never a tier, a mind
/// or whether a human plays it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorFacts {
    /// The id.
    pub id: EntityId,
    /// The name.
    pub name: String,
    /// `player`, `npc` or `mob`.
    pub kind: String,
    /// Where it stands.
    pub room: RoomId,
    /// HP now.
    pub hp: i32,
    /// HP at full health.
    pub hp_max: i32,
    /// `unhurt`, `badly hurt`, `reeling`, `dying`, `dead` or `unconscious`.
    pub condition: String,
    /// `st`, `dx`, `iq`, `ht`.
    pub attrs: BTreeMap<String, i32>,
    /// Every pack skill, at its effective level.
    pub skills: BTreeMap<String, i32>,
    /// The prototypes it carries (hidden ones left out).
    pub inventory: Vec<ProtoId>,
    /// The prototypes it wields or wears.
    pub equipped: Vec<ProtoId>,
    /// Its money.
    pub currency: i64,
    /// Its marks. Mind views leave them out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marks: Option<BTreeMap<String, FlagValue>>,
}

/// An exit as a hook sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitFacts {
    /// The short direction, such as `n` or `d`.
    pub dir: String,
    /// Where it leads.
    pub to: RoomId,
    /// Whether it is locked now.
    pub locked: bool,
    /// Whether it has a lock at all.
    pub lockable: bool,
}

/// Someone in the room, as a hook sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFacts {
    /// The id.
    pub id: EntityId,
    /// The name.
    pub name: String,
    /// `player`, `npc` or `mob`.
    pub kind: String,
    /// The condition word.
    pub condition: String,
}

/// The hook's room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomFacts {
    /// The id.
    pub id: RoomId,
    /// The title.
    pub title: String,
    /// Whether the room is dark by nature.
    pub dark: bool,
    /// Whether it has light now.
    pub lit: bool,
    /// Whether the watch keeps the peace.
    pub safe: bool,
    /// The exits.
    pub exits: Vec<ExitFacts>,
    /// Who else is visibly here (at most [`VIEW_LIST_CAP`]).
    pub present: Vec<TargetFacts>,
    /// The prototypes on the floor (at most [`VIEW_LIST_CAP`]).
    pub items: Vec<ProtoId>,
}

/// The item a take or use hook is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemFacts {
    /// The instance.
    pub id: ItemId,
    /// The prototype.
    pub proto: ProtoId,
    /// The name.
    pub name: String,
}

/// An NPC or a mob of the cast, as a hook sees it from anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CastFacts {
    /// The name.
    pub name: String,
    /// Where it is; `None` when it is hidden, dead or away.
    pub room: Option<RoomId>,
    /// Not dead.
    pub alive: bool,
    /// In the world, alive and not hidden.
    pub present: bool,
}

/// Everything a world hook reads. Built by the engine; never holds player
/// text in a serialized field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookInput {
    /// The hook key.
    pub hook: String,
    /// The game clock.
    pub now_ms: u64,
    /// Who set it off.
    pub actor: ActorFacts,
    /// Where.
    pub room: RoomFacts,
    /// Where the actor came from, for `on_enter`.
    pub from: Option<RoomId>,
    /// Player text: the speech, or a verb's arguments. A script gets it
    /// only as an opaque value it can test words in.
    #[serde(skip)]
    pub text: Option<String>,
    /// The resolved target of a verb or of `use .. on`.
    pub target: Option<TargetFacts>,
    /// The item of a take or use hook.
    pub item: Option<ItemFacts>,
    /// The world flags.
    pub flags: BTreeMap<String, FlagValue>,
    /// The NPCs and mobs of the cast.
    pub cast: BTreeMap<EntityId, CastFacts>,
    /// What is left of each reward pool in its period.
    #[serde(default)]
    pub pools: BTreeMap<PoolId, PoolFacts>,
}

/// One thing a hook asks the engine to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptEffect {
    /// A line for the actor.
    Msg(String),
    /// A line for everyone else in the room.
    Narrate(String),
    /// Fail the command with this text (verb and use hooks only, alone).
    Refuse(String),
    /// Unlock an exit of the hook room, both sides.
    Unlock(Direction),
    /// Lock an exit of the hook room, both sides.
    Lock(Direction),
    /// Set a world flag.
    SetFlag(String, FlagValue),
    /// Clear a world flag.
    ClearFlag(String),
    /// Set a mark on the actor.
    Mark(String, FlagValue),
    /// Clear a mark of the actor.
    Unmark(String),
    /// Heal the actor.
    Heal(DamageDice),
    /// Hurt the actor (crushing, after DR).
    Hurt(DamageDice),
    /// Give the actor money and items from a pack reward pool, recorded
    /// in the ledger. At most one per hook, only within what is left of
    /// the pool's budget for the period.
    Reward {
        /// The pool.
        pool: PoolId,
        /// The currency.
        currency: i64,
        /// The items, by prototype.
        items: BTreeMap<ProtoId, u32>,
    },
}

impl ScriptEffect {
    /// The class admission counts it as.
    #[must_use]
    pub const fn class(&self) -> EffectClass {
        match self {
            Self::Msg(_) | Self::Narrate(_) => EffectClass::Text,
            Self::Refuse(_) => EffectClass::Refuse,
            Self::Heal(_) | Self::Hurt(_) => EffectClass::Body,
            Self::Reward { .. } => EffectClass::Mint,
            Self::Unlock(_)
            | Self::Lock(_)
            | Self::SetFlag(..)
            | Self::ClearFlag(_)
            | Self::Mark(..)
            | Self::Unmark(_) => EffectClass::State,
        }
    }

    /// A short form for the observer, such as `unlock(d)`.
    #[must_use]
    pub fn describe(&self) -> String {
        let cut = |t: &str| -> String {
            let mut s: String = t.chars().take(40).collect();
            if t.chars().count() > 40 {
                s.push('…');
            }
            s
        };
        match self {
            Self::Msg(t) => format!("msg({:?})", cut(t)),
            Self::Narrate(t) => format!("narrate({:?})", cut(t)),
            Self::Refuse(t) => format!("refuse({:?})", cut(t)),
            Self::Unlock(d) => format!("unlock({})", d.short()),
            Self::Lock(d) => format!("lock({})", d.short()),
            Self::SetFlag(n, v) => format!("set_flag({n}, {v})"),
            Self::ClearFlag(n) => format!("clear_flag({n})"),
            Self::Mark(n, v) => format!("mark({n}, {v})"),
            Self::Unmark(n) => format!("unmark({n})"),
            Self::Heal(d) => format!("heal({d})"),
            Self::Hurt(d) => format!("hurt({d})"),
            Self::Reward {
                pool,
                currency,
                items,
            } => {
                let mut ask = vec![format!("currency {currency}")];
                ask.extend(items.iter().map(|(p, n)| format!("{p} x{n}")));
                format!("reward({pool}, {})", ask.join(", "))
            }
        }
    }
}

/// Whether `name` is a valid flag or mark name: lowercase words of
/// `[a-z0-9_]` joined by `.`, `:` or `-`, at most [`NAME_MAX`] characters.
#[must_use]
pub fn valid_flag_name(name: &str) -> bool {
    let word = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    name.len() <= NAME_MAX && name.split(['.', ':', '-']).all(word)
}

// ------------------------------------------------------------ the breaker

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin hook_outcome
/// What happened to a hook, as its breaker sees it.
pub enum HookOutcome {
    /// The hook ran and its effects were admitted.
    Ran,
    /// An error, a limit, a panic or an inadmissible effect list.
    Faulted,
    /// The scripts were loaded or reloaded.
    Reloaded,
}
// mirror-end hook_outcome

// mirror-begin hook_allowed
/// Whether a hook with `faults` consecutive faults may run.
#[must_use]
pub const fn hook_allowed(faults: u8, max: u8) -> bool {
    faults < max
}
// mirror-end hook_allowed

// mirror-begin hook_step
/// One breaker step: the new consecutive-fault count and whether the hook may run next.
#[must_use]
pub const fn hook_step(faults: u8, outcome: HookOutcome, max: u8) -> (u8, bool) {
    match outcome {
        HookOutcome::Reloaded => (0, 0 < max),
        HookOutcome::Ran => {
            if faults < max {
                (0, true)
            } else {
                (faults, false)
            }
        }
        HookOutcome::Faulted => {
            let next = faults.saturating_add(1);
            (next, next < max)
        }
    }
}
// mirror-end hook_step

// ------------------------------------------------------------ admission

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin effect_class
/// The class of a script effect, as admission sees it.
pub enum EffectClass {
    /// msg or narrate.
    Text,
    /// refuse.
    Refuse,
    /// heal or hurt.
    Body,
    /// lock, unlock, flags and marks.
    State,
    /// reward: money or items minted from a pack pool, into the ledger.
    Mint,
}
// mirror-end effect_class

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
// mirror-begin effect_tally
/// What a hook's effect list holds so far.
pub struct Tally {
    /// Effects seen (saturating).
    pub effects: u8,
    /// `refuse()` effects seen (saturating).
    pub refuses: u8,
    /// `heal()` and `hurt()` effects seen (saturating).
    pub bodies: u8,
    /// `reward()` effects seen (saturating).
    pub mints: u8,
}
// mirror-end effect_tally

// mirror-begin tally_step
/// Adds one effect to the tally.
#[must_use]
pub const fn tally_step(t: Tally, class: EffectClass) -> Tally {
    let effects = t.effects.saturating_add(1);
    match class {
        EffectClass::Refuse => Tally {
            effects,
            refuses: t.refuses.saturating_add(1),
            bodies: t.bodies,
            mints: t.mints,
        },
        EffectClass::Body => Tally {
            effects,
            refuses: t.refuses,
            bodies: t.bodies.saturating_add(1),
            mints: t.mints,
        },
        EffectClass::Mint => Tally {
            effects,
            refuses: t.refuses,
            bodies: t.bodies,
            mints: t.mints.saturating_add(1),
        },
        EffectClass::Text | EffectClass::State => Tally {
            effects,
            refuses: t.refuses,
            bodies: t.bodies,
            mints: t.mints,
        },
    }
}
// mirror-end tally_step

// mirror-begin tally_admissible
/// Whether a finished tally may be applied.
#[must_use]
pub const fn tally_admissible(t: Tally, max: u8, command_hook: bool) -> bool {
    t.effects <= max
        && t.bodies <= 1
        && t.mints <= 1
        && (t.refuses == 0 || (command_hook && t.effects == 1))
}
// mirror-end tally_admissible

/// The tally of a whole effect list.
#[must_use]
pub fn tally(effects: &[ScriptEffect]) -> Tally {
    effects
        .iter()
        .map(ScriptEffect::class)
        .fold(Tally::default(), tally_step)
}

/// Why an inadmissible tally is refused: the first rule it breaks.
#[must_use]
pub fn tally_reason(t: Tally, max: u8, command_hook: bool) -> String {
    if t.effects > max {
        format!("more than {max} effects")
    } else if t.bodies > 1 {
        "at most one heal() or hurt() per hook".to_owned()
    } else if t.mints > 1 {
        "at most one reward() per hook".to_owned()
    } else if t.refuses > 0 && !command_hook {
        "refuse() is only for verb and on_use scripts".to_owned()
    } else {
        "refuse() must be the only effect".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    use EffectClass::{Body, Mint, Refuse, State, Text};
    use HookOutcome::{Faulted, Ran, Reloaded};

    #[test]
    fn hook_step_matches_its_spec_table() {
        assert_eq!(hook_step(0, Ran, 3), (0, true));
        assert_eq!(hook_step(2, Ran, 3), (0, true));
        assert_eq!(hook_step(3, Ran, 3), (3, false), "tripped stays tripped");
        assert_eq!(hook_step(0, Faulted, 3), (1, true));
        assert_eq!(hook_step(2, Faulted, 3), (3, false));
        assert_eq!(hook_step(255, Faulted, 255), (255, false));
        assert_eq!(hook_step(9, Reloaded, 3), (0, true));
        assert_eq!(hook_step(9, Reloaded, 0), (0, false));
        assert!(hook_allowed(2, 3));
        assert!(!hook_allowed(3, 3));
    }

    fn admissible_spec(s: &[EffectClass], max: u8, cmd: bool) -> bool {
        let count = |c| s.iter().filter(|x| **x == c).count();
        s.len() <= usize::from(max)
            && count(Body) <= 1
            && count(Mint) <= 1
            && (count(Refuse) == 0 || (cmd && s.len() == 1))
    }

    fn admit(s: &[EffectClass], max: u8, cmd: bool) -> bool {
        tally_admissible(
            s.iter().copied().fold(Tally::default(), tally_step),
            max,
            cmd,
        )
    }

    #[test]
    fn tally_admits_exactly_the_spec() {
        assert!(admit(&[], 8, false));
        assert!(admit(&[Text], 8, false));
        assert!(!admit(&[Text; 17], 16, true));
        assert!(admit(&[Text; 16], 16, true));
        assert!(!admit(&[Body, Body], 8, true));
        assert!(admit(&[Body, Text, State], 8, false));
        assert!(admit(&[Refuse], 8, true));
        assert!(!admit(&[Refuse, Text], 8, true));
        assert!(!admit(&[Refuse], 8, false), "no refuse in an event hook");
        assert!(
            admit(&[Mint, Body, Text], 8, false),
            "a reward in an event hook"
        );
        assert!(!admit(&[Mint, Mint], 8, true), "at most one reward");
        assert!(
            !admit(&[Refuse, Mint], 8, true),
            "never a reward with a refuse"
        );
        let t = [Mint, Mint]
            .iter()
            .copied()
            .fold(Tally::default(), tally_step);
        assert_eq!(tally_reason(t, 8, true), "at most one reward() per hook");
        let t = [Refuse, Text]
            .iter()
            .copied()
            .fold(Tally::default(), tally_step);
        assert_eq!(tally_reason(t, 8, true), "refuse() must be the only effect");
        let t = tally_step(Tally::default(), Refuse);
        assert_eq!(
            tally_reason(t, 8, false),
            "refuse() is only for verb and on_use scripts"
        );
        let t = [Body, Body]
            .iter()
            .copied()
            .fold(Tally::default(), tally_step);
        assert_eq!(
            tally_reason(t, 8, false),
            "at most one heal() or hurt() per hook"
        );
        let t = [Text; 9].iter().copied().fold(Tally::default(), tally_step);
        assert_eq!(tally_reason(t, 8, false), "more than 8 effects");
    }

    #[test]
    fn hook_keys_and_kinds() {
        assert_eq!(HookId::Verb("pray".into()).key(), "verbs.pray");
        assert_eq!(
            HookId::Enter(RoomId::from("crypt")).to_string(),
            "rooms.crypt.on_enter"
        );
        assert_eq!(
            HookId::Take(ProtoId::from("ore")).key(),
            "items.ore.on_take"
        );
        assert!(HookId::Use(ProtoId::from("charm")).is_command());
        assert!(!HookId::Speech(RoomId::from("alley")).is_command());
        assert!(valid_flag_name("ossuary.first_visitor"));
        assert!(valid_flag_name("a:b-c_1"));
        assert!(!valid_flag_name("Bad"));
        assert!(!valid_flag_name("a..b"));
        assert!(!valid_flag_name(&"a".repeat(65)));
        assert_eq!(
            ScriptEffect::SetFlag("a".into(), FlagValue::Text("x".into())).describe(),
            "set_flag(a, \"x\")"
        );
        assert_eq!(
            ScriptEffect::Msg("a".repeat(50)).describe().chars().count(),
            48
        );
        let reward = ScriptEffect::Reward {
            pool: PoolId::from("bounty"),
            currency: 20,
            items: BTreeMap::from([(ProtoId::from("healing_draught"), 1)]),
        };
        assert_eq!(
            reward.describe(),
            "reward(bounty, currency 20, healing_draught x1)"
        );
        assert_eq!(reward.class(), Mint);
    }

    fn class() -> impl Strategy<Value = EffectClass> {
        prop::sample::select(vec![Text, Refuse, Body, State, Mint])
    }

    fn outcome() -> impl Strategy<Value = HookOutcome> {
        prop::sample::select(vec![Ran, Faulted, Reloaded])
    }

    proptest! {
        #[test]
        fn tally_admissible_equals_admissible_spec_for_any_list(
            s in prop::collection::vec(class(), 0..300),
            max in 0u8..=16,
            cmd in any::<bool>(),
        ) {
            prop_assert_eq!(admit(&s, max, cmd), admissible_spec(&s, max, cmd));
        }

        #[test]
        fn a_hook_runs_at_most_max_consecutive_faulting_times(
            steps in prop::collection::vec(outcome(), 0..400),
            max in 0u8..=255,
        ) {
            let (mut faults, mut allowed) = (0u8, hook_allowed(0, max));
            let mut run_faulting = 0u32;
            for s in steps {
                if s == Reloaded {
                    run_faulting = 0;
                } else if !allowed {
                    // A tripped hook is not called; only a reload reopens it.
                    continue;
                } else if s == Faulted {
                    run_faulting += 1;
                }
                if s == Ran {
                    run_faulting = 0;
                }
                (faults, allowed) = hook_step(faults, s, max);
                prop_assert!(run_faulting <= u32::from(max));
                prop_assert_eq!(allowed, hook_allowed(faults, max));
            }
        }
    }
}
