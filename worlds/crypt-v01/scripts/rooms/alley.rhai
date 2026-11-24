// The Dark Alley and the Cellar Landing: `listen` at the grate or on the
// stair, and the fence's curtain answers his name. Speech is opaque: this
// script tests words in it and never repeats them.

fn on_speech(ctx) {
    let v = ctx.cast.vich;
    if ctx.text.has_any(["vich", "fence"]) && v.present && v.room == "fence_backroom" {
        // narrate() skips the speaker, who is often alone here.
        return [msg("Behind the east door, the curtain twitches, then hangs still."),
                narrate("The curtain over the east door twitches, then hangs still.")];
    }
    []
}

fn on_listen(ctx) {
    let r = check(ctx.actor.skills.observation + ctx.config.modifier);
    if !r.success {
        return [msg("Water, and your own breath. Nothing else.")];
    }
    let heard = [];
    if ctx.cast.giant_rat_1.alive || ctx.cast.giant_rat_2.alive {
        heard.push("claws skittering over broken glass");
    }
    if ctx.cast.skeletal_warden.alive {
        heard.push("far below, something dragging metal over stone");
    }
    if heard.is_empty() {
        return [msg("Only water, dripping somewhere deep. Whatever was down there is quiet now.")];
    }
    let line = "You hold still and listen: " + heard[0];
    if heard.len() > 1 {
        line += ", and " + heard[1];
    }
    [msg(line + ".")]
}
