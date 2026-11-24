use std::sync::Mutex;

use mud_world::{ManualClock, World, WorldPack};

use super::*;
use crate::test_support::{PW, accounts, auth, game, home};

async fn open(
    g: &Arc<Game>,
    p: &Arc<Presence>,
    a: &Arc<Accounts>,
    name: &str,
    transport: Transport,
) -> (Session, Vec<Line>) {
    let who = auth(a, g, name).await;
    Session::login(
        Arc::clone(g),
        Arc::clone(p),
        Arc::clone(a),
        who,
        home(),
        transport,
    )
    .unwrap()
}

async fn say(s: &mut Session, line: &str) -> Output {
    s.input(Secret::from(line)).await
}

#[tokio::test]
async fn two_players_hear_each_other_and_logout_needs_the_last_tab() {
    let (g, p, a) = (game(), Arc::new(Presence::default()), accounts().await);
    let (ada, greet) = open(&g, &p, &a, "ada", Transport::Web).await;
    assert!(greet[1].text.contains("Town Square"));
    let (bo, _) = open(&g, &p, &a, "bo", Transport::Web).await;
    let mut rx = g.subscribe();
    let out = ada.command("say hi bo");
    assert_eq!(out[0].text, "You say, \"hi bo\"");
    let ev = rx.try_recv().unwrap();
    assert_eq!(bo.render(&ev).unwrap().text, "ada says, \"hi bo\"");
    assert!(ada.render(&ev).is_none(), "no echo of your own line");
    assert_eq!(ada.command("dance")[0].kind, LineKind::Error);
    let (ada2, _) = open(&g, &p, &a, "ada", Transport::Web).await;
    drop(ada);
    assert!(
        g.read(|w| w.entity(&EntityId::from("pc_ada")).unwrap().online),
        "a tab is still open"
    );
    drop(ada2);
    assert!(!g.read(|w| w.entity(&EntityId::from("pc_ada")).unwrap().online));
}

#[test]
fn board_posts_are_rendered_from_the_resolution_once() {
    let posts = serde_json::json!({ "posts": [
        { "author": { "id": "pc_ada", "name": "ada" }, "untrusted_text": "ore wanted" },
        { "author": { "id": "torin", "name": "Torin" }, "untrusted_text": "<b>15s</b>" },
    ]});
    let want = "The notice board:\n  [1] ada: ore wanted\n  [2] Torin: <b>15s</b>";
    assert_eq!(with_board_posts("The notice board:", &posts), want);
    assert_eq!(with_board_posts(want, &posts), want, "no duplicates");
    assert_eq!(
        with_board_posts("The notice board: bare wood.", &serde_json::json!({})),
        "The notice board: bare wood."
    );
}

#[tokio::test]
async fn a_player_reads_the_board_through_a_session() {
    let (g, p, a) = (game(), Arc::new(Presence::default()), accounts().await);
    let (mut ada, _) = open(&g, &p, &a, "ada", Transport::Telnet).await;
    let _ = say(&mut ada, "n").await;
    assert_eq!(
        say(&mut ada, "board post ore wanted").await.lines[0].kind,
        LineKind::Narrative
    );
    let out = say(&mut ada, "board read").await.lines;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text.matches("  [1] ada: ore wanted").count(), 1);
}

/// A tab closing while another tab of the same player logs in must never
/// leave the new session attached to an offline entity.
#[tokio::test(flavor = "multi_thread")]
async fn a_login_racing_the_last_logout_stays_online() {
    let (g, p, a) = (game(), Arc::new(Presence::default()), accounts().await);
    let who = auth(&a, &g, "ada").await;
    let id = EntityId::from("pc_ada");
    let stale = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                for _ in 0..2_000 {
                    let (sess, _) = Session::login(
                        Arc::clone(&g),
                        Arc::clone(&p),
                        Arc::clone(&a),
                        who.clone(),
                        home(),
                        Transport::Web,
                    )
                    .unwrap();
                    if !g.read(|w| w.entity(&id).unwrap().online) {
                        stale.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    drop(sess);
                }
            });
        }
    });
    assert_eq!(
        stale.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a live session saw its entity offline"
    );
    assert!(!g.read(|w| w.entity(&id).unwrap().online));
}

