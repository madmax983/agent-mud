//! The account service, rule by rule, at bcrypt cost 4.

use std::net::IpAddr;
use std::sync::Arc;

use mud_world::{NameOwner, World, WorldPack};

use super::*;

const T0: u64 = 1_800_000_000_000;

fn cfg() -> AccountsConfig {
    AccountsConfig {
        bcrypt_cost: 4,
        ..AccountsConfig::default()
    }
}

async fn open_with(c: AccountsConfig) -> Accounts {
    Accounts::open(None, c, 0, T0).await.unwrap()
}

async fn accounts() -> Accounts {
    open_with(cfg()).await
}

fn key(s: &str) -> NameKey {
    NameKey::parse(s).unwrap()
}

fn addr(s: &str) -> Peer {
    Peer::Addr(s.parse::<IpAddr>().unwrap())
}

fn pw(s: &str) -> Secret {
    Secret::from(s)
}

const ADA_PW: &str = "lantern-and-jerkin";

async fn make(a: &Accounts, name: &str, password: &str, peer: Peer, now: u64) -> Authenticated {
    a.create(
        name,
        &key(name),
        Holder::Free,
        &pw(password),
        NewPurpose::Create,
        peer,
        now,
    )
    .await
    .unwrap()
}

async fn sign(
    a: &Accounts,
    name: &str,
    password: &str,
    holder: Holder,
    peer: Peer,
    now: u64,
) -> Result<SignIn, AuthError> {
    a.sign_in(&key(name), holder, &pw(password), peer, now)
        .await
}

fn signed(r: Result<SignIn, AuthError>) -> Authenticated {
    match r {
        Ok(SignIn::Ok(a)) => a,
        other => panic!("not signed in: {other:?}"),
    }
}

#[tokio::test]
async fn a_new_account_logs_in_with_its_password_only() {
    let a = accounts().await;
    let home = addr("10.0.0.1");
    assert_eq!(
        a.route(&key("ada"), Holder::Free, T0).await.unwrap(),
        Route::Create
    );
    let made = make(&a, "Ada", ADA_PW, home, T0).await;
    assert_eq!((made.name(), made.via()), ("Ada", AuthVia::Created));
    assert_eq!(made.entity(), &EntityId::from("pc_ada"));
    assert_eq!(
        a.route(&key("ada"), Holder::Human, T0).await.unwrap(),
        Route::Password
    );
    let back = signed(sign(&a, "ADA", ADA_PW, Holder::Human, home, T0 + 1).await);
    assert_eq!(
        (back.name(), back.via(), back.since_ms()),
        ("Ada", AuthVia::Password, T0 + 1)
    );
    assert!(back.series().is_none());
    let wrong = sign(
        &a,
        "ada",
        "lantern-and-jerkin!",
        Holder::Human,
        home,
        T0 + 2,
    )
    .await;
    assert!(matches!(wrong, Err(AuthError::WrongPassword)));
    let s = a.stats();
    assert_eq!((s.ok, s.failed, s.created), (2, 1, 1));
}

#[tokio::test]
async fn npc_names_are_refused_and_agent_pcs_ask_for_a_password() {
    let a = accounts().await;
    let w = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1);
    let holder = |n: &str| Holder::from(&w.name_owner(n).unwrap());
    for npc in ["marta", "hale"] {
        assert_eq!(
            a.route(&key(npc), holder(npc), T0).await.unwrap(),
            Route::Refuse
        );
    }
    assert_eq!(holder("kestrel"), Holder::OtherPlayer);
    assert_eq!(
        a.route(&key("kestrel"), holder("kestrel"), T0)
            .await
            .unwrap(),
        Route::Password
    );
    assert!(matches!(w.name_owner("ada"), Ok(NameOwner::Free(_))));
}

