//! The mutable state of a world: entities, item instances and rooms.
//!
//! Every item instance lives in exactly one place: a room floor, an
//! entity's inventory, or a container. [`WorldState::check`] verifies it.
//! The pack holds everything static; this holds only what changes, so the
//! store can snapshot it whole.

use std::collections::{BTreeMap, BTreeSet};

use mud_rules::{Attributes, Pools};
use serde::{Deserialize, Serialize};

use crate::engine::LoginError;
use crate::event::ActorRef;
use crate::hooks::{FLAG_CAP, FlagValue, MARK_CAP};
use crate::ids::{EntityId, ItemId, PoolId, ProtoId, RoomId, Seq, ShopId, SkillId};
use crate::pack::{
    CastDef, Direction, EntityKind, RoomDef, RoomItem, SELF_WORDS, Slot, Tier, TraitRef, WorldPack,
};
use crate::rewards::{MintRecord, PoolSpent};

/// One entity.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent status flags of one entity"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    /// The id.
    pub id: EntityId,
    /// The display name.
    pub name: String,
    /// Other names.
    pub aliases: Vec<String>,
    /// What it is, as the world shows it.
    pub kind: EntityKind,
    /// The line `look` shows.
    pub short: String,
    /// What `examine` shows.
    pub description: String,
    /// Where it is.
    pub room: RoomId,
    /// Where it started (mobs respawn here).
    pub home: RoomId,
    /// Attributes.
    pub attrs: Attributes,
    /// HP and FP.
    pub pools: Pools,
    /// Skill levels.
    pub skills: BTreeMap<SkillId, i32>,
    /// Traits.
    pub traits: Vec<TraitRef>,
    /// Money.
    pub currency: i64,
    /// The shop it keeps.
    pub shop: Option<ShopId>,
    /// What it carries, equipped or not.
    pub inventory: Vec<ItemId>,
    /// What it wields or wears. Each is also in `inventory`.
    pub equipped: BTreeMap<Slot, ItemId>,
    /// Natural DR.
    pub dr: i32,
    /// It attacks players.
    pub hostile: bool,
    /// It keeps the peace.
    pub guard: bool,
    /// Minutes until a dead mob returns.
    pub respawn_minutes: Option<u32>,
    /// Connected and present. An offline entity is not in the world.
    pub online: bool,
    /// A human controls it. Never shown to entities.
    pub human: bool,
    /// The mind tier. Never shown to entities.
    pub tier: Tier,
    /// Defending until its next action.
    pub defending: bool,
    /// Knocked out.
    pub unconscious: bool,
    /// Dead; a mob comes back at this time.
    pub dead_until_ms: Option<u64>,
    /// Hidden by a stealth success.
    pub hidden: bool,
    /// The last entity that attacked it, and when.
    pub last_attacked_by: Option<(EntityId, u64)>,
    /// A haggle discount on the next purchase from this keeper.
    pub haggle_with: Option<EntityId>,
    /// Marks that world scripts keep on it. No view shows them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub marks: BTreeMap<String, FlagValue>,
}

impl Entity {
    /// Whether it is in the world and alive.
    #[must_use]
    pub const fn present(&self) -> bool {
        self.online && self.dead_until_ms.is_none()
    }

    /// Whether it can act.
    #[must_use]
    pub const fn can_act(&self) -> bool {
        self.present() && !self.unconscious
    }

