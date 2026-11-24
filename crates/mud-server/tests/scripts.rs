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
use mud_world::hooks::{HookId, HookInput, HookLimits, HookReply, VerbInfo, WorldDice, WorldHooks};
use mud_world::{EntityId, ErrorCode, HooksChange, World};
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
    cfg.accounts.bcrypt_cost = 4;
    tweak(&mut cfg);
    let built = Arc::new(build(&cfg).await.unwrap());
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
async fn reloading_scripts_needs_the_admin_token_and_scripts() {
    let (addr, built) = boot(|_| {}).await;
    assert_eq!(post(addr, "/observer/scripts/reload").await.0, 401);
    assert_eq!(
        post(addr, "/observer/scripts/reload?token=nope").await.0,
        401
    );
    let token = built.mud.admin_token.clone();
    // The bundled pack has nothing to read again; its reload re-arms the
    // same scripts: breakers reset, and a marker records it.
    let version = built.mud.scripts.as_ref().unwrap().version();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["version"], version);
    assert_eq!(v["previous"], version);
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

/// The bundled scripts, except that every call faults.
#[derive(Debug)]
struct Faulting(Arc<mud_script::ScriptHost>);

impl WorldHooks for Faulting {
    fn version(&self) -> String {
        self.0.version()
    }
    fn has(&self, hook: &HookId) -> bool {
        self.0.has(hook)
    }
    fn verb(&self, name: &str) -> Option<VerbInfo> {
        self.0.verb(name)
    }
    fn verbs(&self) -> Vec<VerbInfo> {
        self.0.verbs()
    }
    fn limits(&self) -> HookLimits {
        self.0.limits()
    }
    fn call(&self, _: &HookId, _: &HookInput, _: WorldDice) -> HookReply {
        HookReply {
            outcome: Err("an input the script did not expect".into()),
            log: Vec::new(),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tripped_bundled_hook_survives_a_restart_and_reopens_on_reload() {
    let (addr, built) = boot(|_| {}).await;
    let token = built.mud.admin_token.clone();
    let host = Arc::clone(built.mud.scripts.as_ref().unwrap());
    let game = &built.mud.game;
    let ada = game.login("ada").unwrap();
    assert!(game.execute(&ada, "s").ok);
    game.write(|w, now| {
        w.set_hooks(
            Some(Arc::new(Faulting(Arc::clone(&host))) as Arc<dyn WorldHooks>),
            now,
            HooksChange::Start,
        );
    });
    for _ in 0..3 {
        let r = game.execute(&ada, "listen");
        assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed));
    }
    // A restart installs the same version, so the saved faults stand.
    game.write(|w, now| {
        w.set_hooks(
            Some(Arc::clone(&host) as Arc<dyn WorldHooks>),
            now,
            HooksChange::Start,
        );
    });
    assert!(
        game.read(World::hook_status)
            .iter()
            .any(|s| s.key == "verbs.listen" && s.tripped)
    );
    let r = game.execute(&ada, "listen");
    assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed), "{r:?}");
    // The operator's Reload reopens it, bundled pack or not.
    let reloads = host.reloads();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(host.reloads(), reloads + 1, "mind breakers follow it too");
    assert!(game.read(World::hook_status).iter().all(|s| !s.tripped));
    let r = game.execute(&ada, "listen");
    assert!(r.ok, "{r:?}");
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
    let mut audit = built.mud.game.subscribe_audit();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 422, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["version"], old);
    assert!(
        v["problems"][0].as_str().unwrap().contains("on_listen"),
        "{v}"
    );
    // The observer's feed says so too: the Reload button shows no body.
    let refused = audit.try_recv().unwrap();
    let line = web::script_line(&refused).unwrap();
    assert!(
        line.starts_with(&format!(
            "scripts reload refused (still {old}): 1 problem · "
        )),
        "{line}"
    );
    assert!(line.contains("on_listen"), "{line}");
    assert_eq!(built.mud.scripts.as_ref().unwrap().version(), old);
    // Fixed, with one more word: it takes.
    std::fs::write(&alley, good.replace("Nothing else.", "Nothing more.")).unwrap();
    let (status, body) = post(addr, &format!("/observer/scripts/reload?token={token}")).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["previous"], old);
    assert_ne!(v["version"], old);
    assert_eq!(v["files"], 7);
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
    assert!(html.contains("Reload"), "the bundled pack re-arms: {html}");
    assert!(html.contains("0/3"), "each mind shows its faults: {html}");
    assert!(!html.contains("see feed"));
    assert!(
        html.contains("warden_bounty (per day)") && html.contains("currency 60, healing_draught 3"),
        "the reward pools and what is left of them: {html}"
    );
    assert!(html.contains("0 mints: currency 0, 0 items"), "{html}");
    let host = built.mud.scripts.as_ref().unwrap();
    let hale = EntityId::from("guard_01");
    for _ in 0..3 {
        host.step_mind(
            &hale,
            host.reloads(),
            mud_world::hooks::HookOutcome::Faulted,
        );
    }
    let html = http()
        .get(format!("http://{addr}/observer/scripts?token={token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let row = html
        .split("<tr>")
        .find(|r| r.contains("minds.guard_01.on_wake"))
        .unwrap();
    assert!(row.contains("3/3") && row.contains("TRIPPED"), "{row}");
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
    let line = web::script_line(&mud_world::Audit::Minted {
        hook: "verbs.report".into(),
        entity: EntityId::from("pc_ada"),
        pool: mud_world::PoolId::from("warden_bounty"),
        currency: 20,
        items: vec![mud_world::ProtoId::from("healing_draught")],
        at_ms: 0,
    })
    .unwrap();
    assert_eq!(
        line,
        "verbs.report minted from warden_bounty (pc_ada) · currency 20, healing_draught"
    );
}

#[test]
fn check_reports_scripts_and_lists_every_problem() {
    let dir = pack_copy();
    let lines = check_pack(dir.path()).unwrap();
    assert!(lines[1].starts_with("scripts rhai-"), "{lines:?}");
    assert!(lines[1].ends_with("(7 files: 1 mind, 2 rooms, 2 items, 3 verbs) — ok"));
    assert!(
        lines[0].ends_with("3 shops, 1 reward pool — ok"),
        "{lines:?}"
    );
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

#[tokio::test]
async fn serve_refuses_a_broken_script() {
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
    let err = build(&cfg).await.unwrap_err().to_string();
    assert!(err.contains("dry run minds.guard_01.on_wake"), "{err}");
    assert!(err.contains("night_start"), "{err}");
}

#[tokio::test]
async fn runtime_rhai_changes_no_view() {
    // Hale is the only ambient NPC. On littleguy (which needs no key to
    // build), the `runtime = "rhai"` override is the only reason he runs
    // his Rhai mind in `on`; in `off` he runs on littleguy.
    let mut on = MudConfig::default();
    on.world.db = None;
    on.minds.ambient = Runtime::Littleguy;
    let mut off = on.clone();
    off.scripts.allow_runtime_override = false;
    on.accounts.bcrypt_cost = 4;
    off.accounts.bcrypt_cost = 4;
    let (with_rhai, without) = (build(&on).await.unwrap(), build(&off).await.unwrap());
    let hale = EntityId::from("guard_01");
    assert!(!with_rhai.littleguy.iter().any(|m| m.serves(&hale)));
    assert!(without.littleguy.iter().any(|m| m.serves(&hale)));
    for b in [&with_rhai, &without] {
        let ada = b.mud.game.login("ada").unwrap();
        assert!(b.mud.game.execute(&hale, "say evening").ok);
        assert_eq!(ada.as_str(), "pc_ada");
    }
    let ada = EntityId::from("pc_ada");
    for id in [&ada, &EntityId::from("pc_kestrel"), &hale] {
        assert_eq!(
            with_rhai.mud.game.look(id),
            without.mud.game.look(id),
            "{id}"
        );
        assert_eq!(
            with_rhai.mud.game.sheet(id),
            without.mud.game.sheet(id),
            "{id}"
        );
        assert_eq!(
            with_rhai.mud.game.events(id, mud_world::Seq(0), 50),
            without.mud.game.events(id, mud_world::Seq(0), 50),
            "{id}"
        );
        let look = with_rhai.mud.game.look(id).to_string();
        for bad in ["runtime", "rhai", "tier", "mind", "littleguy", "scripted"] {
            assert!(!look.contains(bad), "{bad} in {look}");
        }
    }
    let who = |b: &Built| b.mud.game.execute(&ada, "who").narrative;
    assert_eq!(who(&with_rhai), who(&without));
    let said = with_rhai
        .mud
        .game
        .events(&ada, mud_world::Seq(0), 50)
        .to_string();
    assert!(said.contains("evening"), "ada heard Hale: {said}");
}