/// A writer into a shared buffer, for a trace log.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for Buffer {
    type Writer = Self;
    fn make_writer(&self) -> Self {
        self.clone()
    }
}

/// CLAUDE.md: secrets stay out of the world. Passwords, hashes and tokens
/// never reach `WorldState`, events, `Audit`, the observer, logs or
/// `World::execute`.
#[tokio::test]
async fn passwords_never_reach_the_world() {
    let buf = Buffer::default();
    let _log = tracing::subscriber::set_default(crate::logging::subscriber("trace", buf.clone()));
    let (g, p, a) = (game(), Arc::new(Presence::default()), accounts().await);
    let mut events = g.subscribe();
    let mut audit = g.subscribe_audit();
    let (mut ada, _) = open(&g, &p, &a, "ada", Transport::Web).await;
    let mut who = ada.auth().clone();
    let token = a
        .remember(&mut who, true, g.now_ms())
        .await
        .unwrap()
        .unwrap();
    let new_pw = "a-brand-new-secret";
    let mut out = Vec::new();
    for line in [
        "password hunter2",
        "password",
        PW,
        new_pw,
        new_pw,
        "say done",
        "help",
    ] {
        out.push(say(&mut ada, line).await);
    }
    assert_eq!(
        out[0].lines[0].text,
        "Type password alone; you will be asked for it privately."
    );
    assert_eq!(out[1].lines[0].kind, LineKind::Secret(Secretness::Current));
    assert_eq!(out[1].lines[0].text, "Current password:");
    assert_eq!(out[2].lines[0].text, "New password (8 or more characters):");
    assert_eq!(out[3].lines[0].text, "Type it again:");
    assert_eq!(
        out[4].lines[0].text,
        "Password changed. Your other sessions and remembered browsers are signed out."
    );
    let mut world_text = String::new();
    while let Ok(e) = events.try_recv() {
        world_text.push_str(&serde_json::to_string(&*e).unwrap());
    }
    while let Ok(x) = audit.try_recv() {
        world_text.push_str(&serde_json::to_string(&*x).unwrap());
    }
    world_text.push_str(&g.read(|w| serde_json::to_string(&w.snapshot()).unwrap()));
    world_text.push_str(&g.look(ada.entity()).to_string());
    world_text.push_str(&g.sheet(ada.entity()).to_string());
    world_text.push_str(&ada.command("who")[0].text);
    for o in &out {
        for l in &o.lines {
            world_text.push_str(&l.text);
        }
    }
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(world_text.contains("say done") || world_text.contains("done"));
    assert!(log.contains("mud::auth"), "{log}");
    let (_, raw_token) = token.value.expose().split_once(':').unwrap();
    for (what, text) in [("world", &world_text), ("log", &log)] {
        for secret in ["hunter2", PW, new_pw, raw_token, "$2b$"] {
            assert!(!text.contains(secret), "{what} holds {secret}");
        }
    }
}

fn clocked() -> (Arc<Game>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let g = Game::new(
        World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1),
        None,
        clock.clone(),
    );
    (g, clock)
}

#[tokio::test]
async fn a_change_elsewhere_ends_the_session_on_revalidate() {
    let (g, clock) = clocked();
    let (p, a) = (Arc::new(Presence::default()), accounts().await);
    let (mut one, _) = open(&g, &p, &a, "ada", Transport::Telnet).await;
    let (two, _) = open(&g, &p, &a, "ada", Transport::Telnet).await;
    assert!(two.revalidate().await.unwrap());
    clock.advance(1_000);
    for line in ["password", PW, "brand-new-words", "brand-new-words"] {
        let _ = say(&mut one, line).await;
    }
    assert!(!two.revalidate().await.unwrap(), "the other session ends");
    assert!(
        one.revalidate().await.unwrap(),
        "the changing session goes on"
    );
    clock.advance(1_000);
    let k = mud_accounts::NameKey::parse("ada").unwrap();
    a.admin_signout(&k, g.now_ms()).await.unwrap();
    assert!(!one.revalidate().await.unwrap());
}

