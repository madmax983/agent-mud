//! Refine (ADR 0015, phase 4) against fake daemons that keep an lg-like
//! overlay pointer: lg's of littleguy #122, which checks the pointer an
//! activate expects, or an older lg's, which answers `previous` only.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mud_world::side::StewardStored;

use super::council::Vote;
use super::fake::{ActivateMode, FakeDaemons, ProposeMode, VOTE_USAGE};
use super::state::{OverlaySource, ROW_SCHEMA};
use super::tests::{HOUR, MIN, Opts, Rig, TOKEN, rig, torin};
use super::vet::{default_menu, overlay_digest, render_guardrails};
use super::*;

/// A proposal from lg's proposer: evidence only, never put.
const PROPOSAL: &str = r#"{"guardrails":"Be brief with buyers.","max_steps":9}"#;

fn menu_cfg() -> StewardConfig {
    StewardConfig {
        refine: RefineMode::Menu,
        refine_min_wakes: 0,
        ..StewardConfig::default()
    }
}

fn refine_rig() -> Rig {
    rig(Opts {
        cfg: menu_cfg(),
        ..Opts::default()
    })
}

/// Every member of the council picks `ids`.
fn pick(r: &Rig, ids: &[&str]) {
    for _ in 0..3 {
        r.judge.push(Vote {
            yes: false,
            ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            reason: Some("fits".into()),
        });
    }
}

fn text_of(ids: &[&str]) -> String {
    let ids: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
    render_guardrails(&default_menu(), &ids)
}

fn rails(ids: &[&str]) -> String {
    serde_json::json!({ "guardrails": text_of(ids) }).to_string()
}

fn overlay_events(events: &[StewardEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter(|e| e.item == "overlay")
        .map(|e| (e.action.clone(), e.code.clone()))
        .collect()
}

/// The first pass puts the baseline (v1); the second proposes (v2, the
/// proposer's) and activates the menu text `ids` (v3).
async fn baseline_then_text(r: &Rig, ids: &[&str]) {
    r.pass().await;
    assert_eq!(r.row().await.overlay.baseline, Some(1));
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(r, ids);
    r.pass().await;
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(3), "{row:?}");
    assert!(row.overlay.active.is_some());
}

#[tokio::test]
async fn the_first_activation_is_preceded_by_an_empty_baseline() {
    let mut r = refine_rig();
    r.pass().await;
    assert_eq!(
        r.d().kinds(),
        [
            "approvals",
            "plans",
            "overlays",
            "overlay_put",
            "overlays",
            "overlay_activate",
            "notes"
        ]
    );
    assert_eq!(r.d().puts(), ["{}"]);
    let row = r.row().await;
    assert_eq!(
        (row.overlay.pointer, row.overlay.baseline),
        (Some(1), Some(1))
    );
    assert!(row.intent.is_none() && row.overlay.paused.is_none());
    r.d().clear_lines();
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.pass().await;
    assert_eq!(
        r.d().kinds(),
        [
            "plans",
            "overlays",
            "refine_propose",
            "overlay_put",
            "overlays",
            "overlay_activate",
            "notes"
        ]
    );
    assert_eq!(r.d().puts(), [rails(&["brevity"])]);
    assert_eq!(r.d().moves(), [(None, 1), (Some(1), 3)]);
    let ev = r.steward_events();
    assert!(
        overlay_events(&ev).contains(&("activated".into(), "text".into())),
        "{ev:?}"
    );
    let view = r.sup.autonomy(&torin()).unwrap().overlay.unwrap();
    assert_eq!((view.pointer, view.baseline), (Some(3), Some(1)));
    assert_eq!(view.chain.len(), 2);
    assert!(view.chain[0].active && view.chain[0].text == text_of(&["brevity"]));
    assert_eq!(view.chain[1].label, "baseline");
    let ctx = r.macro_wake().await;
    assert!(
        ctx.steward
            .iter()
            .any(|n| n.code == "overlay_on" && n.title.is_none()),
        "{:?}",
        ctx.steward
    );
}

#[tokio::test]
async fn an_existing_human_overlay_is_never_replaced_by_the_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let fake = FakeDaemons::new(&["torin"], TOKEN);
    for _ in 0..3 {
        fake.of("torin")
            .human_put(r#"{"guardrails":"- the operator's own"}"#);
    }
    fake.of("torin").human_activate(3);
    let mut r = rig(Opts {
        db: Some(db.clone()),
        fake: Some(Arc::clone(&fake)),
        cfg: menu_cfg(),
        ..Opts::default()
    });
    r.pass().await;
    r.advance(200 * HOUR);
    r.pass().await;
    assert_eq!(r.d().count("overlay_put"), 0, "never replaced");
    assert_eq!(r.d().count("overlay_activate"), 0);
    assert_eq!(r.d().count("refine_propose"), 0, "paused");
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(3));
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "preexisting");
    assert!(has_overlay(&r.steward_events(), "paused", "preexisting"));
    drop(r);
    let mut r = rig(Opts {
        db: Some(db),
        fake: Some(fake),
        cfg: StewardConfig {
            resume_refine: vec!["torin".into()],
            ..menu_cfg()
        },
        now: super::tests::T0 + MIN,
        ..Opts::default()
    });
    assert!(has_overlay(&r.steward_events(), "resumed", "preexisting"));
    r.pass().await;
    assert_eq!(r.d().puts(), ["{}"]);
    assert_eq!(
        r.d().moves().last(),
        Some(&(Some(3), 4)),
        "the baseline goes on from the human's version"
    );
    let row = r.row().await;
    assert_eq!(row.overlay.baseline, Some(4));
    assert!(row.overlay.paused.is_none());
}

fn has_overlay(events: &[StewardEvent], action: &str, code: &str) -> bool {
    events
        .iter()
        .any(|e| e.item == "overlay" && e.action == action && e.code == code)
}

fn count_overlay(events: &[StewardEvent], action: &str, code: &str) -> usize {
    events
        .iter()
        .filter(|e| e.item == "overlay" && e.action == action && e.code == code)
        .count()
}

#[tokio::test]
async fn every_activate_sends_the_pointer_it_read() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    assert_eq!(
        r.d().expecteds(),
        [
            Some(serde_json::json!({ "active": null, "head": null })),
            Some(serde_json::json!({ "active": 1, "head": 1 })),
        ],
        "first contact from nothing, then the text from the baseline"
    );
    let row = r.row().await;
    assert_eq!(row.overlay.rollback_to, Some(1), "a guarded move is exact");
    assert!(row.overlay.paused.is_none() && row.intent.is_none());
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "activated", "text"), "{ev:?}");
    assert_eq!(count_overlay(&ev, "unguarded", "old_lg"), 0);
    assert_eq!(count_overlay(&ev, "moved", "overlay_moved"), 0);
}

#[tokio::test]
async fn a_human_move_between_read_and_activate_is_refused_by_lg_and_pauses() {
    let mut r = refine_rig();
    r.pass().await;
    // A version a human chose on lg's refine tab (v2).
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    // The human activates theirs after the re-read and before ours runs:
    // lg's guard refuses ours.
    r.d().activate_modes(&[ActivateMode::HumanFirst(human)]);
    r.pass().await;
    assert_eq!(
        r.d().activates(),
        [1, 4],
        "nothing restored, nothing resent"
    );
    assert_eq!(
        r.d().moves(),
        [(None, 1), (Some(1), human)],
        "lg moved only for the human"
    );
    assert_eq!(r.d().active(), Some(human), "the human's choice stands");
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(human));
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "rolled_back");
    assert!(row.intent.is_none() && row.overlay.late.is_empty());
    assert!(row.overlay.active.is_none() && row.overlay.pending.is_none());
    assert!(
        row.is_vetoed(overlay_digest(&text_of(&["brevity"]))),
        "the text about to go live is vetoed, as on an older lg"
    );
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "moved", "overlay_moved"), "{ev:?}");
    assert!(has_overlay(&ev, "paused", "rolled_back"));
    assert!(!has_overlay(&ev, "restored", "restored"));
    assert!(!has_overlay(&ev, "activated", "text"));
    r.d().clear_lines();
    for _ in 0..3 {
        r.advance(200 * HOUR);
        r.pass().await;
    }
    assert_eq!(r.d().count("overlay_activate"), 0, "no further activate");
    assert_eq!(r.d().count("refine_propose"), 0);
}

#[tokio::test]
async fn a_lost_answer_after_a_move_away_and_back_is_a_human_veto() {
    let mut r = refine_rig();
    r.pass().await;
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    // A human activates theirs and rolls it back while ours waits, so lg
    // is on the baseline again with a new head; ours is refused, and its
    // answer is lost.
    r.d()
        .activate_modes(&[ActivateMode::AwayAndBackSilently(human)]);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 4], "never sent again");
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "the head moved: a human acted, though lg shows the version read: {row:?}"
    );
    assert_eq!(row.overlay.pointer, Some(1));
    assert!(row.intent.is_none());
    assert!(
        row.overlay.late.is_empty(),
        "a guarded activate is refused once the head moved: never late"
    );
    assert!(
        row.is_vetoed(overlay_digest(&text_of(&["brevity"]))),
        "it may have run and been rolled back: never proposed again"
    );
    assert!(row.overlay.active.is_none());
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "paused", "rolled_back"), "{ev:?}");
    assert!(!has_overlay(&ev, "unconfirmed", "unconfirmed"));
    r.d().clear_lines();
    r.advance(200 * HOUR);
    r.pass().await;
    assert_eq!(r.d().count("overlay_activate"), 0);
}