#[tokio::test]
async fn every_password_answer_costs_one_verify() {
    let sealed = open_with(AccountsConfig {
        legacy: Legacy::Sealed,
        ..cfg()
    })
    .await;
    let a = accounts().await;
    let home = addr("10.0.0.1");
    make(&a, "ada", ADA_PW, home, T0).await;
    make(&a, "bo", ADA_PW, home, T0).await;
    a.admin_set_locked(&key("bo"), true, T0).await.unwrap();
    let mut answers: Vec<(&str, Result<SignIn, AuthError>, u64)> = Vec::new();
    for (what, name, password, holder) in [
        ("right", "ada", ADA_PW, Holder::Human),
        ("wrong", "ada", "not-the-password", Holder::Human),
        (
            "kestrel",
            "kestrel",
            "not-the-password",
            Holder::OtherPlayer,
        ),
        ("locked", "bo", ADA_PW, Holder::Human),
    ] {
        let before = a.verifications();
        let r = sign(&a, name, password, holder, addr("10.0.0.2"), T0 + 1).await;
        answers.push((what, r, a.verifications() - before));
    }
    let long = "x".repeat(100);
    let before = a.verifications();
    let r = sign(&a, "ada", &long, Holder::Human, addr("10.0.0.3"), T0 + 1).await;
    answers.push(("100 bytes", r, a.verifications() - before));
    let before = sealed.verifications();
    let r = sign(&sealed, "old", "not-the-password", Holder::Human, home, T0).await;
    answers.push(("sealed legacy", r, sealed.verifications() - before));
    for (what, _, n) in &answers {
        assert_eq!(*n, 1, "{what}");
    }
    assert!(matches!(answers[0].1, Ok(SignIn::Ok(_))));
    assert!(matches!(answers[3].1, Err(AuthError::Locked)));
    for i in [1, 2, 4, 5] {
        assert!(
            matches!(answers[i].1, Err(AuthError::WrongPassword)),
            "{}",
            answers[i].0
        );
    }
    let before = a.verifications();
    let claim = a
        .answer_claim(&key("old"), "old", &pw("a-fine-new-password"), home, T0)
        .await
        .unwrap();
    assert_eq!(claim, Ok(()));
    assert_eq!(a.verifications() - before, 1, "a claim answer");
}

/// Plays 25 wrong answers to `name` from 10.0.0.9 on a server where ada
/// has an account made from 10.0.0.1.
async fn wrong_answers(name: &str, holder: Holder) -> Vec<String> {
    let a = accounts().await;
    make(&a, "ada", ADA_PW, addr("10.0.0.1"), T0).await;
    let mut out = Vec::new();
    for i in 0..25 {
        let r = sign(
            &a,
            name,
            "not-the-password",
            holder,
            addr("10.0.0.9"),
            T0 + i * 700,
        )
        .await;
        out.push(match r {
            Ok(_) => "ok".to_owned(),
            Err(e) => e.to_string(),
        });
    }
    out
}

#[tokio::test]
async fn an_agent_pc_and_a_human_fail_identically() {
    let human = wrong_answers("ada", Holder::Human).await;
    let agent = wrong_answers("kestrel", Holder::OtherPlayer).await;
    assert_eq!(human, agent);
    assert!(
        human
            .iter()
            .any(|t| t.starts_with("Too many failed logins")),
        "{human:?}"
    );
}

#[tokio::test]
async fn a_held_name_is_refused_without_hashing_and_frees_after_the_delay() {
    let a = accounts().await;
    make(&a, "ada", ADA_PW, addr("10.0.0.1"), T0).await;
    let away = addr("10.0.0.9");
    // name_address: 5 free, then 1 s.
    for _ in 0..6 {
        let r = sign(&a, "ada", "nope-nope-nope", Holder::Human, away, T0).await;
        assert!(matches!(r, Err(AuthError::WrongPassword)));
    }
    let before = a.verifications();
    let held = sign(&a, "ada", ADA_PW, Holder::Human, away, T0 + 10).await;
    assert!(
        matches!(held, Err(AuthError::NameHeld { wait_ms: 990 })),
        "{held:?}"
    );
    assert_eq!(a.verifications(), before, "no hashing while held");
    assert!(matches!(
        a.check_name(&key("ada"), away, T0 + 10).await,
        Err(AuthError::NameHeld { .. })
    ));
    signed(sign(&a, "ada", ADA_PW, Holder::Human, away, T0 + 1_000).await);
    assert!(a.stats().held >= 2);
}

#[tokio::test]
async fn attackers_elsewhere_cannot_hold_the_owner_at_a_known_address() {
    let a = accounts().await;
    let home = addr("10.0.0.1");
    make(&a, "ada", ADA_PW, home, T0).await;
    signed(sign(&a, "ada", ADA_PW, Holder::Human, home, T0).await);
    let blocks = ["2001:db8:1::5", "2001:db8:2::5", "2001:db8:3::5"];
    let mut now = T0;
    for i in 0..60 {
        let p = addr(blocks[i % 3]);
        loop {
            match sign(&a, "ada", "guess-guess-guess", Holder::Human, p, now).await {
                Err(AuthError::WrongPassword) => break,
                Err(
                    AuthError::NameHeld { wait_ms }
                    | AuthError::AddressHeld { wait_ms }
                    | AuthError::WorkHeld { wait_ms },
                ) => now += wait_ms,
                other => panic!("{other:?}"),
            }
        }
    }
    signed(sign(&a, "ada", ADA_PW, Holder::Human, home, now).await);
    let elsewhere = sign(&a, "ada", ADA_PW, Holder::Human, addr("10.0.0.9"), now).await;
    assert!(
        matches!(elsewhere, Err(AuthError::NameHeld { .. })),
        "{elsewhere:?}"
    );
}

