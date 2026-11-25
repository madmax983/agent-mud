use proptest::prelude::*;

use super::*;
use crate::test_support::{PW, accounts, accounts_with, game, home};

fn dialogue(g: &Arc<Game>, a: &Arc<Accounts>, peer: Peer) -> Dialogue {
    Dialogue::new(Arc::clone(g), Arc::clone(a), Transport::Web, peer)
}

async fn say(d: &mut Dialogue, s: &str) -> Reply {
    d.answer(Secret::from(s), false).await
}

fn texts(reply: &Reply) -> Vec<(LineKind, &str)> {
    reply
        .lines
        .iter()
        .map(|l| (l.kind, l.text.as_str()))
        .collect()
}

const CUR: LineKind = LineKind::Secret(Secretness::Current);
const NEW: LineKind = LineKind::Secret(Secretness::New);

fn done(reply: Reply) -> Authenticated {
    match reply.next {
        Next::Done(a) => *a,
        other => panic!("not done: {other:?}"),
    }
}

async fn make(g: &Arc<Game>, a: &Arc<Accounts>, name: &str) {
    let mut d = dialogue(g, a, home());
    say(&mut d, name).await;
    say(&mut d, PW).await;
    done(say(&mut d, PW).await);
}

#[tokio::test]
async fn a_new_player_makes_a_character() {
    let (g, a) = (game(), accounts().await);
    let mut d = dialogue(&g, &a, home());
    assert_eq!(
        texts(&d.greeting()),
        [(LineKind::Prompt, "What is your name?")]
    );
    let reply = say(&mut d, "Ada").await;
    assert_eq!(
        texts(&reply),
        [(
            NEW,
            "Nobody here is called Ada. Choose a password to make a new character (8 or more characters):"
        )]
    );
    assert_eq!(texts(&say(&mut d, PW).await), [(NEW, "Type it again:")]);
    let reply = say(&mut d, PW).await;
    assert_eq!(
        texts(&reply),
        [(
            LineKind::System,
            "Welcome, Ada. Your password is the only way back to this character; keep it."
        )]
    );
    assert!(matches!(&reply.controls[0], Control::Authed { name } if name == "Ada"));
    assert!(
        matches!(&reply.controls[1], Control::Remember(t) if !t.persist),
        "a tab token"
    );
    let auth = done(reply);
    assert_eq!(auth.entity(), &mud_world::EntityId::from("pc_ada"));
    assert_eq!(auth.via(), AuthVia::Created);
}

#[tokio::test]
async fn a_returning_player_needs_the_password() {
    let (g, a) = (game(), accounts().await);
    make(&g, &a, "ada").await;
    let mut d = dialogue(&g, &a, home());
    assert_eq!(texts(&say(&mut d, "ada").await), [(CUR, "Password:")]);
    let reply = say(&mut d, "not-the-password").await;
    assert_eq!(
        texts(&reply),
        [(LineKind::Error, "Wrong password."), (CUR, "Password:")]
    );
    let reply = d.answer(Secret::from(PW), true).await;
    assert!(
        matches!(&reply.controls[1], Control::Remember(t) if t.persist),
        "stay signed in"
    );
    assert_eq!(done(reply).via(), AuthVia::Password);
}

#[tokio::test]
async fn kestrel_reads_exactly_like_a_protected_account() {
    let (g, a) = (game(), accounts().await);
    make(&g, &a, "ada").await;
    let elsewhere = Peer::Addr("10.9.9.9".parse().unwrap());
    let mut seen = Vec::new();
    for name in ["ada", "kestrel"] {
        let mut d = dialogue(&g, &a, elsewhere);
        let before = a.verifications();
        let mut lines = Vec::new();
        for answer in [name, "wrong-pass-123", "wrong-pass-123", "wrong-pass-123"] {
            let reply = say(&mut d, answer).await;
            lines.extend(reply.lines.iter().map(|l| (l.kind, l.text.clone())));
            lines.push((
                LineKind::System,
                format!("{:?}", matches!(reply.next, Next::Close(_))),
            ));
        }
        seen.push((lines, a.verifications() - before));
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0].1, 3, "one verify per answer");
    assert!(
        seen[0]
            .0
            .iter()
            .any(|(_, t)| t == "Wrong password. Farewell.")
    );
}