#[tokio::test]
async fn a_human_move_between_read_and_activate_on_an_older_lg_is_restored() {
    let mut r = refine_rig();
    // An lg before #122 drops `expected` and moves unchecked.
    r.d().make_old();
    r.pass().await;
    // A version a human chose on lg's refine tab (v2).
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    // The human activates theirs after the re-read and before ours runs.
    r.d().activate_modes(&[ActivateMode::HumanFirst(human)]);
    r.pass().await;
    assert_eq!(
        r.d().activates(),
        [1, 4, human],
        "one activate of the human's version, which lg named as previous"
    );
    assert_eq!(r.d().active(), Some(human), "the human's choice stands");
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(human));
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "rolled_back");
    assert!(row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "restored", "restored"), "{ev:?}");
    assert!(has_overlay(&ev, "paused", "rolled_back"));
    assert_eq!(
        count_overlay(&ev, "unguarded", "old_lg"),
        1,
        "said once per daemon start, over three unguarded answers"
    );
    r.d().clear_lines();
    for _ in 0..3 {
        r.advance(200 * HOUR);
        r.pass().await;
    }
    assert_eq!(r.d().count("overlay_activate"), 0, "no further activate");
    assert_eq!(r.d().count("refine_propose"), 0);
}

#[tokio::test]
async fn a_rollback_during_propose_is_not_overridden() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    r.advance(169 * HOUR);
    // A human rolls back (text -> baseline) while the proposer runs.
    r.d()
        .propose_mode(ProposeMode::RollbackFirst(PROPOSAL.into()));
    pick(&r, &["pacing"]);
    let before = r.d().activates().len();
    r.pass().await;
    assert_eq!(r.d().activates().len(), before, "no activate is sent");
    assert_eq!(r.d().active(), Some(1), "the rollback stands");
    let row = r.row().await;
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "rolled_back");
    assert_eq!(row.overlay.pointer, Some(1));
    assert!(row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert!(has_overlay(&r.steward_events(), "paused", "rolled_back"));
}

#[tokio::test]
async fn a_human_activation_over_the_baseline_is_detected() {
    let mut r = refine_rig();
    r.pass().await;
    let v = r.d().human_put(r#"{"guardrails":"- set by hand"}"#);
    r.d().human_activate(v);
    r.d().clear_lines();
    r.pass().await;
    let row = r.row().await;
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "rolled_back");
    assert_eq!(row.overlay.pointer, Some(v));
    assert_eq!(r.d().count("overlay_activate"), 0);
    assert_eq!(r.d().active(), Some(v));
    assert!(has_overlay(&r.steward_events(), "paused", "rolled_back"));
}

#[tokio::test]
async fn a_late_activate_then_rollback_is_never_resent() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3]);
    let row = r.row().await;
    assert_eq!(
        row.overlay
            .late
            .iter()
            .map(|l| l.version)
            .collect::<Vec<_>>(),
        [3],
        "given up on, not applied"
    );
    assert_eq!(row.overlay.pointer, Some(1));
    assert!(row.intent.is_none() && row.overlay.paused.is_none());
    assert!(has_overlay(
        &r.steward_events(),
        "unconfirmed",
        "unconfirmed"
    ));
    r.advance(MIN);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3], "never sent again");
    // lg's loop runs it at last.
    r.d().apply_held();
    assert_eq!(r.d().active(), Some(3));
    r.pass().await;
    assert_eq!(
        r.d().activates(),
        [1, 3, 1],
        "undone once, back to the baseline"
    );
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert!(row.overlay.late.is_empty() && row.overlay.paused.is_none());
    assert!(has_overlay(
        &r.steward_events(),
        "late_undone",
        "unconfirmed"
    ));
    // A human rolls that back: their move, adopted, paused.
    r.d().rollback();
    assert_eq!(r.d().active(), Some(3));
    r.pass().await;
    r.advance(200 * HOUR);
    r.pass().await;
    assert_eq!(
        r.d().activates(),
        [1, 3, 1],
        "no activate after the human's move"
    );
    let row = r.row().await;
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "rolled_back");
    assert_eq!(row.overlay.pointer, Some(3));
}

#[tokio::test]
async fn an_unanswered_activate_that_applied_is_undone_and_paused() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::ApplySilently]);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3, 1], "applied, then undone");
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(1));
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "unconfirmed");
    assert!(row.overlay.active.is_none());
    assert!(has_overlay(&r.steward_events(), "paused", "unconfirmed"));
}

#[tokio::test]
async fn one_roll_back_returns_to_the_baseline_after_two_texts() {
    let r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["pacing"]);
    r.pass().await;
    assert_eq!(
        r.d().moves(),
        [(None, 1), (Some(1), 3), (Some(3), 1), (Some(1), 5)],
        "baseline -> A, A -> baseline, baseline -> B"
    );
    assert_eq!(r.d().overlay(5), rails(&["pacing"]));
    r.d().rollback();
    assert_eq!(
        r.d().active(),
        Some(1),
        "one Roll back lands on the baseline"
    );
    assert_eq!(r.d().overlay(1), "{}");
}

#[tokio::test]
async fn a_late_overlay_put_is_found_not_duplicated() {
    let r = refine_rig();
    r.d().state.lock().unwrap().drop_put = true;
    r.pass().await;
    let row = r.row().await;
    assert!(
        matches!(
            row.intent.as_ref().map(|i| &i.action),
            Some(IntentAction::Put {
                source: OverlaySource::Baseline,
                ..
            })
        ),
        "{row:?}"
    );
    assert_eq!(r.d().count("overlay_activate"), 0);
    r.pass().await;
    assert_eq!(r.d().count("overlay_put"), 1, "no second put");
    assert_eq!(
        r.d().count("overlay_show"),
        2,
        "found by its content, then shown again before it is activated"
    );
    assert_eq!(r.d().activates(), [1]);
    let row = r.row().await;
    assert_eq!(row.overlay.baseline, Some(1));
    assert!(row.intent.is_none());
}

#[tokio::test]
async fn a_human_rollback_pauses_refine() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    r.d().rollback();
    r.pass().await;
    let row = r.row().await;
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "rolled_back");
    assert!(row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert!(row.overlay.active.is_none());
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "paused", "rolled_back"), "{ev:?}");
    r.d().clear_lines();
    for _ in 0..3 {
        r.advance(200 * HOUR);
        r.pass().await;
    }
    for kind in ["refine_propose", "overlay_put", "overlay_activate"] {
        assert_eq!(r.d().count(kind), 0, "{kind} while paused");
    }
    let ctx = r.macro_wake().await;
    assert!(ctx.steward.iter().any(|n| n.code == "overlay_off"));
}

#[tokio::test]
async fn vetoed_texts_survive_any_number_of_goal_declines() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let fake = FakeDaemons::new(&["torin"], TOKEN);
    let cfg = StewardConfig {
        max_decisions_per_pass: 50,
        ..menu_cfg()
    };
    let r = rig(Opts {
        db: Some(db.clone()),
        fake: Some(Arc::clone(&fake)),
        cfg: cfg.clone(),
        ..Opts::default()
    });
    baseline_then_text(&r, &["brevity"]).await;
    r.d().rollback();
    r.pass().await;
    let digest = overlay_digest(&text_of(&["brevity"]));
    assert!(r.row().await.is_vetoed(digest));
    // 250 goal declines (a link in each title: no judge call).
    for batch in 0..5 {
        for i in 0..50 {
            let id = 100 + batch * 50 + i;
            r.d()
                .propose(id, &format!("Visit www.shop{id}.example"), "x", Some(60));
        }
        r.pass().await;
    }
    let row = r.row().await;
    assert_eq!(row.declined.len(), DECLINED_CAP_FOR_TEST);
    assert!(row.is_vetoed(digest), "never evicted");
    drop(r);
    // The operator resumes refine; the council picks the vetoed text again.
    let mut r = rig(Opts {
        db: Some(db),
        fake: Some(fake),
        cfg: StewardConfig {
            resume_refine: vec!["torin".into()],
            ..cfg
        },
        now: super::tests::T0 + 300 * HOUR,
        ..Opts::default()
    });
    let puts = r.d().count("overlay_put");
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.pass().await;
    assert_eq!(
        r.d().count("refine_propose"),
        2,
        "proposed again after the resume"
    );
    assert_eq!(
        r.d().count("overlay_put"),
        puts,
        "a vetoed text is never put"
    );
    assert!(has_overlay(&r.steward_events(), "declined", "vetoed"));
}

const DECLINED_CAP_FOR_TEST: usize = super::state::DECLINED_CAP;

