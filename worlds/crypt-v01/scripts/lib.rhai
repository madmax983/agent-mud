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
