//! World packs: the data that makes a setting.
//!
//! The engine knows nothing about swords or silver. A pack is five TOML
//! files in one folder: `world.toml` (rooms), `skills.toml`, `items.toml`,
//! `economy.toml` and `cast.toml`. [`WorldPack::load`] parses them and
//! checks every cross reference, so a broken pack fails at start with a
//! list of each problem.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use mud_rules::{Attributes, DamageDice, DamageType};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ids::{EntityId, PoolId, ProtoId, RoomId, ShopId, SkillId};
use crate::rewards::{POOL_CURRENCY_MAX, POOL_ITEMS_MAX, RewardPool, valid_pool_id};

/// The words a command uses for the speaker. No player, and no cast
/// entry, may answer to one.
pub(crate) const SELF_WORDS: &[&str] = &["me", "self", "myself"];

/// Why a pack did not load.
#[derive(Debug, Error)]
pub enum PackError {
    /// A file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// The file.
        path: String,
        /// The cause.
        source: std::io::Error,
    },
    /// A file is not valid TOML for its schema.
    #[error("parsing {file}: {message}")]
    Parse {
        /// The file.
        file: &'static str,
        /// The parser message.
        message: String,
    },
    /// The files parse, but references between them are broken.
    #[error("the world pack is invalid:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

/// A compass or vertical direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// North.
    N,
    /// South.
    S,
    /// East.
    E,
    /// West.
    W,
    /// Northeast.
    Ne,
    /// Northwest.
    Nw,
    /// Southeast.
    Se,
    /// Southwest.
    Sw,
    /// Up.
    U,
    /// Down.
    D,
}

impl Direction {
    /// Every direction, in display order.
    pub const ALL: [Self; 10] = [
        Self::N,
        Self::S,
        Self::E,
        Self::W,
        Self::Ne,
        Self::Nw,
        Self::Se,
        Self::Sw,
        Self::U,
        Self::D,
    ];

    /// The full name, such as `north`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::N => "north",
            Self::S => "south",
            Self::E => "east",
            Self::W => "west",
            Self::Ne => "northeast",
            Self::Nw => "northwest",
            Self::Se => "southeast",
            Self::Sw => "southwest",
            Self::U => "up",
            Self::D => "down",
        }
    }

    /// The short form, such as `n`.
    #[must_use]
    pub const fn short(self) -> &'static str {
        match self {
            Self::N => "n",
            Self::S => "s",
            Self::E => "e",
            Self::W => "w",
            Self::Ne => "ne",
            Self::Nw => "nw",
            Self::Se => "se",
            Self::Sw => "sw",
            Self::U => "u",
            Self::D => "d",
        }
    }

    /// How a departure this way reads after a name: `leaves north`,
    /// `climbs up`.
    #[must_use]
    pub const fn leaving(self) -> &'static str {
        match self {
            Self::N => "leaves north",
            Self::S => "leaves south",
            Self::E => "leaves east",
            Self::W => "leaves west",
            Self::Ne => "leaves northeast",
            Self::Nw => "leaves northwest",
            Self::Se => "leaves southeast",
            Self::Sw => "leaves southwest",
            Self::U => "climbs up",
            Self::D => "climbs down",
        }
    }

    /// How an arrival from this side reads after a name: `arrives from
    /// the south`, `arrives from above`.
    #[must_use]
    pub const fn arriving_from(self) -> &'static str {
        match self {
            Self::N => "arrives from the north",
            Self::S => "arrives from the south",
            Self::E => "arrives from the east",
            Self::W => "arrives from the west",
            Self::Ne => "arrives from the northeast",
            Self::Nw => "arrives from the northwest",
            Self::Se => "arrives from the southeast",
            Self::Sw => "arrives from the southwest",
            Self::U => "arrives from above",
            Self::D => "arrives from below",
        }
    }

    /// The direction back.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::N => Self::S,
            Self::S => Self::N,
            Self::E => Self::W,
            Self::W => Self::E,
            Self::Ne => Self::Sw,
            Self::Sw => Self::Ne,
            Self::Nw => Self::Se,
            Self::Se => Self::Nw,
            Self::U => Self::D,
            Self::D => Self::U,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Direction {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|d| d.short() == t || d.name() == t)
            .ok_or(())
    }
}

/// The `[world]` table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorldMeta {
    /// The pack name.
    pub name: String,
    /// A display title.
    pub title: String,
    /// Where a new player appears.
    pub start_room: RoomId,
    /// Where a dead player wakes.
    pub respawn_room: RoomId,
    /// The skill penalty in an unlit dark room.
    #[serde(default = "default_darkness")]
    pub darkness_penalty: i32,
    /// The message of the day.
    #[serde(default)]
    pub motd: String,
}

const fn default_darkness() -> i32 {
    -3
}

/// The longest recovery step the pack may set, in seconds (an hour).
pub const MAX_RECOVERY_SECS: u64 = 3_600;

/// `[recovery]` in `world.toml`: how a downed entity comes to.
///
/// Every `every_secs`, each entity in the world regains 1 HP, up to its
/// maximum, and an unconscious one above 0 HP comes to. With
/// `come_to_roll`, an unconscious one still at 0 HP or below, but above
/// -HP, also rolls that attribute each step and comes to on a success.
/// A pack without the table keeps a step of 60 seconds and no roll. A
/// key the table does not know fails the pack: a typo must not fall back
/// to a default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRules {
    /// The attribute an unconscious entity rolls to come to, such as
    /// `ht`. `None`: only regeneration past 0 HP brings it to.
    #[serde(default)]
    pub come_to_roll: Option<String>,
    /// The seconds between two recovery steps, 1 to
    /// [`MAX_RECOVERY_SECS`].
    #[serde(default = "default_recovery_secs")]
    pub every_secs: u64,
}

const fn default_recovery_secs() -> u64 {
    60
}

impl Default for RecoveryRules {
    fn default() -> Self {
        Self {
            come_to_roll: None,
            every_secs: default_recovery_secs(),
        }
    }
}

impl RecoveryRules {
    /// The time between two recovery steps, in ms.
    #[must_use]
    pub const fn every_ms(&self) -> u64 {
        self.every_secs.saturating_mul(1_000)
    }

    /// The rule in one or two sentences, as `help`, the Incapacitated
    /// refusal and an agent's system prompt say it.
    #[must_use]
    pub fn rule(&self) -> String {
        let every = match self.every_secs {
            60 => "minute".to_owned(),
            n if n % 60 == 0 => format!("{} minutes", n / 60),
            n => format!("{n} seconds"),
        };
        let roll = self.come_to_roll.as_ref().map_or_else(String::new, |a| {
            format!(
                ", or on a successful {} roll each step while above -HP",
                a.to_ascii_uppercase()
            )
        });
        format!(
            "You regain 1 HP every {every} while in the world. At 0 HP or below you may pass out; \
             you come to above 0 HP{roll}, and anyone in the room sees it."
        )
    }
}

