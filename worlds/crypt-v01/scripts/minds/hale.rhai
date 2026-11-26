// Hale keeps the Town Square. He reads whoever walks in by what they are in
// the world: ev.actor.kind is "player" for a human and for kestrel alike,
// and no script can tell them apart. Night is config.night_from to
// config.night_to, UTC hours. Speech is opaque: Hale tests words in it.
//
// One wake can batch several arrivals of one player: Hale counts a visit
// once per wake. He does not ask who is still here: ambient wakes come in
// batches, after the fact, so most arrivals have walked on by then.
//
// Hale says the grate is locked only when he stands at it and sees it
// locked (ctx.here.exits, config.grate_room and config.grate_dir). Off his
// post (ctx.me.post, from cast.toml), he takes one step back toward it.

/// What Hale says about the grate from where he stands: the truth when he
/// can see it, and no claim about the lock when he cannot.
fn grate_line(ctx) {
    if ctx.here.room_id == ctx.config.grate_room {
        for x in ctx.here.exits {
            if x.dir == ctx.config.grate_dir {
                if x.locked {
                    return "say grate's locked for a reason. vich has the only key, and vich has a price.";
                }
                return "say that grate's standing open. go down there and the watch won't come after you.";
            }
        }
    }
    "say stay off that grate. vich has the only key, and vich has a price."
}

fn on_wake(ctx) {
    let today = day_utc(ctx.now_ms);
    let mem = ctx.memory;
    if !("day" in mem) || mem.day != today || mem.warned.len() > 32 {
        mem = #{ day: today, warned: #{} };
    }
    let h = hour_utc(ctx.now_ms);
    let night = h >= ctx.config.night_from || h < ctx.config.night_to;
    let lines = [];
    let greeted = #{};
    for ev in ctx.events {
        if ev.is_self || ev.actor == () || ev.actor.kind != "player" { continue; }
        let id = ev.actor.id;
        let name = ev.actor.name;
        let times = if id in mem.warned { mem.warned[id] } else { 0 };
        if ev.kind == "enter" {
            if id in greeted { continue; }
            greeted[id] = true;
            mem.warned[id] = times + 1;
            if times == 0 && night {
                lines.push(`say evening, ${name}. the alley's shut after dark. go down that grate and the watch won't come for you.`);
            } else if times == 0 {
                lines.push("say move along. and stay out of the alley after dark.");
            } else if times == 1 {
                lines.push(`say back again, ${name}? stay off that grate.`);
            }
        } else if ev.kind == "speech" && ev.text.has_any(["alley", "grate", "crypt", "cellar"]) {
            lines.push(grate_line(ctx));
        }
        if lines.len() >= 2 { break; }
    }
    let post = ctx.me.post;
    if post != () && !post.at_post && post.way != () && post.way.len() > 0 {
        lines.push(`move ${post.way[0]}`);
    }
    #{ lines: lines, memory: mem }
}
