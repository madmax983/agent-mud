//! Items and the economy: take, drop, give, buy, sell, list, equip, use.

use std::fmt::Write as _;

use serde_json::{Value, json};

use super::{Ctx, Done, Emit, ErrorCode, Fail, StateChange, World};
use crate::event::EventKind;
use crate::ids::{EntityId, ItemId, ShopId};
use crate::pack::{Direction, ShopDef};
use crate::rewards::MintRecord;
use crate::state::Holder;

impl World {
    fn room_items(&self, viewer: &EntityId) -> Vec<ItemId> {
        self.state
            .entities
            .get(viewer)
            .and_then(|e| self.state.rooms.get(&e.room))
            .map(|r| r.items.clone())
            .unwrap_or_default()
    }

    fn carried(&self, viewer: &EntityId, word: &str) -> Result<ItemId, Fail> {
        let inv = self.me(viewer)?.inventory.clone();
        self.find_item(&inv, word)
            .ok_or_else(|| Fail::new(ErrorCode::NoSuchItem, format!("you carry no {word:?}")))
    }

    fn item_tags(&self, item: ItemId) -> Vec<String> {
        self.state
            .items
            .get(&item)
            .and_then(|it| self.pack.items.get(&it.proto))
            .map(|d| d.tags.clone())
            .unwrap_or_default()
    }

    pub(crate) fn take(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        word: &str,
        from: Option<&str>,
    ) -> Done {
        let me = self.me(actor)?.clone();
        if !self.lit(&me.room) {
            return Err(Fail::new(
                ErrorCode::NoSuchItem,
                "you grope in the dark and find nothing",
            ));
        }
        let floor = self.room_items(actor);
        let (item, source) = if let Some(c) = from {
            let pool: Vec<ItemId> = floor.iter().chain(&me.inventory).copied().collect();
            let container = self.find_item(&pool, c).ok_or_else(|| {
                Fail::new(ErrorCode::NoSuchItem, format!("you see no {c:?} here"))
            })?;
            let it = &self.state.items[&container];
            if it.locked {
                return Err(Fail::new(
                    ErrorCode::ContainerLocked,
                    format!("the {} is locked", self.item_name(container)),
                ));
            }
            let inner = self.find_item(&it.contents.clone(), word).ok_or_else(|| {
                Fail::new(ErrorCode::NoSuchItem, format!("there is no {word:?} in it"))
            })?;
            (inner, Some(container))
        } else {
            let item = self.find_item(&floor, word).ok_or_else(|| {
                Fail::new(ErrorCode::NoSuchItem, format!("you see no {word:?} here"))
            })?;
            (item, None)
        };
        let proto = self.item_proto(item);
        if proto
            .as_ref()
            .and_then(|p| self.pack.items.get(p))
            .is_some_and(|d| d.fixed)
        {
            return Err(Fail::new(ErrorCode::CannotTake, "it will not budge"));
        }
        if self.state.items.get(&item).is_some_and(|i| i.stock) {
            return Err(Fail::new(
                ErrorCode::PolicyTradeRequired,
                "that is merchandise; it leaves the shop only through a sale (list, buy)",
            ));
        }
        self.relocate_item(
            ctx,
            item,
            Holder::Entity {
                entity: actor.clone(),
            },
        );
        let name = self.item_name(item);
        let mut tags = self.item_tags(item);
        if source.is_some() {
            tags.push("loot".to_owned());
        }
        let spec = Emit::new(
            EventKind::ItemTake,
            Some(me.room.clone()),
            format!("{} picks up the {name}.", me.name),
        )
        .actor(actor, "take")
        .tags(tags)
        .data(json!({ "item": { "id": item, "name": name } }));
        self.emit(ctx, spec);
        Ok((
            format!("You take the {name}."),
            json!({ "item": item, "name": name }),
        ))
    }

    pub(crate) fn drop_item(&mut self, ctx: &mut Ctx, actor: &EntityId, word: &str) -> Done {
        let item = self.carried(actor, word)?;
        let me = self.me(actor)?.clone();
        if self.state.items.get(&item).is_some_and(|i| i.stock) && me.shop.is_some() {
            return Err(Fail::new(
                ErrorCode::PolicyTradeRequired,
                "that is merchandise; it leaves the shop only through a sale",
            ));
        }
        self.relocate_item(
            ctx,
            item,
            Holder::Room {
                room: me.room.clone(),
            },
        );
        let name = self.item_name(item);
        let spec = Emit::new(
            EventKind::ItemDrop,
            Some(me.room.clone()),
            format!("{} drops the {name}.", me.name),
        )
        .actor(actor, "drop")
        .tags(self.item_tags(item))
        .data(json!({ "item": { "id": item, "name": name } }));
        self.emit(ctx, spec);
        Ok((format!("You drop the {name}."), json!({ "item": item })))
    }