#[tokio::test]
async fn success_does_not_reset_the_address_or_name_keys() {
    let a = accounts().await;
    let mal = addr("10.0.0.5");
    make(&a, "mal", ADA_PW, mal, T0).await;
    // 21 misses from mal's address over names it never had: past free.
    for i in 0..21 {
        let _ = sign(
            &a,
            &format!("zz{i}"),
            "guess-guess-1",
            Holder::Free,
            mal,
            T0,
        )
        .await;
    }
    signed(sign(&a, "mal", ADA_PW, Holder::Human, mal, T0).await);
    let after = sign(&a, "yy", "guess-guess-1", Holder::Free, mal, T0).await;
    assert!(
        matches!(after, Err(AuthError::AddressHeld { .. })),
        "{after:?}"
    );
    // The name key: misses on ada from many addresses, then ada at home.
    make(&a, "ada", ADA_PW, addr("10.0.0.1"), T0).await;
    for i in 0..11 {
        let _ = sign(
            &a,
            "ada",
            "guess-guess-1",
            Holder::Human,
            addr(&format!("10.1.0.{i}")),
            T0,
        )
        .await;
    }
    signed(sign(&a, "ada", ADA_PW, Holder::Human, addr("10.0.0.1"), T0).await);
    let away = sign(&a, "ada", ADA_PW, Holder::Human, addr("10.2.0.1"), T0).await;
    assert!(matches!(away, Err(AuthError::NameHeld { .. })), "{away:?}");
}

#[tokio::test]
async fn claim_answers_are_charged_to_the_address() {
    let a = accounts().await;
    let p = addr("10.0.0.7");
    // work: 60 free; the 61st is charged and sets the first delay, so the
    // next answer is held (`free + 1` pass at one instant).
    for i in 0..61 {
        let r = a
            .answer_claim(&key("old"), "old", &pw("a-fine-new-password"), p, T0)
            .await;
        assert!(matches!(r, Ok(Ok(()))), "{i}: {r:?}");
    }
    let before = a.verifications();
    let r = a
        .answer_claim(&key("old"), "old", &pw("a-fine-new-password"), p, T0)
        .await;
    assert!(matches!(r, Err(AuthError::WorkHeld { .. })), "{r:?}");
    assert_eq!(a.verifications(), before, "no verify for a held answer");
    let weak = a
        .answer_claim(&key("old"), "old", &pw("old12345"), addr("10.0.0.8"), T0)
        .await;
    assert!(
        matches!(weak, Ok(Err(PasswordRejection::ContainsName))),
        "{weak:?}"
    );
}

#[tokio::test]
async fn legacy_humans_are_claimed_in_the_window_and_sealed_after() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    assert!(a.legacy_open(T0));
    assert_eq!(
        a.route(&key("old"), Holder::Human, T0).await.unwrap(),
        Route::Claim
    );
    let got = a
        .create(
            "old",
            &key("old"),
            Holder::Human,
            &pw("a-fine-new-password"),
            NewPurpose::Claim,
            p,
            T0,
        )
        .await
        .unwrap();
    assert_eq!(got.via(), AuthVia::Claimed);
    let list = a.admin_list().await.unwrap();
    assert_eq!(
        (list[0].name.as_str(), list[0].origin),
        ("old", Origin::Claimed)
    );
    let shut = T0 + 30 * DAY_MS;
    assert!(!a.legacy_open(shut));
    assert_eq!(a.legacy_closes_ms(), Some(shut));
    assert_eq!(
        a.route(&key("later"), Holder::Human, shut).await.unwrap(),
        Route::Password
    );
    let r = sign(&a, "later", "a-fine-new-password", Holder::Human, p, shut).await;
    assert!(matches!(r, Err(AuthError::WrongPassword)));
    let again = a
        .create(
            "old",
            &key("old"),
            Holder::Human,
            &pw("another-password"),
            NewPurpose::Claim,
            p,
            T0,
        )
        .await;
    assert!(matches!(again, Err(AuthError::NameJustTaken)), "{again:?}");
    let forever = open_with(AccountsConfig {
        legacy_claim_days: 0,
        ..cfg()
    })
    .await;
    assert!(forever.legacy_open(u64::MAX));
    let humans = [EntityId::from("pc_old"), EntityId::from("pc_later")];
    assert_eq!(a.legacy_count(&humans).await.unwrap(), 1);
}