#[tokio::test]
async fn resume_refine_clears_only_an_earlier_boots_pause() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let fake = FakeDaemons::new(&["torin"], TOKEN);
    let resume = StewardConfig {
        resume_refine: vec!["torin".into()],
        ..menu_cfg()
    };
    let r = rig(Opts {
        db: Some(db.clone()),
        fake: Some(Arc::clone(&fake)),
        cfg: resume.clone(),
        ..Opts::default()
    });
    r.pass().await;
    let v = r.d().human_put("{}");
    r.d().human_activate(v);
    r.pass().await;
    let row = r.row().await;
    assert_eq!(row.overlay.paused.as_ref().unwrap().boot, r.game.boot());
    // A second Steward of the same boot keeps the pause.
    let same_boot = Steward::new(
        Arc::clone(&r.sup),
        vec![Arc::clone(&r.fake) as Arc<dyn OwnedDaemons>],
        None,
        resume.clone(),
        BTreeMap::from([(
            torin(),
            StewardStored {
                rev: 99,
                schema: ROW_SCHEMA,
                body: row.body(),
            },
        )]),
        r.game.boot(),
    );
    assert!(
        same_boot
            .row(&torin())
            .await
            .unwrap()
            .overlay
            .paused
            .is_some(),
        "a pause of this boot stays"
    );
    drop((same_boot, r));
    let mut r = rig(Opts {
        db: Some(db.clone()),
        fake: Some(Arc::clone(&fake)),
        cfg: resume.clone(),
        now: super::tests::T0 + MIN,
        ..Opts::default()
    });
    assert!(
        r.row().await.overlay.paused.is_none(),
        "an earlier boot's pause ends"
    );
    assert!(has_overlay(&r.steward_events(), "resumed", "rolled_back"));
    drop(r);
    // The resume was written: a third boot, still named, finds no pause.
    let mut r = rig(Opts {
        db: Some(db),
        fake: Some(fake),
        cfg: resume,
        now: super::tests::T0 + 2 * MIN,
        ..Opts::default()
    });
    assert!(r.row().await.overlay.paused.is_none());
    assert!(!has_overlay(&r.steward_events(), "resumed", "rolled_back"));
}

#[tokio::test]
async fn an_immediate_wake_starts_right_after_a_refine_claim() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Slow(
        Duration::from_millis(250),
        PROPOSAL.into(),
    ));
    let steward = Arc::clone(&r.steward);
    let pass = tokio::spawn(async move { steward.pass().await });
    while r.d().count("refine_propose") == 0 {
        tokio::task::yield_now().await;
    }
    assert!(r.sup.is_busy(&torin()), "claimed while the proposer runs");
    let ada = r.game.login("ada").unwrap();
    assert!(r.game.execute(&ada, "tell torin got ore?").ok);
    r.step().await;
    assert!(
        r.mind.seen.lock().unwrap().is_empty(),
        "no wake under the claim"
    );
    pass.await.unwrap();
    assert!(!r.sup.is_busy(&torin()), "released");
    for _ in 0..30 {
        r.step().await;
        if !r.mind.seen.lock().unwrap().is_empty() {
            break;
        }
        r.advance(1_000);
    }
    assert_eq!(r.mind.seen.lock().unwrap().len(), 1, "the tell's wake runs");
}

#[tokio::test]
async fn no_refine_claim_with_a_player_or_a_pending_wake() {
    let mut r = refine_rig();
    r.pass().await;
    let ada = r.game.login("ada").unwrap();
    assert!(r.game.execute(&ada, "w").ok, "to the smithy");
    r.pass().await;
    assert_eq!(r.d().count("refine_propose"), 0, "a player is in the room");
    assert!(r.game.execute(&ada, "e").ok);
    // Drain what ada's moves woke, so only the macro tick waits.
    for _ in 0..30 {
        r.step().await;
        r.advance(1_000);
    }
    r.sup.fire_macro_tick(&torin());
    r.pass().await;
    assert_eq!(r.d().count("refine_propose"), 0, "a wake waits");
    r.step().await;
    r.pass().await;
    assert_eq!(r.d().count("refine_propose"), 1, "then it runs");
}

#[tokio::test]
async fn a_refine_timeout_drains_before_release() {
    let mut r = refine_rig();
    r.pass().await;
    // The proposer runs longer than the Steward waits (400 ms).
    r.d().propose_mode(ProposeMode::Slow(
        Duration::from_millis(1_200),
        PROPOSAL.into(),
    ));
    let steward = Arc::clone(&r.steward);
    let started = std::time::Instant::now();
    let pass = tokio::spawn(async move { steward.pass().await });
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        r.sup.is_busy(&torin()),
        "the claim holds while lg's loop drains"
    );
    assert!(r.d().count("reload") >= 1, "probing lg's loop");
    pass.await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1_200));
    assert!(!r.sup.is_busy(&torin()), "released after the drain");
    assert_eq!(
        r.d().count("overlay_put"),
        1,
        "the late proposal is never used"
    );
    let row = r.row().await;
    assert!(row.intent.is_none());
    assert!(row.overlay.last_refine_ms.is_some());
    assert!(!has_overlay(&r.steward_events(), "stuck", "refine_propose"));
}

#[tokio::test]
async fn refine_spend_reaches_the_budget() {
    let r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    let (_, spend) = r.sup.budget_of(&torin()).unwrap();
    assert_eq!(
        spend.local.calls,
        1 + 3,
        "the proposer's estimate and three votes"
    );
    assert_eq!(spend.local.tokens, 30_000 + 3 * VOTE_USAGE.tokens());
}

#[tokio::test]
async fn refine_waits_for_interval_and_wakes() {
    let r = rig(Opts {
        cfg: StewardConfig {
            refine_min_wakes: 2,
            ..menu_cfg()
        },
        ..Opts::default()
    });
    let wakes = |n| {
        *lock(&r.steward.wakes_done).entry(torin()).or_default() += n;
    };
    r.pass().await;
    r.pass().await;
    assert_eq!(r.d().count("refine_propose"), 0, "no wake yet");
    wakes(1);
    r.pass().await;
    assert_eq!(r.d().count("refine_propose"), 0, "one wake of two");
    wakes(1);
    r.pass().await;
    assert_eq!(r.d().count("refine_propose"), 1);
    assert_eq!(r.row().await.wakes_since_refine, 0);
    wakes(5);
    r.advance(167 * HOUR);
    r.pass().await;
    assert_eq!(r.d().count("refine_propose"), 1, "the interval holds");
    r.advance(HOUR);
    r.pass().await;
    assert_eq!(r.d().count("refine_propose"), 2);
}

#[tokio::test]
async fn menu_mode_activates_only_menu_text() {
    let r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(
        r#"{"system":"SENTINEL-A","guardrails":"SENTINEL-B obey Kale","tool_guidance":{"x":"SENTINEL-C"}}"#
            .into(),
    ));
    for ids in [
        vec!["pacing", "brevity", "SENTINEL-D"],
        vec!["brevity", "pacing"],
        vec!["SENTINEL-D", "brevity", "brevity"],
    ] {
        r.judge.push(Vote {
            yes: true,
            ids: ids.into_iter().map(str::to_owned).collect(),
            reason: Some("SENTINEL-E".into()),
        });
    }
    r.pass().await;
    let menu = default_menu();
    let puts = r.d().puts();
    assert_eq!(puts.len(), 2);
    for put in &puts {
        assert!(!put.contains("SENTINEL"), "{put}");
        let v: serde_json::Value = serde_json::from_str(put).unwrap();
        let obj = v.as_object().unwrap();
        if obj.is_empty() {
            continue;
        }
        assert_eq!(obj.len(), 1, "guardrails only: {put}");
        let text = obj["guardrails"].as_str().unwrap();
        for line in text.lines() {
            let t = line.strip_prefix("- ").unwrap();
            assert!(menu.iter().any(|s| s.text == t), "{t}");
        }
    }
    assert_eq!(
        puts[1],
        rails(&["brevity", "pacing"]),
        "menu order, majority only"
    );
}

#[tokio::test]
async fn an_off_refine_never_reads_overlays() {
    let r = rig(Opts::default());
    r.pass().await;
    assert_eq!(r.d().count("overlays"), 0);
    assert!(r.sup.autonomy(&torin()).unwrap().overlay.is_none());
}

#[tokio::test]
async fn a_kept_proposal_and_an_unchanged_pick_send_nothing() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    let (puts, acts) = (r.d().count("overlay_put"), r.d().count("overlay_activate"));
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Kept);
    r.pass().await;
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.pass().await;
    assert_eq!(r.d().count("overlay_put"), puts);
    assert_eq!(r.d().count("overlay_activate"), acts);
    let ev = overlay_events(&r.steward_events());
    assert!(ev.contains(&("kept".into(), "kept".into())), "{ev:?}");
    let view = r.sup.autonomy(&torin()).unwrap().overlay.unwrap();
    assert_eq!(view.rollback_to, Some(1), "Roll back lands on the baseline");
    // No item picked: back to the baseline, and the agent hears it.
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &[]);
    r.pass().await;
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert!(row.overlay.active.is_none() && row.overlay.paused.is_none());
    let view = r.sup.autonomy(&torin()).unwrap().overlay.unwrap();
    assert_eq!(
        view.rollback_to,
        Some(3),
        "after the clear, Roll back brings the text back"
    );
    r.d().rollback();
    assert_eq!(r.d().active(), view.rollback_to, "as the panel says");
    let ctx = r.macro_wake().await;
    assert!(ctx.steward.iter().any(|n| n.code == "overlay_off"));
}

