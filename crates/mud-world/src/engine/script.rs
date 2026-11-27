//! World hooks in the engine: building a hook's view, running it, and
//! admitting, validating and applying its effects.
//!
//! At most one hook runs per command, and nothing a hook does sets off
//! another: effects are applied with the engine's own helpers, which run
//! no hooks, and login, respawn and ticks run none either.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use mud_rules::{DamageType, injury};
use serde_json::{Value, json};

use super::look::{kind_word, wound_word};
use super::{Audit, Ctx, Done, Emit, ErrorCode, Fail, HooksChange, StateChange, World};
use crate::event::EventKind;
use crate::hooks::{
    ActorFacts, CastFacts, EFFECT_TEXT_MAX, ExitFacts, FAULT_MESSAGE_MAX, FLAG_CAP, FLAG_TEXT_MAX,
    FlagValue, HookId, HookInput, HookOutcome, HookReply, ItemFacts, MARK_CAP, RoomFacts,
    ScriptEffect, TargetFacts, USE_UP_MAX, VIEW_LIST_CAP, WorldHooks, hook_allowed, hook_step,
    tally, tally_admissible, tally_reason, valid_flag_name,
};
use crate::ids::{EntityId, ItemId, PoolId, ProtoId, RoomId};
use crate::pack::{Direction, EntityKind};
use crate::parser::{Command, forbidden_in_text};
use crate::rewards::{AskError, MintRecord, MintedItem, PoolFacts};
use crate::state::{Entity, Holder};

/// What the actor reads when a verb or `use` hook declines: server text.
pub const DECLINED: &str = "Nothing comes of that just now.";

/// Why an effect list cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Inadmissible {
    /// A script bug: the hook faults, and its breaker counts it.
    #[error("{0}")]
    Fault(String),
    /// The world cannot take it now (a pool spent for the period, marks or
    /// flags full): the hook declines, nothing applies, and its breaker
    /// does not move, so no player can take a hook down by asking again.
    #[error("{0}")]
    Declined(String),
}

impl From<String> for Inadmissible {
    fn from(message: String) -> Self {
        Self::Fault(message)
    }
}

/// The breaker state of one hook, for the observer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookStatus {
    /// The hook key.
    pub key: String,
    /// Its consecutive faults.
    pub faults: u8,
    /// Whether its breaker is open.
    pub tripped: bool,
}

/// What a hook run came to.
#[derive(Debug)]
pub enum HookRun {
    /// No script handles it, or scripts are off.
    Absent,
    /// Its breaker is open; it was not called.
    Skipped,
    /// It faulted; nothing was applied.
    Faulted,
    /// The world could not take its effects now; nothing was applied, and
    /// its breaker did not move.
    Declined,
    /// Its effects were applied.
    Applied {
        /// The lines for the actor, in order.
        msgs: Vec<String>,
        /// The refusal, when the hook refused the command.
        refused: Option<String>,
    },
}

/// The world a hook call is about.
pub struct HookCall {
    pub hook: HookId,
    pub room: RoomId,
    pub from: Option<RoomId>,
    pub text: Option<String>,
    pub target: Option<EntityId>,
    pub item: Option<ItemId>,
}

impl HookCall {
    pub(super) const fn new(hook: HookId, room: RoomId) -> Self {
        Self {
            hook,
            room,
            from: None,
            text: None,
            target: None,
            item: None,
        }
    }
}

/// One reward to mint.
struct Mint<'a> {
    room: &'a RoomId,
    key: &'a str,
    pool: &'a PoolId,
    currency: i64,
    items: &'a BTreeMap<ProtoId, u32>,
}

fn capped(message: &str) -> String {
    message.chars().take(FAULT_MESSAGE_MAX).collect()
}

impl World {
    // ------------------------------------------------------------ scripts

    /// Installs the world's scripts, or none.
    ///
    /// When the version differs from the one last recorded, or on any
    /// reload, every hook breaker resets and a marker event (perceived by
    /// no entity) records the new version from this sequence number on.
    pub fn set_hooks(&mut self, hooks: Option<Arc<dyn WorldHooks>>, now_ms: u64, why: HooksChange) {
        let version = hooks
            .as_ref()
            .map_or_else(|| "none".to_owned(), |h| h.version());
        let previous = self.state.scripts_version.clone();
        self.hooks = hooks;
        if why == HooksChange::Start && previous.as_deref() == Some(version.as_str()) {
            return;
        }
        let max = self.hooks.as_ref().map_or(0, |h| h.limits().max_faults);
        let faults = std::mem::take(&mut self.state.hook_faults);
        for (key, n) in faults {
            let (next, _) = hook_step(n, HookOutcome::Reloaded, max);
            if next > 0 {
                self.state.hook_faults.insert(key, next);
            }
        }
        self.state.scripts_version = Some(version.clone());
        let spec = Emit {
            only: Some(Vec::new()),
            ..Emit::new(EventKind::System, None, format!("scripts {version} loaded"))
                .tags(["scripts".to_owned()])
                .data(json!({ "version": version, "previous": previous, "why": why.name() }))
        };
        self.emit_bare(now_ms, spec);
        self.audit.push(Audit::ScriptsLoaded {
            version,
            previous,
            why: why.name().to_owned(),
            at_ms: now_ms,
        });
    }

    /// The installed scripts, if any.
    #[must_use]
    pub fn hooks(&self) -> Option<&Arc<dyn WorldHooks>> {
        self.hooks.as_ref()
    }

