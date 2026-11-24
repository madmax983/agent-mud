//! Helpers the server's unit tests share.

use std::sync::Arc;

use mud_accounts::{
    Accounts, AccountsConfig, Authenticated, Holder, NameKey, NewPurpose, Peer, Secret,
};
use mud_world::{Game, ManualClock, World, WorldPack};

/// The password the helpers give every character.
pub const PW: &str = "lantern-and-jerkin";

/// A fresh crypt-v01 game on a manual clock.
pub fn game() -> Arc<Game> {
    Game::new(
        World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1),
        None,
        Arc::new(ManualClock::new(1_800_000_000_000)),
    )
}

/// Accounts in memory at bcrypt cost 4.
pub async fn accounts() -> Arc<Accounts> {
    accounts_with(|_| {}).await
}

/// [`accounts`] with `tweak` applied to the config.
pub async fn accounts_with(tweak: impl FnOnce(&mut AccountsConfig)) -> Arc<Accounts> {
    let mut c = AccountsConfig {
        bcrypt_cost: 4,
        ..AccountsConfig::default()
    };
    tweak(&mut c);
    Arc::new(Accounts::open(None, c, 0, 1_800_000_000_000).await.unwrap())
}

/// A local address.
pub fn home() -> Peer {
    Peer::Addr("127.0.0.1".parse().unwrap())
}

/// Makes `name` an account with [`PW`] (or signs it in if it has one),
/// as the login dialogue would.
pub async fn auth(accounts: &Accounts, game: &Game, name: &str) -> Authenticated {
    let key = NameKey::parse(name).unwrap();
    let holder = Holder::from(&game.name_owner(name).unwrap());
    let now = game.now_ms();
    if let Ok(a) = accounts
        .create(
            name,
            &key,
            holder,
            &Secret::from(PW),
            NewPurpose::Create,
            home(),
            now,
        )
        .await
    {
        return a;
    }
    match accounts
        .sign_in(&key, holder, &Secret::from(PW), home(), now)
        .await
        .unwrap()
    {
        mud_accounts::SignIn::Ok(a) => a,
        mud_accounts::SignIn::MustChange(_) => panic!("{name} must change its password"),
    }
}