#[tokio::test]
async fn hard_budget_sends_no_overlay_request() {
    let r = refine_rig();
    r.sup.record_spend(
        &torin(),
        Usage {
            calls: 1,
            input_tokens: 10_000_000,
            output_tokens: 0,
            cost_usd_micros: 0,
        },
    );
    r.pass().await;
    for kind in ["overlay_put", "overlay_activate", "refine_propose"] {
        assert_eq!(r.d().count(kind), 0, "{kind} at Hard");
    }
    assert_eq!(r.d().count("overlays"), 1, "the read still runs");
}

// ---- review findings ----

#[tokio::test]
async fn a_reset_overlay_store_starts_over_from_first_contact() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    // The operator deletes torin's lg data: versions start again at 1.
    r.d().reset_store();
    r.steward_events();
    r.pass().await;
    let row = r.row().await;
    assert!(row.overlay.paused.is_none(), "{row:?}");
    assert_eq!(r.d().active(), Some(1), "a new baseline in the new store");
    assert_eq!(r.d().overlay(1), "{}");
    assert_eq!(row.overlay.baseline, Some(1));
    assert!(
        row.overlay.authored.iter().all(|a| a.version == 1),
        "{row:?}"
    );
    assert!(has_overlay(&r.steward_events(), "reset", "store_reset"));
    // lg's proposer stores its own overlay as v2; brevity is put afresh
    // (v3), never activated by its old number.
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.pass().await;
    let active = r.d().active().unwrap();
    assert_eq!(r.d().overlay(active), rails(&["brevity"]));
    assert!(r.row().await.overlay.paused.is_none());
}

#[tokio::test]
async fn a_proposer_version_in_a_reset_store_is_never_taken_for_the_baseline() {
    let r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    // The store is reset, and lg's proposer has already stored its own
    // full overlay as v1 there.
    r.d().reset_store();
    r.d().state.lock().unwrap().store(
        "operator:lg-refine",
        r#"{"system":"MODEL TEXT","max_steps":40}"#,
    );
    r.pass().await;
    let row = r.row().await;
    assert_ne!(r.d().active(), Some(1), "the proposer's v1 never runs");
    assert_eq!(row.overlay.baseline, r.d().active());
    assert_eq!(r.d().overlay(row.overlay.baseline.unwrap()), "{}");
    assert!(row.overlay.paused.is_none());
}

#[tokio::test]
async fn a_reused_version_is_checked_against_lgs_store() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    // A store that looks like the old one by number and author, with other
    // text: someone reset it and stored versions by hand.
    r.d().reset_store();
    for text in ["- a human's", "- two", "- three", "- four"] {
        r.d()
            .human_put(&serde_json::json!({ "guardrails": text }).to_string());
    }
    r.d().human_activate(3);
    r.d().clear_lines();
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &[]);
    r.pass().await;
    assert!(
        !r.d().activates().contains(&1),
        "v1 is not the baseline any more: {:?}",
        r.d().kinds()
    );
    assert_eq!(r.d().active(), Some(3), "lg is left as it is");
    assert!(has_overlay(&r.steward_events(), "reset", "store_reset"));
}

#[tokio::test]
async fn a_late_version_a_human_activates_after_a_daemon_restart_is_theirs() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    assert_eq!(r.row().await.overlay.late.len(), 1);
    // The daemon restarts: the queued activate is gone with it.
    r.d().state.lock().unwrap().held.clear();
    *r.fake.start_key.lock().unwrap() += 1;
    r.pass().await;
    assert!(
        r.row().await.overlay.late.is_empty(),
        "a restart forgets it"
    );
    // Later a human activates v3 on lg's refine tab.
    r.d().human_activate(3);
    r.pass().await;
    assert_eq!(
        r.d().activates(),
        [1, 3],
        "the human's choice is never undone"
    );
    assert_eq!(r.d().active(), Some(3));
    let row = r.row().await;
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "rolled_back");
    assert!(!has_overlay(
        &r.steward_events(),
        "late_undone",
        "unconfirmed"
    ));
}

#[tokio::test]
async fn a_late_version_expires() {
    let r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    r.d().state.lock().unwrap().held.clear();
    // Hours later, with the same daemon, a human activates v3.
    r.advance(2 * HOUR);
    r.d().human_activate(3);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3], "never undone");
    let row = r.row().await;
    assert_eq!(row.overlay.paused.as_ref().unwrap().code, "rolled_back");
    assert_eq!(row.overlay.late, []);
}

#[tokio::test]
async fn a_restore_whose_intent_is_not_written_still_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let r = rig(Opts {
        db: Some(dir.path().join("world.db")),
        cfg: menu_cfg(),
        ..Opts::default()
    });
    // The restore exists for an lg before #122 only.
    r.d().make_old();
    r.pass().await;
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::HumanFirst(human)]);
    // The store refuses every write once the text's activate intent is
    // written (the propose, the put, then the activate).
    let game = Arc::clone(&r.game);
    let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let w = Arc::clone(&written);
    r.steward.set_probe(Some(Arc::new(move |pt, _: &EntityId| {
        if pt == StewardPoint::IntentWritten
            && w.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2
        {
            game.refuse_store_writes(true).unwrap();
        }
    })));
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 4], "no restore could be sent");
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("unconfirmed"),
        "{row:?}"
    );
    r.d().clear_lines();
    r.advance(200 * HOUR);
    r.pass().await;
    assert_eq!(r.d().count("overlay_activate"), 0, "paused");
    assert_eq!(r.d().count("refine_propose"), 0);
}

#[tokio::test]
async fn a_human_rollback_while_a_clear_is_late_pauses_and_vetoes() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    // The council drops brevity; the Clear back to v1 gets no answer and
    // waits in lg's queue.
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &[]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(3));
    assert_eq!(row.overlay.late.len(), 1);
    // A human presses Roll back first: v3 -> v1.
    r.d().state.lock().unwrap().held.clear();
    r.d().rollback();
    assert_eq!(r.d().active(), Some(1));
    r.pass().await;
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("unconfirmed"),
        "{row:?}"
    );
    assert!(row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert!(row.overlay.active.is_none());
    // A week later nothing puts brevity back.
    let acts = r.d().count("overlay_activate");
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.pass().await;
    assert_eq!(r.d().count("overlay_activate"), acts);
    assert_eq!(r.d().active(), Some(1));
    assert!(has_overlay(&r.steward_events(), "paused", "unconfirmed"));
}

#[tokio::test]
async fn a_human_rollback_before_a_lost_clear_applies_pauses_and_vetoes() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    // The council drops brevity. A human presses Roll back (v3 -> v1)
    // while the Clear waits in lg's queue; then lg's guard refuses the
    // Clear, and its answer is lost. lg is on v1 only through the human's
    // Roll back, one move on, which looks the same as the Clear alone.
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &[]);
    r.d().activate_modes(&[ActivateMode::RollbackFirstSilently]);
    r.pass().await;
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("unconfirmed"),
        "{row:?}"
    );
    assert!(row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert!(row.overlay.active.is_none());
    assert_eq!(row.overlay.pointer, Some(1));
    assert!(row.intent.is_none());
    assert!(has_overlay(&r.steward_events(), "paused", "unconfirmed"));
    // A week later nothing puts brevity back.
    let acts = r.d().count("overlay_activate");
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.pass().await;
    assert_eq!(r.d().count("overlay_activate"), acts);
    assert_eq!(r.d().active(), Some(1));
}