/// Who holds a name is read again just before a login binds: a name an
/// agent PC took while the new password was typed is refused, and nothing
/// is handed on to bind.
#[tokio::test]
async fn a_name_an_agent_takes_mid_dialogue_never_binds() {
    let (g, a) = (game(), accounts().await);
    make(&g, &a, "zed").await;
    let key = NameKey::parse("zed").unwrap();
    let temp = a
        .admin_reset("zed", &key, Holder::Free, g.now_ms())
        .await
        .unwrap();
    let mut d = dialogue(&g, &a, home());
    assert_eq!(texts(&say(&mut d, "zed").await), [(CUR, "Password:")]);
    let reply = say(&mut d, temp.expose()).await;
    assert_eq!(reply.lines[0].kind, NEW, "{:?}", texts(&reply));
    assert_eq!(texts(&say(&mut d, PW).await), [(NEW, "Type it again:")]);
    // The agent PC kestrel now answers to Zed (as when a pack renames a
    // cast member).
    g.write(|w, _| {
        let mut st = w.snapshot();
        if let Some(e) = st
            .entities
            .get_mut(&mud_world::EntityId::from("pc_kestrel"))
        {
            e.name = "Zed".to_owned();
            e.aliases.clear();
        }
        *w = mud_world::World::from_state(Arc::clone(w.pack()), st, Vec::new());
    });
    assert_eq!(d.holder("zed"), Holder::OtherPlayer);
    let reply = say(&mut d, PW).await;
    assert_eq!(
        texts(&reply),
        [(LineKind::Error, "that name is taken. What is your name?")]
    );
    assert!(!matches!(reply.next, Next::Done(_)));
    // And a right password for it now binds nothing either.
    let mut again = dialogue(&g, &a, home());
    say(&mut again, "zed").await;
    let reply = say(&mut again, PW).await;
    assert_eq!(
        texts(&reply),
        [(LineKind::Error, "Wrong password."), (CUR, "Password:")]
    );
}

/// Answers from a mix of names and passwords; whatever the order, a
/// dialogue that finishes is bound to a human or a new character, never
/// to an agent PC or an NPC.
fn arb_answer() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("kestrel".to_owned()),
        Just("Kestrel".to_owned()),
        Just("marta".to_owned()),
        Just("hale".to_owned()),
        Just("guard_01".to_owned()),
        Just("pc_kestrel".to_owned()),
        Just("ada".to_owned()),
        Just(PW.to_owned()),
        Just(String::new()),
        Just("wrong-pass-123".to_owned()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn no_dialogue_binds_an_agent_or_an_npc(answers in proptest::collection::vec(arb_answer(), 1..12)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (g, a) = (game(), accounts().await);
            let mut d = dialogue(&g, &a, home());
            for answer in &answers {
                let reply = say(&mut d, answer).await;
                match reply.next {
                    Next::Done(auth) => {
                        let e = g.read(|w| w.entity(auth.entity()).cloned());
                        assert!(e.is_none_or(|e| e.human), "bound to {:?}", auth.entity());
                        assert_eq!(auth.entity().as_str(), "pc_ada");
                        break;
                    }
                    Next::Close(_) => break,
                    Next::Ask => {}
                }
            }
        });
    }
}

#[tokio::test]
async fn three_wrong_passwords_close() {
    let (g, a) = (game(), accounts().await);
    let mut d = dialogue(&g, &a, home());
    say(&mut d, "kestrel").await;
    for _ in 0..2 {
        assert!(matches!(
            say(&mut d, "wrong-pass-123").await.next,
            Next::Ask
        ));
    }
    let reply = say(&mut d, "wrong-pass-123").await;
    assert_eq!(
        texts(&reply),
        [(LineKind::Error, "Wrong password. Farewell.")]
    );
    assert!(matches!(reply.next, Next::Close(CloseKind::Policy)));
}

#[tokio::test]
async fn twenty_answers_close() {
    let (g, a) = (game(), accounts().await);
    let mut d = dialogue(&g, &a, home());
    for _ in 0..MAX_ANSWERS {
        assert!(matches!(say(&mut d, "x").await.next, Next::Ask));
    }
    let reply = say(&mut d, "x").await;
    assert_eq!(
        texts(&reply),
        [(LineKind::Error, "Too many tries. Farewell.")]
    );
    assert!(matches!(reply.next, Next::Close(CloseKind::Policy)));
    let mut d = dialogue(&g, &a, home());
    for _ in 0..MAX_ANSWERS {
        d.too_long();
    }
    assert!(matches!(d.too_long().next, Next::Close(_)));
}

#[tokio::test]
async fn an_empty_answer_returns_to_the_name() {
    let (g, a) = (game(), accounts().await);
    for name in ["kestrel", "zed", "ada"] {
        let mut d = dialogue(&g, &a, home());
        say(&mut d, name).await;
        let before = a.verifications();
        let reply = say(&mut d, "").await;
        assert_eq!(
            texts(&reply),
            [(LineKind::Prompt, "What is your name?")],
            "{name}"
        );
        assert_eq!(a.verifications(), before);
    }
    let mut d = dialogue(&g, &a, home());
    say(&mut d, "zed").await;
    say(&mut d, PW).await;
    assert_eq!(
        texts(&say(&mut d, "").await),
        [(LineKind::Prompt, "What is your name?")]
    );
}

