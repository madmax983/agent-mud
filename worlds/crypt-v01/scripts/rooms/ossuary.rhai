// The Ossuary Crypt: a welcome for the first one down, and `pray` at the
// cracked sarcophagus (once a day, never while the warden stands).

fn on_enter(ctx) {
    let fx = [];
    if !("ossuary.first_visitor" in ctx.flags) {
        fx.push(set_flag("ossuary.first_visitor", ctx.actor.name));
        fx.push(narrate("Dust sifts from the vault. Nothing living has stood here in a long time."));
    }
    if warden_here(ctx) {
        if ctx.room.lit {
            fx.push(msg("Rows of skulls watch you come in. One of them, wired to a body, turns its head."));
        } else {
            fx.push(msg("In the dark, bone scrapes on stone, and stops when you stop."));
        }
    }
    fx
}

fn on_pray(ctx) {
    if done_today(ctx.actor.marks, "prayed_day", ctx.now_ms) {
        return [msg("You have asked the dead for enough today. They keep their silence.")];
    }
    if warden_here(ctx) {
        return [refuse("You start to kneel. The warden's grave-blade comes up. Not while it stands.")];
    }
    let r = check(ctx.actor.attrs.iq + ctx.config.modifier);
    let fx = [mark("prayed_day", day_utc(ctx.now_ms)),
              narrate(`${ctx.actor.name} kneels by the cracked sarcophagus.`)];
    if r.success {
        fx += [msg("A cold calm settles over you. Your wounds ache less."), heal("1d")];
    } else if r.critical {
        fx += [msg("The skulls clatter in their niches. Something answers, and it is not kind."), hurt("1d-2")];
    } else {
        fx.push(msg("You wait in the silence. Nothing answers."));
    }
    fx
}