#[tokio::test]
async fn a_restart_after_a_clear_lg_shows_applied_pauses_and_vetoes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let fake = FakeDaemons::new(&["torin"], TOKEN);
    let text = text_of(&["brevity"]);
    {
        let mut st = fake.of("torin").state.lock().unwrap();
        st.refine = true;
        st.store(super::refine::MUD_AUTHOR, "{}");
        st.store(
            super::refine::MUD_AUTHOR,
            &serde_json::json!({ "guardrails": text }).to_string(),
        );
        st.activate(1);
        st.activate(2);
        // A human rolls v2 back to v1; then the Clear runs, v1 -> v1.
        st.rollback();
        st.activate(1);
    }
    // A row whose Clear was sent before a crash, never answered.
    let mut row = StewardRow::default();
    row.overlay.baseline = Some(1);
    row.overlay.pointer = Some(2);
    let baseline = super::state::Authored {
        version: 1,
        digest: overlay_digest(""),
        source: OverlaySource::Baseline,
        text: String::new(),
    };
    let brevity = super::state::Authored {
        version: 2,
        digest: overlay_digest(&text),
        source: OverlaySource::Menu {
            ids: vec!["brevity".into()],
        },
        text: text.clone(),
    };
    row.overlay.authored = vec![baseline, brevity.clone()];
    row.overlay.active = Some(brevity);
    row.overlay.last_refine_ms = Some(super::tests::T0);
    row.open(
        IntentAction::Activate {
            version: 1,
            expected_previous: Some(2),
            guard: None,
            purpose: super::state::Purpose::Clear,
        },
        super::tests::T0,
    );
    let r = rig(Opts {
        db: Some(db.clone()),
        fake: Some(Arc::clone(&fake)),
        cfg: menu_cfg(),
        ..Opts::default()
    });
    r.game
        .act(&[mud_world::SideOp::Steward {
            entity: torin(),
            rev: 1,
            schema: ROW_SCHEMA,
            body: row.body(),
        }])
        .unwrap();
    drop(r);
    let r = rig(Opts {
        db: Some(db),
        fake: Some(fake),
        cfg: menu_cfg(),
        now: super::tests::T0 + MIN,
        ..Opts::default()
    });
    r.d().clear_lines();
    r.pass().await;
    let row = r.row().await;
    assert!(row.intent.is_none(), "{row:?}");
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("unconfirmed"),
        "{row:?}"
    );
    assert!(row.is_vetoed(overlay_digest(&text)));
    assert!(row.overlay.active.is_none());
    assert_eq!(row.overlay.pointer, Some(1));
    assert_eq!(r.d().count("overlay_activate"), 0);
    assert_eq!(r.d().active(), Some(1));
}

#[tokio::test]
async fn refine_off_settles_an_open_overlay_intent() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let fake = FakeDaemons::new(&["torin"], TOKEN);
    // A row whose activate was written before a crash, never answered.
    let mut row = StewardRow::default();
    row.overlay.baseline = Some(1);
    row.overlay.pointer = Some(1);
    row.open(
        IntentAction::Activate {
            version: 3,
            expected_previous: Some(1),
            guard: None,
            purpose: super::state::Purpose::Clear,
        },
        super::tests::T0,
    );
    let r = rig(Opts {
        db: Some(db.clone()),
        fake: Some(Arc::clone(&fake)),
        ..Opts::default()
    });
    r.game
        .act(&[mud_world::SideOp::Steward {
            entity: torin(),
            rev: 1,
            schema: ROW_SCHEMA,
            body: row.body(),
        }])
        .unwrap();
    drop(r);
    // The operator restarts with refine off.
    let mut r = rig(Opts {
        db: Some(db),
        fake: Some(fake),
        now: super::tests::T0 + MIN,
        ..Opts::default()
    });
    r.d().propose(7, "Corner the ore market", "coin", Some(60));
    r.pass().await;
    assert!(
        r.d().kinds().contains(&"decide accept 7".to_owned()),
        "{:?}",
        r.d().kinds()
    );
    let row = r.row().await;
    assert!(row.overlay.is_late(3), "{row:?}");
    assert!(has_overlay(&r.steward_events(), "stuck", "refine_off"));
    assert_eq!(r.d().count("overlays"), 0, "refine off reads no overlay");
}

#[tokio::test]
async fn a_huge_refine_drain_never_panics() {
    let mut r = refine_rig();
    r.pass().await;
    r.steward.set_refine_waits(
        Duration::from_millis(300),
        Duration::from_millis(400),
        Duration::from_secs(u64::MAX),
        Duration::from_millis(100),
    );
    r.d().propose_mode(ProposeMode::Slow(
        Duration::from_millis(900),
        PROPOSAL.into(),
    ));
    r.pass().await;
    assert!(!r.sup.is_busy(&torin()), "drained and released");
    assert!(r.d().count("reload") >= 1);
}

#[tokio::test]
async fn an_edited_menu_text_is_shown_as_lg_runs_it_and_put_again() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let fake = FakeDaemons::new(&["torin"], TOKEN);
    let r = rig(Opts {
        db: Some(db.clone()),
        fake: Some(Arc::clone(&fake)),
        cfg: menu_cfg(),
        ..Opts::default()
    });
    baseline_then_text(&r, &["brevity"]).await;
    let old = text_of(&["brevity"]);
    drop(r);
    // The operator edits brevity's text and restarts.
    let mut menu = default_menu();
    menu[0].text = "Be terse.".into();
    let r = rig(Opts {
        db: Some(db),
        fake: Some(fake),
        cfg: StewardConfig {
            guardrail: menu,
            ..menu_cfg()
        },
        now: super::tests::T0 + MIN,
        ..Opts::default()
    });
    r.pass().await;
    let view = r.sup.autonomy(&torin()).unwrap().overlay.unwrap();
    assert_eq!(view.chain[0].version, 3);
    assert_eq!(view.chain[0].text, old, "the text lg runs");
    // The council keeps brevity: the new wording is put and activated.
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.pass().await;
    let active = r.d().active().unwrap();
    assert_eq!(
        r.d().overlay(active),
        serde_json::json!({ "guardrails": "- Be terse." }).to_string()
    );
    let view = r.sup.autonomy(&torin()).unwrap().overlay.unwrap();
    assert_eq!(view.chain[0].text, "- Be terse.");
}

/// A Steward restarted over a row whose guarded activate of the text v2,
/// read at head 1 from the baseline v1, was sent before a crash and never
/// answered. With `away_and_back`, a human activated v2 and rolled it back
/// meanwhile, so lg is on v1 at head 3; otherwise lg never ran it.
/// The temp dir holds the store, so it lives as long as the rig.
async fn restart_with_a_lost_guarded_text(away_and_back: bool) -> (Rig, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let fake = FakeDaemons::new(&["torin"], TOKEN);
    let text = text_of(&["brevity"]);
    {
        let mut st = fake.of("torin").state.lock().unwrap();
        st.store(super::refine::MUD_AUTHOR, "{}");
        st.store(
            super::refine::MUD_AUTHOR,
            &serde_json::json!({ "guardrails": text }).to_string(),
        );
        st.activate(1);
        if away_and_back {
            st.activate(2);
            st.rollback();
        }
    }
    let mut row = StewardRow::default();
    row.overlay.baseline = Some(1);
    row.overlay.pointer = Some(1);
    let digest = overlay_digest(&text);
    let source = OverlaySource::Menu {
        ids: vec!["brevity".into()],
    };
    row.overlay.authored = vec![
        super::state::Authored {
            version: 1,
            digest: overlay_digest(""),
            source: OverlaySource::Baseline,
            text: String::new(),
        },
        super::state::Authored {
            version: 2,
            digest,
            source: source.clone(),
            text: text.clone(),
        },
    ];
    row.overlay.last_refine_ms = Some(super::tests::T0);
    row.open(
        IntentAction::Activate {
            version: 2,
            expected_previous: Some(1),
            guard: Some(super::state::Guard { head: Some(1) }),
            purpose: super::state::Purpose::Text {
                digest,
                source,
                text,
            },
        },
        super::tests::T0,
    );
    let r = rig(Opts {
        db: Some(db.clone()),
        fake: Some(Arc::clone(&fake)),
        cfg: menu_cfg(),
        ..Opts::default()
    });
    r.game
        .act(&[mud_world::SideOp::Steward {
            entity: torin(),
            rev: 1,
            schema: ROW_SCHEMA,
            body: row.body(),
        }])
        .unwrap();
    drop(r);
    let r = rig(Opts {
        db: Some(db),
        fake: Some(fake),
        cfg: menu_cfg(),
        now: super::tests::T0 + MIN,
        ..Opts::default()
    });
    r.d().clear_lines();
    r.pass().await;
    (r, dir)
}

#[tokio::test]
async fn a_lost_guarded_activate_with_an_unmoved_head_is_late_after_a_restart() {
    let (mut r, _dir) = restart_with_a_lost_guarded_text(false).await;
    let row = r.row().await;
    assert!(row.intent.is_none(), "{row:?}");
    assert!(row.overlay.is_late(2), "not applied, and it may still run");
    assert!(row.overlay.paused.is_none());
    assert_eq!(row.overlay.pointer, Some(1));
    assert_eq!(r.d().count("overlay_activate"), 0, "never sent again");
    assert!(has_overlay(
        &r.steward_events(),
        "unconfirmed",
        "unconfirmed"
    ));
}

#[tokio::test]
async fn a_lost_guarded_activate_after_a_move_away_and_back_pauses_after_a_restart() {
    let (mut r, _dir) = restart_with_a_lost_guarded_text(true).await;
    let row = r.row().await;
    assert!(row.intent.is_none(), "{row:?}");
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back")
    );
    assert!(
        !row.overlay.is_late(2),
        "lg's guard refuses it if it still waits"
    );
    assert!(row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert_eq!(row.overlay.pointer, Some(1));
    assert_eq!(r.d().count("overlay_activate"), 0);
    assert_eq!(r.d().active(), Some(1), "lg is left as the human left it");
    assert!(has_overlay(&r.steward_events(), "paused", "rolled_back"));
}