#[tokio::test]
async fn reset_forces_a_new_password_once() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    let err = a
        .admin_reset("kestrel", &key("kestrel"), Holder::OtherPlayer, T0)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "kestrel is not a human character; only human characters have accounts"
    );
    let temp = a
        .admin_reset("old", &key("old"), Holder::Human, T0)
        .await
        .unwrap();
    assert_eq!(temp.len_bytes(), 16);
    assert!(temp.expose().bytes().all(|b| TEMP_ALPHABET.contains(&b)));
    assert_eq!(a.admin_list().await.unwrap()[0].origin, Origin::Reset);
    assert!(a.admin_list().await.unwrap()[0].must_change);
    let ticket = match sign(&a, "old", temp.expose(), Holder::Human, p, T0 + 1).await {
        Ok(SignIn::MustChange(t)) => t,
        other => panic!("{other:?}"),
    };
    assert_eq!(ticket.name(), "old");
    let weak = a.finish_reset(ticket, &pw("short"), p, T0 + 2).await;
    assert!(matches!(
        weak,
        Err(AuthError::Weak(PasswordRejection::TooShort { min: 8 }))
    ));
    let ticket = match sign(&a, "old", temp.expose(), Holder::Human, p, T0 + 3).await {
        Ok(SignIn::MustChange(t)) => t,
        other => panic!("{other:?}"),
    };
    let done = a
        .finish_reset(ticket, &pw("a-fine-new-password"), p, T0 + 4)
        .await
        .unwrap();
    assert_eq!(done.via(), AuthVia::Reset);
    signed(sign(&a, "old", "a-fine-new-password", Holder::Human, p, T0 + 5).await);
    let stale = sign(&a, "old", temp.expose(), Holder::Human, p, T0 + 6).await;
    assert!(matches!(stale, Err(AuthError::WrongPassword)));
    // A second reset of an existing account keeps its origin.
    a.admin_reset("old", &key("old"), Holder::Human, T0 + 7)
        .await
        .unwrap();
    assert_eq!(a.admin_list().await.unwrap()[0].origin, Origin::Reset);
}

async fn reset_ticket(a: &Accounts, name: &str, p: Peer, now: u64) -> (Secret, ResetTicket) {
    let temp = a
        .admin_reset(name, &key(name), Holder::Human, now)
        .await
        .unwrap();
    match sign(a, name, temp.expose(), Holder::Human, p, now + 1).await {
        Ok(SignIn::MustChange(t)) => (temp, t),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_reset_ticket_is_void_once_the_account_changes() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    // A second reset: the leaked first ticket no longer sets a password.
    let (_, leaked) = reset_ticket(&a, "old", p, T0).await;
    let second = a
        .admin_reset("old", &key("old"), Holder::Human, T0 + 10)
        .await
        .unwrap();
    let r = a
        .finish_reset(leaked, &pw("the-attackers-choice"), p, T0 + 11)
        .await;
    assert!(matches!(r, Err(AuthError::ResetStale)), "{r:?}");
    assert!(matches!(
        sign(&a, "old", "the-attackers-choice", Holder::Human, p, T0 + 12).await,
        Err(AuthError::WrongPassword)
    ));
    assert!(matches!(
        sign(&a, "old", second.expose(), Holder::Human, p, T0 + 13).await,
        Ok(SignIn::MustChange(_))
    ));
    // A lock: the ticket is refused as locked, and nothing is written.
    let (_, ticket) = reset_ticket(&a, "bo", p, T0 + 20).await;
    a.admin_set_locked(&key("bo"), true, T0 + 22).await.unwrap();
    let r = a
        .finish_reset(ticket, &pw("the-attackers-choice"), p, T0 + 23)
        .await;
    assert!(matches!(r, Err(AuthError::Locked)), "{r:?}");
    assert!(
        a.admin_list()
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == "bo" && s.must_change)
    );
    // A ticket redeemed once cannot be replayed through a second sign-in's
    // ticket either: after a finish, must_change is off.
    let (_, first) = reset_ticket(&a, "cy", p, T0 + 30).await;
    let (_, twin) = {
        let list = a.admin_list().await.unwrap();
        assert!(list.iter().any(|s| s.name == "cy" && s.must_change));
        let temp_again = a
            .admin_reset("cy", &key("cy"), Holder::Human, T0 + 31)
            .await
            .unwrap();
        match sign(&a, "cy", temp_again.expose(), Holder::Human, p, T0 + 32).await {
            Ok(SignIn::MustChange(t)) => (temp_again, t),
            other => panic!("{other:?}"),
        }
    };
    assert!(matches!(
        a.finish_reset(first, &pw("a-fine-new-password"), p, T0 + 33)
            .await,
        Err(AuthError::ResetStale)
    ));
    a.finish_reset(twin, &pw("a-fine-new-password"), p, T0 + 34)
        .await
        .unwrap();
}