    /// Whether `verb` is a custom verb of the world's scripts.
    #[must_use]
    pub fn is_custom_verb(&self, verb: &str) -> bool {
        self.hooks.as_ref().is_some_and(|h| h.is_verb(verb))
    }

    /// The custom verbs `actor` can use where it stands, by name.
    #[must_use]
    pub fn verbs_here(&self, actor: &EntityId) -> Vec<String> {
        let (Some(h), Some(e)) = (&self.hooks, self.state.entities.get(actor)) else {
            return Vec::new();
        };
        let mut out: Vec<String> = h
            .verbs()
            .into_iter()
            .filter(|v| v.works_in(&e.room))
            .map(|v| v.name)
            .collect();
        out.sort();
        out
    }

    /// The help text for `actor`: the built-in verbs, and the custom
    /// verbs usable where it stands.
    #[must_use]
    pub fn help_text(&self, actor: &EntityId) -> String {
        let mut help = crate::parser::HELP.to_owned();
        help.push_str("\nrecovery: ");
        help.push_str(&self.recovery_rule());
        help.push(' ');
        help.push_str(&self.death_rule());
        help.push_str("\nfighting: ");
        help.push_str(&self.pack.combat.rule());
        let here = self.verbs_here(actor);
        if let Some(h) = &self.hooks
            && !here.is_empty()
        {
            let usages: Vec<String> = here
                .iter()
                .filter_map(|v| h.verb(v))
                .map(|v| v.usage)
                .collect();
            help.push_str("\nhere: ");
            help.push_str(&usages.join(" | "));
        }
        help
    }

    /// The breakers that are not at rest: every hook with a fault.
    #[must_use]
    pub fn hook_status(&self) -> Vec<HookStatus> {
        let max = self.hooks.as_ref().map_or(0, |h| h.limits().max_faults);
        self.state
            .hook_faults
            .iter()
            .map(|(key, n)| HookStatus {
                key: key.clone(),
                faults: *n,
                tripped: !hook_allowed(*n, max),
            })
            .collect()
    }

    // ------------------------------------------------------------ views

    /// An entity as a hook sees it, with its marks. `kind` only: never a
    /// tier, a mind or whether a human plays it.
    #[must_use]
    pub fn actor_facts(&self, id: &EntityId) -> Option<ActorFacts> {
        let e = self.state.entities.get(id)?;
        let visible = |i: &ItemId| {
            self.state
                .items
                .get(i)
                .filter(|it| self.pack.items.get(&it.proto).is_some_and(|d| !d.hidden))
                .map(|it| it.proto.clone())
        };
        Some(ActorFacts {
            id: e.id.clone(),
            name: e.name.clone(),
            kind: kind_word(e.kind).to_owned(),
            room: e.room.clone(),
            hp: e.pools.hp,
            hp_max: e.pools.hp_max,
            condition: condition(e).to_owned(),
            attrs: BTreeMap::from([
                ("st".to_owned(), e.attrs.st),
                ("dx".to_owned(), e.attrs.dx),
                ("iq".to_owned(), e.attrs.iq),
                ("ht".to_owned(), e.attrs.ht),
            ]),
            skills: self
                .pack
                .skills
                .keys()
                .filter_map(|s| Some((s.to_string(), self.skill_level(e, s)?)))
                .collect(),
            inventory: e.inventory.iter().filter_map(visible).collect(),
            equipped: e.equipped.values().filter_map(visible).collect(),
            currency: e.currency,
            marks: Some(e.marks.clone()),
        })
    }

    fn target_facts(e: &Entity) -> TargetFacts {
        TargetFacts {
            id: e.id.clone(),
            name: e.name.clone(),
            kind: kind_word(e.kind).to_owned(),
            condition: condition(e).to_owned(),
        }
    }

    fn room_facts(&self, room: &RoomId, actor: Option<&EntityId>) -> RoomFacts {
        let def = self.pack.rooms.get(room);
        let exits = def.map_or_else(Vec::new, |d| {
            d.exits
                .iter()
                .map(|(dir, to)| ExitFacts {
                    dir: dir.short().to_owned(),
                    to: to.clone(),
                    locked: self.exit_is_locked(room, *dir),
                    lockable: d.locks.contains_key(dir),
                })
                .collect()
        });
        let present = self
            .present_in(room)
            .filter(|e| Some(&e.id) != actor && !e.hidden)
            .take(VIEW_LIST_CAP)
            .map(Self::target_facts)
            .collect();
        let items = self.state.rooms.get(room).map_or_else(Vec::new, |r| {
            r.items
                .iter()
                .filter_map(|i| self.state.items.get(i))
                .filter(|it| self.pack.items.get(&it.proto).is_some_and(|d| !d.hidden))
                .take(VIEW_LIST_CAP)
                .map(|it| it.proto.clone())
                .collect()
        });
        RoomFacts {
            id: room.clone(),
            title: def.map_or_else(String::new, |d| d.title.clone()),
            dark: def.is_some_and(|d| d.dark),
            lit: self.lit(room),
            safe: def.is_some_and(|d| d.safe),
            exits,
            present,
            items,
        }
    }

    fn cast_facts(&self) -> BTreeMap<EntityId, CastFacts> {
        self.pack
            .cast
            .keys()
            .filter_map(|id| self.state.entities.get(id))
            .filter(|e| e.kind != EntityKind::Player)
            .map(|e| {
                let present = e.present() && !e.hidden;
                (
                    e.id.clone(),
                    CastFacts {
                        name: e.name.clone(),
                        room: present.then(|| e.room.clone()),
                        alive: e.dead_until_ms.is_none(),
                        present,
                        deaths: e.deaths,
                    },
                )
            })
            .collect()
    }