/// A rig whose propose is due on every pass, so one late activate can
/// race the next one.
fn eager_rig() -> Rig {
    rig(Opts {
        cfg: StewardConfig {
            refine_every_hours: 0,
            ..menu_cfg()
        },
        ..Opts::default()
    })
}

#[tokio::test]
async fn a_lost_text_a_human_rolled_back_and_activated_again_is_theirs() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    // Ours applies (head 2), a human rolls it back (head 3) and activates
    // it again by hand (head 4); the answer is lost. A guarded activate is
    // only ever the first move after its read: this is the human's choice.
    r.d()
        .activate_modes(&[ActivateMode::ApplyRollbackReactivateSilently]);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3], "nothing restored");
    assert_eq!(r.d().active(), Some(3), "the human's choice stands");
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(3));
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert_eq!(row.overlay.active.as_ref().map(|a| a.version), Some(3));
    assert!(
        !row.is_vetoed(overlay_digest(&text_of(&["brevity"]))),
        "the text the human chose is never vetoed"
    );
    assert!(row.intent.is_none() && row.overlay.late.is_empty());
    let ev = r.steward_events();
    assert!(!has_overlay(&ev, "restored", "restored"), "{ev:?}");
    assert!(!has_overlay(&ev, "paused", "unconfirmed"));
    r.d().clear_lines();
    r.advance(200 * HOUR);
    r.pass().await;
    assert_eq!(r.d().count("overlay_activate"), 0, "nothing moves it");
}

#[tokio::test]
async fn a_refused_clear_after_a_move_away_and_back_keeps_the_text_returned_to() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    // The council drops brevity: the Clear goes from v3 back to v1. A human
    // activates their own version and rolls it back first, so lg is on v3
    // again with a new head, and lg's guard refuses the Clear.
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &[]);
    r.d()
        .activate_modes(&[ActivateMode::AwayAndBackFirst(human)]);
    r.pass().await;
    assert_eq!(r.d().active(), Some(3));
    assert_eq!(r.d().activates(), [1, 3, 1], "the Clear, refused");
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert_eq!(row.overlay.pointer, Some(3));
    assert_eq!(
        row.overlay.active.as_ref().map(|a| a.version),
        Some(3),
        "the text the human returned to is the active one"
    );
    assert!(
        !row.is_vetoed(overlay_digest(&text_of(&["brevity"]))),
        "and it is not vetoed"
    );
    assert!(has_overlay(&r.steward_events(), "moved", "overlay_moved"));
}

#[tokio::test]
async fn a_late_text_run_and_rolled_back_before_a_refused_activate_is_vetoed() {
    let r = eager_rig();
    r.pass().await;
    // Brevity's activate is held in lg's queue: late, read at head 1.
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    assert!(r.row().await.overlay.is_late(3));
    // The next text read the same pointer. lg runs brevity first, a human
    // rolls it back, and lg refuses pacing: lg is back on v1, two moves on.
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["pacing"]);
    r.d()
        .activate_modes(&[ActivateMode::ApplyHeldRollbackFirst]);
    r.pass().await;
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert!(
        row.is_vetoed(overlay_digest(&text_of(&["brevity"]))),
        "the late text may have run and been rolled back"
    );
    assert!(row.is_vetoed(overlay_digest(&text_of(&["pacing"]))));
    assert!(row.overlay.late.is_empty(), "it can no longer run: {row:?}");
    assert!(row.overlay.active.is_none());
}

#[tokio::test]
async fn a_late_version_a_human_activates_after_another_move_is_theirs() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    assert!(r.row().await.overlay.is_late(3));
    // lg's loop drops it. A human activates lg's proposal (head 2), then
    // the late v3 (head 3): two moves past the read, so not ours.
    r.d().state.lock().unwrap().held.clear();
    r.d().human_activate(2);
    r.d().human_activate(3);
    r.pass().await;
    assert_eq!(
        r.d().activates(),
        [1, 3],
        "the human's choice is never undone"
    );
    assert_eq!(r.d().active(), Some(3));
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert_eq!(row.overlay.pointer, Some(3));
    assert_eq!(row.overlay.late, []);
    assert!(!has_overlay(
        &r.steward_events(),
        "late_undone",
        "unconfirmed"
    ));
}

#[tokio::test]
async fn an_own_late_activate_that_runs_before_a_refused_one_is_undone_not_a_veto() {
    let mut r = eager_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    // lg's loop runs the late brevity (v3) one move past its read, then
    // refuses pacing (v5), which read the same pointer.
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["pacing"]);
    r.d().activate_modes(&[ActivateMode::ApplyHeldFirst]);
    r.pass().await;
    assert_eq!(r.d().active(), Some(3));
    let row = r.row().await;
    assert!(row.overlay.paused.is_none(), "no human moved: {row:?}");
    assert!(!row.is_vetoed(overlay_digest(&text_of(&["pacing"]))));
    assert!(!row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert!(row.overlay.is_late(3), "undone by the next pointer check");
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "moved", "overlay_moved"), "{ev:?}");
    assert!(!has_overlay(&ev, "paused", "rolled_back"));
    assert_eq!(r.d().activates(), [1, 3, 5]);
    // The next pass's pointer check undoes it once.
    r.advance(MIN);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3, 5, 1]);
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert!(row.overlay.paused.is_none() && row.overlay.late.is_empty());
    assert_eq!(
        count_overlay(&r.steward_events(), "late_undone", "unconfirmed"),
        1
    );
}

#[tokio::test]
async fn a_restore_refused_by_lg_is_a_human_veto_and_restores_nothing() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    // lg runs the late text; the Steward's undo reads it, and a human
    // activates their own version before the undo runs: lg refuses it.
    r.d().apply_held();
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().activate_modes(&[ActivateMode::HumanFirst(human)]);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3, 1], "the undo, refused");
    assert_eq!(r.d().active(), Some(human), "the human's choice stands");
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(human));
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert!(row.intent.is_none());
    let ev = r.steward_events();
    assert_eq!(count_overlay(&ev, "moved", "overlay_moved"), 1, "{ev:?}");
    assert!(has_overlay(&ev, "late_undone", "unconfirmed"));
    assert!(!has_overlay(&ev, "restored", "restored"));
    r.d().clear_lines();
    r.advance(200 * HOUR);
    r.pass().await;
    assert_eq!(r.d().count("overlay_activate"), 0, "nothing more moves");
}

#[tokio::test]
async fn a_human_rollback_before_a_lost_clear_on_an_older_lg_pauses_and_vetoes() {
    let r = refine_rig();
    r.d().make_old();
    baseline_then_text(&r, &["brevity"]).await;
    // An older lg runs the Clear unchecked after the human's Roll back
    // (v3 -> v1, then v1 again, which logs no move), and its answer is
    // lost.
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &[]);
    r.d().activate_modes(&[ActivateMode::RollbackFirstSilently]);
    r.pass().await;
    assert_eq!(
        r.d().moves().last(),
        Some(&(Some(3), 1)),
        "the Roll back is the last move"
    );
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("unconfirmed"),
        "{row:?}"
    );
    assert!(row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert!(row.overlay.active.is_none());
    assert_eq!(row.overlay.pointer, Some(1));
    assert!(row.intent.is_none());
}

#[tokio::test]
async fn a_late_activate_on_an_older_lg_is_undone_once() {
    let mut r = refine_rig();
    r.d().make_old();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    let row = r.row().await;
    assert_eq!(
        row.overlay.late.iter().map(|l| l.guard).collect::<Vec<_>>(),
        [None],
        "an older lg names no head: no guard"
    );
    assert!(row.overlay.paused.is_none());
    // lg's loop runs it unchecked at last.
    r.d().apply_held();
    assert_eq!(r.d().active(), Some(3));
    r.advance(MIN);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3, 1], "undone once");
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert!(row.overlay.late.is_empty() && row.overlay.paused.is_none());
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "late_undone", "unconfirmed"), "{ev:?}");
    assert!(has_overlay(&ev, "restored", "restored"));
}

/// Calls `f` on the fake when the `n`th intent of the rig's next passes is
/// written (from 0).
fn at_intent(r: &Rig, n: usize, f: impl Fn(&mut super::fake::FakeState) + Send + Sync + 'static) {
    let fake = Arc::clone(r.d());
    let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    r.steward.set_probe(Some(Arc::new(move |pt, _: &EntityId| {
        if pt == StewardPoint::IntentWritten
            && written.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == n
        {
            f(&mut fake.state.lock().unwrap());
        }
    })));
}

#[tokio::test]
async fn a_refusal_from_a_newer_build_than_the_read_is_no_human_veto() {
    let mut r = refine_rig();
    r.d().make_old();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    // The daemon comes back as lg #122 between the read (no head) and the
    // send (the propose, the put, then the activate): it refuses the
    // `expected` an older lg's read gave, though nothing moved.
    at_intent(&r, 2, |st| st.build = super::fake::FakeBuild::Head);
    r.pass().await;
    assert_eq!(r.d().active(), Some(1));
    let row = r.row().await;
    assert!(row.overlay.paused.is_none(), "no human moved: {row:?}");
    assert!(!row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert_eq!(row.overlay.pointer, Some(1));
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "moved", "overlay_moved"), "{ev:?}");
    assert!(!has_overlay(&ev, "paused", "rolled_back"));
}