/// An account whose name an agent PC or an NPC holds (a pack once
/// dropped the cast and a human took the name) never signs in, even with
/// the right password: it reads as a wrong one, at one verify, and counts
/// as one.
#[tokio::test]
async fn a_right_password_never_binds_an_agent_pc_or_an_npc() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    make(&a, "kestrel", ADA_PW, p, T0).await;
    for (i, holder) in [Holder::OtherPlayer, Holder::NonPlayer]
        .into_iter()
        .enumerate()
    {
        let (before, failed) = (a.verifications(), a.stats().failed);
        let r = sign(&a, "kestrel", ADA_PW, holder, p, T0 + 1 + i as u64).await;
        assert!(matches!(r, Err(AuthError::WrongPassword)), "{r:?}");
        assert_eq!(a.verifications(), before + 1);
        assert_eq!(a.stats().failed, failed + 1);
        let minted = Accounts::mint(&key("kestrel"), "kestrel", holder, AuthVia::Password, T0);
        assert!(
            matches!(minted, Err(AuthError::WrongPassword)),
            "{minted:?}"
        );
    }
    signed(sign(&a, "kestrel", ADA_PW, Holder::Human, p, T0 + 5).await);
}

#[tokio::test]
async fn a_locked_account_is_refused_only_after_the_right_password() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    make(&a, "ada", ADA_PW, p, T0).await;
    a.admin_set_locked(&key("ada"), true, T0).await.unwrap();
    assert!(matches!(
        sign(&a, "ada", "wrong-wrong-wrong", Holder::Human, p, T0).await,
        Err(AuthError::WrongPassword)
    ));
    assert!(matches!(
        sign(&a, "ada", ADA_PW, Holder::Human, p, T0).await,
        Err(AuthError::Locked)
    ));
    a.admin_set_locked(&key("ada"), false, T0).await.unwrap();
    signed(sign(&a, "ada", ADA_PW, Holder::Human, p, T0).await);
    assert!(matches!(
        a.admin_set_locked(&key("zed"), true, T0).await,
        Err(AccountError::NoAccount(n)) if n == "zed"
    ));
    assert!(a.admin_signout(&key("zed"), T0).await.is_err());
}

#[tokio::test]
async fn resume_is_never_throttled() {
    let a = accounts().await;
    let home = addr("10.0.0.1");
    let mut ada = make(&a, "ada", ADA_PW, home, T0).await;
    let tok = a.remember(&mut ada, true, T0).await.unwrap().unwrap();
    assert!(tok.persist);
    let attacker = addr("10.0.0.9");
    let mut now = T0;
    for _ in 0..80 {
        match sign(&a, "ada", "guess-guess-guess", Holder::Human, attacker, now).await {
            Err(
                AuthError::WorkHeld { .. }
                | AuthError::NameHeld { .. }
                | AuthError::AddressHeld { .. },
            ) => {}
            _ => now += 1,
        }
    }
    assert!(a.check_name(&key("ada"), attacker, now).await.is_err());
    let back = a.resume(&tok.value, attacker, now).await.unwrap();
    assert_eq!((back.name(), back.via()), ("ada", AuthVia::Resumed));
    assert_eq!(back.series(), Some(&tok.series));
}

#[tokio::test]
async fn a_resume_token_is_static_and_a_wrong_token_signs_the_device_out() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    let mut ada = make(&a, "ada", ADA_PW, p, T0).await;
    let tok = a.remember(&mut ada, false, T0).await.unwrap().unwrap();
    assert!(!tok.persist);
    for i in 1..=2 {
        a.resume(&tok.value, p, T0 + i).await.unwrap();
    }
    let forged = Secret::new(format!("{}:not-the-token", tok.series.as_str()));
    assert!(matches!(
        a.resume(&forged, p, T0 + 3).await,
        Err(AuthError::ResumeRejected)
    ));
    assert!(
        matches!(
            a.resume(&tok.value, p, T0 + 4).await,
            Err(AuthError::ResumeRejected)
        ),
        "signed out"
    );
    assert!(matches!(
        a.resume(&pw("garbage"), p, T0).await,
        Err(AuthError::ResumeRejected)
    ));
    assert!(matches!(
        a.resume(&pw("x:y"), p, T0).await,
        Err(AuthError::ResumeRejected)
    ));
    assert_eq!(a.stats().resume_rejected, 4);
    assert_eq!(a.stats().resumed, 2);
    let mut again = signed(sign(&a, "ada", ADA_PW, Holder::Human, p, T0).await);
    let t2 = a.remember(&mut again, true, T0).await.unwrap().unwrap();
    a.forget(&t2.series).await.unwrap();
    assert!(
        a.resume(&t2.value, p, T0).await.is_err(),
        "logout forgets it"
    );
}