#[tokio::test]
async fn an_overlong_answer_at_the_password_prompt_costs_no_verify() {
    let (g, a) = (game(), accounts().await);
    let mut d = dialogue(&g, &a, home());
    let reply = d.too_long();
    assert_eq!(
        texts(&reply),
        [(
            LineKind::Error,
            "That line is too long; nothing was done. What is your name?"
        )]
    );
    say(&mut d, "kestrel").await;
    let before = a.verifications();
    let reply = d.too_long();
    assert_eq!(
        texts(&reply),
        [
            (LineKind::Error, "That line is too long; nothing was done."),
            (LineKind::Prompt, "What is your name?")
        ]
    );
    assert_eq!(a.verifications(), before);
    say(&mut d, "zed").await;
    let reply = d.too_long();
    assert_eq!(reply.lines[0].text, "A password is at most 72 bytes long.");
    assert_eq!(reply.lines[1].kind, NEW);
    say(&mut d, PW).await;
    let reply = d.too_long();
    assert_eq!(reply.lines[0].text, "A password is at most 72 bytes long.");
}

#[tokio::test]
async fn mismatched_or_weak_passwords_ask_again() {
    let (g, a) = (game(), accounts().await);
    let mut d = dialogue(&g, &a, home());
    say(&mut d, "ada").await;
    let reply = say(&mut d, "short").await;
    assert_eq!(
        reply.lines[0].text,
        "A password needs at least 8 characters."
    );
    assert!(
        reply.lines[1]
            .text
            .starts_with("Nobody here is called ada.")
    );
    say(&mut d, PW).await;
    let reply = say(&mut d, "lantern-and-jerkins").await;
    assert_eq!(reply.lines[0].text, "The passwords did not match.");
    assert_eq!(reply.lines[1].kind, NEW);
    say(&mut d, PW).await;
    done(say(&mut d, PW).await);
}

#[tokio::test]
async fn a_claim_asks_password_then_confirms() {
    let (g, a) = (game(), accounts().await);
    g.login("old").unwrap();
    let mut d = dialogue(&g, &a, home());
    assert_eq!(texts(&say(&mut d, "old").await), [(CUR, "Password:")]);
    let before = a.verifications();
    let reply = say(&mut d, "short").await;
    assert_eq!(
        a.verifications(),
        before + 1,
        "a claim answer costs one verify"
    );
    assert_eq!(
        texts(&reply),
        [(
            NEW,
            "old has no password yet. A password needs at least 8 characters. Choose one (8 or more characters):"
        )]
    );
    say(&mut d, PW).await;
    let reply = say(&mut d, PW).await;
    assert_eq!(
        reply.lines[0].text,
        "old is yours now. Use this password next time."
    );
    assert_eq!(done(reply).via(), AuthVia::Claimed);
    let mut d = dialogue(&g, &a, home());
    say(&mut d, "old").await;
    let reply = say(&mut d, PW).await;
    assert_eq!(done(reply).via(), AuthVia::Password);
    // A first answer that passes the policy is confirmed at once.
    g.login("older").unwrap();
    let mut d = dialogue(&g, &a, home());
    say(&mut d, "older").await;
    let reply = say(&mut d, PW).await;
    assert_eq!(
        texts(&reply),
        [(
            NEW,
            "older has no password yet; the one you typed will become it. Type it again to confirm:"
        )]
    );
    assert_eq!(done(say(&mut d, PW).await).via(), AuthVia::Claimed);
}

#[tokio::test]
async fn a_held_name_says_how_long() {
    let (g, a) = (game(), accounts().await);
    let p = Peer::Addr("10.0.0.5".parse().unwrap());
    for _ in 0..2 {
        let mut d = dialogue(&g, &a, p);
        say(&mut d, "kestrel").await;
        for _ in 0..3 {
            say(&mut d, "wrong-pass-123").await;
        }
    }
    let mut d = dialogue(&g, &a, p);
    let reply = say(&mut d, "kestrel").await;
    assert_eq!(
        texts(&reply),
        [
            (
                LineKind::Error,
                "Too many failed logins for that name. Try again in 1 second."
            ),
            (LineKind::Prompt, "What is your name?")
        ]
    );
    assert!(matches!(reply.next, Next::Ask));
}