/// `[death]` in `world.toml`: what dying costs a player or an NPC. Mobs
/// keep their own respawn (`respawn_minutes`).
///
/// A pack without the table keeps a free full heal at `respawn_room`. A
/// key the table does not know fails the pack, as for `[recovery]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeathRules {
    /// The share of its maximum HP the dead wake with, above 0 and at
    /// most 1. The HP is rounded, and at least 1.
    #[serde(default = "one")]
    pub respawn_hp_frac: f64,
    /// Whether the dead drop their purse where they fell, as one
    /// `purse_item` that holds the coins.
    #[serde(default)]
    pub drop_purse: bool,
    /// The item prototype a dropped purse is made of. It must carry
    /// `currency` (its own amount is ignored: the purse holds what the
    /// dead carried). Required with `drop_purse`.
    #[serde(default)]
    pub purse_item: Option<ProtoId>,
}

impl Default for DeathRules {
    fn default() -> Self {
        Self {
            respawn_hp_frac: one(),
            drop_purse: false,
            purse_item: None,
        }
    }
}

impl DeathRules {
    /// The HP the dead wake with, out of `hp_max`: the share, rounded,
    /// from 1 to `hp_max` (1 when `hp_max` is below 1).
    #[must_use]
    pub fn respawn_hp(&self, hp_max: i32) -> i32 {
        let share = (f64::from(hp_max) * self.respawn_hp_frac.clamp(0.0, 1.0)).round();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the share is clamped to 0..=1, so the product lies between 0 and hp_max"
        )]
        let hp = share as i32;
        hp.clamp(1, hp_max.max(1))
    }

    /// The rule in one sentence, as `help` and an agent's system prompt
    /// say it.
    #[must_use]
    pub fn rule(&self) -> String {
        let hp = if self.respawn_hp_frac >= 1.0 {
            "whole".to_owned()
        } else {
            format!(
                "with about {}% of your HP",
                (self.respawn_hp_frac * 100.0).round()
            )
        };
        let purse = if self.drop_purse {
            ", and your purse stays where you fell"
        } else {
            ""
        };
        format!("If you die, you wake at the respawn point {hp}{purse}.")
    }
}

/// `[combat]` in `world.toml`: how a fight lets go of you, and what takes
/// a turn in one.
///
/// A pack without the table keeps the strict rule: with a hostile in the
/// room, every way out needs `flee`, and `use` is free. A key the table
/// does not know fails the pack, as for `[recovery]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CombatRules {
    /// Whether walking back out by the way you came in is a normal move,
    /// with no flee roll and no strike, even with a hostile in the room.
    /// Every other exit still needs `flee`. The strike on entering a lair
    /// stays.
    #[serde(default)]
    pub retreat_free: bool,
    /// Whether a `use` that acts takes the user's turn in a fight: each
    /// hostile in the room that can act strikes the user once after the
    /// item works, as after `attack` or `defend`. A heal, a purse, a key
    /// and a scripted use act. Looking into a container and putting gear
    /// on (`use` on a worn item is `equip`) do not. Unset means false, so
    /// a pack that does not say keeps a free `use`.
    #[serde(default)]
    pub use_draws_strike: bool,
}

impl CombatRules {
    /// How to leave a fight, and what using an item in one costs, in a few
    /// sentences, as `help` and an agent's system prompt say it.
    #[must_use]
    pub fn rule(&self) -> String {
        let leave = if self.retreat_free {
            "With a hostile in the room, you may walk back out the way you came in; any other \
             way needs flee, a DX check, and each hostile strikes you if it fails. A bare flee \
             runs back the way you came when no hostile waits there, else to a room with no \
             hostile in it."
        } else {
            "With a hostile in the room, every way out needs flee, a DX check, and each hostile \
             strikes you if it fails. A bare flee runs back the way you came when no hostile \
             waits there, else to a room with no hostile in it."
        };
        if self.use_draws_strike {
            format!(
                "{leave} With a hostile in the room, use takes your turn as attack and defend \
                 do: the item works, then each hostile strikes you once. Looking into a \
                 container or putting on gear stays free."
            )
        } else {
            leave.to_owned()
        }
    }
}

/// A lock on an exit.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LockDef {
    /// The lock id. A key item with the same `key` opens it.
    pub lock: String,
    /// The modifier of a lockpicking attempt.
    #[serde(default)]
    pub pick: i32,
    /// What `look` says about it.
    #[serde(default)]
    pub describe: Option<String>,
    /// Minutes after it is opened that it locks itself again, on both
    /// sides. `None`: it stays open until something locks it.
    #[serde(default)]
    pub relock_minutes: Option<u32>,
}

/// An item placed in a room by the pack.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RoomItem {
    /// A plain item.
    Proto(ProtoId),
    /// A container and what is in it.
    WithContents {
        /// The container.
        proto: ProtoId,
        /// Its contents.
        #[serde(default)]
        contents: Vec<ProtoId>,
    },
}

impl RoomItem {
    /// The prototype placed.
    #[must_use]
    pub const fn proto(&self) -> &ProtoId {
        match self {
            Self::Proto(p) | Self::WithContents { proto: p, .. } => p,
        }
    }
}

/// One room.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoomDef {
    /// The id.
    pub id: RoomId,
    /// The title.
    pub title: String,
    /// The long description.
    pub description: String,
    /// Guards keep the peace here: attacks are refused.
    #[serde(default)]
    pub safe: bool,
    /// Unlit without a light source.
    #[serde(default)]
    pub dark: bool,
    /// The room has a notice board.
    #[serde(default)]
    pub board: bool,
    /// Exits by direction.
    #[serde(default)]
    pub exits: BTreeMap<Direction, RoomId>,
    /// Locks on exits.
    #[serde(default)]
    pub locks: BTreeMap<Direction, LockDef>,
    /// Items placed at start.
    #[serde(default)]
    pub items: Vec<RoomItem>,
}

/// How hard a skill is to learn. It sets the untrained default.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Difficulty {
    /// Default attribute -4.
    Easy,
    /// Default attribute -5.
    Average,
    /// Default attribute -6.
    Hard,
}

impl Difficulty {
    /// The default penalty.
    #[must_use]
    pub const fn default_penalty(self) -> i32 {
        match self {
            Self::Easy => -4,
            Self::Average => -5,
            Self::Hard => -6,
        }
    }
}