#[tokio::test]
async fn tab_tokens_expire_after_reconnect_minutes_and_device_tokens_slide() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    let mut ada = make(&a, "ada", ADA_PW, p, T0).await;
    let tab = a.remember(&mut ada, false, T0).await.unwrap().unwrap();
    let dev = a.remember(&mut ada, true, T0).await.unwrap().unwrap();
    let min = 60_000;
    assert!(
        a.resume(&tab.value, p, T0 + 14 * min).await.is_ok(),
        "slid to +29 min"
    );
    assert!(
        a.resume(&tab.value, p, T0 + 44 * min).await.is_err(),
        "expired"
    );
    let day = DAY_MS;
    assert!(a.resume(&dev.value, p, T0 + 29 * day).await.is_ok());
    assert!(a.resume(&dev.value, p, T0 + 58 * day).await.is_ok(), "slid");
    assert!(a.resume(&dev.value, p, T0 + 89 * day).await.is_err());
    let off = open_with(AccountsConfig {
        remember_days: 0,
        reconnect_minutes: 0,
        ..cfg()
    })
    .await;
    let mut bo = make(&off, "bo", ADA_PW, p, T0).await;
    assert!(off.remember(&mut bo, true, T0).await.unwrap().is_none());
    let tab_only = open_with(AccountsConfig {
        remember_days: 0,
        ..cfg()
    })
    .await;
    let mut cy = make(&tab_only, "cy", ADA_PW, p, T0).await;
    assert!(
        !tab_only
            .remember(&mut cy, true, T0)
            .await
            .unwrap()
            .unwrap()
            .persist
    );
}

#[tokio::test]
async fn revalidate_slides_a_live_tab_token() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    let mut ada = make(&a, "ada", ADA_PW, p, T0).await;
    let tab = a.remember(&mut ada, false, T0).await.unwrap().unwrap();
    let min = 60_000;
    // Ticks every 5 minutes for an hour keep the tab token alive.
    for i in 1..=12 {
        assert!(a.revalidate(&ada, T0 + i * 5 * min).await.unwrap());
    }
    assert!(a.resume(&tab.value, p, T0 + 70 * min).await.is_ok());
}

#[tokio::test]
async fn changing_the_password_ends_other_sessions_but_not_this_one() {
    let a = accounts().await;
    let p = addr("10.0.0.1");
    let mut mine = make(&a, "ada", ADA_PW, p, T0).await;
    let my_tok = a.remember(&mut mine, true, T0).await.unwrap().unwrap();
    let mut other = signed(sign(&a, "ada", ADA_PW, Holder::Human, p, T0 + 1).await);
    let other_tok = a.remember(&mut other, true, T0 + 1).await.unwrap().unwrap();
    let wrong = a
        .change_password(
            &mut mine,
            &pw("nope-nope-nope"),
            &pw("brand-new-words"),
            p,
            T0 + 5,
        )
        .await;
    assert!(matches!(wrong, Err(AuthError::WrongPassword)));
    let weak = a
        .change_password(&mut mine, &pw(ADA_PW), &pw("ada"), p, T0 + 6)
        .await;
    assert!(matches!(weak, Err(AuthError::Weak(_))));
    a.change_password(&mut mine, &pw(ADA_PW), &pw("brand-new-words"), p, T0 + 10)
        .await
        .unwrap();
    assert_eq!(mine.since_ms(), T0 + 10);
    assert!(a.revalidate(&mine, T0 + 11).await.unwrap());
    assert!(!a.revalidate(&other, T0 + 11).await.unwrap());
    assert!(a.resume(&my_tok.value, p, T0 + 12).await.is_ok());
    assert!(a.resume(&other_tok.value, p, T0 + 12).await.is_err());
    signed(sign(&a, "ada", "brand-new-words", Holder::Human, p, T0 + 13).await);
}

