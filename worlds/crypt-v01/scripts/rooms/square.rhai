// The Town Square: Hale pays the watch's bounty on the skeletal warden.
// Bring back its notched grave-blade while the ossuary is quiet, and
// `report` to him. The silver and the draught come from the pack's
// `warden_bounty` pool (economy.toml), never from nowhere: each payment is
// in the ledger. The watch pays once for each time the warden is put down
// (the world flag `warden_bounty.paid` holds the death it paid for, from
// ctx.cast.skeletal_warden.deaths), once a day per player, and the pool
// pays three a day.

fn on_report(ctx) {
    let hale = ctx.cast.guard_01;
    if !(hale.present && hale.room == ctx.room.id) {
        return [refuse("There is no one from the watch here to report to.")];
    }
    if !carries(ctx.actor, "warden_blade") {
        return [msg("Hale looks you over. \"Report what? Bring me proof, and the watch pays.\"")];
    }
    let warden = ctx.cast.skeletal_warden;
    if warden.alive {
        return [msg("Hale turns the grave-blade over. \"That thing walks again. Come back when the ossuary is quiet.\"")];
    }
    if "warden_bounty.paid" in ctx.flags && ctx.flags["warden_bounty.paid"] == warden.deaths {
        return [msg("Hale shakes his head. \"The watch already paid for that one. Put it down again, and we'll talk.\"")];
    }
    if done_today(ctx.actor.marks, "bounty_day", ctx.now_ms) {
        return [msg("Hale shakes his head. \"You had the watch's coin today. Once a day, that's the rule.\"")];
    }
    let pay = ctx.config.silver;
    let purse = ctx.pools.warden_bounty;
    if purse.currency < pay || purse.items.healing_draught < 1 {
        return [msg("Hale spreads empty hands. \"The watch's purse is empty till tomorrow. Good work, all the same.\"")];
    }
    // The guards and the reward are admitted together, or not at all.
    [set_flag("warden_bounty.paid", warden.deaths),
     mark("bounty_day", day_utc(ctx.now_ms)),
     msg("Hale weighs the notched grave-blade and nods. \"The ossuary is quiet, then. The watch pays for that.\""),
     narrate(`Hale counts silver into ${ctx.actor.name}'s hand.`),
     reward("warden_bounty", #{ currency: pay, items: #{ healing_draught: 1 } })]
}
