// The rumor of the day. Each UTC day of the game clock hides one cache
// below: in one of five dark rooms, at one of three spots (the corner,
// the floor or a wall). The day number picks both, so every player and
// every agent agrees on today's cache, and it moves at midnight.
//
// - `eavesdrop` at the Flagon (an IQ roll, once a day) catches talk of the
//   room, as a riddle. A critical failure catches the wrong room.
// - `case` in Vich's backroom (Streetwise, or IQ less a little; once a
//   day, and only with Vich there) catches the spot, never the room.
// - `dig <spot>` in the right room pays the first finder of the day from
//   the `rumor_cache` pool. Each player digs at most config.tries times a
//   day, so guessing blind rarely pays. The flag cache.found holds the day
//   the cache was found.
//
// The three verbs share one table, and each binding has its own config,
// so the table and the day's hash are functions here, in one place.

/// The rooms a cache may lie in, with the riddle the Flagon's talk tells.
fn cache_rooms() {
    [
        #{ room: "flooded_cistern", riddle: "where the black water stands to the knee" },
        #{ room: "wine_vault", riddle: "among the broken casks, where the glass crunches" },
        #{ room: "ossuary_crypt", riddle: "where the skulls are stacked to the vault" },
        #{ room: "root_cellar", riddle: "under the rotting turnips, by the nest" },
        #{ room: "collapsed_drift", riddle: "down the dwarf stair, where the timbers groan" },
    ]
}

/// The spots a cache may be dug into, with the talk that tells of each.
fn cache_spots() {
    [
        #{ word: "corner", talk: "tucked into the far corner" },
        #{ word: "floor", talk: "under a loose stone in the floor" },
        #{ word: "wall", talk: "in a gap in the wall" },
    ]
}

/// A number the day picks, the same all day and for everyone: the high
/// bits of a multiplicative hash, so the rooms do not come round in order.
fn day_hash(day) {
    (((day + 7) * 2654435761) % 4294967296) / 65536
}

/// Today's cache: #{ room, riddle, spot, talk, index }.
fn todays_cache(ctx) {
    let rooms = cache_rooms();
    let spots = cache_spots();
    let h = day_hash(day_utc(ctx.now_ms));
    let i = h % rooms.len();
    let s = (h / rooms.len()) % spots.len();
    #{ room: rooms[i].room, riddle: rooms[i].riddle, spot: spots[s].word, talk: spots[s].talk, index: i }
}

/// Whether today's cache was already dug up.
fn found_today(ctx) {
    "cache.found" in ctx.flags && ctx.flags["cache.found"] == day_utc(ctx.now_ms)
}

fn on_eavesdrop(ctx) {
    if done_today(ctx.actor.marks, "eavesdrop_day", ctx.now_ms) {
        return [msg("You have heard all the talk worth hearing today. The rest is weather and the price of ale.")];
    }
    let c = todays_cache(ctx);
    let r = check(ctx.actor.attrs.iq + ctx.config.modifier);
    let fx = [mark("eavesdrop_day", day_utc(ctx.now_ms)),
              narrate(`${ctx.actor.name} nurses a drink and listens to the room.`)];
    if r.success {
        let line = `Over the clatter of mugs you catch it: something was hidden below today, ${c.riddle}.`;
        if found_today(ctx) {
            line += " And word is someone already dug it up.";
        }
        fx.push(msg(line));
    } else if r.critical {
        // The wrong room, told as surely as the right one.
        let rooms = cache_rooms();
        let wrong = rooms[(c.index + 1 + below(rooms.len() - 1)) % rooms.len()];
        fx.push(msg(`Over the clatter of mugs you catch it: something was hidden below today, ${wrong.riddle}.`));
    } else {
        fx.push(msg("Talk of rain, and the price of ale. Nothing worth the coin."));
    }
    fx
}

fn on_case(ctx) {
    let v = ctx.cast.vich;
    if !(v.present && v.room == ctx.room.id) {
        return [refuse("With the fence away, the backroom hears nothing worth knowing.")];
    }
    if done_today(ctx.actor.marks, "case_day", ctx.now_ms) {
        return [msg("You have cased the backroom's talk today already. Nobody new has come through.")];
    }
    let level = ctx.actor.skills.streetwise;
    let alt = ctx.actor.attrs.iq + ctx.config.modifier;
    if alt > level {
        level = alt;
    }
    let r = check(level);
    let fx = [mark("case_day", day_utc(ctx.now_ms)),
              narrate(`${ctx.actor.name} leans on a crate and keeps an ear to the talk.`)];
    if r.success {
        let c = todays_cache(ctx);
        fx.push(msg(`Between the crates you hear it plain: whoever hid today's cache put it ${c.talk}. Nobody says which room.`));
    } else {
        fx.push(msg("Nothing passes through the backroom but smoke and lies."));
    }
    fx
}

fn on_dig(ctx) {
    let words = [];
    for s in cache_spots() {
        words.push(s.word);
    }
    let spot = ctx.text.pick(words);
    if spot == () {
        return [refuse("Dig where? The corner, the floor or a wall.")];
    }
    if hostile_here(ctx) {
        return [refuse("Not with something here that wants you dead.")];
    }
    let n = count_today(ctx.actor.marks, "dig", ctx.now_ms);
    if n >= ctx.config.tries {
        return [msg("Your fingers are raw and your nails are split. Dig again tomorrow.")];
    }
    let c = todays_cache(ctx);
    let fx = [mark("dig", count_mark(ctx.now_ms, n + 1)),
              narrate(`${ctx.actor.name} scrapes at the ${spot}.`)];
    if c.room != ctx.room.id || c.spot != spot {
        fx.push(msg(`You dig at the ${spot} until your nails split. Nothing.`));
        return fx;
    }
    if found_today(ctx) {
        fx.push(msg(`At the ${spot} you find a fresh hole, and nothing in it. Someone was here first.`));
        return fx;
    }
    let pay = ctx.config.silver;
    let purse = ctx.pools.rumor_cache;
    if purse.currency < pay || purse.items.healing_draught < 1 {
        fx.push(msg(`At the ${spot} you find a fresh hole, and nothing in it. Someone was here first.`));
        return fx;
    }
    fx += [set_flag("cache.found", day_utc(ctx.now_ms)),
           msg(`At the ${spot}, under a hand's depth of muck, your fingers close on an oilcloth bundle.`),
           reward("rumor_cache", #{ currency: pay, items: #{ healing_draught: 1 } })];
    fx
}