#[tokio::test]
async fn revalidate_sees_locks_resets_signouts_and_password_changes_but_not_expiry() {
    let a = open_with(AccountsConfig {
        reconnect_minutes: 15,
        ..cfg()
    })
    .await;
    let p = addr("10.0.0.1");
    let mut ada = make(&a, "ada", ADA_PW, p, T0).await;
    a.remember(&mut ada, false, T0).await.unwrap();
    assert!(
        a.revalidate(&ada, T0 + DAY_MS).await.unwrap(),
        "expiry never ends a live session"
    );
    a.admin_set_locked(&key("ada"), true, T0 + 1).await.unwrap();
    assert!(!a.revalidate(&ada, T0 + 2).await.unwrap());
    a.admin_set_locked(&key("ada"), false, T0 + 3)
        .await
        .unwrap();
    let fresh = signed(sign(&a, "ada", ADA_PW, Holder::Human, p, T0 + 4).await);
    assert!(a.revalidate(&fresh, T0 + 5).await.unwrap());
    assert_eq!(a.admin_signout(&key("ada"), T0 + 6).await.unwrap(), 0);
    assert!(!a.revalidate(&fresh, T0 + 7).await.unwrap());
    let fresh = signed(sign(&a, "ada", ADA_PW, Holder::Human, p, T0 + 8).await);
    a.admin_reset("ada", &key("ada"), Holder::Human, T0 + 9)
        .await
        .unwrap();
    assert!(!a.revalidate(&fresh, T0 + 10).await.unwrap());
    let gone = Authenticated {
        key: key("zed"),
        ..fresh
    };
    assert!(!a.revalidate(&gone, T0 + 11).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_creation_of_one_name_makes_one_account() {
    let a = Arc::new(accounts().await);
    let mut tasks = Vec::new();
    for i in 0..4u8 {
        let a = Arc::clone(&a);
        tasks.push(tokio::spawn(async move {
            a.create(
                "ada",
                &key("ada"),
                Holder::Free,
                &pw(ADA_PW),
                NewPurpose::Create,
                addr(&format!("10.0.0.{i}")),
                T0,
            )
            .await
        }));
    }
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(AuthError::NameJustTaken) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(a.admin_list().await.unwrap().len(), 1);
}

/// Guesses that start together from many addresses are gated and charged
/// one at a time: the name key admits at most `free + 1` at one instant,
/// as the proved rate bound says, however many run at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_guesses_from_many_addresses_share_the_name_key() {
    let mut c = cfg();
    c.throttle.name.free = 0;
    let a = Arc::new(open_with(c).await);
    make(&a, "ada", ADA_PW, addr("10.0.0.1"), T0).await;
    let start = Arc::new(tokio::sync::Barrier::new(32));
    let mut tasks = Vec::new();
    for i in 0..32u8 {
        let (a, start) = (Arc::clone(&a), Arc::clone(&start));
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            sign(
                &a,
                "ada",
                "guess-guess-guess",
                Holder::Human,
                addr(&format!("10.9.0.{i}")),
                T0 + 1,
            )
            .await
        }));
    }
    let mut wrong = 0;
    for t in tasks {
        match t.await.unwrap() {
            Err(AuthError::WrongPassword) => wrong += 1,
            Err(AuthError::NameHeld { .. }) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(wrong, 1, "free + 1 at one instant");
    // The owner at home is not held: a right password from a known pair
    // skips the name key, and an earlier success there made it known.
    signed(sign(&a, "ada", ADA_PW, Holder::Human, addr("10.0.0.1"), T0 + 2).await);
}

/// A right password from a new address is not left charged to the loose
/// keys: only failures count there.
#[tokio::test]
async fn a_right_password_is_not_counted_as_a_failure() {
    let mut c = cfg();
    c.throttle.name.free = 1;
    c.throttle.address.free = 1;
    let a = open_with(c).await;
    make(&a, "ada", ADA_PW, addr("10.0.0.1"), T0).await;
    for i in 0..5u8 {
        signed(
            sign(
                &a,
                "ada",
                ADA_PW,
                Holder::Human,
                addr(&format!("10.3.0.{i}")),
                T0,
            )
            .await,
        );
    }
    // Five first logins from five addresses: the name key never filled.
    let r = sign(
        &a,
        "ada",
        "guess-guess-guess",
        Holder::Human,
        addr("10.4.0.1"),
        T0,
    )
    .await;
    assert!(matches!(r, Err(AuthError::WrongPassword)), "{r:?}");
}

/// Creations that start together from one address count against its cap
/// before any of them hashes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creations_from_one_address_respect_the_cap() {
    let a = Arc::new(
        open_with(AccountsConfig {
            new_characters_per_address_per_day: 1,
            ..cfg()
        })
        .await,
    );
    let start = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for i in 0..8u8 {
        let (a, start) = (Arc::clone(&a), Arc::clone(&start));
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            let name = format!("n{i}");
            a.create(
                &name,
                &key(&name),
                Holder::Free,
                &pw(ADA_PW),
                NewPurpose::Create,
                addr("10.0.0.5"),
                T0,
            )
            .await
        }));
    }
    let mut made = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => made += 1,
            Err(AuthError::NewPerAddress) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(made, 1);
    assert_eq!(a.admin_list().await.unwrap().len(), 1);
}

