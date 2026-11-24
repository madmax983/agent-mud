//! `mud account …`: the operator's account commands.
//!
//! ```text
//! mud account list
//! mud account reset   <name>   # a temporary password, printed once
//! mud account lock    <name>   # refuses logins; live sessions end within revalidate_secs
//! mud account unlock  <name>
//! mud account signout <name>   # forgets every browser; live sessions end within revalidate_secs
//! ```
//!
//! They work on `accounts.db` while the server runs (SQLite's busy timeout
//! is 5 s here). A reset needs the world file, to know a human holds the
//! name; it never creates one.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Context as _;
use mud_accounts::{AccountError, Accounts, Holder, NameKey};

use crate::config::MudConfig;

/// One account command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountOp {
    /// Every account.
    List,
    /// A temporary password.
    Reset(String),
    /// Refuse logins.
    Lock(String),
    /// Allow logins again.
    Unlock(String),
    /// Forget every browser and end live sessions.
    Signout(String),
}

/// What an in-memory world says.
pub const IN_MEMORY: &str =
    "The world is in memory ([world] db is unset); there are no saved accounts to manage.";

/// What a reset says without a saved world.
pub const NO_WORLD: &str = "The world file has no saved state; there are no characters to reset.";

/// `YYYY-MM-DD HH:MM` UTC.
#[must_use]
pub fn utc(ms: u64) -> String {
    let secs = ms / 1_000;
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3_600,
        rem % 3_600 / 60
    )
}

fn key(name: &str) -> anyhow::Result<NameKey> {
    NameKey::parse(name).ok_or_else(|| anyhow::anyhow!("{}", mud_world::LoginError::Invalid))
}

/// Who holds `name` in the saved world, or `None` without one. Opening the
/// store upgrades it and bumps its boot number, as any open does; a file
/// newer than this code is refused.
async fn saved_holder(
    world: &Path,
    name: &str,
    seed: u64,
    now_ms: u64,
) -> anyhow::Result<Option<Holder>> {
    if !world.exists() {
        return Ok(None);
    }
    let (path, n) = (world.to_path_buf(), name.to_owned());
    tokio::task::spawn_blocking(move || {
        let loaded = mud_world::store::Store::open(&path, seed, now_ms)?.load(0)?;
        let Some(l) = loaded else {
            return Ok(None);
        };
        let owner = l.state.name_owner(&n).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Some(Holder::from(&owner)))
    })
    .await?
}