    fn item_facts(&self, item: ItemId) -> Option<ItemFacts> {
        let it = self.state.items.get(&item)?;
        Some(ItemFacts {
            id: item,
            proto: it.proto.clone(),
            name: self.item_name(item),
        })
    }

    fn hook_input(&self, actor: ActorFacts, call: &HookCall, now_ms: u64) -> HookInput {
        HookInput {
            hook: call.hook.key(),
            now_ms,
            room: self.room_facts(&call.room, Some(&actor.id)),
            actor,
            from: call.from.clone(),
            text: Some(call.text.clone().unwrap_or_default()),
            target: call
                .target
                .as_ref()
                .and_then(|t| self.state.entities.get(t))
                .map(Self::target_facts),
            item: call.item.and_then(|i| self.item_facts(i)),
            flags: self.state.flags.clone(),
            cast: self.cast_facts(),
            pools: self.pool_facts(now_ms),
        }
    }

    /// What is left of every reward pool at `now_ms`.
    #[must_use]
    pub fn pool_facts(&self, now_ms: u64) -> BTreeMap<PoolId, PoolFacts> {
        self.pack
            .economy
            .rewards
            .iter()
            .map(|(id, pool)| {
                (
                    id.clone(),
                    pool.facts(self.state.pool_spent.get(id), now_ms),
                )
            })
            .collect()
    }

    /// The input a dry run gives `hook` in `room`: a fresh player from the
    /// template stands there, and the hook's item is a stand-in.
    #[must_use]
    pub fn probe_input(&self, room: &RoomId, hook: &HookId, now_ms: u64) -> HookInput {
        let t = &self.pack.player_template;
        let probe = Entity {
            id: EntityId::from("pc_probe"),
            name: "probe".to_owned(),
            aliases: Vec::new(),
            kind: EntityKind::Player,
            short: String::new(),
            description: String::new(),
            room: room.clone(),
            home: room.clone(),
            attrs: t.attrs,
            pools: mud_rules::Pools::full(&t.attrs),
            skills: t.skills.clone(),
            traits: Vec::new(),
            currency: t.currency,
            shop: None,
            inventory: Vec::new(),
            equipped: BTreeMap::new(),
            dr: 0,
            hostile: false,
            guard: false,
            respawn_minutes: None,
            online: true,
            human: false,
            tier: crate::pack::Tier::None,
            defending: false,
            unconscious: false,
            dead_until_ms: None,
            hidden: false,
            last_attacked_by: None,
            haggle_with: None,
            came_from: None,
            marks: BTreeMap::new(),
            deaths: 0,
        };
        let actor = ActorFacts {
            id: probe.id.clone(),
            name: probe.name.clone(),
            kind: "player".to_owned(),
            room: room.clone(),
            hp: probe.pools.hp,
            hp_max: probe.pools.hp_max,
            condition: "unhurt".to_owned(),
            attrs: BTreeMap::from([
                ("st".to_owned(), t.attrs.st),
                ("dx".to_owned(), t.attrs.dx),
                ("iq".to_owned(), t.attrs.iq),
                ("ht".to_owned(), t.attrs.ht),
            ]),
            skills: self
                .pack
                .skills
                .keys()
                .filter_map(|s| Some((s.to_string(), self.skill_level(&probe, s)?)))
                .collect(),
            inventory: t.inventory.clone(),
            equipped: t.equip.clone(),
            currency: t.currency,
            marks: Some(BTreeMap::new()),
        };
        let mut call = HookCall::new(hook.clone(), room.clone());
        let mut input = self.hook_input(actor, &call, now_ms);
        if let HookId::Take(p) | HookId::Use(p) = hook {
            call.item = None;
            input.item = Some(ItemFacts {
                id: ItemId(0),
                proto: p.clone(),
                name: self
                    .pack
                    .items
                    .get(p)
                    .map_or_else(String::new, |d| d.name.clone()),
            });
        }
        input
    }

    // ------------------------------------------------------------ admission