#[tokio::test]
async fn a_held_address_closes() {
    let a = accounts_with(|c| c.throttle.address.free = 1).await;
    let g = game();
    let p = Peer::Addr("10.0.0.6".parse().unwrap());
    for name in ["zed", "kestrel"] {
        let mut d = dialogue(&g, &a, p);
        say(&mut d, name).await;
        if name == "kestrel" {
            say(&mut d, "wrong-pass-123").await;
        }
    }
    let mut d = dialogue(&g, &a, p);
    say(&mut d, "kestrel").await;
    say(&mut d, "wrong-pass-123").await;
    let mut d = dialogue(&g, &a, p);
    let reply = say(&mut d, "torin_x").await;
    assert!(
        reply.lines[0]
            .text
            .starts_with("Too many failed logins from your address."),
        "{reply:?}"
    );
    assert!(matches!(reply.next, Next::Close(CloseKind::Policy)));
}

#[tokio::test]
async fn a_racing_creation_is_reported() {
    let (g, a) = (game(), accounts().await);
    let mut slow = dialogue(&g, &a, home());
    say(&mut slow, "ada").await;
    say(&mut slow, PW).await;
    make(&g, &a, "ada").await;
    let reply = say(&mut slow, PW).await;
    assert_eq!(
        texts(&reply),
        [
            (LineKind::Error, "Someone just took that name."),
            (LineKind::Prompt, "What is your name?")
        ]
    );
}

#[tokio::test]
async fn a_reset_password_must_be_changed_before_play() {
    let (g, a) = (game(), accounts().await);
    g.login("old").unwrap();
    let key = NameKey::parse("old").unwrap();
    let temp = a
        .admin_reset("old", &key, Holder::Human, g.now_ms())
        .await
        .unwrap();
    let mut d = dialogue(&g, &a, home());
    say(&mut d, "old").await;
    let reply = say(&mut d, temp.expose()).await;
    assert_eq!(
        texts(&reply),
        [(
            NEW,
            "Your password was reset by the keeper of this world. Choose a new one (8 or more characters):"
        )]
    );
    assert!(matches!(reply.next, Next::Ask));
    say(&mut d, PW).await;
    let reply = say(&mut d, PW).await;
    assert_eq!(reply.lines[0].text, "Password changed.");
    assert_eq!(done(reply).via(), AuthVia::Reset);
}

#[tokio::test]
async fn a_rejected_resume_forgets_and_asks_the_name() {
    let (g, a) = (game(), accounts().await);
    let mut d = dialogue(&g, &a, home());
    let reply = d.resume(Secret::from("no-such:token")).await;
    assert_eq!(
        texts(&reply),
        [
            (LineKind::Error, "Your saved login has expired."),
            (LineKind::Prompt, "What is your name?")
        ]
    );
    assert!(matches!(reply.controls[..], [Control::Forget]));
    // A good token logs in with no question, and is not re-issued.
    let mut d = dialogue(&g, &a, home());
    say(&mut d, "ada").await;
    say(&mut d, PW).await;
    let reply = d.answer(Secret::from(PW), true).await;
    let token = match &reply.controls[1] {
        Control::Remember(t) => Secret::from(t.value.expose()),
        other => panic!("{other:?}"),
    };
    let mut d = dialogue(&g, &a, home());
    let reply = d.resume(token).await;
    assert_eq!(reply.controls.len(), 1, "authed only");
    assert_eq!(done(reply).via(), AuthVia::Resumed);
}

#[tokio::test]
async fn a_command_at_a_password_prompt_is_the_answer_and_never_audited() {
    let (g, a) = (game(), accounts().await);
    let mut audit = g.subscribe_audit();
    let mut events = g.subscribe();
    let mut d = dialogue(&g, &a, home());
    say(&mut d, "kestrel").await;
    let reply = say(&mut d, "say hello").await;
    assert_eq!(reply.lines[0].text, "Wrong password.");
    assert!(audit.try_recv().is_err(), "no Audit::Command");
    assert!(events.try_recv().is_err(), "no event");
}

#[tokio::test]
async fn npc_names_and_bad_names_are_refused_at_once() {
    let (g, a) = (game(), accounts().await);
    let mut d = dialogue(&g, &a, home());
    let reply = say(&mut d, "marta").await;
    assert_eq!(
        texts(&reply),
        [(LineKind::Error, "that name is taken. What is your name?")]
    );
    let reply = say(&mut d, "me").await;
    assert_eq!(
        texts(&reply),
        [(
            LineKind::Error,
            "that word is reserved; choose another name. What is your name?"
        )]
    );
    let reply = say(&mut d, "pc_ada").await;
    assert_eq!(
        reply.lines[0].text,
        "that word is reserved; choose another name. What is your name?"
    );
    let closed = accounts_with(|c| c.allow_new = false).await;
    let mut d = dialogue(&g, &closed, home());
    let reply = say(&mut d, "zed").await;
    assert_eq!(
        texts(&reply),
        [
            (
                LineKind::Error,
                "This world is not taking new characters right now."
            ),
            (LineKind::Prompt, "What is your name?")
        ]
    );
    assert!(format!("{d:?}").contains("Dialogue"));
}
