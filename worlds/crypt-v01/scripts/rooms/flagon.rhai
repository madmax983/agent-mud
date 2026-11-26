// Liar's bones at the Rusty Flagon. `bones` shakes five dice under a cup.
// The roll goes to the player alone, and the room sees only the shake.
// `call` lifts every cup at the table and clears them. Bids are plain
// `say`, and stakes are plain `give`: the house keeps the dice honest, and
// the players, or Marta holding the pot, keep the bets.
//
// A hook can read and write only the actor's marks, so a cup lives in a
// world flag that `call` can read for everyone present: bones.<id> holds
// day * 100000 + the five dice as digits (3,5,5,2,6 is 35526). A cup from
// an earlier day is stale; each shake sweeps a few stale cups away, so the
// flags stay few. The mark bones counts a player's shakes today
// (day * 100 + n), at most config.rounds_per_day.

/// The five dice of a cup, as `3 5 5 2 6`.
fn cup_text(cup) {
    let digits = [];
    let n = cup;
    for i in 0..5 {
        digits.push(n % 10);
        n = n / 10;
    }
    let t = "";
    for i in 0..5 {
        if i > 0 {
            t += " ";
        }
        t += `${digits[4 - i]}`;
    }
    t
}

/// The cup today's flag value holds, or () for no cup or a stale one.
fn cup_of(flags, key, day) {
    if !(key in flags) {
        return ();
    }
    let v = flags[key];
    if v / 100000 == day {
        v % 100000
    } else {
        ()
    }
}

fn on_bones(ctx) {
    let n = count_today(ctx.actor.marks, "bones", ctx.now_ms);
    if n >= ctx.config.rounds_per_day {
        return [msg("Marta takes the cup out of your hand. \"That's enough bones for one day.\"")];
    }
    let day = day_utc(ctx.now_ms);
    let cup = 0;
    for i in 0..5 {
        cup = cup * 10 + d6();
    }
    let mine = `bones.${ctx.actor.id}`;
    let fx = [set_flag(mine, day * 100000 + cup),
              mark("bones", count_mark(ctx.now_ms, n + 1)),
              msg(`You shake the cup, slap it down and peek under the rim: ${cup_text(cup)}.`),
              narrate(`${ctx.actor.name} shakes the bone cup, slaps it down and peeks under the rim.`)];
    for k in ctx.flags {
        if fx.len() >= ctx.config.max_effects {
            break;
        }
        if k != mine && k.starts_with("bones.") && cup_of(ctx.flags, k, day) == () {
            fx.push(clear_flag(k));
        }
    }
    fx
}

fn on_call(ctx) {
    let day = day_utc(ctx.now_ms);
    let mine = `bones.${ctx.actor.id}`;
    let my_cup = cup_of(ctx.flags, mine, day);
    if my_cup == () {
        return [msg("You have no cup down. Shake the bones first.")];
    }
    let shown = [`${ctx.actor.name} ${cup_text(my_cup)}`];
    let cups = [my_cup];
    let fx = [clear_flag(mine)];
    // Two effects stay for the lines: a table of up to six cups.
    for p in ctx.room.present {
        if fx.len() >= ctx.config.max_effects - 2 {
            break;
        }
        let key = `bones.${p.id}`;
        let cup = cup_of(ctx.flags, key, day);
        if cup != () {
            shown.push(`${p.name} ${cup_text(cup)}`);
            cups.push(cup);
            fx.push(clear_flag(key));
        }
    }
    if cups.len() < 2 {
        return [msg("Nobody else at the table has a cup down.")];
    }
    // How many of each face are on the table, to settle the last bid.
    let faces = [0, 0, 0, 0, 0, 0];
    for c in cups {
        let n = c;
        for i in 0..5 {
            let f = n % 10;
            faces[f - 1] += 1;
            n = n / 10;
        }
    }
    let tally = "";
    for f in 0..6 {
        if f > 0 {
            tally += ", ";
        }
        tally += `${faces[f]} ${f + 1}s`;
    }
    let line = `The cups come up: ${shown[0]}`;
    for i in 1..shown.len() {
        line += `; ${shown[i]}`;
    }
    line += `. On the table: ${tally}.`;
    fx += [msg(`You call. ${line}`), narrate(`${ctx.actor.name} calls. ${line}`)];
    fx
}
