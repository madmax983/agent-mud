// The old dwarf drift under the ossuary.
//
// - `clear` in the ossuary hauls the rubble off the dwarf stair: a ST
//   roll, and only with nothing standing over the stair. Success unlocks
//   the way down; the rubble slides back an hour later (world.toml). A
//   critical failure drops a stone on the one who heaves.
// - `mine` in the drift works the seam: it needs a miner's pick carried,
//   the crawler down, and an HT roll. Each player mines once a day, and a
//   success mints one chunk of ore from the `drift_ore` pool, four a day
//   in all. So the ore Torin pays for comes back each day, a few chunks at
//   a time.

fn on_clear(ctx) {
    if hostile_here(ctx) {
        return [refuse("You reach for the rubble, and the warden's grave-blade comes up. Not while it stands.")];
    }
    let open = false;
    for x in ctx.room.exits {
        if x.dir == ctx.config.dir && !x.locked {
            open = true;
        }
    }
    if open {
        return [msg("The dwarf stair is clear. It will not stay so for long.")];
    }
    let r = check(ctx.actor.attrs.st + ctx.config.modifier);
    if r.success {
        return [unlock(ctx.config.dir),
                msg("You heave the last big stones aside. The dwarf stair lies open, for now."),
                narrate(`${ctx.actor.name} hauls the rubble off the old stair. It lies open.`)];
    }
    if r.critical {
        return [msg("The pile shifts, and a stone cracks across your shin."),
                narrate(`The rubble shifts under ${ctx.actor.name}, and a stone comes down hard.`),
                hurt("1d-2")];
    }
    [msg("You strain at the rubble. It shifts, and settles back."),
     narrate(`${ctx.actor.name} strains at the rubble on the old stair.`)]
}

fn on_mine(ctx) {
    if hostile_here(ctx) {
        return [refuse("Not with the crawler's mandibles clicking at your back.")];
    }
    if !carries(ctx.actor, "miners_pick") {
        return [refuse("You claw at the seam with your fingers. You need a miner's pick.")];
    }
    if done_today(ctx.actor.marks, "mined_day", ctx.now_ms) {
        return [msg("Your arms are spent. The seam will keep till tomorrow.")];
    }
    if ctx.pools.drift_ore.items.iron_ore < 1 {
        return [msg("You tap along the seam. Every loose chunk is gone today; by tomorrow the damp will have worked more free.")];
    }
    let r = check(ctx.actor.attrs.ht + ctx.config.modifier + dark_mod(ctx, ctx.config.dark));
    let fx = [mark("mined_day", day_utc(ctx.now_ms)),
              narrate(`${ctx.actor.name} swings a pick at the seam.`)];
    if r.success {
        fx += [msg("A chunk of dark ore breaks free and thuds at your feet. You heft it: dwarven iron."),
               reward("drift_ore", #{ items: #{ iron_ore: 1 } })];
    } else if r.critical {
        fx += [msg("A slab peels off the roof and catches your shoulder."), hurt("1d-2")];
    } else {
        fx.push(msg("You hack at the seam until your arms burn. Nothing worth carrying comes loose."));
    }
    fx
}