    /// The reference events use.
    #[must_use]
    pub fn actor_ref(&self) -> ActorRef {
        ActorRef {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }

    /// Whether `word` names it.
    #[must_use]
    pub fn matches(&self, word: &str) -> bool {
        let w = word.trim().to_ascii_lowercase();
        !w.is_empty()
            && (self.id.as_str() == w
                || self.name.to_ascii_lowercase() == w
                || self
                    .name
                    .to_ascii_lowercase()
                    .split_whitespace()
                    .any(|p| p == w)
                || self.aliases.iter().any(|a| a.to_ascii_lowercase() == w))
    }

    /// The self-control number of a trait it has.
    #[must_use]
    pub fn trait_control(&self, name: &str) -> Option<u8> {
        self.traits
            .iter()
            .find(|t| t.name == name)
            .and_then(|t| t.control)
    }

    /// Whether it has a trait at all.
    #[must_use]
    pub fn has_trait(&self, name: &str) -> bool {
        self.traits.iter().any(|t| t.name == name)
    }
}

/// One item instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The id.
    pub id: ItemId,
    /// The prototype.
    pub proto: ProtoId,
    /// What is inside, for a container.
    pub contents: Vec<ItemId>,
    /// A locked container.
    pub locked: bool,
    /// Merchandise of a shop: its keeper may not give it away.
    pub stock: bool,
    /// What the keeper paid for it, for stock bought from a player. The
    /// keeper never resells it for less.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paid: Option<i64>,
}

/// One notice on a board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardPost {
    /// The event that posted it.
    pub seq: Seq,
    /// Who.
    pub author: ActorRef,
    /// The notice. Untrusted.
    pub text: String,
    /// When.
    pub at_ms: u64,
}

/// The changing state of one room.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomState {
    /// Items on the floor.
    pub items: Vec<ItemId>,
    /// Locked exits opened since start.
    pub unlocked: BTreeSet<Direction>,
    /// Board posts, oldest first.
    pub board: Vec<BoardPost>,
}

/// The whole changing state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldState {
    /// Entities.
    pub entities: BTreeMap<EntityId, Entity>,
    /// Item instances.
    pub items: BTreeMap<ItemId, Item>,
    /// Rooms.
    pub rooms: BTreeMap<RoomId, RoomState>,
    /// The next item id.
    pub next_item: u64,
    /// The next event sequence number.
    pub next_seq: u64,
    /// The dice state, so a restart continues the stream.
    pub dice_state: u64,
    /// When each (entity, trait) last rolled self-control, for cooldowns.
    #[serde(with = "pairs")]
    pub nudge_rolled_ms: BTreeMap<(EntityId, String), u64>,
    /// When regeneration last ran.
    pub last_regen_ms: u64,
    /// World flags that scripts set. No view shows them.
    #[serde(default)]
    pub flags: BTreeMap<String, FlagValue>,
    /// The consecutive faults of each world hook, by hook key. A hook at
    /// zero has no entry.
    #[serde(default)]
    pub hook_faults: BTreeMap<String, u8>,
    /// The version of the scripts last loaded (`none` when scripts are
    /// off), so history says which version ran from which event.
    #[serde(default)]
    pub scripts_version: Option<String>,
    /// What each reward pool has given in the period it counts in.
    #[serde(default)]
    pub pool_spent: BTreeMap<PoolId, PoolSpent>,
    /// The ledger: every reward a script was ever given, in order. Money
    /// and items are conserved except for these mints.
    #[serde(default)]
    pub mints: Vec<MintRecord>,
}

mod pairs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::ids::EntityId;

    type Map = BTreeMap<(EntityId, String), u64>;

    pub fn serialize<S: Serializer>(map: &Map, s: S) -> Result<S::Ok, S::Error> {
        let v: Vec<(&EntityId, &String, &u64)> = map.iter().map(|((a, b), c)| (a, b, c)).collect();
        v.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Map, D::Error> {
        let v: Vec<(EntityId, String, u64)> = Vec::deserialize(d)?;
        Ok(v.into_iter().map(|(a, b, c)| ((a, b), c)).collect())
    }
}

/// Where an item is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum Holder {
    /// On a room floor.
    Room {
        /// The room.
        room: RoomId,
    },
    /// Carried.
    Entity {
        /// The carrier.
        entity: EntityId,
    },
    /// Inside a container.
    Container {
        /// The container.
        item: ItemId,
    },
}

/// Who holds a login name. Server-internal: never part of a view, an
/// event or an MCP result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameOwner {
    /// Nobody: a new character would get this id.
    Free(EntityId),
    /// A human character.
    Human(EntityId),
    /// A player character no human plays, or another player the name
    /// matches.
    OtherPlayer,
    /// An NPC or a mob.
    NonPlayer,
}