    /// Checks each effect against the world at game time `now_ms`, before
    /// any is applied. The list is applied whole or not at all, so a mark
    /// or a flag that could not be kept (its table full) declines the whole
    /// list: a reward never pays when its once-per-player guard is lost.
    ///
    /// # Errors
    /// The first effect that cannot apply, and why: a fault, or a decline.
    pub fn check_effects(
        &self,
        room: &RoomId,
        actor: &EntityId,
        effects: &[ScriptEffect],
        now_ms: u64,
    ) -> Result<(), Inadmissible> {
        let def = self.pack.rooms.get(room);
        let mut marks: BTreeSet<&str> = self
            .state
            .entities
            .get(actor)
            .map(|e| e.marks.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let mut flags: BTreeSet<&str> = self.state.flags.keys().map(String::as_str).collect();
        // What the list uses up so far, by prototype, against what the
        // actor carries before the list.
        let mut using: BTreeMap<&ProtoId, u32> = BTreeMap::new();
        let mut rewarded = false;
        // A hurt after a use_up: a death there would take the price and
        // skip any later reward, while a player keeps their gear on death.
        let mut hurt_after_price = false;
        for fx in effects {
            match fx {
                ScriptEffect::Msg(t) | ScriptEffect::Narrate(t) | ScriptEffect::Refuse(t) => {
                    check_text(t)?;
                }
                ScriptEffect::Unlock(d) | ScriptEffect::Lock(d) => {
                    if !def.is_some_and(|r| r.locks.contains_key(d)) {
                        let which = if matches!(fx, ScriptEffect::Unlock(_)) {
                            "unlock"
                        } else {
                            "lock"
                        };
                        return Err(Inadmissible::Fault(format!(
                            "{which}({:?}): no lock on that exit here",
                            d.short()
                        )));
                    }
                }
                ScriptEffect::SetFlag(n, v) | ScriptEffect::Mark(n, v) => {
                    check_name(n)?;
                    if let FlagValue::Text(t) = v
                        && t.chars().count() > FLAG_TEXT_MAX
                    {
                        return Err(Inadmissible::Fault(format!(
                            "the value of {n} is over {FLAG_TEXT_MAX} characters"
                        )));
                    }
                    let (table, cap, what) = if matches!(fx, ScriptEffect::Mark(..)) {
                        (&mut marks, MARK_CAP, "marks")
                    } else {
                        (&mut flags, FLAG_CAP, "flags")
                    };
                    if !table.contains(n.as_str()) && table.len() >= cap {
                        return Err(Inadmissible::Declined(format!(
                            "{what} full ({cap}); {n} would not be kept"
                        )));
                    }
                    table.insert(n);
                }
                ScriptEffect::ClearFlag(n) => {
                    check_name(n)?;
                    flags.remove(n.as_str());
                }
                ScriptEffect::Unmark(n) => {
                    check_name(n)?;
                    marks.remove(n.as_str());
                }
                ScriptEffect::Heal(_) | ScriptEffect::Hurt(_) => {
                    if matches!(fx, ScriptEffect::Hurt(_)) && def.is_some_and(|r| r.safe) {
                        return Err(Inadmissible::Fault(
                            "hurt() is not allowed in a safe room".to_owned(),
                        ));
                    }
                    if !self.state.entities.get(actor).is_some_and(Entity::present) {
                        return Err(Inadmissible::Fault("the actor is not here".to_owned()));
                    }
                    if matches!(fx, ScriptEffect::Hurt(_)) && !using.is_empty() {
                        hurt_after_price = true;
                    }
                }
                ScriptEffect::Reward {
                    pool,
                    currency,
                    items,
                } => {
                    // The proof goes only with its pay.
                    if hurt_after_price {
                        return Err(Inadmissible::Fault(
                            "hurt() comes before use_up()".to_owned(),
                        ));
                    }
                    rewarded = true;
                    self.check_reward(actor, pool, *currency, items, now_ms)?;
                }
                ScriptEffect::UseUp { proto, count } => {
                    self.check_use_up(actor, proto, *count, rewarded, &mut using)?;
                }
            }
        }
        Ok(())
    }

    /// Checks one `reward` against its pool's budget at game time
    /// `now_ms` and the actor. A bad ask is a fault; a spent pool is a
    /// decline.
    fn check_reward(
        &self,
        actor: &EntityId,
        pool: &PoolId,
        currency: i64,
        items: &BTreeMap<ProtoId, u32>,
        now_ms: u64,
    ) -> Result<(), Inadmissible> {
        let def = self.pack.economy.rewards.get(pool).ok_or_else(|| {
            Inadmissible::Fault(format!("reward({pool}): no such pool in economy.toml"))
        })?;
        def.admit(self.state.pool_spent.get(pool), now_ms, currency, items)
            .map_err(|e| match e {
                AskError::Bad(m) => Inadmissible::Fault(format!("reward({pool}): {m}")),
                AskError::Spent(m) => Inadmissible::Declined(format!("reward({pool}): {m}")),
            })?;
        if !self.state.entities.get(actor).is_some_and(Entity::present) {
            return Err(Inadmissible::Fault("the actor is not here".to_owned()));
        }
        Ok(())
    }

    /// Checks one `use_up` against the pack and the actor. A bad ask is a
    /// fault; too few to give is a decline, as a spent pool is. `using`
    /// sums the calls of the list so far, so two calls of one prototype
    /// count together.
    fn check_use_up<'a>(
        &self,
        actor: &EntityId,
        proto: &'a ProtoId,
        count: u32,
        rewarded: bool,
        using: &mut BTreeMap<&'a ProtoId, u32>,
    ) -> Result<(), Inadmissible> {
        let fault = |m: String| Err(Inadmissible::Fault(m));
        if !(1..=USE_UP_MAX).contains(&count) {
            return fault(format!(
                "use_up({proto}): n is a count of 1 to {USE_UP_MAX}"
            ));
        }
        // A reward's items can then never be the price, and the rule
        // reads in list order.
        if rewarded {
            return fault("use_up() comes before reward()".to_owned());
        }
        let Some(def) = self.pack.items.get(proto) else {
            return fault(format!("use_up({proto}): no such item in items.toml"));
        };
        if def.hidden {
            return fault(format!("use_up({proto}): a hidden item"));
        }
        if def.currency.is_some() {
            return fault(format!(
                "use_up({proto}): it holds money; a purse is emptied with use"
            ));
        }
        if !self.state.entities.get(actor).is_some_and(Entity::present) {
            return fault("the actor is not here".to_owned());
        }
        let need = using.entry(proto).or_insert(0);
        *need = need.saturating_add(count);
        let have = u32::try_from(self.use_up_candidates(actor, proto).len()).unwrap_or(u32::MAX);
        if *need > have {
            return Err(Inadmissible::Declined(format!(
                "use_up({proto}): carries {have}, asks {need}"
            )));
        }
        Ok(())
    }

