//! A real littleguy daemon plays through the MUD's MCP endpoint.
//!
//! Runs when `LG_BIN` names an `lg` binary (CI builds one). The model is
//! littleguy's offline echo, so no key is needed: the check is that the
//! daemon accepts its generated config, reaches `/mcp` with its scoped
//! bearer token, lists the four MUD tools, and answers the wake.

use std::sync::Arc;
use std::time::Duration;

use autumn_web::test::TestApp;
use mud_server::config::{MudConfig, Runtime};
use mud_server::{TypingGate, build, settings, web};
use mud_supervisor::runtime::ObserverEvent;
use mud_world::EntityId;

#[tokio::test(flavor = "multi_thread")]
async fn a_real_lg_serve_connects_to_mcp_and_answers_a_macro_tick() {
    let Ok(bin) = std::env::var("LG_BIN") else {
        eprintln!("LG_BIN is not set: skipping the littleguy end-to-end check");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut cfg = MudConfig::default();
    cfg.server.bind = addr;
    cfg.world.db = None;
    cfg.minds.persistent = Runtime::Littleguy;
    cfg.littleguy.bin = bin.into();
    cfg.littleguy.data_dir = dir.path().to_path_buf();
    cfg.supervisor.first_macro_tick_secs = [3_600, 3_600];
    let built = Arc::new(build(&cfg).unwrap());
    assert_eq!(built.tokens.len(), 3);
    let mud = Arc::clone(&built.mud);
    let mcp = mud_mcp::router(
        Arc::clone(&built.mud.game),
        Arc::clone(&built.tokens),
        Arc::new(TypingGate(Arc::clone(built.mud.sup.gate()))),
    );
    let router = TestApp::new()
        .config(settings(&cfg).unwrap())
        .routes(web::routes())
        .state_initializer(move |s| s.insert_extension((*mud).clone()))
        .build()
        .into_router()
        .merge(mcp);
    tokio::spawn(async move {
        autumn_web::reexports::axum::serve(listener, router)
            .await
            .ok()
    });
    let mut obs = built.mud.sup.subscribe();
    let tasks = built.mud.sup.spawn(Duration::from_millis(50));
    built.mud.sup.fire_macro_tick(&EntityId::from("torin"));
    let finished = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let ObserverEvent::WakeFinished {
                entity, ok, note, ..
            } = &*obs.recv().await.unwrap()
                && entity.as_str() == "torin"
            {
                return (*ok, note.clone());
            }
        }
    })
    .await
    .expect("torin's wake did not finish");
    let log = std::fs::read_to_string(dir.path().join("torin/lg.log")).unwrap_or_default();
    assert!(finished.0, "the wake failed: {:?}\n{log}", finished.1);
    assert!(
        log.contains("mud__mud_command"),
        "littleguy did not list the MUD tools:\n{log}"
    );
    assert!(
        !log.contains("gives no tools"),
        "the MCP connection failed:\n{log}"
    );
    for t in tasks {
        t.abort();
    }
    for lg in &built.littleguy {
        lg.shutdown().await;
    }
}
