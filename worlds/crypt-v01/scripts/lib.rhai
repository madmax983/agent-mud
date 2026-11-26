// Helpers for every crypt-v01 script. Functions only: a file may not run
// code at the top level, so constants live in scripts.toml `config`.

/// Whether the skeletal warden stands in the hook's room.
fn warden_here(ctx) {
    let w = ctx.cast.skeletal_warden;
    w.present && w.room == ctx.room.id
}

/// Whether marks.<key> already holds today's day number.
fn done_today(marks, key, now_ms) {
    key in marks && marks[key] == day_utc(now_ms)
}

/// Whether `actor` carries an item of prototype `proto`.
fn carries(actor, proto) {
    for p in actor.inventory {
        if p == proto {
            return true;
        }
    }
    false
}

/// Whether a mob that can still fight is in the hook's room. A mob out
/// cold does not count; a dead one is not in the room at all.
fn hostile_here(ctx) {
    for p in ctx.room.present {
        if p.kind == "mob" && p.condition != "unconscious" {
            return true;
        }
    }
    false
}

/// The modifier of a roll made in the dark: `dark` (a negative number from
/// the binding's config) when the room is dark and nobody holds a light.
fn dark_mod(ctx, dark) {
    if ctx.room.dark && !ctx.room.lit {
        dark
    } else {
        0
    }
}

/// How many times today a player did something that marks.<key> counts.
/// The mark holds day * 100 + n, so it starts at 0 again each day.
fn count_today(marks, key, now_ms) {
    if !(key in marks) {
        return 0;
    }
    let v = marks[key];
    if v / 100 == day_utc(now_ms) {
        v % 100
    } else {
        0
    }
}

/// The value of a counting mark that says `n` times today.
fn count_mark(now_ms, n) {
    day_utc(now_ms) * 100 + n
}