    /// Reads an amount such as `10`, `10s` or `10 silver`.
    fn amount(&self, what: &str) -> Option<i64> {
        let cur = &self.pack.economy.currency;
        let t = what.trim().to_ascii_lowercase();
        let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let rest = t[digits.len()..].trim();
        let unit_ok = rest.is_empty()
            || rest == cur.short.to_ascii_lowercase()
            || rest == cur.name.to_ascii_lowercase()
            || rest == "coins"
            || rest == "coin";
        if unit_ok { digits.parse().ok() } else { None }
    }

    pub(crate) fn give(&mut self, ctx: &mut Ctx, actor: &EntityId, what: &str, to: &str) -> Done {
        let me = self.me(actor)?.clone();
        let target = self
            .find_here(actor, to)
            .filter(|t| t != actor)
            .ok_or_else(|| Fail::new(ErrorCode::NoSuchTarget, format!("you see no {to:?} here")))?;
        let tname = self.state.entities[&target].name.clone();
        let cur = self.pack.economy.currency.clone();
        if let Some(n) = self.amount(what) {
            if n <= 0 {
                return Err(Fail::new(ErrorCode::BadArgs, "give a positive amount"));
            }
            if me.currency < n {
                return Err(Fail::new(
                    ErrorCode::InsufficientFunds,
                    format!("you have only {}{}", me.currency, cur.short),
                ));
            }
            self.change_currency(ctx, actor, -n);
            self.change_currency(ctx, &target, n);
            let spec = Emit::new(
                EventKind::Give,
                Some(me.room.clone()),
                format!("{} gives {n}{} to {tname}.", me.name, cur.short),
            )
            .actor(actor, "give")
            .target(&target)
            .tags(["coin".to_owned()])
            .data(json!({ "amount": n }));
            self.emit(ctx, spec);
            return Ok((
                format!("You give {n}{} to {tname}.", cur.short),
                json!({ "amount": n, "to": target }),
            ));
        }
        let item = self.carried(actor, what)?;
        if me.shop.is_some() && self.state.items.get(&item).is_some_and(|i| i.stock) {
            return Err(Fail::new(
                ErrorCode::PolicyTradeRequired,
                "that is merchandise; it leaves the shop only through a completed sale",
            ));
        }
        self.relocate_item(
            ctx,
            item,
            Holder::Entity {
                entity: target.clone(),
            },
        );
        let name = self.item_name(item);
        let spec = Emit::new(
            EventKind::Give,
            Some(me.room.clone()),
            format!("{} gives the {name} to {tname}.", me.name),
        )
        .actor(actor, "give")
        .target(&target)
        .tags(self.item_tags(item))
        .data(json!({ "item": { "id": item, "name": name } }));
        self.emit(ctx, spec);
        Ok((
            format!("You give the {name} to {tname}."),
            json!({ "item": item, "to": target }),
        ))
    }