    /// The items of `proto` that `actor` can give up, in the order
    /// `use_up` takes them: the spares first, then the ones in use, each in
    /// inventory order. Never merchandise, a container that holds
    /// something, or an item with money sealed in it. Admission and apply
    /// both ask this, so they agree.
    pub(super) fn use_up_candidates(&self, actor: &EntityId, proto: &ProtoId) -> Vec<ItemId> {
        let Some(e) = self.state.entities.get(actor) else {
            return Vec::new();
        };
        let fits = |i: &&ItemId| {
            self.state.items.get(*i).is_some_and(|it| {
                it.proto == *proto
                    && !it.stock
                    && it.contents.is_empty()
                    && it.sealed_currency(&self.pack).is_none()
            })
        };
        let in_use = |i: &ItemId| e.equipped.values().any(|q| q == i);
        let (used, spare): (Vec<ItemId>, Vec<ItemId>) =
            e.inventory.iter().filter(fits).partition(|i| in_use(i));
        spare.into_iter().chain(used).collect()
    }

    // ------------------------------------------------------------ running

    /// Runs one hook for `actor`. Deterministic and panic-safe: a hook that
    /// faults or panics applies nothing and consumes no dice.
    pub(crate) fn run_hook(&mut self, ctx: &mut Ctx, actor: &EntityId, call: &HookCall) -> HookRun {
        let Some(hooks) = self.hooks.clone() else {
            return HookRun::Absent;
        };
        if !hooks.has(&call.hook) {
            return HookRun::Absent;
        }
        let key = call.hook.key();
        let limits = hooks.limits();
        let faults = self.state.hook_faults.get(&key).copied().unwrap_or(0);
        if !hook_allowed(faults, limits.max_faults) {
            return HookRun::Skipped;
        }
        let Some(facts) = self.actor_facts(actor) else {
            return HookRun::Absent;
        };
        let input = self.hook_input(facts, call, ctx.now);
        let dice = self.dice.clone();
        let reply =
            std::panic::catch_unwind(AssertUnwindSafe(|| hooks.call(&call.hook, &input, dice)))
                .unwrap_or_else(|_| HookReply {
                    outcome: Err("the hook panicked".to_owned()),
                    log: Vec::new(),
                });
        for line in reply.log {
            self.audit.push(Audit::ScriptLog {
                hook: key.clone(),
                entity: actor.clone(),
                line,
                at_ms: ctx.now,
            });
        }
        let checked = reply
            .outcome
            .map_err(Inadmissible::Fault)
            .and_then(|(fx, dice)| {
                let t = tally(&fx);
                if !tally_admissible(t, limits.max_effects, call.hook.is_command()) {
                    return Err(Inadmissible::Fault(tally_reason(
                        t,
                        limits.max_effects,
                        call.hook.is_command(),
                    )));
                }
                self.check_effects(&call.room, actor, &fx, ctx.now)?;
                Ok((fx, dice))
            });
        match checked {
            Ok((fx, dice)) => {
                self.dice = dice;
                let (next, _) = hook_step(faults, HookOutcome::Ran, limits.max_faults);
                self.store_faults(&key, next);
                self.audit.push(Audit::ScriptEffects {
                    hook: key.clone(),
                    entity: actor.clone(),
                    effects: fx.iter().map(ScriptEffect::describe).collect(),
                    at_ms: ctx.now,
                });
                self.apply_effects(ctx, actor, call, &key, fx)
            }
            Err(Inadmissible::Declined(message)) => {
                // Not the script's fault: nothing applies, the dice are
                // untouched, and the breaker stays where it was.
                let (next, _) = hook_step(faults, HookOutcome::Declined, limits.max_faults);
                self.store_faults(&key, next);
                self.script_log(ctx, &key, actor, format!("declined: {}", capped(&message)));
                HookRun::Declined
            }
            Err(Inadmissible::Fault(message)) => {
                let (next, allowed) = hook_step(faults, HookOutcome::Faulted, limits.max_faults);
                self.store_faults(&key, next);
                self.audit.push(Audit::ScriptFault {
                    hook: key,
                    entity: actor.clone(),
                    message: capped(&message),
                    faults: next,
                    tripped: !allowed,
                    at_ms: ctx.now,
                });
                HookRun::Faulted
            }
        }
    }

    fn store_faults(&mut self, key: &str, n: u8) {
        if n == 0 {
            self.state.hook_faults.remove(key);
        } else {
            self.state.hook_faults.insert(key.to_owned(), n);
        }
    }

    fn script_log(&mut self, ctx: &Ctx, key: &str, actor: &EntityId, line: String) {
        self.audit.push(Audit::ScriptLog {
            hook: key.to_owned(),
            entity: actor.clone(),
            line,
            at_ms: ctx.now,
        });
    }