#[tokio::test]
async fn a_tab_session_outlives_reconnect_minutes() {
    let (g, clock) = clocked();
    let (p, a) = (Arc::new(Presence::default()), accounts().await);
    let (ada, _) = open(&g, &p, &a, "ada", Transport::Web).await;
    clock.advance(20 * 60_000);
    let (_bo, _) = open(&g, &p, &a, "bo", Transport::Web).await;
    assert!(ada.revalidate().await.unwrap());
}

#[tokio::test]
async fn help_lists_the_account_words() {
    let (g, p, a) = (game(), Arc::new(Presence::default()), accounts().await);
    let (mut ada, _) = open(&g, &p, &a, "ada", Transport::Telnet).await;
    let out = say(&mut ada, "help").await;
    assert_eq!(out.lines[0].kind, LineKind::Narrative);
    assert_eq!(out.lines.last().unwrap().text, ACCOUNT_HELP);
    assert!(out.close.is_none());
}

#[tokio::test]
async fn logout_forgets_this_browser() {
    let (g, p, a) = (game(), Arc::new(Presence::default()), accounts().await);
    let who = auth(&a, &g, "ada").await;
    let mut kept = who.clone();
    let token = a
        .remember(&mut kept, true, g.now_ms())
        .await
        .unwrap()
        .unwrap();
    let (mut ada, _) = Session::login(
        Arc::clone(&g),
        Arc::clone(&p),
        Arc::clone(&a),
        kept,
        home(),
        Transport::Web,
    )
    .unwrap();
    let out = say(&mut ada, "logout").await;
    assert_eq!(
        out.lines[0].text,
        "Farewell. This browser will ask for your password next time."
    );
    assert!(matches!(out.controls[..], [Control::Forget]));
    assert_eq!(out.close, Some(CloseKind::Normal));
    assert!(a.resume(&token.value, home(), g.now_ms()).await.is_err());
    // On telnet, logout is quit.
    let (mut t, _) = Session::login(
        Arc::clone(&g),
        p,
        Arc::clone(&a),
        who,
        home(),
        Transport::Telnet,
    )
    .unwrap();
    let out = say(&mut t, "LOGOUT").await;
    assert_eq!(out.lines[0].text, "Farewell.");
    assert!(out.controls.is_empty());
    assert_eq!(say(&mut t, "quit").await.close, Some(CloseKind::Normal));
}

#[tokio::test]
async fn a_password_change_can_be_cancelled_mistyped_or_refused() {
    let (g, p, a) = (game(), Arc::new(Presence::default()), accounts().await);
    let (mut ada, _) = open(&g, &p, &a, "ada", Transport::Telnet).await;
    let _ = say(&mut ada, "password").await;
    assert_eq!(
        say(&mut ada, "").await.lines[0].text,
        "Password change cancelled."
    );
    let _ = say(&mut ada, "password").await;
    let _ = say(&mut ada, PW).await;
    let weak = say(&mut ada, "short").await;
    assert_eq!(
        weak.lines[0].text,
        "A password needs at least 8 characters."
    );
    assert_eq!(weak.lines[1].text, "New password (8 or more characters):");
    let _ = say(&mut ada, "brand-new-words").await;
    let differ = say(&mut ada, "brand-new-wordz").await;
    assert_eq!(differ.lines[0].text, "The passwords did not match.");
    let _ = say(&mut ada, "brand-new-words").await;
    let _ = say(&mut ada, "brand-new-words").await;
    // Three wrong current passwords close the connection.
    for i in 1..=3 {
        for line in [
            "password",
            "not-the-password",
            "brand-new-words",
            "brand-new-words",
        ] {
            let out = say(&mut ada, line).await;
            if line == "brand-new-words" && out.lines[0].kind == LineKind::Error {
                let want = if i == 3 {
                    "Wrong password. Farewell."
                } else {
                    "Wrong password."
                };
                assert_eq!(out.lines[0].text, want);
                assert_eq!(out.close.is_some(), i == 3);
            }
        }
    }
    assert!(format!("{:?}", Change::Current).contains("Current"));
}