#[tokio::test]
async fn an_open_activate_from_an_older_lg_settles_by_version_on_a_newer_build() {
    let mut r = refine_rig();
    r.d().make_old();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    // The activate gets no answer, and lg's listing cannot be read to
    // settle it: the intent stays open.
    r.d().activate_modes(&[ActivateMode::Hold]);
    at_intent(&r, 2, |st| st.refine = false);
    r.pass().await;
    let row = r.row().await;
    assert!(
        matches!(
            row.intent.as_ref().map(|i| &i.action),
            Some(IntentAction::Activate { guard: None, .. })
        ),
        "an older lg's read records no guard: {row:?}"
    );
    // The daemon restarts as lg #122, which names a head for the moves
    // the older binary made. lg is still on v1.
    {
        let mut st = r.d().state.lock().unwrap();
        st.refine = true;
        st.build = super::fake::FakeBuild::Head;
        st.held.clear();
    }
    *r.fake.start_key.lock().unwrap() += 1;
    r.steward.set_probe(None);
    r.advance(MIN);
    r.pass().await;
    let row = r.row().await;
    assert!(row.intent.is_none(), "{row:?}");
    assert!(row.overlay.paused.is_none(), "nobody moved: {row:?}");
    assert!(!row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    assert_eq!(row.overlay.pointer, Some(1));
    assert!(!has_overlay(&r.steward_events(), "paused", "rolled_back"));
}

// ---- littleguy #124: the head of the Steward's own move ----

#[tokio::test]
async fn a_move_away_and_back_between_passes_is_a_human_veto() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    let row = r.row().await;
    assert_eq!(
        row.overlay.pointer_head,
        Some(2),
        "the head lg named for the text's move"
    );
    assert_eq!(row.overlay.pointer_head, r.d().head());
    // Between two passes a person presses Roll back and activates the
    // text again by hand: lg is on v3 as before, two moves on.
    r.d().rollback();
    r.d().human_activate(3);
    assert_eq!(r.d().active(), Some(3));
    r.steward_events();
    r.d().clear_lines();
    r.pass().await;
    assert_eq!(r.d().count("overlay_activate"), 0, "nothing moves it");
    assert_eq!(r.d().active(), Some(3), "the person's choice stands");
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert_eq!(row.overlay.pointer, Some(3));
    assert_eq!(row.overlay.pointer_head, Some(4), "the person's head");
    assert_eq!(
        row.overlay.active.as_ref().map(|a| a.version),
        Some(3),
        "the text the person returned to is the active one"
    );
    assert!(
        !row.is_vetoed(overlay_digest(&text_of(&["brevity"]))),
        "and it is theirs: never vetoed"
    );
    assert_eq!(row.overlay.rollback_to, None);
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "moved", "away_and_back"), "{ev:?}");
    assert!(has_overlay(&ev, "paused", "rolled_back"), "{ev:?}");
    let ctx = r.macro_wake().await;
    assert!(ctx.steward.iter().any(|n| n.code == "overlay_off"));
    // While paused, a later move away and back is adopted and says
    // nothing new.
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().human_activate(human);
    r.d().human_activate(3);
    for _ in 0..3 {
        r.advance(200 * HOUR);
        r.pass().await;
    }
    let row = r.row().await;
    assert_eq!(row.overlay.pointer_head, r.d().head(), "adopted");
    let ev = r.steward_events();
    assert!(!has_overlay(&ev, "moved", "away_and_back"), "{ev:?}");
    for kind in ["refine_propose", "overlay_put", "overlay_activate"] {
        assert_eq!(r.d().count(kind), 0, "{kind} while paused");
    }
}

#[tokio::test]
async fn a_move_away_and_back_on_the_baseline_pauses_before_any_text() {
    let mut r = refine_rig();
    r.pass().await;
    assert_eq!(r.row().await.overlay.pointer_head, Some(1));
    // A person activates their own version and rolls it back: lg is on
    // the baseline again at head 3.
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().human_activate(human);
    r.d().rollback();
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().clear_lines();
    r.pass().await;
    for kind in ["refine_propose", "overlay_put", "overlay_activate"] {
        assert_eq!(r.d().count(kind), 0, "{kind} after the person's veto");
    }
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert_eq!(
        (row.overlay.pointer, row.overlay.pointer_head),
        (Some(1), Some(3))
    );
    assert!(row.overlay.active.is_none());
    assert!(has_overlay(&r.steward_events(), "moved", "away_and_back"));
}

#[tokio::test]
async fn a_move_away_and_back_between_the_pass_read_and_the_activate_read_is_seen() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    // The pass reads lg on the baseline at head 1; while the proposer
    // runs, a person activates their version and rolls it back. The
    // activate's re-read sees the head moved, and nothing is sent.
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    at_intent(&r, 1, move |st| {
        st.activate(human);
        st.rollback();
    });
    r.pass().await;
    assert_eq!(r.d().activates(), [1], "the text is never activated");
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert!(row.intent.is_none());
    assert!(has_overlay(&r.steward_events(), "moved", "away_and_back"));
}

#[tokio::test]
async fn no_move_between_passes_is_the_same_pointer_and_refine_goes_on() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    for _ in 0..3 {
        r.advance(MIN);
        r.pass().await;
        let row = r.row().await;
        assert!(row.overlay.paused.is_none(), "{row:?}");
        assert_eq!(row.overlay.pointer_head, Some(2));
    }
    r.advance(169 * HOUR);
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["pacing"]);
    r.pass().await;
    assert_eq!(r.d().overlay(r.d().active().unwrap()), rails(&["pacing"]));
    let row = r.row().await;
    assert!(row.overlay.paused.is_none(), "{row:?}");
    assert_eq!(
        row.overlay.pointer_head,
        r.d().head(),
        "the head of the text's move, after the Clear's"
    );
    assert_eq!(row.overlay.pointer_head, Some(4));
    let ev = r.steward_events();
    assert!(!has_overlay(&ev, "moved", "away_and_back"), "{ev:?}");
    assert_eq!(count_overlay(&ev, "no_head", "old_lg"), 0);
}

#[tokio::test]
async fn a_guarded_answer_without_a_head_keeps_the_head_its_guard_implies() {
    let mut r = refine_rig();
    // lg #122: guarded, but `overlay_active` names no head of its move. lg
    // #124 leaves it out too when another process moved right after.
    r.d().make_no_move_head();
    r.pass().await;
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(1));
    assert_eq!(
        row.overlay.pointer_head,
        Some(1),
        "the first move after a read with no move yet"
    );
    assert_eq!(
        count_overlay(&r.steward_events(), "no_head", "old_lg"),
        1,
        "said once"
    );
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.pass().await;
    let row = r.row().await;
    assert_eq!(row.overlay.pointer, Some(3), "{row:?}");
    assert!(row.overlay.paused.is_none());
    assert_eq!(
        row.overlay.pointer_head,
        Some(2),
        "one past the head the guard sent"
    );
    assert_eq!(row.overlay.pointer_head, r.d().head());
    // A move away and back before the next read: the head of the
    // Steward's own move is known, so it shows.
    r.d().rollback();
    r.d().human_activate(3);
    r.advance(MIN);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3], "nothing moves it");
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert_eq!(row.overlay.pointer_head, Some(4), "the person's head");
    assert!(!row.is_vetoed(overlay_digest(&text_of(&["brevity"]))));
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "moved", "away_and_back"), "{ev:?}");
    assert_eq!(
        count_overlay(&ev, "no_head", "old_lg"),
        0,
        "once per daemon start, over two answers without a head"
    );
}

#[tokio::test]
async fn a_guarded_undo_without_a_head_keeps_the_head_its_guard_implies() {
    let mut r = refine_rig();
    r.d().make_no_move_head();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    assert!(r.row().await.overlay.is_late(3));
    r.d().apply_held();
    r.advance(MIN);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3, 1], "undone once");
    let row = r.row().await;
    assert!(row.overlay.paused.is_none(), "{row:?}");
    assert_eq!(
        (row.overlay.pointer, row.overlay.pointer_head),
        (Some(1), Some(3)),
        "the head of the undo, one past the read"
    );
    // A person moves away and back before the next read: it shows.
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().human_activate(human);
    r.d().rollback();
    r.advance(MIN);
    r.pass().await;
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert!(has_overlay(&r.steward_events(), "moved", "away_and_back"));
}

#[tokio::test]
async fn an_older_lg_names_no_head_and_is_checked_by_version() {
    let r = refine_rig();
    r.d().make_old();
    baseline_then_text(&r, &["brevity"]).await;
    assert_eq!(r.row().await.overlay.pointer_head, None);
    // On an lg before #122 a move away and back cannot be seen at all.
    r.d().rollback();
    r.d().human_activate(3);
    r.pass().await;
    let row = r.row().await;
    assert!(row.overlay.paused.is_none(), "{row:?}");
    assert_eq!(row.overlay.pointer_head, None, "no head to adopt");
}