/// One skill.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillDef {
    /// The display name.
    pub name: String,
    /// The controlling attribute (`st`, `dx`, `iq`, `ht`).
    pub attr: String,
    /// The difficulty.
    pub difficulty: Difficulty,
    /// A weapon skill that can parry.
    #[serde(default)]
    pub combat: bool,
    /// The engine effect of a success.
    #[serde(default)]
    pub effect: Option<SkillEffect>,
}

/// An engine effect that a skill success has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillEffect {
    /// Opens a locked exit or container.
    PickLock,
    /// Hides the actor from the room.
    Hide,
    /// Takes 10% off the next purchase from the target keeper.
    Haggle,
}

/// An equipment slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    /// The weapon hand.
    Weapon,
    /// Body armor.
    Torso,
    /// A held light.
    Light,
}

/// Weapon damage, fixed or strength-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WeaponDamage {
    /// Fixed dice.
    Dice(DamageDice),
    /// Swing damage of the wielder's ST, plus a modifier.
    Swing(i8),
    /// Thrust damage of the wielder's ST, plus a modifier.
    Thrust(i8),
}

impl WeaponDamage {
    /// The dice for a wielder of strength `st`.
    #[must_use]
    pub const fn dice(self, st: i32) -> DamageDice {
        match self {
            Self::Dice(d) => d,
            Self::Swing(m) => mud_rules::swing(st).plus(m),
            Self::Thrust(m) => mud_rules::thrust(st).plus(m),
        }
    }
}

impl FromStr for WeaponDamage {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim().to_ascii_lowercase();
        let modifier = |rest: &str| -> Result<i8, String> {
            if rest.is_empty() {
                Ok(0)
            } else {
                rest.trim_start_matches('+')
                    .parse()
                    .map_err(|_| format!("bad damage modifier in {s:?}"))
            }
        };
        if let Some(rest) = t.strip_prefix("sw") {
            return Ok(Self::Swing(modifier(rest)?));
        }
        if let Some(rest) = t.strip_prefix("thr") {
            return Ok(Self::Thrust(modifier(rest)?));
        }
        t.parse::<DamageDice>()
            .map(Self::Dice)
            .map_err(|e| e.to_string())
    }
}

/// One item prototype.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of one item"
)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ItemDef {
    /// The display name.
    pub name: String,
    /// Other words that refer to it.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// What `examine` shows.
    #[serde(default)]
    pub description: String,
    /// The base price.
    #[serde(default)]
    pub value: i64,
    /// Where it is worn or held.
    #[serde(default)]
    pub slot: Option<Slot>,
    /// The skill a weapon uses.
    #[serde(default)]
    pub skill: Option<SkillId>,
    /// The raw damage string of a weapon.
    #[serde(default)]
    pub damage: Option<String>,
    /// The damage type tag of a weapon.
    #[serde(default)]
    pub damage_type: Option<String>,
    /// Armor DR.
    #[serde(default)]
    pub dr: i32,
    /// A light source when held.
    #[serde(default)]
    pub light: bool,
    /// Dice healed by `use`.
    #[serde(default)]
    pub heal: Option<String>,
    /// A skill whose success doubles the healing.
    #[serde(default)]
    pub heal_skill: Option<SkillId>,
    /// The lock this key opens.
    #[serde(default)]
    pub key: Option<String>,
    /// Free tags, such as `loot` or `ore`. Shops buy by tag.
    #[serde(default)]
    pub tags: Vec<String>,
    /// It cannot be picked up.
    #[serde(default)]
    pub fixed: bool,
    /// It holds other items.
    #[serde(default)]
    pub container: bool,
    /// A container lock and its lockpicking modifier.
    #[serde(default)]
    pub lock: Option<i32>,
    /// Currency inside, released by `use`.
    #[serde(default)]
    pub currency: Option<i64>,
    /// Natural weapons and the like: never listed.
    #[serde(default)]
    pub hidden: bool,
}

impl ItemDef {
    /// The parsed weapon damage, if it is a weapon.
    #[must_use]
    pub fn weapon_damage(&self) -> Option<(WeaponDamage, DamageType)> {
        let dmg = self.damage.as_deref()?.parse().ok()?;
        let kind = self
            .damage_type
            .as_deref()
            .and_then(DamageType::from_tag)
            .unwrap_or(DamageType::Crushing);
        Some((dmg, kind))
    }

    /// The parsed healing dice.
    #[must_use]
    pub fn heal_dice(&self) -> Option<DamageDice> {
        self.heal.as_deref()?.parse().ok()
    }

    /// Whether `word` names this item.
    #[must_use]
    pub fn matches(&self, word: &str) -> bool {
        let w = word.trim().to_ascii_lowercase();
        !w.is_empty()
            && (self.name.to_ascii_lowercase() == w
                || self
                    .name
                    .to_ascii_lowercase()
                    .split_whitespace()
                    .any(|p| p == w)
                || self.aliases.iter().any(|a| a.to_ascii_lowercase() == w))
    }
}

/// The currency.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CurrencyDef {
    /// The long name, such as `silver`.
    pub name: String,
    /// The suffix, such as `s`.
    pub short: String,
}

/// One shop.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShopDef {
    /// The entity that keeps it.
    pub keeper: EntityId,
    /// The merchandise it starts with.
    #[serde(default)]
    pub stock: Vec<ProtoId>,
    /// The tags or item ids it buys.
    #[serde(default)]
    pub buys: Vec<String>,
    /// The share of the value it pays.
    #[serde(default = "half")]
    pub buy_rate: f64,
    /// The multiple of the value it charges.
    #[serde(default = "one")]
    pub sell_markup: f64,
    /// Fixed buying prices by item.
    #[serde(default)]
    pub prices: BTreeMap<ProtoId, i64>,
    /// The item prototype of the shop's fixture: a fixed container in the
    /// shop room that holds the stock, so the stock stays in the shop
    /// whoever walks off. `None`: the stock lives on the keeper's body,
    /// and the shop goes wherever the keeper goes.
    #[serde(default)]
    pub fixture: Option<ProtoId>,
    /// The shop room, where the fixture stands. Defaults to the keeper's
    /// starting room. Only for a shop with a fixture.
    #[serde(default)]
    pub room: Option<RoomId>,
    /// An honesty box: the fixture sells at its posted prices with nobody
    /// minding it, and the coins go to the keeper. Buying from customers
    /// still needs the keeper. Only for a shop with a fixture.
    #[serde(default)]
    pub unattended: bool,
}

const fn half() -> f64 {
    0.5
}
const fn one() -> f64 {
    1.0
}

/// `economy.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Economy {
    /// The currency.
    pub currency: CurrencyDef,
    /// The shops.
    #[serde(default)]
    pub shops: BTreeMap<ShopId, ShopDef>,
    /// The reward pools world scripts may mint from (ADR 0010).
    #[serde(default)]
    pub rewards: BTreeMap<PoolId, RewardPool>,
}

