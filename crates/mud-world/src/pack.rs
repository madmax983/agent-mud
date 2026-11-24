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

use crate::ids::{EntityId, ProtoId, RoomId, ShopId, SkillId};

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
    /// Only a human-controlled entity.
    Human,
    /// Only an agent or mob.
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