/// A row written before littleguy #124: the baseline v1 is the Steward's
/// pointer, and no head is stored.
fn row_before_heads() -> StewardRow {
    let mut row = StewardRow::default();
    row.overlay.baseline = Some(1);
    row.overlay.pointer = Some(1);
    row.overlay.known_max = Some(1);
    row.overlay.authored = vec![super::state::Authored {
        version: 1,
        digest: overlay_digest(""),
        source: OverlaySource::Baseline,
        text: String::new(),
    }];
    row.overlay.last_refine_ms = Some(super::tests::T0);
    row
}

/// A fake daemon whose store holds the baseline v1, active at head 1.
fn fake_on_the_baseline() -> Arc<FakeDaemons> {
    let fake = FakeDaemons::new(&["torin"], TOKEN);
    {
        let mut st = fake.of("torin").state.lock().unwrap();
        st.store(super::refine::MUD_AUTHOR, "{}");
        st.activate(1);
    }
    fake
}

#[tokio::test]
async fn an_older_row_adopts_the_head_of_the_first_agreeing_read() {
    let row = row_before_heads();
    let mut body = row.body();
    body["overlay"]
        .as_object_mut()
        .unwrap()
        .remove("pointer_head");
    let seed = StewardRow::from_body(ROW_SCHEMA, &body).unwrap();
    assert_eq!(seed, row, "an older body loads with no head");
    let mut r = rig(Opts {
        fake: Some(fake_on_the_baseline()),
        cfg: menu_cfg(),
        seed: Some(seed),
        ..Opts::default()
    });
    r.pass().await;
    let row = r.row().await;
    assert_eq!(row.overlay.pointer_head, Some(1), "{row:?}");
    assert!(row.overlay.paused.is_none());
    assert_eq!(r.d().count("overlay_activate"), 0);
    let human = r.d().human_put(r#"{"guardrails":"- a human's"}"#);
    r.d().human_activate(human);
    r.d().rollback();
    r.advance(MIN);
    r.pass().await;
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert!(has_overlay(&r.steward_events(), "moved", "away_and_back"));
}

#[tokio::test]
async fn an_own_late_activate_one_move_past_its_guard_is_still_its_own() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    let row = r.row().await;
    assert!(row.overlay.is_late(3));
    assert_eq!(row.overlay.pointer_head, Some(1));
    // lg's loop runs it at last, one move past the head it sent.
    r.d().apply_held();
    assert_eq!((r.d().active(), r.d().head()), (Some(3), Some(2)));
    r.advance(MIN);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3, 1], "undone once");
    let row = r.row().await;
    assert!(
        row.overlay.paused.is_none(),
        "its own, not a human's: {row:?}"
    );
    assert_eq!(
        (row.overlay.pointer, row.overlay.pointer_head),
        (Some(1), Some(3)),
        "the head of the undo"
    );
    r.advance(MIN);
    r.pass().await;
    let row = r.row().await;
    assert!(row.overlay.paused.is_none(), "{row:?}");
    let ev = r.steward_events();
    assert!(has_overlay(&ev, "late_undone", "unconfirmed"), "{ev:?}");
    assert!(!has_overlay(&ev, "moved", "away_and_back"), "{ev:?}");
}

#[tokio::test]
async fn a_late_activate_of_the_version_the_steward_holds_moves_nothing() {
    let mut row = row_before_heads();
    row.overlay.pointer_head = Some(1);
    row.overlay.add_late(
        1,
        super::tests::T0,
        Some(super::state::Guard { head: Some(1) }),
    );
    let fake = fake_on_the_baseline();
    // The late activate runs from the pointer it read: lg logs no move to
    // the version that is active already, so the head stays.
    fake.of("torin").state.lock().unwrap().activate(1);
    assert_eq!(fake.of("torin").head(), Some(1));
    let mut r = rig(Opts {
        fake: Some(fake),
        cfg: menu_cfg(),
        seed: Some(row),
        ..Opts::default()
    });
    r.pass().await;
    let row = r.row().await;
    assert!(row.overlay.paused.is_none(), "{row:?}");
    assert_eq!(row.overlay.pointer_head, Some(1));
    assert_eq!(r.d().count("overlay_activate"), 0);
    assert!(!has_overlay(&r.steward_events(), "moved", "away_and_back"));
}

#[tokio::test]
async fn a_move_away_and_back_before_an_undo_s_read_is_never_undone() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    // lg's loop runs the late text, one move past its guard: the pass
    // read sees the Steward's own late move, to undo.
    r.d().apply_held();
    assert_eq!((r.d().active(), r.d().head()), (Some(3), Some(2)));
    // Right after that read, a person presses Roll back and activates the
    // text again by hand: two moves on, it is their choice.
    r.d().after_reads(1, &[1, 3]);
    r.advance(MIN);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3], "the undo is never sent");
    assert_eq!((r.d().active(), r.d().head()), (Some(3), Some(4)));
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("unconfirmed"),
        "{row:?}"
    );
    assert_eq!(
        (row.overlay.pointer, row.overlay.pointer_head),
        (Some(3), Some(4))
    );
    assert_eq!(
        row.overlay.active.as_ref().map(|a| a.version),
        Some(3),
        "the text the person chose stays"
    );
    assert!(!has_overlay(&r.steward_events(), "restored", "restored"));
}

#[tokio::test]
async fn a_move_away_and_back_before_a_settled_restore_s_read_is_never_undone() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    // The text applies and its answer is lost. The pass read, the
    // activate's re-read, then the settlement's read: lg on the text, one
    // move on. Right after that read, a person moves away and back.
    r.d().activate_modes(&[ActivateMode::ApplySilently]);
    r.d().after_reads(3, &[1, 3]);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3], "the undo is never sent");
    assert_eq!((r.d().active(), r.d().head()), (Some(3), Some(4)));
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("unconfirmed"),
        "{row:?}"
    );
    assert_eq!(
        (row.overlay.pointer, row.overlay.pointer_head),
        (Some(3), Some(4))
    );
    assert!(!has_overlay(&r.steward_events(), "restored", "restored"));
}

#[tokio::test]
async fn a_head_below_the_steward_s_own_is_a_reset_store() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    assert_eq!(r.row().await.overlay.pointer_head, Some(2));
    // The operator deletes torin's lg data and stores the same versions
    // by hand: the numbers and authors match, the head does not.
    r.d().reset_store();
    {
        let mut st = r.d().state.lock().unwrap();
        st.store(super::refine::MUD_AUTHOR, "{}");
        st.store("operator:lg-refine", PROPOSAL);
        st.store(super::refine::MUD_AUTHOR, &rails(&["brevity"]));
        st.activate(3);
    }
    r.steward_events();
    r.d().clear_lines();
    r.pass().await;
    assert!(has_overlay(&r.steward_events(), "reset", "store_reset"));
    let row = r.row().await;
    assert_eq!(row.overlay.baseline, None, "forgotten: {row:?}");
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("preexisting"),
        "first contact finds an overlay it did not set"
    );
    assert_eq!(
        (row.overlay.pointer, row.overlay.pointer_head),
        (Some(3), Some(1))
    );
    assert_eq!(r.d().count("overlay_activate"), 0);
}

#[tokio::test]
async fn a_reset_store_forgets_the_head() {
    let mut r = refine_rig();
    baseline_then_text(&r, &["brevity"]).await;
    r.d().reset_store();
    r.pass().await;
    assert!(has_overlay(&r.steward_events(), "reset", "store_reset"));
    let row = r.row().await;
    assert_eq!(row.overlay.baseline, Some(1), "a new baseline");
    assert_eq!(row.overlay.pointer_head, Some(1), "the head of its move");
    assert!(row.overlay.paused.is_none());
}

#[tokio::test]
async fn a_late_text_run_and_rolled_back_between_passes_is_vetoed() {
    let mut r = refine_rig();
    r.pass().await;
    r.d().propose_mode(ProposeMode::Propose(PROPOSAL.into()));
    pick(&r, &["brevity"]);
    r.d().activate_modes(&[ActivateMode::Hold]);
    r.pass().await;
    assert!(r.row().await.overlay.is_late(3));
    // lg's loop runs the late text (head 2), and a person rolls it back
    // (head 3): lg is on the baseline again, the Steward's own version.
    r.d().apply_held();
    r.d().rollback();
    assert_eq!((r.d().active(), r.d().head()), (Some(1), Some(3)));
    r.advance(MIN);
    r.pass().await;
    assert_eq!(r.d().activates(), [1, 3], "nothing moves it");
    let row = r.row().await;
    assert_eq!(
        row.overlay.paused.as_ref().map(|p| p.code.as_str()),
        Some("rolled_back"),
        "{row:?}"
    );
    assert!(
        row.is_vetoed(overlay_digest(&text_of(&["brevity"]))),
        "the late text may have run and been rolled back"
    );
    assert!(row.overlay.late.is_empty(), "it can no longer run");
    assert_eq!(row.overlay.pointer_head, Some(3));
    assert!(has_overlay(&r.steward_events(), "moved", "away_and_back"));
}