/// What kind of entity it is, as the world shows it.
///
/// A hidden agent PC is a `Player`, exactly as a human is. Nothing a
/// player can see tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    /// A player character.
    Player,
    /// A townsperson.
    Npc,
    /// A monster.
    Mob,
}

/// Which runtime thinks for an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// No mind: a human slot or an object.
    #[default]
    None,
    /// The server runs it.
    Mob,
    /// A light agent on autumn-plugin-agent.
    Ambient,
    /// A durable littleguy agent.
    Persistent,
    /// A hidden agent PC.
    Turing,
}

/// The friction settings of a hidden agent PC.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct TuringSpec {
    /// Windows like `17:00-23:30`, in UTC.
    #[serde(default)]
    pub active_windows_utc: Vec<String>,
    /// The bound of the daily offset of each window, in minutes.
    #[serde(default)]
    pub daily_offset_minutes: u32,
    /// The chance to wake on an unaddressed room batch.
    #[serde(default = "default_wake_chance")]
    pub wake_chance: f64,
    /// Words that always wake it.
    #[serde(default)]
    pub goal_keywords: Vec<String>,
    /// Typing speed, in words per minute.
    #[serde(default = "default_wpm")]
    pub wpm: f64,
    /// The gap between distracted spells, in minutes.
    #[serde(default = "default_afk_every")]
    pub afk_every_minutes: [u32; 2],
    /// How long a distracted spell lasts, in minutes.
    #[serde(default = "default_afk_len")]
    pub afk_minutes: [u32; 2],
}

const fn default_wake_chance() -> f64 {
    0.35
}
const fn default_wpm() -> f64 {
    55.0
}
const fn default_afk_every() -> [u32; 2] {
    [20, 60]
}
const fn default_afk_len() -> [u32; 2] {
    [2, 6]
}

/// How an entity's mind runs.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct MindSpec {
    /// The tier.
    #[serde(default)]
    pub tier: Tier,
    /// The runtime name, such as `littleguy` or `plugin-agent`.
    #[serde(default)]
    pub runtime: Option<String>,
    /// The model alias, resolved by the supervisor config.
    #[serde(default)]
    pub model: Option<String>,
    /// The macro-tick interval bounds, in hours.
    #[serde(default)]
    pub macro_tick_hours: Option<[f64; 2]>,
    /// The wake cooldown bounds, in seconds.
    #[serde(default)]
    pub cooldown_secs: Option<[u64; 2]>,
    /// The Turing settings.
    #[serde(default)]
    pub turing: Option<TuringSpec>,
}

/// What wakes a script rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptTrigger {
    /// A macro tick.
    MacroTick,
    /// Someone enters the room.
    Enter,
    /// Someone speaks in the room.
    Speech,
    /// A direct tell.
    Tell,
    /// The entity logs in.
    Login,
    /// The entity is attacked.
    Attacked,
    /// A self-control nudge.
    Nudge,
}

/// Who may set off a script rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Who {
    /// Anyone.
    #[default]
    Any,
    /// Only a player character, whoever plays it (a human or an agent
    /// PC): scripts react to players alike, so they reveal no tier.
    /// `player` is accepted as a synonym.
    #[serde(alias = "player")]
    Human,
    /// Only a non-player: an NPC or a mob.
    Agent,
}

/// One rule of a deterministic script.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ScriptRule {
    /// The trigger.
    pub on: ScriptTrigger,
    /// Fire only on the nth occurrence (1-based).
    #[serde(default)]
    pub nth: Option<u32>,
    /// Who may set it off.
    #[serde(default)]
    pub who: Who,
    /// Only in this room.
    #[serde(default)]
    pub room: Option<RoomId>,
    /// At least one of these words must appear in the text.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// The commands to run.
    #[serde(rename = "do")]
    pub commands: Vec<String>,
}

/// A trait an entity has, with its self-control number.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TraitRef {
    /// The trait name, from the trait library.
    pub name: String,
    /// The self-control number. None means no roll.
    #[serde(default)]
    pub control: Option<u8>,
}

/// One trait of the library.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TraitDef {
    /// Event kinds that set it off.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Words in speech that set it off.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// The nudge text. `{target}` and `{trigger}` are filled in.
    #[serde(default)]
    pub cue: Option<String>,
    /// A server rule, such as `no_first_strike`.
    #[serde(default)]
    pub policy: Option<String>,
}

/// One entity of the cast.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CastDef {
    /// The display name.
    pub name: String,
    /// Other names.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The line `look` shows.
    #[serde(default)]
    pub short: String,
    /// What `examine` shows.
    #[serde(default)]
    pub description: String,
    /// The kind.
    pub kind: EntityKind,
    /// Where it starts.
    pub room: RoomId,
    /// Its attributes.
    #[serde(default)]
    pub attrs: Attributes,
    /// Its skill levels.
    #[serde(default)]
    pub skills: BTreeMap<SkillId, i32>,
    /// Its traits.
    #[serde(default)]
    pub traits: Vec<TraitRef>,
    /// Its money.
    #[serde(default)]
    pub currency: i64,
    /// The shop it keeps.
    #[serde(default)]
    pub shop: Option<ShopId>,
    /// Items it carries.
    #[serde(default)]
    pub inventory: Vec<ProtoId>,
    /// Items it wears or wields (taken from its inventory).
    #[serde(default)]
    pub equip: Vec<ProtoId>,
    /// Natural DR.
    #[serde(default)]
    pub dr: i32,
    /// It attacks players on sight.
    #[serde(default)]
    pub hostile: bool,
    /// It answers trouble in safe rooms.
    #[serde(default)]
    pub guard: bool,
    /// Minutes until a dead mob returns.
    #[serde(default)]
    pub respawn_minutes: Option<u32>,
    /// Whether a respawned mob comes back with its starting gear
    /// (`inventory`, wielding or wearing what `equip` names), made new.
    /// Unset means yes. Only a mob with `respawn_minutes` may set it;
    /// `false` keeps a one-off piece, such as a unique key, one-off.
    #[serde(default)]
    pub respawn_kit: Option<bool>,
    /// A soft post: the rooms it keeps. Nothing holds it there; away from
    /// it, its macro-tick cue says the way back.
    #[serde(default)]
    pub post: Option<PostDef>,
    /// The persona prompt. Private.
    #[serde(default)]
    pub persona: String,
    /// The seeded goal. Private.
    #[serde(default)]
    pub goal: String,
    /// The canned answer of the deterministic fallback.
    #[serde(default)]
    pub fallback: String,
    /// How its mind runs.
    #[serde(default)]
    pub mind: MindSpec,
    /// Its deterministic script.
    #[serde(default)]
    pub script: Vec<ScriptRule>,
}