    pub(super) fn apply_effects(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        call: &HookCall,
        key: &str,
        effects: Vec<ScriptEffect>,
    ) -> HookRun {
        let mut msgs = Vec::new();
        let mut refused = None;
        let mut died = false;
        // A use_up that found too little: no later reward pays without
        // its price.
        let mut short = false;
        for fx in effects {
            if died && !matches!(fx, ScriptEffect::Msg(_)) {
                let line = format!("skipped {}: the actor died", fx.describe());
                self.script_log(ctx, key, actor, line);
                continue;
            }
            match fx {
                ScriptEffect::Msg(t) => msgs.push(t.trim().to_owned()),
                ScriptEffect::Refuse(t) => refused = Some(t.trim().to_owned()),
                ScriptEffect::Narrate(t) => {
                    let spec = Emit {
                        except: Some(actor.clone()),
                        ..Emit::new(EventKind::System, Some(call.room.clone()), t.trim())
                            .tags(["script".to_owned(), key.to_owned()])
                    };
                    self.emit(ctx, spec);
                }
                ScriptEffect::Unlock(d) => self.set_exit_lock(ctx, &call.room, d, false),
                ScriptEffect::Lock(d) => self.set_exit_lock(ctx, &call.room, d, true),
                ScriptEffect::SetFlag(n, v) => {
                    if self.state.flags.len() >= FLAG_CAP && !self.state.flags.contains_key(&n) {
                        self.script_log(
                            ctx,
                            key,
                            actor,
                            format!("flags full ({FLAG_CAP}); {n} skipped"),
                        );
                    } else {
                        self.state.flags.insert(n, v);
                    }
                }
                ScriptEffect::ClearFlag(n) => {
                    self.state.flags.remove(&n);
                }
                ScriptEffect::Mark(n, v) => {
                    if let Some(e) = self.state.entities.get_mut(actor) {
                        if e.marks.len() >= MARK_CAP && !e.marks.contains_key(&n) {
                            self.script_log(
                                ctx,
                                key,
                                actor,
                                format!("marks full ({MARK_CAP}); {n} skipped"),
                            );
                        } else {
                            e.marks.insert(n, v);
                        }
                    }
                }
                ScriptEffect::Unmark(n) => {
                    if let Some(e) = self.state.entities.get_mut(actor) {
                        e.marks.remove(&n);
                    }
                }
                ScriptEffect::Heal(dice) => self.heal_actor(ctx, actor, dice),
                ScriptEffect::Hurt(dice) => died = self.hurt_actor(ctx, actor, dice),
                ScriptEffect::UseUp { proto, count } => {
                    if let Some(line) = self.use_up(ctx, actor, call, key, &proto, count) {
                        msgs.push(line);
                    } else {
                        short = true;
                    }
                }
                ScriptEffect::Reward { pool, .. } if short => {
                    let line = format!("skipped reward({pool}): its price was not paid");
                    self.script_log(ctx, key, actor, line);
                }
                ScriptEffect::Reward {
                    pool,
                    currency,
                    items,
                } => {
                    let mint = Mint {
                        room: &call.room,
                        key,
                        pool: &pool,
                        currency,
                        items: &items,
                    };
                    if let Some(line) = self.mint(ctx, actor, &mint) {
                        msgs.push(line);
                    }
                }
            }
        }
        HookRun::Applied { msgs, refused }
    }

    /// Uses up `count` items of `proto` that the actor carries: each leaves
    /// the world, unequipped first if in use, and an event tells the
    /// actor. Returns the line for the actor, or `None` (logged) when the
    /// actor no longer carries enough, and then takes nothing.
    fn use_up(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        call: &HookCall,
        key: &str,
        proto: &ProtoId,
        count: u32,
    ) -> Option<String> {
        let ids = self.use_up_candidates(actor, proto);
        let n = usize::try_from(count).unwrap_or(usize::MAX);
        if ids.len() < n {
            let line = format!("skipped use_up({proto}): the actor no longer carries it");
            self.script_log(ctx, key, actor, line);
            return None;
        }
        let mut used = Vec::new();
        for id in ids.into_iter().take(n) {
            let slot = self
                .state
                .entities
                .get(actor)
                .and_then(|e| e.equipped.iter().find(|(_, i)| **i == id).map(|(s, _)| *s));
            // detach clears the slot without a word, so the change that
            // says so comes first.
            if let Some(slot) = slot {
                ctx.changes.push(StateChange::Equipped {
                    entity: actor.clone(),
                    slot,
                    item: None,
                });
            }
            self.destroy_item(ctx, id);
            used.push(json!({ "id": id, "proto": proto }));
        }
        let line = format!(
            "You give up {}.",
            self.item_phrase(&BTreeMap::from([(proto.clone(), count)]))
        );
        let spec = Emit {
            only: Some(vec![actor.clone()]),
            ..Emit::new(EventKind::System, Some(call.room.clone()), line.clone())
                .actor(actor, "use_up")
                .tags(["script".to_owned(), key.to_owned(), "use_up".to_owned()])
                .data(json!({ "proto": proto, "items": used }))
        };
        self.emit(ctx, spec);
        Some(line)
    }

    /// Mints an admitted reward for the actor: the pool's spent count
    /// moves on, the money and the new items go to the actor, an event
    /// tells the actor, and the ledger records it. Returns the line for
    /// the actor. The ask was checked before any effect applied; it is
    /// checked again here, and skipped (logged) if it no longer fits.
    fn mint(&mut self, ctx: &mut Ctx, actor: &EntityId, m: &Mint<'_>) -> Option<String> {
        let pack = Arc::clone(&self.pack);
        let def = pack.economy.rewards.get(m.pool)?;
        let admitted = def.admit(
            self.state.pool_spent.get(m.pool),
            ctx.now,
            m.currency,
            m.items,
        );
        let present = self.state.entities.get(actor).is_some_and(Entity::present);
        let spent = match admitted {
            Ok(s) if present => s,
            Ok(_) => {
                let line = format!("skipped reward({}): the actor is not here", m.pool);
                self.script_log(ctx, m.key, actor, line);
                return None;
            }
            Err(e) => {
                let line = format!("skipped reward({}): {e}", m.pool);
                self.script_log(ctx, m.key, actor, line);
                return None;
            }
        };
        let period = spent.period;
        self.state.pool_spent.insert(m.pool.clone(), spent);
        if m.currency > 0 {
            self.change_currency(ctx, actor, m.currency);
        }
        let to = Holder::Entity {
            entity: actor.clone(),
        };
        let mut made = Vec::new();
        for (proto, n) in m.items {
            for _ in 0..*n {
                let id = self.state.new_item(proto, &pack);
                self.state.attach(id, &to);
                ctx.changes.push(StateChange::Item {
                    item: id,
                    proto: proto.clone(),
                    from: None,
                    to: Some(to.clone()),
                });
                made.push(MintedItem {
                    id,
                    proto: proto.clone(),
                });
            }
        }
        let line = self.reward_line(m.currency, m.items);
        let spec = Emit {
            only: Some(vec![actor.clone()]),
            ..Emit::new(EventKind::System, Some(m.room.clone()), line.clone())
                .actor(actor, "reward")
                .tags(["script".to_owned(), m.key.to_owned(), "reward".to_owned()])
                .data(json!({
                    "pool": m.pool,
                    "currency": m.currency,
                    "items": made,
                }))
        };
        let seq = self.emit(ctx, spec);
        let record = MintRecord {
            seq,
            pool: m.pool.clone(),
            period,
            hook: m.key.to_owned(),
            entity: actor.clone(),
            currency: m.currency,
            items: made,
            at_ms: ctx.now,
        };
        self.record_mint(ctx, record);
        Some(line)
    }

