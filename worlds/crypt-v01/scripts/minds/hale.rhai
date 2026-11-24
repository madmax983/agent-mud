// Hale keeps the Town Square. He reads whoever walks in by what they are in
// the world: ev.actor.kind is "player" for a human and for kestrel alike,
// and no script can tell them apart. Night is config.night_from to
// config.night_to, UTC hours. Speech is opaque: Hale tests words in it.
fn on_wake(ctx) {
    let today = day_utc(ctx.now_ms);
    let mem = ctx.memory;
    if !("day" in mem) || mem.day != today || mem.warned.len() > 32 {
        mem = #{ day: today, warned: #{} };
    }
    let h = hour_utc(ctx.now_ms);
    let night = h >= ctx.config.night_from || h < ctx.config.night_to;
    let lines = [];
    for ev in ctx.events {
        if ev.is_self || ev.actor == () || ev.actor.kind != "player" { continue; }
        let id = ev.actor.id;
        let name = ev.actor.name;
        let times = if id in mem.warned { mem.warned[id] } else { 0 };
        if ev.kind == "enter" {
            mem.warned[id] = times + 1;
            if times == 0 && night {
                lines.push(`say evening, ${name}. the alley's shut after dark. go down that grate and the watch won't come for you.`);
            } else if times == 0 {
                lines.push("say move along. and stay out of the alley after dark.");
            } else if times == 1 {
                lines.push(`say back again, ${name}? the grate stays shut.`);
            }
        } else if ev.kind == "speech" && ev.text.has_any(["alley", "grate", "crypt", "cellar"]) {
            lines.push("say grate's locked for a reason. vich has the only key, and vich has a price.");
        }
        if lines.len() >= 2 { break; }
    }
    #{ lines: lines, memory: mem }
}