impl CastDef {
    /// Whether it is a mob that respawns.
    #[must_use]
    pub const fn respawns(&self) -> bool {
        matches!(self.kind, EntityKind::Mob) && self.respawn_minutes.is_some()
    }

    /// Whether it gets its starting gear back when it respawns: a mob that
    /// respawns, unless its entry sets `respawn_kit = false`.
    #[must_use]
    pub fn respawns_with_kit(&self) -> bool {
        self.respawns() && self.respawn_kit.unwrap_or(true)
    }
}

/// A soft post of a cast entry (`post = { rooms = [...] }`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PostDef {
    /// The rooms of the post. The first is where the cue sends it back.
    pub rooms: Vec<RoomId>,
}

/// The template of a new human player.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PlayerTemplate {
    /// Attributes.
    #[serde(default)]
    pub attrs: Attributes,
    /// Skills.
    #[serde(default)]
    pub skills: BTreeMap<SkillId, i32>,
    /// Money.
    #[serde(default)]
    pub currency: i64,
    /// Starting items.
    #[serde(default)]
    pub inventory: Vec<ProtoId>,
    /// Starting equipment.
    #[serde(default)]
    pub equip: Vec<ProtoId>,
}

#[derive(Deserialize)]
struct WorldFile {
    world: WorldMeta,
    #[serde(default)]
    recovery: RecoveryRules,
    #[serde(default)]
    death: DeathRules,
    #[serde(default)]
    combat: CombatRules,
    #[serde(default)]
    rooms: Vec<RoomDef>,
}

#[derive(Deserialize)]
struct SkillsFile {
    #[serde(default)]
    skills: BTreeMap<SkillId, SkillDef>,
}

#[derive(Deserialize)]
struct ItemsFile {
    #[serde(default)]
    items: BTreeMap<ProtoId, ItemDef>,
}

#[derive(Deserialize)]
struct CastFile {
    #[serde(default)]
    traits: BTreeMap<String, TraitDef>,
    #[serde(default)]
    player_template: PlayerTemplate,
    #[serde(default)]
    cast: BTreeMap<EntityId, CastDef>,
}

/// A loaded, checked world pack.
#[derive(Debug, Clone)]
pub struct WorldPack {
    /// `[world]`.
    pub world: WorldMeta,
    /// `[recovery]`: how a downed entity comes to.
    pub recovery: RecoveryRules,
    /// `[death]`: what dying costs.
    pub death: DeathRules,
    /// `[combat]`: how a fight lets go.
    pub combat: CombatRules,
    /// Rooms by id.
    pub rooms: BTreeMap<RoomId, RoomDef>,
    /// Room ids in file order.
    pub room_order: Vec<RoomId>,
    /// Skills.
    pub skills: BTreeMap<SkillId, SkillDef>,
    /// Item prototypes.
    pub items: BTreeMap<ProtoId, ItemDef>,
    /// The economy.
    pub economy: Economy,
    /// The trait library.
    pub traits: BTreeMap<String, TraitDef>,
    /// The new-player template.
    pub player_template: PlayerTemplate,
    /// The cast.
    pub cast: BTreeMap<EntityId, CastDef>,
}

/// The raw text of the five pack files.
#[derive(Debug, Clone, Copy)]
pub struct PackSources<'a> {
    /// `world.toml`.
    pub world: &'a str,
    /// `skills.toml`.
    pub skills: &'a str,
    /// `items.toml`.
    pub items: &'a str,
    /// `economy.toml`.
    pub economy: &'a str,
    /// `cast.toml`.
    pub cast: &'a str,
}

fn parse<T: for<'de> Deserialize<'de>>(file: &'static str, text: &str) -> Result<T, PackError> {
    toml::from_str(text).map_err(|e| PackError::Parse {
        file,
        message: e.to_string(),
    })
}

impl WorldPack {
    /// What happens to a body in this pack, in a few sentences: its
    /// recovery rule, its death rule, then how to leave a fight. An agent's system prompt
    /// carries it, so the agent reasons from the same rule `help` gives.
    #[must_use]
    pub fn body_rules(&self) -> String {
        format!(
            "{} {} {}",
            self.recovery.rule(),
            self.death.rule(),
            self.combat.rule()
        )
    }