    /// The line a reward tells its receiver, such as `You receive 20s and
    /// a healing draught.` Server text: pack names and numbers only.
    pub(super) fn reward_line(&self, currency: i64, items: &BTreeMap<ProtoId, u32>) -> String {
        let mut parts = Vec::new();
        if currency > 0 {
            parts.push(format!("{currency}{}", self.pack.economy.currency.short));
        }
        parts.extend(self.item_parts(items));
        format!("You receive {}.", join_list(&parts))
    }

    /// Items by prototype as a reader says them, such as `a notched
    /// grave-blade` or `3 x iron ore`, joined with `, ` and ` and `.
    /// Server text: pack names and numbers only.
    pub(super) fn item_phrase(&self, items: &BTreeMap<ProtoId, u32>) -> String {
        join_list(&self.item_parts(items))
    }

    fn item_parts(&self, items: &BTreeMap<ProtoId, u32>) -> Vec<String> {
        let mut parts = Vec::new();
        for (proto, n) in items.iter().filter(|(_, n)| **n > 0) {
            let name = self
                .pack
                .items
                .get(proto)
                .map_or_else(|| proto.to_string(), |d| d.name.clone());
            let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
                "an"
            } else {
                "a"
            };
            parts.push(if *n == 1 {
                format!("{article} {name}")
            } else {
                format!("{n} x {name}")
            });
        }
        parts
    }

    /// Locks or unlocks an exit and its far side.
    pub(super) fn set_exit_lock(
        &mut self,
        ctx: &mut Ctx,
        room: &RoomId,
        dir: Direction,
        locked: bool,
    ) {
        let far = self
            .pack
            .rooms
            .get(room)
            .and_then(|d| d.exits.get(&dir))
            .cloned();
        let mut sides = vec![(room.clone(), dir)];
        if let Some(to) = far {
            sides.push((to, dir.opposite()));
        }
        for (r, d) in sides {
            if let Some(rs) = self.state.rooms.get_mut(&r) {
                if locked {
                    rs.unlocked.remove(&d);
                    rs.unlocked_at.remove(&d);
                } else {
                    rs.unlocked.insert(d);
                    rs.unlocked_at.insert(d, ctx.now);
                }
            }
        }
        ctx.changes.push(if locked {
            StateChange::Locked {
                room: room.clone(),
                dir,
            }
        } else {
            StateChange::Unlocked {
                room: room.clone(),
                dir,
            }
        });
    }

    /// Heals the actor, up to its maximum HP. A heal that lifts one out
    /// cold above 0 HP brings it to ([`World::come_to`]). A hook runs only
    /// for an actor that can act, so through a hook this never meets one
    /// out cold; the call keeps every heal on the one come-to path.
    pub(super) fn heal_actor(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        dice: mud_rules::DamageDice,
    ) {
        let roll = dice.roll(&mut self.dice, DamageType::Crushing).max(0);
        let Some(e) = self.state.entities.get_mut(actor) else {
            return;
        };
        let amount = roll.min((e.pools.hp_max - e.pools.hp).max(0));
        e.pools.hp += amount;
        let hp = e.pools.hp;
        ctx.changes.push(StateChange::Hp {
            entity: actor.clone(),
            delta: amount,
            hp,
        });
        if hp > 0 {
            self.come_to(ctx, actor);
        }
    }

    /// Hurts the actor. Returns whether it died of it.
    fn hurt_actor(&mut self, ctx: &mut Ctx, actor: &EntityId, dice: mud_rules::DamageDice) -> bool {
        let roll = dice.roll(&mut self.dice, DamageType::Crushing);
        let Some(e) = self.state.entities.get(actor).cloned() else {
            return false;
        };
        let n = injury(roll, self.armor_dr(&e), DamageType::Crushing);
        if n <= 0 {
            return false;
        }
        let before = ctx.changes.len();
        self.wound(ctx, actor, n, actor);
        ctx.changes[before..].iter().any(|c| {
            matches!(c, StateChange::Status { entity, status, value: true } if entity == actor && status == "dead")
                || matches!(c, StateChange::Moved { entity, .. } if entity == actor)
        })
    }

    // ------------------------------------------------------------ hook points

    /// A custom verb: the hook decides the command.
    pub(crate) fn custom_verb(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        name: &str,
        args: &str,
    ) -> Done {
        let info = self
            .hooks
            .as_ref()
            .and_then(|h| h.verb(name))
            .ok_or_else(|| {
                Fail::new(
                    ErrorCode::UnknownVerb,
                    format!("unknown verb {name:?}; type `help`"),
                )
            })?;
        let room = self.me(actor)?.room.clone();
        if !info.works_in(&room) {
            return Err(Fail::new(
                ErrorCode::NotHere,
                format!("you cannot {name} here"),
            ));
        }
        let mut call = HookCall::new(HookId::Verb(name.to_owned()), room);
        call.text = Some(args.to_owned());
        let word = args.trim().to_ascii_lowercase();
        if !word.is_empty() {
            call.target = self.find_here(actor, &word);
        }
        self.command_hook(ctx, actor, &call)
    }

    /// `use` of an item that has no engine use, when a script handles it.
    /// `None` when no script does.
    pub(crate) fn use_hook(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        item: ItemId,
        on: Option<&str>,
    ) -> Option<Done> {
        let proto = self.item_proto(item)?;
        let hook = HookId::Use(proto);
        if !self.hooks.as_ref().is_some_and(|h| h.has(&hook)) {
            return None;
        }
        let room = self.me(actor).ok()?.room.clone();
        let mut call = HookCall::new(hook, room);
        call.item = Some(item);
        if let Some(w) = on {
            match self.find_here(actor, w) {
                Some(t) => call.target = Some(t),
                None => {
                    return Some(Err(Fail::new(
                        ErrorCode::NoSuchTarget,
                        format!("you see no {w:?} here"),
                    )));
                }
            }
        }
        Some(self.command_hook(ctx, actor, &call))
    }

    fn command_hook(&mut self, ctx: &mut Ctx, actor: &EntityId, call: &HookCall) -> Done {
        let key = call.hook.key();
        match self.run_hook(ctx, actor, call) {
            HookRun::Applied {
                refused: Some(text),
                ..
            } => Err(Fail::new(ErrorCode::Refused, text)),
            HookRun::Applied { msgs, .. } => {
                let narrative = if msgs.is_empty() {
                    "Nothing happens.".to_owned()
                } else {
                    msgs.join("\n")
                };
                Ok((narrative, json!({ "script": key })))
            }
            HookRun::Declined => Err(Fail::new(ErrorCode::Refused, DECLINED)),
            HookRun::Absent | HookRun::Skipped | HookRun::Faulted => {
                Err(Fail::new(ErrorCode::ScriptFailed, "Nothing happens."))
            }
        }
    }

    /// The event hook of a command that succeeded, if any: entering a
    /// room, speaking, or taking an item. Returns the lines for the actor.
    /// Its faults never fail the command.
    pub(crate) fn event_hook(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        cmd: &Command,
        resolution: &Value,
    ) -> Vec<String> {
        if self.hooks.is_none() {
            return Vec::new();
        }
        let Some(me) = self.state.entities.get(actor).filter(|e| e.can_act()) else {
            return Vec::new();
        };
        let here = me.room.clone();
        let call = match cmd {
            Command::Move { .. } | Command::Flee { .. } => {
                // The walk itself; a death after it moves the actor again.
                let walked = ctx.changes.iter().find_map(|c| match c {
                    StateChange::Moved { entity, from, to } if entity == actor => {
                        Some((from.clone(), to.clone()))
                    }
                    _ => None,
                });
                // A hidden mover sets off nothing, as it emits no arrival.
                let Some((from, to)) = walked.filter(|(_, to)| *to == here && !me.hidden) else {
                    return Vec::new();
                };
                let mut call = HookCall::new(HookId::Enter(to.clone()), to);
                call.from = Some(from);
                call
            }
            Command::Say { text } => {
                let mut call = HookCall::new(HookId::Speech(here.clone()), here);
                call.text = Some(text.clone());
                call
            }
            Command::Take { .. } => {
                let Some(item) = resolution["item"].as_u64().map(ItemId) else {
                    return Vec::new();
                };
                let Some(proto) = self.item_proto(item) else {
                    return Vec::new();
                };
                let mut call = HookCall::new(HookId::Take(proto), here);
                call.item = Some(item);
                call
            }
            _ => return Vec::new(),
        };
        match self.run_hook(ctx, actor, &call) {
            HookRun::Applied { msgs, .. } => msgs,
            HookRun::Absent | HookRun::Skipped | HookRun::Faulted | HookRun::Declined => Vec::new(),
        }
    }
}

const fn condition(e: &Entity) -> &'static str {
    if e.unconscious {
        "unconscious"
    } else {
        wound_word(e.pools.wound_state())
    }
}

/// `a`, `a and b`, `a, b and c`.
fn join_list(parts: &[String]) -> String {
    match parts.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => parts.concat(),
    }
}

fn check_text(t: &str) -> Result<(), String> {
    let t = t.trim();
    if t.is_empty() {
        return Err("text is empty".to_owned());
    }
    if t.chars().count() > EFFECT_TEXT_MAX {
        return Err(format!("text is over {EFFECT_TEXT_MAX} characters"));
    }
    if t.chars().any(forbidden_in_text) {
        return Err("text has a control or direction character".to_owned());
    }
    Ok(())
}

fn check_name(n: &str) -> Result<(), String> {
    if valid_flag_name(n) {
        Ok(())
    } else {
        Err(format!(
            "{n:?} is not a flag name (lowercase words of a-z, 0-9 and _, joined by . : or -)"
        ))
    }
}
