//! Refine (ADR 0015, phase 4) against fake daemons that keep an lg-like
//! overlay pointer and answer `previous` as lg does.

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

#[tokio::test]
async fn a_human_move_between_read_and_activate_is_restored() {
    let mut r = refine_rig();
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