    /// Loads and checks the pack in `dir`.
    ///
    /// # Errors
    /// A file is missing, does not parse, or references are broken.
    pub fn load(dir: &Path) -> Result<Self, PackError> {
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read_to_string(&path).map_err(|source| PackError::Io {
                path: path.display().to_string(),
                source,
            })
        };
        let (world, skills, items, economy, cast) = (
            read("world.toml")?,
            read("skills.toml")?,
            read("items.toml")?,
            read("economy.toml")?,
            read("cast.toml")?,
        );
        Self::from_sources(PackSources {
            world: &world,
            skills: &skills,
            items: &items,
            economy: &economy,
            cast: &cast,
        })
    }

    /// Parses and checks a pack from text.
    ///
    /// # Errors
    /// A file does not parse, or references are broken.
    pub fn from_sources(src: PackSources<'_>) -> Result<Self, PackError> {
        let world: WorldFile = parse("world.toml", src.world)?;
        let skills: SkillsFile = parse("skills.toml", src.skills)?;
        let items: ItemsFile = parse("items.toml", src.items)?;
        let economy: Economy = parse("economy.toml", src.economy)?;
        let cast: CastFile = parse("cast.toml", src.cast)?;
        let room_order = world.rooms.iter().map(|r| r.id.clone()).collect();
        let pack = Self {
            world: world.world,
            recovery: world.recovery,
            death: world.death,
            combat: world.combat,
            rooms: world.rooms.into_iter().map(|r| (r.id.clone(), r)).collect(),
            room_order,
            skills: skills.skills,
            items: items.items,
            economy,
            traits: cast.traits,
            player_template: cast.player_template,
            cast: cast.cast,
        };
        pack.validate()?;
        Ok(pack)
    }

    /// The pack shipped in this repository, compiled in.
    ///
    /// # Errors
    /// Only if the shipped pack is broken, which a test forbids.
    pub fn crypt_v01() -> Result<Self, PackError> {
        Self::from_sources(PackSources {
            world: include_str!("../../../worlds/crypt-v01/world.toml"),
            skills: include_str!("../../../worlds/crypt-v01/skills.toml"),
            items: include_str!("../../../worlds/crypt-v01/items.toml"),
            economy: include_str!("../../../worlds/crypt-v01/economy.toml"),
            cast: include_str!("../../../worlds/crypt-v01/cast.toml"),
        })
    }

    #[expect(clippy::too_many_lines, reason = "one flat list of checks reads best")]
    fn validate(&self) -> Result<(), PackError> {
        let mut problems = Vec::new();
        let mut room = |id: &RoomId, what: String| {
            if !self.rooms.contains_key(id) {
                problems.push(format!("{what} names unknown room {id:?}"));
            }
        };
        room(&self.world.start_room, "world.start_room".into());
        room(&self.world.respawn_room, "world.respawn_room".into());
        for r in self.rooms.values() {
            for (dir, to) in &r.exits {
                room(to, format!("room {} exit {dir}", r.id));
            }
        }
        for (id, c) in &self.cast {
            room(&c.room, format!("cast {id}"));
        }
        if self.rooms.len() != self.room_order.len() {
            problems.push("two rooms share an id".into());
        }
        for r in self.rooms.values() {
            for dir in r.locks.keys() {
                if !r.exits.contains_key(dir) {
                    problems.push(format!("room {} locks {dir}, which is not an exit", r.id));
                }
            }
            for it in &r.items {
                let mut ids = vec![it.proto()];
                if let RoomItem::WithContents { contents, .. } = it {
                    ids.extend(contents);
                }
                for p in ids {
                    if !self.items.contains_key(p) {
                        problems.push(format!("room {} places unknown item {p:?}", r.id));
                    }
                }
            }
        }
        for (id, s) in &self.skills {
            if Attributes::default().get(&s.attr).is_none() {
                problems.push(format!("skill {id} uses unknown attribute {:?}", s.attr));
            }
        }
        for (id, it) in &self.items {
            if let Some(d) = &it.damage
                && d.parse::<WeaponDamage>().is_err()
            {
                problems.push(format!("item {id} has bad damage {d:?}"));
            }
            if let Some(t) = &it.damage_type
                && DamageType::from_tag(t).is_none()
            {
                problems.push(format!("item {id} has bad damage_type {t:?}"));
            }
            if let Some(h) = &it.heal
                && h.parse::<DamageDice>().is_err()
            {
                problems.push(format!("item {id} has bad heal {h:?}"));
            }
            for s in it.skill.iter().chain(it.heal_skill.iter()) {
                if !self.skills.contains_key(s) {
                    problems.push(format!("item {id} uses unknown skill {s:?}"));
                }
            }
        }
        problems.extend(self.recovery_problems());
        problems.extend(self.shop_problems());
        for (id, shop) in &self.economy.shops {
            match self.cast.get(&shop.keeper) {
                None => problems.push(format!("shop {id} has unknown keeper {:?}", shop.keeper)),
                Some(k) if k.shop.as_ref() != Some(id) => problems.push(format!(
                    "shop {id}: keeper {} does not name it as its shop",
                    shop.keeper
                )),
                Some(_) => {}
            }
            for p in shop.stock.iter().chain(shop.prices.keys()) {
                if !self.items.contains_key(p) {
                    problems.push(format!("shop {id} names unknown item {p:?}"));
                }
            }
        }
        for (id, pool) in &self.economy.rewards {
            problems.extend(pool_problems(id, pool, &self.items));
        }
        let template_items = self
            .player_template
            .inventory
            .iter()
            .chain(&self.player_template.equip);
        for p in template_items {
            if !self.items.contains_key(p) {
                problems.push(format!("player_template names unknown item {p:?}"));
            }
        }
        for p in &self.player_template.equip {
            if !self.player_template.inventory.contains(p) {
                problems.push(format!("player_template equips {p:?} without carrying it"));
            }
        }
        for s in self.player_template.skills.keys() {
            if !self.skills.contains_key(s) {
                problems.push(format!("player_template names unknown skill {s:?}"));
            }
        }
        for (id, c) in &self.cast {
            for s in c.skills.keys() {
                if !self.skills.contains_key(s) {
                    problems.push(format!("cast {id} has unknown skill {s:?}"));
                }
            }
            for p in c.inventory.iter().chain(&c.equip) {
                if !self.items.contains_key(p) {
                    problems.push(format!("cast {id} carries unknown item {p:?}"));
                }
            }
            for p in &c.equip {
                if !c.inventory.contains(p) {
                    problems.push(format!("cast {id} equips {p:?} without carrying it"));
                }
            }
            for t in &c.traits {
                if !self.traits.contains_key(&t.name) {
                    problems.push(format!("cast {id} has unknown trait {:?}", t.name));
                }
            }
            if let Some(shop) = &c.shop
                && !self.economy.shops.contains_key(shop)
            {
                problems.push(format!("cast {id} keeps unknown shop {shop:?}"));
            }
            if c.respawn_kit.is_some() && !c.respawns() {
                problems.push(format!(
                    "cast {id} sets respawn_kit, but only a mob with respawn_minutes respawns"
                ));
            }
            // `find_here` reads these words as the viewer, so an entity
            // answering to one could never be targeted by it.
            let words = c
                .name
                .split_whitespace()
                .chain(c.aliases.iter().map(String::as_str));
            for w in words {
                if SELF_WORDS.iter().any(|s| w.eq_ignore_ascii_case(s)) {
                    problems.push(format!("cast {id} answers to the reserved word {w:?}"));
                }
            }
            // A player-kind entity must carry a human-shaped id (`pc_<name>`,
            // as `World::login` makes), or its id alone would reveal that
            // an agent plays it.
            if c.kind == EntityKind::Player
                && id.as_str() != format!("pc_{}", c.name.to_ascii_lowercase())
            {
                problems.push(format!(
                    "cast {id} is a player; its id must be pc_{}",
                    c.name.to_ascii_lowercase()
                ));
            }
            if c.mind.tier == Tier::Turing && c.mind.turing.is_none() {
                problems.push(format!(
                    "cast {id} is a turing PC with no [mind.turing] table"
                ));
            }
            if let Some(t) = &c.mind.turing {
                for w in &t.active_windows_utc {
                    if crate::schedule::parse_window(w).is_none() {
                        problems.push(format!("cast {id} has bad active window {w:?}"));
                    }
                }
            }
            for rule in &c.script {
                if let Some(r) = &rule.room {
                    room_check(&self.rooms, r, &mut problems, id);
                }
                for cmd in &rule.commands {
                    if let Err(e) = crate::parser::parse(cmd) {
                        problems.push(format!("cast {id} script command {cmd:?}: {e}"));
                    }
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(PackError::Invalid(problems))
        }
    }

    /// The trait definition and the policy of an entity's trait.
    #[must_use]
    pub fn trait_def(&self, name: &str) -> Option<&TraitDef> {
        self.traits.get(name)
    }
}

impl WorldPack {
    /// The problems of `[recovery]` and `[death]`.
    fn recovery_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let r = &self.recovery;
        if !(1..=MAX_RECOVERY_SECS).contains(&r.every_secs) {
            problems.push(format!("recovery.every_secs is 1 to {MAX_RECOVERY_SECS}"));
        }
        if let Some(a) = &r.come_to_roll
            && Attributes::default().get(a).is_none()
        {
            problems.push(format!(
                "recovery.come_to_roll names unknown attribute {a:?}"
            ));
        }
        let d = &self.death;
        if !(d.respawn_hp_frac > 0.0 && d.respawn_hp_frac <= 1.0) {
            problems.push("death.respawn_hp_frac is above 0 and at most 1".into());
        }
        match (&d.purse_item, d.drop_purse) {
            (None, true) => problems.push("death.drop_purse needs death.purse_item".into()),
            (Some(p), _) => match self.items.get(p) {
                None => problems.push(format!(
                    "death.purse_item names unknown item {:?}",
                    p.as_str()
                )),
                Some(def) if def.currency.is_none() => {
                    problems.push(format!("death.purse_item {p} must carry currency"));
                }
                Some(_) => {}
            },
            (None, false) => {}
        }
        problems
    }

    /// The room a shop trades in: its `room`, else its keeper's starting
    /// room. `None` for a shop with no fixture (it trades wherever its
    /// keeper stands) or an unknown shop.
    #[must_use]
    pub fn shop_room(&self, shop: &ShopId) -> Option<&RoomId> {
        let def = self.economy.shops.get(shop)?;
        def.fixture.as_ref()?;
        def.room
            .as_ref()
            .or_else(|| self.cast.get(&def.keeper).map(|k| &k.room))
    }

    /// The problems of shop fixtures, soft posts and relocking exits.
    fn shop_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut fixtures = BTreeMap::new();
        for (id, shop) in &self.economy.shops {
            match &shop.fixture {
                None => {
                    if shop.room.is_some() {
                        problems.push(format!("shop {id}: room needs a fixture"));
                    }
                    if shop.unattended {
                        problems.push(format!("shop {id}: unattended needs a fixture"));
                    }
                }
                Some(f) => {
                    match self.items.get(f) {
                        None => problems.push(format!(
                            "shop {id} fixture names unknown item {:?}",
                            f.as_str()
                        )),
                        Some(d) if !(d.container && d.fixed) || d.lock.is_some() => {
                            problems.push(format!(
                                "shop {id} fixture {f} must be a fixed, unlocked container"
                            ));
                        }
                        Some(_) => {}
                    }
                    if let Some(other) = fixtures.insert(f, id) {
                        problems.push(format!("shops {other} and {id} share the fixture {f}"));
                    }
                    if let Some(r) = &shop.room
                        && !self.rooms.contains_key(r)
                    {
                        problems.push(format!("shop {id} names unknown room {r:?}"));
                    }
                }
            }
        }
        for (id, c) in &self.cast {
            if let Some(post) = &c.post {
                if post.rooms.is_empty() {
                    problems.push(format!("cast {id}: post needs at least one room"));
                }
                for r in &post.rooms {
                    if !self.rooms.contains_key(r) {
                        problems.push(format!("cast {id} post names unknown room {r:?}"));
                    }
                }
            }
        }
        for r in self.rooms.values() {
            for (dir, l) in &r.locks {
                if l.relock_minutes == Some(0) {
                    problems.push(format!(
                        "room {} lock {dir}: relock_minutes is at least 1",
                        r.id
                    ));
                }
            }
        }
        problems
    }
}

