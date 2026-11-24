// The bone charm, loot with no use until now: it hums near the restless dead.
fn on_use(ctx) {
    let w = ctx.cast.skeletal_warden;
    if ctx.target != () && ctx.target.id == "skeletal_warden" {
        return [msg("You hold the charm up. The warden's skull tilts, as if it hears a name it once had.")];
    }
    if w.present && w.room == ctx.room.id {
        return [msg("The charm buzzes in your fist like a trapped wasp. The warden's skull turns toward the sound."),
                narrate(`${ctx.actor.name}'s fist rattles with something small and angry.`)];
    }
    if w.alive {
        return [msg("The charm is faintly warm, and it leans, very slightly, toward the crypt.")];
    }
    [msg("The charm lies cold and quiet in your palm.")]
}
