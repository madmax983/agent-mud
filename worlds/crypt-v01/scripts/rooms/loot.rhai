// `search` the fallen. Each death of a mob pays its first good searcher
// once: one roll on the mob's loot table (scripts.toml config), paid from
// the table's per-day pool (economy.toml), so every coin is in the ledger.
//
// - The world flag loot.<mob> holds the death that was paid, from
//   ctx.cast.<mob>.deaths. A respawned mob that dies again pays again.
// - The mark searched.<mob> holds the death a player already searched.
//   A failed IQ roll finds nothing, and that player cannot try the same
//   body again, but the next player can.
// - A mob that still stands, or lies out cold, cannot be searched: finish
//   it first.

/// The first body in this room that is dead, unpaid, and not yet searched
/// by this player, or ().
fn body_to_search(ctx) {
    for id in ctx.config.rooms[ctx.room.id] {
        let m = ctx.cast[id];
        if m.alive {
            continue;
        }
        let paid = `loot.${id}`;
        if paid in ctx.flags && ctx.flags[paid] == m.deaths {
            continue;
        }
        let tried = `searched.${id}`;
        if tried in ctx.actor.marks && ctx.actor.marks[tried] == m.deaths {
            continue;
        }
        return id;
    }
    ()
}

/// One entry of a loot table, picked by a roll of 0 to 99 on the weights.
/// Weights that add up to less than 100 leave the rest as nothing.
fn pick_entry(rolls) {
    let r = below(100);
    let acc = 0;
    for e in rolls {
        acc += e.weight;
        if r < acc {
            return e;
        }
    }
    #{ weight: 0 }
}

fn on_search(ctx) {
    let room = ctx.room.id;
    if !(room in ctx.config.rooms) {
        return [refuse("There is nothing here worth searching.")];
    }
    if hostile_here(ctx) {
        return [refuse("Not with teeth still snapping at you.")];
    }
    let id = body_to_search(ctx);
    if id == () {
        return [msg("You turn over bones and rubbish. Nothing here has died lately that you have not already searched.")];
    }
    let m = ctx.cast[id];
    let who = ctx.actor.name;
    let level = ctx.actor.attrs.iq + ctx.config.modifier + dark_mod(ctx, ctx.config.dark);
    let r = check(level);
    if !r.success {
        return [mark(`searched.${id}`, m.deaths),
                msg(`You search the ${m.name}'s remains and come up with nothing but filth.`),
                narrate(`${who} searches the ${m.name}'s remains, and comes up empty.`)];
    }
    let table = ctx.config.tables[ctx.config.mobs[id]];
    let e = pick_entry(table.rolls);
    let pool = ctx.pools[table.pool];
    let coin = if "coin" in e { e.coin } else { 0 };
    let ask = #{};
    let fits = true;
    if coin > 0 {
        ask.currency = coin;
        fits = pool.currency >= coin;
    }
    if "item" in e {
        let items = #{};
        items[e.item] = 1;
        ask.items = items;
        fits = fits && e.item in pool.items && pool.items[e.item] >= 1;
    }
    // The body is spent whatever it held: the flag and the mark go with
    // the reward, or alone when it held nothing.
    let fx = [set_flag(`loot.${id}`, m.deaths), mark(`searched.${id}`, m.deaths)];
    if ask.is_empty() {
        fx += [msg(`You search the ${m.name}'s remains. Nothing worth taking.`),
               narrate(`${who} searches the ${m.name}'s remains, and finds nothing worth taking.`)];
    } else if !fits {
        fx += [msg("The pickings below are thin today. You find nothing worth taking."),
               narrate(`${who} searches the ${m.name}'s remains, and finds nothing worth taking.`)];
    } else {
        fx += [msg(`You search the ${m.name}'s remains and find something.`),
               narrate(`${who} searches the ${m.name}'s remains, and pockets something.`),
               reward(table.pool, ask)];
    }
    fx
}
