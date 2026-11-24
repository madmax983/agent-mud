// Lifting ore from under the warden's eye costs blood on a failed DX roll.
fn on_take(ctx) {
    if !warden_here(ctx) {
        return [];
    }
    let r = check(ctx.actor.attrs.dx + ctx.config.modifier);
    if r.success {
        return [msg("You snatch the ore as the grave-blade whistles past your knuckles.")];
    }
    [msg("The grave-blade bites your wrist as you pull the ore free."),
     narrate(`The warden's blade flicks at ${ctx.actor.name}'s hand.`),
     hurt("1d-1")]
}