/// The problems of one reward pool: a valid id, a currency budget of 0 to
/// [`POOL_CURRENCY_MAX`], known items of 1 to [`POOL_ITEMS_MAX`] each, and
/// something to give.
fn pool_problems(
    id: &PoolId,
    pool: &RewardPool,
    items: &BTreeMap<ProtoId, ItemDef>,
) -> Vec<String> {
    let mut out = Vec::new();
    if !valid_pool_id(id.as_str()) {
        out.push(format!(
            "reward pool {:?}: an id is 1 to 32 of a-z, 0-9 and _",
            id.as_str()
        ));
    }
    if !(0..=POOL_CURRENCY_MAX).contains(&pool.currency) {
        out.push(format!(
            "reward pool {id}: currency is 0 to {POOL_CURRENCY_MAX}"
        ));
    }
    for (p, n) in &pool.items {
        if !items.contains_key(p) {
            out.push(format!(
                "reward pool {id} names unknown item {:?}",
                p.as_str()
            ));
        }
        if !(1..=POOL_ITEMS_MAX).contains(n) {
            out.push(format!(
                "reward pool {id}: {p} is 1 to {POOL_ITEMS_MAX} a period"
            ));
        }
    }
    if pool.currency == 0 && pool.items.is_empty() {
        out.push(format!("reward pool {id} gives nothing"));
    }
    out
}