    /// The shop `actor` trades with here: the one named `word` (its
    /// keeper or its fixture), or the only one here.
    ///
    /// A shop with a fixture trades in its shop room, while its keeper
    /// can act there; an `unattended` one also sells (`selling`) with
    /// nobody minding it. A shop with no fixture trades wherever its
    /// keeper stands and can act. Nobody trades with its own shop.
    fn shop_here(
        &self,
        actor: &EntityId,
        word: Option<&str>,
        selling: bool,
    ) -> Result<ShopHere, Fail> {
        let me = self.me(actor)?;
        let mut open = Vec::new();
        let mut closed = None;
        for (sid, def) in &self.pack.economy.shops {
            if &def.keeper == actor {
                continue;
            }
            let keeper = self.state.entities.get(&def.keeper);
            let minded = keeper.is_some_and(|k| k.can_act() && k.room == me.room);
            let fixture = self.state.shop_fixtures.get(sid).copied();
            let named = |w: &str| {
                keeper.is_some_and(|k| k.matches(w))
                    || fixture.is_some_and(|f| self.find_item(&[f], w).is_some())
            };
            if word.is_some_and(|w| !named(w)) {
                continue;
            }
            let here = match fixture {
                Some(_) => self.pack.shop_room(sid) == Some(&me.room),
                None => minded,
            };
            if !here {
                continue;
            }
            let shop = ShopHere {
                id: sid.clone(),
                def: def.clone(),
                keeper: def.keeper.clone(),
                fixture,
                minded,
            };
            if minded || (selling && def.unattended) {
                open.push(shop);
            } else {
                closed = Some(shop);
            }
        }
        match open.len() {
            1 => Ok(open.remove(0)),
            0 => Err(closed.map_or_else(
                || Fail::new(ErrorCode::NoKeeper, "nobody here is selling"),
                |c| {
                    let what = c.fixture.map(|f| self.item_name(f)).unwrap_or_default();
                    Fail::new(
                        ErrorCode::NoKeeper,
                        format!("nobody is minding the {what} right now"),
                    )
                },
            )),
            _ => Err(Fail::new(ErrorCode::BadArgs, "name the shopkeeper")),
        }
    }

    /// The stock of a shop: what its fixture holds as stock, or, with no
    /// fixture, what its keeper carries as stock.
    pub(crate) fn shop_stock(&self, shop: &ShopId) -> Vec<ItemId> {
        let held = self.state.shop_fixtures.get(shop).map_or_else(
            || {
                self.pack
                    .economy
                    .shops
                    .get(shop)
                    .and_then(|d| self.state.entities.get(&d.keeper))
                    .map(|k| k.inventory.clone())
                    .unwrap_or_default()
            },
            |f| {
                self.state
                    .items
                    .get(f)
                    .map(|i| i.contents.clone())
                    .unwrap_or_default()
            },
        );
        held.into_iter()
            .filter(|i| self.state.items.get(i).is_some_and(|it| it.stock))
            .collect()
    }

    /// What `buyer` pays for `item`: its value at the markup, less a
    /// haggle, but never less than what the keeper paid for it, so selling
    /// and buying back cannot drain a keeper.
    fn sale_price(&self, shop: &ShopDef, item: ItemId, buyer: &EntityId, keeper: &EntityId) -> i64 {
        let haggled = self
            .state
            .entities
            .get(buyer)
            .is_some_and(|b| b.haggle_with.as_ref() == Some(keeper));
        self.price_of(shop, item, haggled)
    }