/// The prefix of every player character's id. A new name may not start
/// with it: `pc_ada` would otherwise be a character that `ada` locks out.
pub const PLAYER_ID_PREFIX: &str = "pc_";

impl WorldState {
    /// Who holds the login name `name`.
    ///
    /// # Errors
    /// [`LoginError::Invalid`] for a name that is not 2 to 16 letters,
    /// digits or underscores; [`LoginError::Reserved`] for a word that
    /// means the speaker, or a new name starting with `pc_`.
    pub fn name_owner(&self, name: &str) -> Result<NameOwner, LoginError> {
        let name = name.trim();
        let valid = (2..=16).contains(&name.chars().count())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(LoginError::Invalid);
        }
        let lower = name.to_ascii_lowercase();
        let id = EntityId::new(format!("{PLAYER_ID_PREFIX}{lower}"));
        // A save from before a name was reserved may still hold such a
        // player; it may come back, but nobody new may take the name.
        let saved_human = self.entities.get(&id).is_some_and(|e| e.human);
        if !saved_human
            && (SELF_WORDS.iter().any(|w| lower == *w) || lower.starts_with(PLAYER_ID_PREFIX))
        {
            return Err(LoginError::Reserved);
        }
        let other = self
            .entities
            .values()
            .find(|e| e.id != id && (e.matches(name) || e.id.as_str() == lower));
        if let Some(e) = other {
            return Ok(if e.kind == EntityKind::Player {
                NameOwner::OtherPlayer
            } else {
                NameOwner::NonPlayer
            });
        }
        Ok(match self.entities.get(&id) {
            Some(e) if e.human => NameOwner::Human(id),
            Some(e) if e.kind == EntityKind::Player => NameOwner::OtherPlayer,
            Some(_) => NameOwner::NonPlayer,
            None => NameOwner::Free(id),
        })
    }

    /// Every human character's id.
    #[must_use]
    pub fn human_ids(&self) -> Vec<EntityId> {
        self.entities
            .values()
            .filter(|e| e.human)
            .map(|e| e.id.clone())
            .collect()
    }

    /// A fresh state from a pack.
    #[must_use]
    pub fn from_pack(pack: &WorldPack, seed: u64) -> Self {
        let mut s = Self {
            entities: BTreeMap::new(),
            items: BTreeMap::new(),
            rooms: pack
                .room_order
                .iter()
                .map(|r| (r.clone(), RoomState::default()))
                .collect(),
            next_item: 1,
            next_seq: 1,
            dice_state: seed,
            nudge_rolled_ms: BTreeMap::new(),
            last_regen_ms: 0,
            flags: BTreeMap::new(),
            hook_faults: BTreeMap::new(),
            scripts_version: None,
            pool_spent: BTreeMap::new(),
            mints: Vec::new(),
        };
        for (rid, def) in &pack.rooms {
            s.place_room_items(rid, def, pack);
        }
        for (id, def) in &pack.cast {
            let e = s.entity_from_cast(id, def, pack);
            s.entities.insert(id.clone(), e);
        }
        s
    }

    /// Puts a room's starting items on its floor.
    fn place_room_items(&mut self, rid: &RoomId, def: &RoomDef, pack: &WorldPack) {
        for placed in &def.items {
            let id = self.new_item(placed.proto(), pack);
            if let RoomItem::WithContents { contents, .. } = placed {
                for c in contents {
                    let inner = self.new_item(c, pack);
                    if let Some(it) = self.items.get_mut(&id) {
                        it.contents.push(inner);
                    }
                }
            }
            if let Some(room) = self.rooms.get_mut(rid) {
                room.items.push(id);
            }
        }
    }

    /// Brings a saved state in line with the pack it resumes under, so a
    /// pack edit never strands the world.
    ///
    /// - A room the pack added appears with its starting items; a room it
    ///   dropped goes, with what lay on its floor, and whoever stood or
    ///   lived there moves to the start room.
    /// - A non-human entity the cast no longer names (a renamed or removed
    ///   cast id) retires: it has no mind behind it, so it would be a ghost.
    ///   Its gear falls to the floor as plain goods, and every reference to
    ///   it is cleared.
    /// - A cast entry the save lacks is spawned fresh.
    ///
    /// Humans are never retired. Renaming a cast id therefore makes a new
    /// character, not a moved one.
    pub fn reconcile(&mut self, pack: &WorldPack) {
        let start = pack.world.start_room.clone();
        for (rid, def) in &pack.rooms {
            if !self.rooms.contains_key(rid) {
                self.rooms.insert(rid.clone(), RoomState::default());
                self.place_room_items(rid, def, pack);
            }
        }
        let dropped: Vec<RoomId> = self
            .rooms
            .keys()
            .filter(|r| !pack.rooms.contains_key(*r))
            .cloned()
            .collect();
        for rid in &dropped {
            if let Some(room) = self.rooms.remove(rid) {
                for item in room.items {
                    self.forget_item(item);
                }
            }
        }
        for e in self.entities.values_mut() {
            if !pack.rooms.contains_key(&e.room) {
                e.room.clone_from(&start);
            }
            if !pack.rooms.contains_key(&e.home) {
                e.home.clone_from(&start);
            }
        }
        let stale: Vec<EntityId> = self
            .entities
            .values()
            .filter(|e| !e.human && !pack.cast.contains_key(&e.id))
            .map(|e| e.id.clone())
            .collect();
        for id in &stale {
            let Some(e) = self.entities.remove(id) else {
                continue;
            };
            for item in &e.inventory {
                if let Some(it) = self.items.get_mut(item) {
                    it.stock = false;
                    it.paid = None;
                }
            }
            if let Some(room) = self.rooms.get_mut(&e.room) {
                room.items.extend(e.inventory);
            }
        }
        if !stale.is_empty() {
            for e in self.entities.values_mut() {
                if e.last_attacked_by
                    .as_ref()
                    .is_some_and(|(a, _)| stale.contains(a))
                {
                    e.last_attacked_by = None;
                }
                if e.haggle_with.as_ref().is_some_and(|k| stale.contains(k)) {
                    e.haggle_with = None;
                }
            }
            self.nudge_rolled_ms.retain(|(e, _), _| !stale.contains(e));
        }
        for (id, def) in &pack.cast {
            if !self.entities.contains_key(id) {
                let e = self.entity_from_cast(id, def, pack);
                self.entities.insert(id.clone(), e);
            }
        }
        // A pool the pack dropped gives nothing more; the ledger keeps
        // what it gave.
        self.pool_spent
            .retain(|p, _| pack.economy.rewards.contains_key(p));
    }

    /// Removes an item and everything inside it from the world.
    fn forget_item(&mut self, item: ItemId) {
        if let Some(it) = self.items.remove(&item) {
            for inner in it.contents {
                self.forget_item(inner);
            }
        }
    }

    /// Makes a new item instance, held by nobody yet.
    pub fn new_item(&mut self, proto: &ProtoId, pack: &WorldPack) -> ItemId {
        let id = ItemId(self.next_item);
        self.next_item += 1;
        let def = pack.items.get(proto);
        self.items.insert(
            id,
            Item {
                id,
                proto: proto.clone(),
                contents: Vec::new(),
                locked: def.is_some_and(|d| d.lock.is_some()),
                stock: false,
                paid: None,
            },
        );
        id
    }

    fn entity_from_cast(&mut self, id: &EntityId, def: &CastDef, pack: &WorldPack) -> Entity {
        let mut inventory = Vec::new();
        let mut equipped = BTreeMap::new();
        let mut pending_equip: Vec<&ProtoId> = def.equip.iter().collect();
        for p in &def.inventory {
            let item = self.new_item(p, pack);
            inventory.push(item);
            if let Some(pos) = pending_equip.iter().position(|e| *e == p)
                && let Some(slot) = pack.items.get(p).and_then(|d| d.slot)
            {
                pending_equip.remove(pos);
                equipped.insert(slot, item);
            }
        }
        if let Some(shop) = def.shop.as_ref().and_then(|s| pack.economy.shops.get(s)) {
            for p in &shop.stock {
                let item = self.new_item(p, pack);
                if let Some(it) = self.items.get_mut(&item) {
                    it.stock = true;
                }
                inventory.push(item);
            }
        }
        let tier = def.mind.tier;
        Entity {
            id: id.clone(),
            name: def.name.clone(),
            aliases: def.aliases.clone(),
            kind: def.kind,
            short: def.short.clone(),
            description: def.description.clone(),
            room: def.room.clone(),
            home: def.room.clone(),
            attrs: def.attrs,
            pools: Pools::full(&def.attrs),
            skills: def.skills.clone(),
            traits: def.traits.clone(),
            currency: def.currency,
            shop: def.shop.clone(),
            inventory,
            equipped,
            dr: def.dr,
            hostile: def.hostile,
            guard: def.guard,
            respawn_minutes: def.respawn_minutes,
            // A hidden agent PC logs in on its schedule; everyone else is here.
            online: tier != Tier::Turing,
            human: false,
            tier,
            defending: false,
            unconscious: false,
            dead_until_ms: None,
            hidden: false,
            last_attacked_by: None,
            haggle_with: None,
            marks: BTreeMap::new(),
        }
    }

    /// Makes a player entity from the template.
    pub fn new_player(
        &mut self,
        id: &EntityId,
        name: &str,
        room: RoomId,
        pack: &WorldPack,
    ) -> &Entity {
        let t = &pack.player_template;
        let mut inventory = Vec::new();
        let mut equipped = BTreeMap::new();
        for p in &t.inventory {
            let item = self.new_item(p, pack);
            inventory.push(item);
            if t.equip.contains(p)
                && let Some(slot) = pack.items.get(p).and_then(|d| d.slot)
            {
                equipped.entry(slot).or_insert(item);
            }
        }
        let e = Entity {
            id: id.clone(),
            name: name.to_owned(),
            aliases: Vec::new(),
            kind: EntityKind::Player,
            short: format!("{name} is here."),
            description: format!("{name}, an adventurer, by the look of the gear."),
            room: room.clone(),
            home: room,
            attrs: t.attrs,
            pools: Pools::full(&t.attrs),
            skills: t.skills.clone(),
            traits: Vec::new(),
            currency: t.currency,
            shop: None,
            inventory,
            equipped,
            dr: 0,
            hostile: false,
            guard: false,
            respawn_minutes: None,
            // `World::login` brings it online and announces it.
            online: false,
            human: true,
            tier: Tier::None,
            defending: false,
            unconscious: false,
            dead_until_ms: None,
            hidden: false,
            last_attacked_by: None,
            haggle_with: None,
            marks: BTreeMap::new(),
        };
        self.entities.insert(id.clone(), e);
        &self.entities[id]
    }

    /// Where an item is, if anywhere.
    #[must_use]
    pub fn holder_of(&self, item: ItemId) -> Option<Holder> {
        for (rid, r) in &self.rooms {
            if r.items.contains(&item) {
                return Some(Holder::Room { room: rid.clone() });
            }
        }
        for (eid, e) in &self.entities {
            if e.inventory.contains(&item) {
                return Some(Holder::Entity {
                    entity: eid.clone(),
                });
            }
        }
        for (iid, it) in &self.items {
            if it.contents.contains(&item) {
                return Some(Holder::Container { item: *iid });
            }
        }
        None
    }

    /// Takes an item out of wherever it is. Unequips it too.
    pub fn detach(&mut self, item: ItemId) -> Option<Holder> {
        let from = self.holder_of(item)?;
        match &from {
            Holder::Room { room } => {
                if let Some(r) = self.rooms.get_mut(room) {
                    r.items.retain(|i| *i != item);
                }
            }
            Holder::Entity { entity } => {
                if let Some(e) = self.entities.get_mut(entity) {
                    e.inventory.retain(|i| *i != item);
                    e.equipped.retain(|_, i| *i != item);
                }
            }
            Holder::Container { item: c } => {
                if let Some(it) = self.items.get_mut(c) {
                    it.contents.retain(|i| *i != item);
                }
            }
        }
        Some(from)
    }

    /// Puts a detached item somewhere.
    pub fn attach(&mut self, item: ItemId, to: &Holder) {
        match to {
            Holder::Room { room } => {
                if let Some(r) = self.rooms.get_mut(room) {
                    r.items.push(item);
                }
            }
            Holder::Entity { entity } => {
                if let Some(e) = self.entities.get_mut(entity) {
                    e.inventory.push(item);
                }
            }
            Holder::Container { item: c } => {
                if let Some(it) = self.items.get_mut(c) {
                    it.contents.push(item);
                }
            }
        }
    }

    /// Moves an item and returns where it was.
    pub fn move_item(&mut self, item: ItemId, to: &Holder) -> Option<Holder> {
        let from = self.detach(item)?;
        self.attach(item, to);
        Some(from)
    }

    /// The total money held by entities, plus money sealed in items.
    #[must_use]
    pub fn total_currency(&self, pack: &WorldPack) -> i64 {
        let held: i64 = self.entities.values().map(|e| e.currency).sum();
        let sealed: i64 = self
            .items
            .values()
            .filter_map(|i| pack.items.get(&i.proto).and_then(|d| d.currency))
            .sum();
        held + sealed
    }

    /// The currency every ledgered mint made.
    #[must_use]
    pub fn minted_currency(&self) -> i64 {
        self.mints.iter().map(|m| m.currency).sum()
    }

    /// Checks the ledger against the pack's pools. Returns each violation.
    ///
    /// - No pool has given more than its budget in the period it counts in.
    /// - What a pool counts as given in its period is exactly the sum of
    ///   its ledger entries for that period.
    #[must_use]
    pub fn check_rewards(&self, pack: &WorldPack) -> Vec<String> {
        let mut bad = Vec::new();
        for (id, spent) in &self.pool_spent {
            let Some(pool) = pack.economy.rewards.get(id) else {
                bad.push(format!("pool {id} is spent but not in the pack"));
                continue;
            };
            if i64::try_from(spent.currency).map_or(true, |c| c > pool.currency) {
                bad.push(format!("pool {id} gave {} currency", spent.currency));
            }
            let ledger: Vec<&MintRecord> = self
                .mints
                .iter()
                .filter(|m| &m.pool == id && m.period == spent.period)
                .collect();
            let money: i64 = ledger.iter().map(|m| m.currency).sum();
            if i64::try_from(spent.currency).ok() != Some(money) {
                bad.push(format!(
                    "pool {id} counts {} currency; its ledger {money}",
                    spent.currency
                ));
            }
            for (proto, budget) in &pool.items {
                let used = spent.items.get(proto).copied().unwrap_or(0);
                if used > u64::from(*budget) {
                    bad.push(format!("pool {id} made {used} {proto}"));
                }
                let made = ledger
                    .iter()
                    .flat_map(|m| &m.items)
                    .filter(|i| &i.proto == proto)
                    .count();
                if u64::try_from(made).ok() != Some(used) {
                    bad.push(format!(
                        "pool {id} counts {used} {proto}; its ledger {made}"
                    ));
                }
            }
        }
        bad
    }

    /// Checks the structural invariants. Returns each violation.
    ///
    /// - Every item is in exactly one place.
    /// - Every equipped item is carried by the entity that equips it.
    /// - Every entity stands in a known room.
    /// - No entity has negative money.
    /// - Every item the ledger names was made once, below `next_item`.
    #[must_use]
    pub fn check(&self) -> Vec<String> {
        let mut seen: BTreeMap<ItemId, usize> = BTreeMap::new();
        let mut bump = |i: &ItemId| *seen.entry(*i).or_default() += 1;
        self.rooms
            .values()
            .flat_map(|r| &r.items)
            .for_each(&mut bump);
        self.entities
            .values()
            .flat_map(|e| &e.inventory)
            .for_each(&mut bump);
        self.items
            .values()
            .flat_map(|i| &i.contents)
            .for_each(&mut bump);
        let mut bad = Vec::new();
        for id in self.items.keys() {
            match seen.get(id).copied().unwrap_or(0) {
                1 => {}
                n => bad.push(format!("{id} is held in {n} places")),
            }
        }
        for id in seen.keys() {
            if !self.items.contains_key(id) {
                bad.push(format!("{id} is held but does not exist"));
            }
        }
        for e in self.entities.values() {
            for (slot, i) in &e.equipped {
                if !e.inventory.contains(i) {
                    bad.push(format!(
                        "{} equips {i} in {slot:?} without carrying it",
                        e.id
                    ));
                }
            }
            if !self.rooms.contains_key(&e.room) {
                bad.push(format!("{} stands in unknown room {}", e.id, e.room));
            }
            if e.currency < 0 {
                bad.push(format!("{} has negative money", e.id));
            }
            if e.marks.len() > MARK_CAP {
                bad.push(format!(
                    "{} holds {} marks; the cap is {MARK_CAP}",
                    e.id,
                    e.marks.len()
                ));
            }
        }
        let mut minted = BTreeSet::new();
        for item in self.mints.iter().flat_map(|m| &m.items) {
            if item.id.0 >= self.next_item || !minted.insert(item.id) {
                bad.push(format!(
                    "the ledger names {} twice or ahead of time",
                    item.id
                ));
            }
        }
        if self.flags.len() > FLAG_CAP {
            bad.push(format!(
                "flags hold {} entries; the cap is {FLAG_CAP}",
                self.flags.len()
            ));
        }
        bad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_world_is_consistent() {
        let pack = WorldPack::crypt_v01().unwrap();
        let s = WorldState::from_pack(&pack, 1);
        assert_eq!(s.check(), Vec::<String>::new());
        let torin = &s.entities[&EntityId::from("torin")];
        assert_eq!(torin.inventory.len(), 6);
        assert!(torin.inventory.iter().all(|i| s.items[i].stock));
        let kestrel = &s.entities[&EntityId::from("pc_kestrel")];
        assert!(!kestrel.online, "a turing PC starts logged out");
        assert_eq!(kestrel.kind, EntityKind::Player);
        assert_eq!(kestrel.equipped.len(), 1);
        let cistern = &s.rooms[&RoomId::from("flooded_cistern")];
        let lockbox = &s.items[&cistern.items[0]];
        assert!(lockbox.locked);
        assert_eq!(lockbox.contents.len(), 3);
    }

    #[test]
    fn moving_items_keeps_one_holder() {
        let pack = WorldPack::crypt_v01().unwrap();
        let mut s = WorldState::from_pack(&pack, 1);
        let ore = s.rooms[&RoomId::from("ossuary_crypt")].items[0];
        let to = Holder::Entity {
            entity: EntityId::from("vich"),
        };
        assert_eq!(
            s.move_item(ore, &to),
            Some(Holder::Room {
                room: RoomId::from("ossuary_crypt")
            })
        );
        assert_eq!(s.holder_of(ore), Some(to));
        assert_eq!(s.check(), Vec::<String>::new());
        let before = s.total_currency(&pack);
        assert!(before > 0);
    }

    #[test]
    fn the_checker_finds_a_duplicate() {
        let pack = WorldPack::crypt_v01().unwrap();
        let mut s = WorldState::from_pack(&pack, 1);
        let ore = s.rooms[&RoomId::from("ossuary_crypt")].items[0];
        s.attach(
            ore,
            &Holder::Entity {
                entity: EntityId::from("vich"),
            },
        );
        assert!(s.check().iter().any(|m| m.contains("2 places")));
    }

    #[test]
    fn players_come_from_the_template() {
        let pack = WorldPack::crypt_v01().unwrap();
        let mut s = WorldState::from_pack(&pack, 1);
        let p = s.new_player(
            &EntityId::from("pc_ada"),
            "ada",
            RoomId::from("town_square"),
            &pack,
        );
        assert!(p.human);
        assert_eq!(p.currency, 25);
        assert_eq!(p.equipped.len(), 1);
        assert_eq!(s.check(), Vec::<String>::new());
        let round: WorldState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round, s);
    }
}