fn room_check(
    rooms: &BTreeMap<RoomId, RoomDef>,
    r: &RoomId,
    problems: &mut Vec<String>,
    id: &EntityId,
) {
    if !rooms.contains_key(r) {
        problems.push(format!("cast {id} script names unknown room {r:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_pack_loads_and_has_eleven_rooms() {
        let pack = WorldPack::crypt_v01().unwrap();
        assert_eq!(pack.rooms.len(), 11);
        assert_eq!(pack.cast.len(), 8);
        assert!(
            pack.rooms[&RoomId::from("dark_alley")]
                .locks
                .contains_key(&Direction::D)
        );
    }

    #[test]
    fn the_pack_loads_from_disk_too() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../worlds/crypt-v01");
        let pack = WorldPack::load(&dir).unwrap();
        assert_eq!(pack.world.name, "crypt-v01");
    }

    #[test]
    fn broken_references_are_all_reported() {
        let world = r#"
[world]
name = "x"
title = "x"
start_room = "nowhere"
respawn_room = "a"
[[rooms]]
id = "a"
title = "A"
description = "a"
exits = { n = "b" }
items = ["ghost"]
"#;
        let cast = r#"
[cast.bob]
name = "Bob"
kind = "npc"
room = "a"
skills = { juggling = 12 }
traits = [{ name = "nope", control = 12 }]
"#;
        let err = WorldPack::from_sources(PackSources {
            world,
            skills: "",
            items: "",
            economy: "[currency]\nname = \"gold\"\nshort = \"g\"\n",
            cast,
        })
        .unwrap_err();
        let text = err.to_string();
        for needle in ["nowhere", "exit north", "ghost", "juggling", "nope"] {
            assert!(text.contains(needle), "{needle} missing from {text}");
        }
    }

    #[test]
    fn a_cast_entry_may_not_answer_to_a_self_word() {
        let world = "[world]\nname = \"x\"\ntitle = \"x\"\nstart_room = \"a\"\nrespawn_room = \"a\"\n[[rooms]]\nid = \"a\"\ntitle = \"A\"\ndescription = \"a\"\n";
        let economy = "[currency]\nname = \"gold\"\nshort = \"g\"\n";
        let cast = "[cast.old]\nname = \"Old Self\"\nkind = \"npc\"\nroom = \"a\"\n[cast.bob]\nname = \"bob\"\naliases = [\"ME\"]\nkind = \"npc\"\nroom = \"a\"\n";
        let err = WorldPack::from_sources(PackSources {
            world,
            skills: "",
            items: "",
            economy,
            cast,
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("cast old answers to the reserved word \"Self\""),
            "{err}"
        );
        assert!(
            err.contains("cast bob answers to the reserved word \"ME\""),
            "{err}"
        );
    }

    #[test]
    fn a_player_cast_entry_needs_a_human_shaped_id() {
        let world = "[world]\nname = \"x\"\ntitle = \"x\"\nstart_room = \"a\"\nrespawn_room = \"a\"\n[[rooms]]\nid = \"a\"\ntitle = \"A\"\ndescription = \"a\"\n";
        let economy = "[currency]\nname = \"gold\"\nshort = \"g\"\n";
        let bad = "[cast.kes]\nname = \"kes\"\nkind = \"player\"\nroom = \"a\"\n";
        let err = WorldPack::from_sources(PackSources {
            world,
            skills: "",
            items: "",
            economy,
            cast: bad,
        })
        .unwrap_err();
        assert!(err.to_string().contains("its id must be pc_kes"), "{err}");
        let good = "[cast.pc_kes]\nname = \"kes\"\nkind = \"player\"\nroom = \"a\"\n";
        assert!(
            WorldPack::from_sources(PackSources {
                world,
                skills: "",
                items: "",
                economy,
                cast: good
            })
            .is_ok()
        );
    }

    #[test]
    fn respawn_kit_is_for_a_mob_that_respawns() {
        let world = "[world]\nname = \"x\"\ntitle = \"x\"\nstart_room = \"a\"\nrespawn_room = \"a\"\n[[rooms]]\nid = \"a\"\ntitle = \"A\"\ndescription = \"a\"\n";
        let economy = "[currency]\nname = \"gold\"\nshort = \"g\"\n";
        let load = |cast: &str| {
            WorldPack::from_sources(PackSources {
                world,
                skills: "",
                items: "",
                economy,
                cast,
            })
        };
        let bad = "[cast.bob]\nname = \"bob\"\nkind = \"npc\"\nroom = \"a\"\nrespawn_kit = true\n\
                   [cast.ghoul]\nname = \"ghoul\"\nkind = \"mob\"\nroom = \"a\"\nrespawn_kit = false\n";
        let err = load(bad).unwrap_err().to_string();
        for id in ["bob", "ghoul"] {
            assert!(
                err.contains(&format!("cast {id} sets respawn_kit")),
                "{err}"
            );
        }
        let good = "[cast.rat]\nname = \"rat\"\nkind = \"mob\"\nroom = \"a\"\nrespawn_minutes = 5\n\
                    [cast.ghoul]\nname = \"ghoul\"\nkind = \"mob\"\nroom = \"a\"\nrespawn_minutes = 5\nrespawn_kit = false\n\
                    [cast.bob]\nname = \"bob\"\nkind = \"npc\"\nroom = \"a\"\n";
        let pack = load(good).unwrap();
        let kit = |id: &str| pack.cast[&EntityId::from(id)].respawns_with_kit();
        assert!(kit("rat"), "unset means yes");
        assert!(!kit("ghoul"));
        assert!(!kit("bob"), "an NPC does not respawn as a mob does");
    }

    #[test]
    fn reward_pools_are_checked() {
        let world = "[world]\nname = \"x\"\ntitle = \"x\"\nstart_room = \"a\"\nrespawn_room = \"a\"\n[[rooms]]\nid = \"a\"\ntitle = \"A\"\ndescription = \"a\"\n";
        let items = "[items.ale]\nname = \"ale\"\n";
        let load = |rewards: &str| {
            WorldPack::from_sources(PackSources {
                world,
                skills: "",
                items,
                economy: &format!("[currency]\nname = \"gold\"\nshort = \"g\"\n{rewards}"),
                cast: "",
            })
        };
        let ok = load("[rewards.tavern_tab]\ncurrency = 5\nitems = { ale = 2 }\nper = \"day\"\n")
            .unwrap();
        let pool = &ok.economy.rewards[&PoolId::from("tavern_tab")];
        assert_eq!(pool.per, crate::rewards::RewardPeriod::Day);
        assert_eq!(pool.items[&ProtoId::from("ale")], 2);
        let err = load(
            "[rewards.Bad]\ncurrency = -1\nper = \"ever\"\n[rewards.empty]\nper = \"day\"\n[rewards.big]\nitems = { ghost = 0, ale = 1001 }\nper = \"day\"\n",
        )
        .unwrap_err()
        .to_string();
        for needle in [
            "reward pool \"Bad\": an id is 1 to 32 of a-z, 0-9 and _",
            "reward pool Bad: currency is 0 to 1000000",
            "reward pool empty gives nothing",
            "reward pool big names unknown item \"ghost\"",
            "reward pool big: ghost is 1 to 1000 a period",
            "reward pool big: ale is 1 to 1000 a period",
        ] {
            assert!(err.contains(needle), "{needle} missing from {err}");
        }
        let err = load("[rewards.x]\ncurrency = 5\nper = \"week\"\n").unwrap_err();
        assert!(err.to_string().contains("parsing economy.toml"), "{err}");
        let err = load("[rewards.x]\ncurrency = 5\nper = \"day\"\ncoins = 3\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn directions_parse_both_forms_and_reverse() {
        assert_eq!("north".parse::<Direction>(), Ok(Direction::N));
        assert_eq!("NW".parse::<Direction>(), Ok(Direction::Nw));
        assert!("sideways".parse::<Direction>().is_err());
        for d in Direction::ALL {
            assert_eq!(d.opposite().opposite(), d);
        }
    }

    #[test]
    fn weapon_damage_forms() {
        assert_eq!("sw+1".parse::<WeaponDamage>(), Ok(WeaponDamage::Swing(1)));
        assert_eq!(
            "thr-1".parse::<WeaponDamage>(),
            Ok(WeaponDamage::Thrust(-1))
        );
        assert_eq!("sw".parse::<WeaponDamage>(), Ok(WeaponDamage::Swing(0)));
        assert!("swx".parse::<WeaponDamage>().is_err());
        assert_eq!(
            "1d+2".parse::<WeaponDamage>(),
            Ok(WeaponDamage::Dice(DamageDice::new(1, 2)))
        );
        assert_eq!(WeaponDamage::Swing(1).dice(13), DamageDice::new(2, 0));
    }
}