/// A creation that fails after its slot was taken gives the slot back.
#[tokio::test]
async fn a_failed_creation_frees_its_slot() {
    let a = open_with(AccountsConfig {
        new_characters_per_address_per_day: 1,
        ..cfg()
    })
    .await;
    let p = addr("10.0.0.5");
    let weak = a
        .create(
            "ada",
            &key("ada"),
            Holder::Free,
            &pw("short"),
            NewPurpose::Create,
            p,
            T0,
        )
        .await;
    assert!(matches!(weak, Err(AuthError::Weak(_))), "{weak:?}");
    make(&a, "ada", ADA_PW, p, T0).await;
    // The name is taken now: the second try loses the race, not the slot.
    let taken = a
        .create(
            "ada",
            &key("ada"),
            Holder::Free,
            &pw(ADA_PW),
            NewPurpose::Create,
            addr("10.0.0.6"),
            T0,
        )
        .await;
    assert!(matches!(taken, Err(AuthError::NameJustTaken)), "{taken:?}");
    make(&a, "bo", ADA_PW, addr("10.0.0.6"), T0).await;
}

#[tokio::test]
async fn new_characters_follow_allow_new_and_the_caps() {
    let closed = open_with(AccountsConfig {
        allow_new: false,
        ..cfg()
    })
    .await;
    assert!(matches!(
        closed.may_create(Peer::Shared, T0),
        Err(AuthError::ClosedToNew)
    ));
    let a = open_with(AccountsConfig {
        new_characters_per_address_per_day: 1,
        new_characters_per_day: 2,
        ..cfg()
    })
    .await;
    make(&a, "ada", ADA_PW, addr("10.0.0.1"), T0).await;
    assert!(matches!(
        a.may_create(addr("10.0.0.1"), T0),
        Err(AuthError::NewPerAddress)
    ));
    make(&a, "bo", ADA_PW, addr("10.0.0.2"), T0).await;
    assert!(matches!(
        a.may_create(addr("10.0.0.3"), T0),
        Err(AuthError::NewPerDay)
    ));
    let r = a
        .create(
            "cy",
            &key("cy"),
            Holder::Free,
            &pw(ADA_PW),
            NewPurpose::Create,
            addr("10.0.0.3"),
            T0,
        )
        .await;
    assert!(matches!(r, Err(AuthError::NewPerDay)));
}

#[tokio::test]
async fn hash_slots_bound_concurrency() {
    let h = Arc::new(Hasher::new(
        10,
        [0; 32],
        std::collections::BTreeMap::new(),
        1,
    ));
    assert_eq!(h.free_slots(), 1);
    let busy = Arc::clone(&h);
    let job = tokio::spawn(async move { busy.hash(&pw("lantern-and-jerkin")).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while h.free_slots() > 0 && !job.is_finished() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert!(
        job.is_finished() || h.free_slots() == 0,
        "one job holds the one slot"
    );
    job.await.unwrap().unwrap();
    assert_eq!(h.free_slots(), 1);
}

#[tokio::test]
async fn a_file_store_keeps_accounts_and_the_legacy_clock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("accounts.db");
    let a = Accounts::open(Some(&path), cfg(), 1_000, T0).await.unwrap();
    assert!(a.created_new());
    make(&a, "ada", ADA_PW, addr("10.0.0.1"), T0).await;
    drop(a);
    let b = Accounts::open(
        Some(&path),
        AccountsConfig {
            bcrypt_cost: 5,
            ..cfg()
        },
        1_000,
        T0 + 99,
    )
    .await
    .unwrap();
    assert!(!b.created_new());
    assert_eq!(b.legacy_opened_ms(), T0);
    let before = b.verifications();
    signed(sign(&b, "ada", ADA_PW, Holder::Human, addr("10.0.0.1"), T0 + 100).await);
    assert_eq!(b.verifications(), before + 1);
    // Re-hashed at the new cost on that login.
    signed(sign(&b, "ada", ADA_PW, Holder::Human, addr("10.0.0.1"), T0 + 101).await);
    assert_eq!(b.config().bcrypt_cost, 5);
    assert_eq!(Legacy::Sealed.as_str(), "sealed");
}
