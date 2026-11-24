//! World scripts in the server: reload, memory reset, the panel, `mud
//! check`, and the runtime opt-in.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::test::TestApp;
use mud_server::config::{MudConfig, Runtime};
use mud_server::{Built, build, check_pack, settings, web};
use mud_world::EntityId;
use serde_json::Value;
use tokio::net::TcpListener;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A copy of crypt-v01 in a temporary folder.
fn pack_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../worlds/crypt-v01");
    copy_dir(&src, dir.path());
    dir
}

async fn boot(tweak: impl FnOnce(&mut MudConfig)) -> (SocketAddr, Arc<Built>) {
    let mut cfg = MudConfig::default();
    cfg.world.db = None;
    cfg.supervisor.typing_scale = 0.0;
    tweak(&mut cfg);
    let built = Arc::new(build(&cfg).unwrap());
    let mud = Arc::clone(&built.mud);
    let router = TestApp::new()
        .config(settings(&cfg).unwrap())
        .routes(web::routes())
        .state_initializer(move |s| mud_server::install(s, &mud))
        .build()
        .into_router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        autumn_web::reexports::axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .ok();
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    (addr, built)
}

fn http() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

async fn post(addr: SocketAddr, path: &str) -> (u16, String) {
    let r = http()
        .post(format!("http://{addr}{path}"))
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn reloading_scripts_needs_the_admin_token_and_a_folder() {
    let (addr, built) = boot(|_| {}).await;
    assert_eq!(post(addr, "/observer/scripts/reload").await.0, 401);
    assert_eq!(
        post(addr, "/observer/scripts/reload?token=nope").await.0,
        401
    );
    let token = built.mud.admin_token.clone();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 409);
    assert_eq!(
        body,
        "the bundled pack cannot be reloaded; set [world] pack to a folder"
    );
    let (addr, built) = boot(|c| c.scripts.enabled = false).await;
    let token = built.mud.admin_token.clone();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 409);
    assert_eq!(body, "scripts are disabled ([scripts] enabled = false)");
    assert_eq!(
        built
            .mud
            .game
            .read(|w| w.state().scripts_version.clone())
            .as_deref(),
        Some("none")
    );
    let ada = built.mud.game.login("ada").unwrap();
    let r = built.mud.game.execute(&ada, "s");
    assert!(r.ok);
    let r = built.mud.game.execute(&ada, "listen");
    assert_eq!(r.error_code, Some(mud_world::ErrorCode::UnknownVerb));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_reload_keeps_the_old_scripts_and_a_good_one_changes_the_version() {
    let dir = pack_copy();
    let pack = dir.path().to_path_buf();
    let (addr, built) = boot(|c| c.world.pack = Some(pack)).await;
    let token = built.mud.admin_token.clone();
    let old = built.mud.scripts.as_ref().unwrap().version();
    let alley = dir.path().join("scripts/rooms/alley.rhai");
    let good = std::fs::read_to_string(&alley).unwrap();
    std::fs::write(
        &alley,
        good.replace("fn on_listen(ctx)", "fn on_listen(ctx, x)"),
    )
    .unwrap();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 422, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["version"], old);
    assert!(
        v["problems"][0].as_str().unwrap().contains("on_listen"),
        "{v}"
    );
    assert_eq!(built.mud.scripts.as_ref().unwrap().version(), old);
    // Fixed, with one more word: it takes.
    std::fs::write(&alley, good.replace("Nothing else.", "Nothing more.")).unwrap();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["previous"], old);
    assert_ne!(v["version"], old);
    assert_eq!(v["files"], 6);
    let marker = built.mud.game.read(|w| w.state().scripts_version.clone());
    assert_eq!(
        marker.as_ref(),
        v["version"].as_str().map(str::to_owned).as_ref()
    );
    // Unchanged files reload too, and say so.
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 200);
    let again: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(again["version"], again["previous"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_that_changes_runtimes_is_refused() {
    let dir = pack_copy();
    let pack = dir.path().to_path_buf();
    let (addr, built) = boot(|c| c.world.pack = Some(pack)).await;
    let token = built.mud.admin_token.clone();
    let manifest = dir.path().join("scripts.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, text.replace("runtime = \"rhai\"\n", "")).unwrap();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 422);
    assert!(
        body.contains("minds.guard_01: runtime changes need a restart"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_reset_needs_the_token_and_clears_one_mind() {
    let (addr, built) = boot(|_| {}).await;
    let token = built.mud.admin_token.clone();
    let hale = EntityId::from("guard_01");
    built
        .mud
        .game
        .save_script_memory(&hale, 1, serde_json::json!({ "day": 1 }));
    assert_eq!(
        post(addr, "/observer/scripts/memory/guard_01/reset")
            .await
            .0,
        401
    );
    let (status, _) = post(
        addr,
        &format!("/observer/scripts/memory/torin/reset?token={token}"),
    )
    .await;
    assert_eq!(status, 404, "torin has no mind script");
    let (status, body) = post(
        addr,
        &format!("/observer/scripts/memory/guard_01/reset?token={token}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        built.mud.game.script_memory(&hale, 1),
        serde_json::json!({})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scripts_panel_shows_hooks_and_minds_escaped() {
    let (addr, built) = boot(|_| {}).await;
    let token = built.mud.admin_token.clone();
    let denied = http()
        .get(format!("http://{addr}/observer/scripts"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let html = http()
        .get(format!("http://{addr}/observer/scripts?token={token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("verbs.pray"), "{html}");
    assert!(html.contains("minds.guard_01.on_wake"));
    assert!(html.contains("Reset memory"));
    assert!(
        html.contains("bundled"),
        "no reload button for the bundled pack"
    );
    let page = http()
        .get(format!("http://{addr}/observer?token={token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("/observer/scripts?token="));
    let line = web::script_line(&mud_world::Audit::ScriptLog {
        hook: "verbs.pray".into(),
        entity: EntityId::from("pc_ada"),
        line: "<b>bold</b>".into(),
        at_ms: 0,
    })
    .unwrap();
    assert_eq!(
        line, "verbs.pray log (pc_ada) · <b>bold</b>",
        "text for textContent"
    );
}

#[test]
fn check_reports_scripts_and_lists_every_problem() {
    let dir = pack_copy();
    let lines = check_pack(dir.path()).unwrap();
    assert!(lines[1].starts_with("scripts rhai-"), "{lines:?}");
    assert!(lines[1].ends_with("(6 files: 1 mind, 2 rooms, 2 items, 2 verbs) — ok"));
    std::fs::write(dir.path().join("scripts/extra.rhai"), "fn x(ctx) { [] }").unwrap();
    let lines = check_pack(dir.path()).unwrap();
    assert_eq!(
        lines[2],
        "warning: scripts/extra.rhai: not used by scripts.toml"
    );
    let manifest = dir.path().join("scripts.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace("on_enter = true", "on_enter = true\non_speech = true")
            .replace("name = \"pray\"", "name = \"look\""),
    )
    .unwrap();
    let err = check_pack(dir.path()).unwrap_err().to_string();
    assert!(err.contains("ossuary.rhai: no fn on_speech(ctx)"), "{err}");
    assert!(err.contains("\"look\" shadows a built-in verb"), "{err}");
}

#[test]
fn serve_refuses_a_broken_script() {
    let dir = pack_copy();
    let hale = dir.path().join("scripts/minds/hale.rhai");
    let text = std::fs::read_to_string(&hale).unwrap();
    std::fs::write(
        &hale,
        text.replace("ctx.config.night_from", "ctx.config.night_start"),
    )
    .unwrap();
    let mut cfg = MudConfig::default();
    cfg.world.db = None;
    cfg.world.pack = Some(dir.path().to_path_buf());
    let err = build(&cfg).unwrap_err().to_string();
    assert!(err.contains("dry run minds.guard_01.on_wake"), "{err}");
    assert!(err.contains("night_start"), "{err}");
}

#[test]
fn runtime_rhai_changes_no_view() {
    let mut on = MudConfig::default();
    on.world.db = None;
    let mut off = on.clone();
    off.scripts.allow_runtime_override = false;
    off.minds.ambient = Runtime::Scripted;
    let (with_rhai, without) = (build(&on).unwrap(), build(&off).unwrap());
    for id in ["pc_kestrel", "guard_01"] {
        let e = EntityId::from(id);
        assert_eq!(
            with_rhai.mud.game.look(&e),
            without.mud.game.look(&e),
            "{id}"
        );
        assert_eq!(
            with_rhai.mud.game.sheet(&e),
            without.mud.game.sheet(&e),
            "{id}"
        );
        let look = with_rhai.mud.game.look(&e).to_string();
        for bad in ["runtime", "rhai", "tier", "mind"] {
            assert!(!look.contains(bad), "{bad} in {look}");
        }
    }
}