/// Runs `op` at `now_ms` and returns what to print.
///
/// # Errors
/// The config, the store or the command failed; a refusal (not a human,
/// no such account) is an error too.
pub async fn run(cfg: &MudConfig, op: AccountOp, now_ms: u64) -> anyhow::Result<String> {
    let world = cfg.world.db.as_deref();
    let (path, acfg) = cfg.accounts.to_config(world)?;
    let Some(path) = path else {
        return Ok(IN_MEMORY.to_owned());
    };
    let accounts = Accounts::open(Some(&path), acfg, 5_000, now_ms)
        .await
        .context("opening the accounts")?;
    match op {
        AccountOp::List => {
            let mut out =
                String::from("name  origin  created  last login  browsers  locked  must change");
            for a in accounts.admin_list().await? {
                let _ = write!(
                    out,
                    "\n{}  {}  {}  {}  {}  {}  {}",
                    a.name,
                    a.origin.as_str(),
                    utc(a.created_ms),
                    a.last_login_ms.map_or_else(|| "never".to_owned(), utc),
                    a.devices,
                    if a.locked { "yes" } else { "no" },
                    if a.must_change { "yes" } else { "no" },
                );
            }
            Ok(out)
        }
        AccountOp::Reset(name) => {
            let k = key(&name)?;
            let holder = match world {
                Some(w) => saved_holder(w, &name, cfg.world.seed, now_ms).await?,
                None => None,
            };
            let Some(holder) = holder else {
                return Ok(NO_WORLD.to_owned());
            };
            let pw = accounts
                .admin_reset(&name, &k, holder, now_ms)
                .await
                .map_err(refusal)?;
            Ok(format!(
                "Temporary password for {}: {}\nThey must choose a new one at their next login.",
                name.trim(),
                pw.expose()
            ))
        }
        AccountOp::Lock(ref name) | AccountOp::Unlock(ref name) => {
            let locked = matches!(op, AccountOp::Lock(_));
            accounts
                .admin_set_locked(&key(name)?, locked, now_ms)
                .await
                .map_err(refusal)?;
            Ok(if locked {
                format!(
                    "{} is locked; live sessions end within {} s.",
                    name.trim(),
                    accounts.config().revalidate_secs
                )
            } else {
                format!("{} is unlocked.", name.trim())
            })
        }
        AccountOp::Signout(name) => {
            let n = accounts
                .admin_signout(&key(&name)?, now_ms)
                .await
                .map_err(refusal)?;
            Ok(format!(
                "{} is signed out: {n} remembered browser(s) forgotten; live sessions end within {} s.",
                name.trim(),
                accounts.config().revalidate_secs
            ))
        }
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as `map_err(refusal)`, which hands over the error by value"
)]
fn refusal(e: AccountError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mud_accounts::{Peer, Secret, SignIn};
    use mud_world::{Game, ManualClock, WorldPack};

    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    /// A config whose world and accounts live in `dir`, with ada a saved
    /// human (with an account) and old a saved human without one.
    async fn rig(dir: &Path) -> MudConfig {
        let mut cfg = MudConfig::default();
        cfg.world.db = Some(dir.join("world.db"));
        cfg.accounts.bcrypt_cost = 4;
        let game = Game::open(
            Arc::new(WorldPack::crypt_v01().unwrap()),
            cfg.world.db.as_deref(),
            1,
            Arc::new(ManualClock::new(NOW)),
        )
        .unwrap();
        game.login("ada").unwrap();
        game.login("old").unwrap();
        drop(game);
        let (path, acfg) = cfg.accounts.to_config(cfg.world.db.as_deref()).unwrap();
        let a = Accounts::open(path.as_deref(), acfg, 1_000, NOW)
            .await
            .unwrap();
        let k = NameKey::parse("ada").unwrap();
        a.create(
            "ada",
            &k,
            Holder::Free,
            &Secret::from("lantern-and-jerkin"),
            mud_accounts::NewPurpose::Create,
            Peer::Shared,
            NOW,
        )
        .await
        .unwrap();
        cfg
    }

    #[tokio::test]
    async fn reset_prints_a_password_that_must_be_changed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = rig(dir.path()).await;
        let out = run(&cfg, AccountOp::Reset("old".into()), NOW + 1)
            .await
            .unwrap();
        let (first, second) = out.split_once('\n').unwrap();
        assert_eq!(second, "They must choose a new one at their next login.");
        let pw = first.strip_prefix("Temporary password for old: ").unwrap();
        assert_eq!(pw.len(), 16);
        let (path, acfg) = cfg.accounts.to_config(cfg.world.db.as_deref()).unwrap();
        let a = Accounts::open(path.as_deref(), acfg, 1_000, NOW)
            .await
            .unwrap();
        let k = NameKey::parse("old").unwrap();
        let r = a
            .sign_in(&k, Holder::Human, &Secret::from(pw), Peer::Shared, NOW + 2)
            .await;
        assert!(matches!(r, Ok(SignIn::MustChange(_))), "{r:?}");
    }

    #[tokio::test]
    async fn reset_refuses_an_agent_pc() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = rig(dir.path()).await;
        let e = run(&cfg, AccountOp::Reset("kestrel".into()), NOW)
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "kestrel is not a human character; only human characters have accounts"
        );
        let e = run(&cfg, AccountOp::Reset("x".into()), NOW)
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("a name is"), "{e}");
    }

    #[tokio::test]
    async fn reset_with_an_empty_world_file_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = MudConfig::default();
        cfg.world.db = Some(dir.path().join("nothing-here.db"));
        cfg.accounts.bcrypt_cost = 4;
        let out = run(&cfg, AccountOp::Reset("old".into()), NOW)
            .await
            .unwrap();
        assert_eq!(out, NO_WORLD);
        assert!(
            !dir.path().join("nothing-here.db").exists(),
            "the world file is not created"
        );
        // A world file with no snapshot yet.
        mud_world::store::Store::open(&dir.path().join("empty.db"), 1, NOW).unwrap();
        cfg.world.db = Some(dir.path().join("empty.db"));
        assert_eq!(
            run(&cfg, AccountOp::Reset("old".into()), NOW)
                .await
                .unwrap(),
            NO_WORLD
        );
    }

    #[tokio::test]
    async fn lock_and_signout_end_live_sessions_via_revalidate() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = rig(dir.path()).await;
        let (path, acfg) = cfg.accounts.to_config(cfg.world.db.as_deref()).unwrap();
        let a = Accounts::open(path.as_deref(), acfg, 1_000, NOW)
            .await
            .unwrap();
        let k = NameKey::parse("ada").unwrap();
        let live = match a
            .sign_in(
                &k,
                Holder::Human,
                &Secret::from("lantern-and-jerkin"),
                Peer::Shared,
                NOW,
            )
            .await
            .unwrap()
        {
            SignIn::Ok(x) => x,
            SignIn::MustChange(_) => panic!(),
        };
        assert!(a.revalidate(&live, NOW).await.unwrap());
        let out = run(&cfg, AccountOp::Lock("ada".into()), NOW + 5)
            .await
            .unwrap();
        assert_eq!(out, "ada is locked; live sessions end within 30 s.");
        assert!(!a.revalidate(&live, NOW + 6).await.unwrap());
        assert_eq!(
            run(&cfg, AccountOp::Unlock("ada".into()), NOW + 7)
                .await
                .unwrap(),
            "ada is unlocked."
        );
        let live = match a
            .sign_in(
                &k,
                Holder::Human,
                &Secret::from("lantern-and-jerkin"),
                Peer::Shared,
                NOW + 8,
            )
            .await
            .unwrap()
        {
            SignIn::Ok(x) => x,
            SignIn::MustChange(_) => panic!(),
        };
        let out = run(&cfg, AccountOp::Signout("ada".into()), NOW + 9)
            .await
            .unwrap();
        assert!(out.starts_with("ada is signed out"), "{out}");
        assert!(!a.revalidate(&live, NOW + 10).await.unwrap());
        let e = run(&cfg, AccountOp::Lock("zed".into()), NOW)
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), "there is no account named zed");
    }

    #[tokio::test]
    async fn list_shows_origin_and_never_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = rig(dir.path()).await;
        run(&cfg, AccountOp::Reset("old".into()), NOW)
            .await
            .unwrap();
        let out = run(&cfg, AccountOp::List, NOW).await.unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "name  origin  created  last login  browsers  locked  must change"
        );
        assert_eq!(
            lines[1],
            "ada  created  2027-01-15 08:00  2027-01-15 08:00  0  no  no"
        );
        assert_eq!(lines[2], "old  reset  2027-01-15 08:00  never  0  no  yes");
        assert!(!out.contains("$2b$"));
    }

    #[tokio::test]
    async fn an_in_memory_world_has_no_accounts() {
        let mut cfg = MudConfig::default();
        cfg.world.db = None;
        assert_eq!(run(&cfg, AccountOp::List, NOW).await.unwrap(), IN_MEMORY);
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00");
        assert_eq!(utc(NOW), "2027-01-15 08:00");
    }
}
