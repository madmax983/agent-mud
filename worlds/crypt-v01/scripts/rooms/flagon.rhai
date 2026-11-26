// Liar's bones at the Rusty Flagon. `bones` shakes five dice under a cup.
// The roll goes to the player alone, and the room sees only the shake.
// `call` lifts every cup at the table and ends the round. Bids are plain
// `say`, and stakes are plain `give`: the house keeps the dice honest, and
// the players, or Marta holding the pot, keep the bets.
//
// A hook can read and write only the actor's marks, so the table lives in
// world flags that `call` can read for everyone present.
//
// - bones_round holds the round: day * 100000 + the rounds called today.
//   A flag from an earlier day means round 0 of today.
// - bones.<id> holds a cup: round * 100000 + the five dice as digits
//   (3,5,5,2,6 is 35526). A cup from an earlier round is stale. So a cup
//   left behind by a player who walked out before the call never counts
//   in a later round.
// - A cup down cannot be shaken again: it stays as it fell until the
//   call. So nobody peeks, hears the bids and shakes for a better hand.
// - `call` ends the round for everyone, so each cup of the round goes
//   stale at once, however many there are. Alone at the table, it tips
//   the caller's dice back. Each shake sweeps a few stale cups away
//   first, so the flags stay few and a full flag table still takes a new
//   shake.
// - The mark bones counts a player's shakes today (day * 100 + n), at
//   most config.rounds_per_day.

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

/// The round now: today's bones_round, or round 0 of today.
fn round_now(flags, day) {
    let first = day * 100000;
    if "bones_round" in flags {
        let r = flags["bones_round"];
        if r / 100000 == day {
            return r;
        }
    }
    first
}

/// The cup a flag holds in round `round`, or () for no cup or a stale one.
fn cup_of(flags, key, round) {
    if !(key in flags) {
        return ();
    }
    let v = flags[key];
    if v / 100000 == round {
        v % 100000
    } else {
        ()
    }
}

fn on_bones(ctx) {
    let day = day_utc(ctx.now_ms);
    let round = round_now(ctx.flags, day);
    let mine = `bones.${ctx.actor.id}`;
    if cup_of(ctx.flags, mine, round) != () {
        return [refuse("Your cup is already down. Leave it be until someone calls.")];
    }
    let n = count_today(ctx.actor.marks, "bones", ctx.now_ms);
    if n >= ctx.config.rounds_per_day {
        return [msg("Marta takes the cup out of your hand. \"That's enough bones for one day.\"")];
    }
    // The sweep goes first: a clear frees its flag before the new cup
    // asks for one, so a full table of stale cups still takes the shake.
    // Four effects stay for the shake itself.
    let fx = [];
    for k in ctx.flags {
        if fx.len() >= ctx.config.max_effects - 4 {
            break;
        }
        if k != mine && k.starts_with("bones.") && cup_of(ctx.flags, k, round) == () {
            fx.push(clear_flag(k));
        }
    }
    let cup = 0;
    for i in 0..5 {
        cup = cup * 10 + d6();
    }
    fx += [set_flag(mine, round * 100000 + cup),
           mark("bones", count_mark(ctx.now_ms, n + 1)),
           msg(`You shake the cup, slap it down and peek under the rim: ${cup_text(cup)}.`),
           narrate(`${ctx.actor.name} shakes the bone cup, slaps it down and peeks under the rim.`)];
    fx
}

fn on_call(ctx) {
    let day = day_utc(ctx.now_ms);
    let round = round_now(ctx.flags, day);
    let mine = `bones.${ctx.actor.id}`;
    let my_cup = cup_of(ctx.flags, mine, round);
    if my_cup == () {
        return [refuse("You have no cup down. Shake the bones first.")];
    }
    // The call ends the round: every cup of it goes stale at once.
    let fx = [set_flag("bones_round", round + 1), clear_flag(mine)];
    let shown = [`${ctx.actor.name} ${cup_text(my_cup)}`];
    let cups = [my_cup];
    let seen = [mine];
    for p in ctx.room.present {
        let key = `bones.${p.id}`;
        let cup = cup_of(ctx.flags, key, round);
        if cup != () {
            shown.push(`${p.name} ${cup_text(cup)}`);
            cups.push(cup);
            seen.push(key);
        }
    }
    // Cups of the round whose owners are not at the table: they left, or
    // keep out of sight. They stay out of the count, and the call says so.
    let away = 0;
    for k in ctx.flags {
        if k.starts_with("bones.") && !seen.contains(k) && cup_of(ctx.flags, k, round) != () {
            away += 1;
        }
    }
    if cups.len() < 2 {
        fx.push(msg("Nobody else at the table has a cup down. You tip your dice back into the cup."));
        return fx;
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
    if away == 1 {
        line += " One more cup lies under a hand that has left the table, out of the count.";
    } else if away > 1 {
        line += ` ${away} more cups lie under hands that have left the table, out of the count.`;
    }
    fx += [msg(`You call. ${line}`), narrate(`${ctx.actor.name} calls. ${line}`)];
    fx
}