    /// What `item` sells for in `shop`: its value with the markup, a tenth
    /// off after a won haggle, never under what the keeper paid, at least 1.
    pub(crate) fn price_of(&self, shop: &ShopDef, item: ItemId, haggled: bool) -> i64 {
        let it = self.state.items.get(&item);
        let value = it
            .and_then(|it| self.pack.items.get(&it.proto))
            .map_or(0, |d| d.value);
        let paid = it.and_then(|it| it.paid).unwrap_or(0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "prices are small"
        )]
        let mut price = ((value as f64) * shop.sell_markup).ceil() as i64;
        if haggled {
            price = price * 9 / 10;
        }
        price.max(paid).max(1)
    }

    /// A keeper's own price list, for its sheet: what it sells and for
    /// how much (the same prices `list` and `buy` use), and what it buys.
    /// `None` for an entity that keeps no shop.
    pub(crate) fn shop_view(&self, keeper: &crate::state::Entity) -> Option<Value> {
        let sid = keeper.shop.as_ref()?;
        let shop = self.pack.economy.shops.get(sid)?;
        let mut sells: std::collections::BTreeMap<String, (i64, u32)> =
            std::collections::BTreeMap::new();
        for i in self.shop_stock(sid) {
            let price = self.price_of(shop, i, false);
            sells.entry(self.item_name(i)).or_insert((price, 0)).1 += 1;
        }
        let cur = &self.pack.economy.currency;
        let counter = self.state.shop_fixtures.get(sid).map(|f| {
            let room = self.pack.shop_room(sid);
            json!({
                "fixture": self.item_name(*f),
                "room": room,
                "room_title": room.and_then(|r| self.pack.rooms.get(r)).map(|r| r.title.clone()),
                "unattended": shop.unattended,
            })
        });
        Some(json!({
            "counter": counter,
            "currency": cur.name,
            "short": cur.short,
            "sells": sells
                .iter()
                .map(|(name, (price, n))| json!({ "name": name, "price": price, "count": n }))
                .collect::<Vec<_>>(),
            "buys": shop.buys,
            "buy_rate": shop.buy_rate,
            "pays": shop.prices,
        }))
    }

    fn buy_price(&self, shop: &ShopDef, item: ItemId) -> Option<i64> {
        let it = self.state.items.get(&item)?;
        let def = self.pack.items.get(&it.proto)?;
        if let Some(p) = shop.prices.get(&it.proto) {
            return Some(*p);
        }
        let wanted = shop
            .buys
            .iter()
            .any(|b| b == it.proto.as_str() || def.tags.contains(b));
        if !wanted || def.value <= 0 {
            return None;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "prices are small"
        )]
        let p = ((def.value as f64) * shop.buy_rate).floor() as i64;
        Some(p.max(1))
    }

    pub(crate) fn list(&self, actor: &EntityId, keeper: Option<&str>) -> Done {
        let here = self.shop_here(actor, keeper, true)?;
        let (kid, shop) = (&here.keeper, &here.def);
        let cur = &self.pack.economy.currency;
        let mut seen = std::collections::BTreeMap::new();
        for i in self.shop_stock(&here.id) {
            let name = self.item_name(i);
            let price = self.sale_price(shop, i, actor, kid);
            seen.entry(name).or_insert((price, 0)).1 += 1;
        }
        let mut out = format!("{} sells:", self.seller_name(&here));
        if seen.is_empty() {
            out.push_str(" nothing right now.");
        }
        let mut rows = Vec::new();
        for (name, (price, n)) in &seen {
            let _ = write!(out, "\n  {name} - {price}{}", cur.short);
            if *n > 1 {
                let _ = write!(out, " (x{n})");
            }
            rows.push(json!({ "name": name, "price": price, "count": n }));
        }
        if !shop.buys.is_empty() {
            let _ = write!(out, "\nBuys: {}.", shop.buys.join(", "));
        }
        if !here.minded {
            let _ = write!(
                out,
                "\nNobody minds it: leave the price and take what you buy."
            );
        }
        Ok((
            out,
            json!({ "keeper": kid, "wares": rows, "buys": shop.buys, "minded": here.minded }),
        ))
    }

    pub(crate) fn buy(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        word: &str,
        from: Option<&str>,
    ) -> Done {
        let here = self.shop_here(actor, from, true)?;
        let (kid, shop) = (here.keeper.clone(), here.def.clone());
        let stock = self.shop_stock(&here.id);
        let kname = self.seller_name(&here);
        let item = self.find_item(&stock, word).ok_or_else(|| {
            Fail::new(
                ErrorCode::NotForSale,
                format!("{kname} has no {word:?} for sale"),
            )
        })?;
        let price = self.sale_price(&shop, item, actor, &kid);
        let me = self.me(actor)?.clone();
        let cur = self.pack.economy.currency.clone();
        if me.currency < price {
            return Err(Fail::new(
                ErrorCode::InsufficientFunds,
                format!(
                    "that costs {price}{}; you have {}{}",
                    cur.short, me.currency, cur.short
                ),
            ));
        }
        self.change_currency(ctx, actor, -price);
        self.change_currency(ctx, &kid, price);
        if let Some(it) = self.state.items.get_mut(&item) {
            it.stock = false;
            it.paid = None;
        }
        self.relocate_item(
            ctx,
            item,
            Holder::Entity {
                entity: actor.clone(),
            },
        );
        if let Some(b) = self.state.entities.get_mut(actor)
            && b.haggle_with.as_ref() == Some(&kid)
        {
            b.haggle_with = None;
        }
        let name = self.item_name(item);
        let mut spec = Emit::new(
            EventKind::Trade,
            Some(me.room.clone()),
            if here.minded {
                format!(
                    "{} buys the {name} from {kname} for {price}{}.",
                    me.name, cur.short
                )
            } else {
                format!(
                    "{} takes the {name} from the {kname} and leaves {price}{} for it.",
                    me.name, cur.short
                )
            },
        )
        .actor(actor, "buy")
        .tags(["trade".to_owned(), "coin".to_owned()]);
        if here.minded {
            spec = spec.target(&kid);
        }
        let spec = spec.data(json!({ "item": { "id": item, "name": name }, "price": price }));
        self.emit(ctx, spec);
        Ok((
            format!("You buy the {name} for {price}{}.", cur.short),
            json!({ "item": item, "price": price, "from": kid }),
        ))
    }

    pub(crate) fn sell(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        word: &str,
        to: Option<&str>,
    ) -> Done {
        let item = self.carried(actor, word)?;
        let here = self.shop_here(actor, to, false)?;
        let (kid, shop) = (here.keeper.clone(), here.def.clone());
        let kname = self.seller_name(&here);
        let name = self.item_name(item);
        let price = self.buy_price(&shop, item).ok_or_else(|| {
            Fail::new(ErrorCode::NotBuying, format!("{kname} does not buy {name}"))
        })?;
        let cur = self.pack.economy.currency.clone();
        // The shop's buy pool pays while it has the price left today; the
        // keeper's purse pays the rest of the time. Goods that hold
        // something (contents, or sealed coins) never go to the pool, so
        // using them up never loses an item or a coin.
        let empty =
            self.state.items.get(&item).is_some_and(|it| {
                it.contents.is_empty() && it.sealed_currency(&self.pack).is_none()
            });
        let pooled = shop.buy_pool.as_ref().filter(|_| empty).and_then(|p| {
            let def = self.pack.economy.rewards.get(p)?;
            let none = std::collections::BTreeMap::new();
            def.admit(self.state.pool_spent.get(p), ctx.now, price, &none)
                .ok()
                .map(|spent| (p.clone(), spent))
        });
        if pooled.is_none() && self.state.entities[&kid].currency < price {
            return Err(Fail::new(
                ErrorCode::KeeperShort,
                format!("{kname} cannot pay {price}{} right now", cur.short),
            ));
        }
        if pooled.is_none() {
            self.change_currency(ctx, &kid, -price);
        }
        self.change_currency(ctx, actor, price);
        let mut tags = vec!["trade".to_owned(), "coin".to_owned()];
        tags.extend(self.item_tags(item));
        if pooled.is_some() {
            // What the pool pays for, the shop uses up: it never goes on
            // sale, so no item is paid from the pool twice, and selling
            // it back and forth never makes money.
            self.destroy_item(ctx, item);
        } else {
            let to = here.fixture.map_or_else(
                || Holder::Entity {
                    entity: kid.clone(),
                },
                |f| Holder::Container { item: f },
            );
            self.relocate_item(ctx, item, to);
            if let Some(it) = self.state.items.get_mut(&item) {
                it.stock = true;
                it.paid = Some(price);
            }
        }
        let me = self.me(actor)?.clone();
        let spec = Emit::new(
            EventKind::Trade,
            Some(me.room.clone()),
            format!(
                "{} sells the {name} to {kname} for {price}{}.",
                me.name, cur.short
            ),
        )
        .actor(actor, "sell")
        .target(&kid)
        .tags(tags)
        .data(json!({
            "item": { "id": item, "name": name },
            "price": price,
            "pool": pooled.as_ref().map(|(p, _)| p),
        }));
        let seq = self.emit(ctx, spec);
        if let Some((pool, spent)) = pooled {
            let period = spent.period;
            self.state.pool_spent.insert(pool.clone(), spent);
            let record = MintRecord {
                seq,
                pool,
                period,
                hook: format!("shops.{}", here.id),
                entity: actor.clone(),
                currency: price,
                items: Vec::new(),
                at_ms: ctx.now,
            };
            self.record_mint(ctx, record);
        }
        Ok((
            format!("You sell the {name} for {price}{}.", cur.short),
            json!({ "item": item, "price": price, "to": kid }),
        ))
    }

    pub(crate) fn equip(&mut self, ctx: &mut Ctx, actor: &EntityId, word: &str) -> Done {
        let item = self.carried(actor, word)?;
        let slot = self
            .item_proto(item)
            .and_then(|p| self.pack.items.get(&p))
            .and_then(|d| d.slot)
            .ok_or_else(|| Fail::new(ErrorCode::NotEquippable, "you cannot equip that"))?;
        let name = self.item_name(item);
        let room = self.me(actor)?.room.clone();
        let actor_name = self.me(actor)?.name.clone();
        if let Some(e) = self.state.entities.get_mut(actor) {
            e.equipped.insert(slot, item);
        }
        ctx.changes.push(StateChange::Equipped {
            entity: actor.clone(),
            slot,
            item: Some(item),
        });
        let spec = Emit::new(
            EventKind::Gear,
            Some(room),
            format!("{actor_name} readies the {name}."),
        )
        .actor(actor, "equip");
        self.emit(ctx, spec);
        Ok((
            format!("You ready the {name}."),
            json!({ "item": item, "slot": slot }),
        ))
    }

    pub(crate) fn unequip(&mut self, ctx: &mut Ctx, actor: &EntityId, word: &str) -> Done {
        let item = self.carried(actor, word)?;
        let me = self.me(actor)?;
        let slot = me
            .equipped
            .iter()
            .find(|(_, i)| **i == item)
            .map(|(s, _)| *s)
            .ok_or_else(|| Fail::new(ErrorCode::NotEquippable, "you are not using that"))?;
        if let Some(e) = self.state.entities.get_mut(actor) {
            e.equipped.remove(&slot);
        }
        ctx.changes.push(StateChange::Equipped {
            entity: actor.clone(),
            slot,
            item: None,
        });
        Ok((
            format!("You put away the {}.", self.item_name(item)),
            json!({ "slot": slot }),
        ))
    }

    pub(crate) fn use_item(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        word: &str,
        on: Option<&str>,
    ) -> Done {
        let me = self.me(actor)?.clone();
        let pool: Vec<ItemId> = me
            .inventory
            .iter()
            .copied()
            .chain(self.room_items(actor))
            .collect();
        let item = self
            .find_item(&pool, word)
            .ok_or_else(|| Fail::new(ErrorCode::NoSuchItem, format!("you have no {word:?}")))?;
        let def = self
            .item_proto(item)
            .and_then(|p| self.pack.items.get(&p).cloned())
            .ok_or_else(|| Fail::new(ErrorCode::NoSuchItem, "that is gone"))?;
        let carried = me.inventory.contains(&item);
        // A key opens a lock on an exit or a container.
        if let Some(lock_id) = &def.key {
            let done = self.use_key(ctx, actor, lock_id, on);
            return self.use_turn(ctx, actor, &me.room, done);
        }
        // A container shows what it holds. It is a look, so it takes no
        // turn in a fight.
        if def.container {
            let it = &self.state.items[&item];
            if it.locked {
                return Err(Fail::new(
                    ErrorCode::ContainerLocked,
                    format!("the {} is locked", def.name),
                ));
            }
            let names: Vec<String> = it.contents.iter().map(|i| self.item_name(*i)).collect();
            let wares = self.state.shop_fixtures.values().any(|f| *f == item);
            let text = if wares {
                if names.is_empty() {
                    format!("The {} stands empty: the shop is sold out.", def.name)
                } else {
                    format!(
                        "The {} holds wares for sale: {}. (list, buy <item>)",
                        def.name,
                        names.join(", ")
                    )
                }
            } else if names.is_empty() {
                format!("The {} is empty.", def.name)
            } else {
                format!(
                    "The {} holds: {}. (take <item> from {})",
                    def.name,
                    names.join(", "),
                    def.name
                )
            };
            return Ok((text, json!({ "contents": names })));
        }
        if !carried {
            return Err(Fail::new(ErrorCode::CannotUse, "pick it up first"));
        }
        // Using up merchandise would hand out its worth without a sale.
        let sealed = self
            .state
            .items
            .get(&item)
            .and_then(|i| i.sealed_currency(&self.pack));
        let consumed = sealed.is_some() || def.heal_dice().is_some();
        if consumed && me.shop.is_some() && self.state.items.get(&item).is_some_and(|i| i.stock) {
            return Err(Fail::new(
                ErrorCode::PolicyTradeRequired,
                "that is merchandise; it leaves the shop only through a completed sale",
            ));
        }
        // Money sealed in an item.
        if let Some(n) = sealed {
            let done = self.empty_purse(ctx, &me, item, &def, n);
            return self.use_turn(ctx, actor, &me.room, Ok(done));
        }
        // Healing.
        if let Some(dice) = def.heal_dice() {
            let target = match on {
                None => actor.clone(),
                Some(w) => self.find_here(actor, w).ok_or_else(|| {
                    Fail::new(ErrorCode::NoSuchTarget, format!("you see no {w:?} here"))
                })?,
            };
            let done = self.heal_with(ctx, &me, item, &def, dice, &target);
            let (text, mut v) = self.use_turn(ctx, actor, &me.room, done)?;
            // The turn may wound the one healed: `hp` is its HP after it.
            if let (Some(t), Some(o)) = (self.state.entities.get(&target), v.as_object_mut()) {
                o.insert("hp".to_owned(), json!(t.pools.hp));
            }
            return Ok((text, v));
        }
        // Gear: `use` is `equip`, and follows its rule.
        if def.slot.is_some() {
            return self.equip(ctx, actor, word);
        }
        // A world script may give an item with no engine use one.
        if let Some(done) = self.use_hook(ctx, actor, item, on) {
            return self.use_turn(ctx, actor, &me.room, done);
        }
        Err(Fail::new(
            ErrorCode::CannotUse,
            format!("you cannot think of a use for the {}", def.name),
        ))
    }

    /// Empties an item that holds `n` coins into `me`'s purse.
    fn empty_purse(
        &mut self,
        ctx: &mut Ctx,
        me: &crate::state::Entity,
        item: ItemId,
        def: &crate::pack::ItemDef,
        n: i64,
    ) -> (String, Value) {
        let actor = &me.id;
        self.destroy_item(ctx, item);
        self.change_currency(ctx, actor, n);
        let cur = self.pack.economy.currency.short.clone();
        let spec = Emit::new(
            EventKind::Gear,
            Some(me.room.clone()),
            format!(
                "{} empties the {} into a palm. Coins clink.",
                me.name, def.name
            ),
        )
        .actor(actor, "use")
        .tags(["coin".to_owned(), "loot".to_owned()])
        .data(json!({ "amount": n }));
        self.emit(ctx, spec);
        (
            format!("You empty the {} and find {n}{cur}.", def.name),
            json!({ "amount": n }),
        )
    }

    fn heal_with(
        &mut self,
        ctx: &mut Ctx,
        me: &crate::state::Entity,
        item: ItemId,
        def: &crate::pack::ItemDef,
        dice: mud_rules::DamageDice,
        target: &EntityId,
    ) -> Done {
        let actor = &me.id;
        // A heal on someone unhurt would waste the item and, in a fight,
        // the turn. It is refused before it rolls, so it costs nothing.
        if let Some(t) = self.state.entities.get(target)
            && t.pools.hp >= t.pools.hp_max
            && !t.unconscious
        {
            let who = if target == actor {
                "you are".to_owned()
            } else {
                format!("{} is", t.name)
            };
            return Err(Fail::new(
                ErrorCode::CannotUse,
                format!("{who} not hurt; the {} would be wasted", def.name),
            ));
        }
        let target = target.clone();
        let mut amount = dice
            .roll(&mut self.dice, mud_rules::DamageType::Crushing)
            .max(0);
        let skill_roll = def.heal_skill.as_ref().map(|skill| {
            let level = self.skill_level(me, skill).unwrap_or(0);
            mud_rules::success_roll(&mut self.dice, level)
        });
        if skill_roll.is_some_and(|r| r.outcome.is_success()) {
            amount *= 2;
        }
        self.destroy_item(ctx, item);
        let (tname, hp) = {
            let Some(t) = self.state.entities.get_mut(&target) else {
                return Err(Fail::new(ErrorCode::NoSuchTarget, "they are gone"));
            };
            let room_to_max = (t.pools.hp_max - t.pools.hp).max(0);
            amount = amount.min(room_to_max);
            t.pools.hp += amount;
            (t.name.clone(), t.pools.hp)
        };
        ctx.changes.push(StateChange::Hp {
            entity: target.clone(),
            delta: amount,
            hp,
        });
        let spec = Emit::new(
            EventKind::Gear,
            Some(me.room.clone()),
            format!("{} uses the {} on {tname}.", me.name, def.name),
        )
        .actor(actor, "use")
        .target(&target)
        .data(json!({ "healed": amount }));
        self.emit(ctx, spec);
        if hp > 0 {
            self.come_to(ctx, &target);
        }
        Ok((
            format!("You use the {}. {tname} recovers {amount} HP.", def.name),
            json!({ "healed": amount, "hp": hp, "skill_roll": skill_roll }),
        ))
    }

    fn use_key(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        lock_id: &str,
        on: Option<&str>,
    ) -> Done {
        let room = self.me(actor)?.room.clone();
        let def = self.pack.rooms.get(&room).cloned();
        let target_dir = on.and_then(|w| w.parse::<Direction>().ok());
        let exit = def.as_ref().and_then(|d| {
            d.locks
                .iter()
                .find(|(dir, l)| l.lock == lock_id && target_dir.is_none_or(|t| t == **dir))
                .map(|(dir, _)| *dir)
        });
        let Some(dir) = exit else {
            return Err(Fail::new(
                ErrorCode::CannotUse,
                "this key fits no lock here",
            ));
        };
        self.unlock_exit(ctx, actor, &room, dir, "key");
        Ok((
            format!("The key turns. The way {} is open.", dir.name()),
            json!({ "unlocked": dir.short() }),
        ))
    }

    pub(crate) fn unlock_exit(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        room: &crate::ids::RoomId,
        dir: Direction,
        how: &str,
    ) {
        if let Some(r) = self.state.rooms.get_mut(room) {
            r.unlocked.insert(dir);
            r.unlocked_at.insert(dir, ctx.now);
        }
        // The far side opens with it.
        if let Some(to) = self
            .pack
            .rooms
            .get(room)
            .and_then(|d| d.exits.get(&dir))
            .cloned()
            && let Some(r) = self.state.rooms.get_mut(&to)
        {
            r.unlocked.insert(dir.opposite());
            r.unlocked_at.insert(dir.opposite(), ctx.now);
        }
        ctx.changes.push(StateChange::Unlocked {
            room: room.clone(),
            dir,
        });
        let name = self
            .state
            .entities
            .get(actor)
            .map_or_else(String::new, |e| e.name.clone());
        let spec = Emit::new(
            EventKind::Gear,
            Some(room.clone()),
            format!("{name} opens the lock on the way {}.", dir.name()),
        )
        .actor(actor, if how == "key" { "use" } else { "attempt" })
        .data(json!({ "unlocked": dir.short(), "how": how }));
        self.emit(ctx, spec);
    }

    pub(crate) fn find_lockable(&self, actor: &EntityId, word: &str) -> Option<Lockable> {
        let me = self.state.entities.get(actor)?;
        let def = self.pack.rooms.get(&me.room)?;
        let dir = word.parse::<Direction>().ok();
        for (d, l) in &def.locks {
            let named = dir == Some(*d)
                || word == "grate"
                || word == "lock"
                || l.lock == word
                || l.describe
                    .as_deref()
                    .is_some_and(|x| x.to_ascii_lowercase().contains(word));
            if named && self.exit_is_locked(&me.room, *d) {
                return Some(if l.pickable {
                    Lockable::Exit(*d, l.pick)
                } else {
                    Lockable::Sealed(*d)
                });
            }
        }
        let pool: Vec<ItemId> = self
            .room_items(actor)
            .into_iter()
            .chain(me.inventory.iter().copied())
            .collect();
        let item = self.find_item(&pool, word)?;
        let it = self.state.items.get(&item)?;
        let modifier = self.pack.items.get(&it.proto)?.lock?;
        it.locked.then_some(Lockable::Container(item, modifier))
    }

    pub(crate) fn unlock_container(&mut self, ctx: &mut Ctx, item: ItemId) -> Value {
        if let Some(it) = self.state.items.get_mut(&item) {
            it.locked = false;
        }
        ctx.changes.push(StateChange::ContainerUnlocked { item });
        let names: Vec<String> = self.state.items[&item]
            .contents
            .iter()
            .map(|i| self.item_name(*i))
            .collect();
        json!({ "contents": names })
    }
}

/// A shop a customer can trade with where it stands.
#[derive(Debug, Clone)]
struct ShopHere {
    /// The shop.
    id: ShopId,
    /// Its pack entry.
    def: ShopDef,
    /// Its keeper, who takes the money.
    keeper: EntityId,
    /// The fixture that holds its stock; `None`: the keeper's body.
    fixture: Option<ItemId>,
    /// Whether the keeper can act here.
    minded: bool,
}

impl World {
    /// Who a customer deals with: the keeper, or, with nobody minding the
    /// shop, its fixture.
    fn seller_name(&self, shop: &ShopHere) -> String {
        match (shop.minded, shop.fixture) {
            (false, Some(f)) => self.item_name(f),
            _ => self
                .state
                .entities
                .get(&shop.keeper)
                .map_or_else(String::new, |k| k.name.clone()),
        }
    }
}

/// Something a lockpick can open.
#[derive(Debug, Clone, Copy)]
pub enum Lockable {
    /// An exit and its modifier.
    Exit(Direction, i32),
    /// A container and its modifier.
    Container(ItemId, i32),
    /// A locked exit whose lock no pick opens (`pickable = false`).
    Sealed(Direction),
}
