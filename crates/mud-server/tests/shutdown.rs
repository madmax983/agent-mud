//! `mud serve` stops promptly on Ctrl+C, and takes every littleguy daemon
//! with it.
//!
//! Regression: each daemon held an MCP session open (an SSE response), the
//! graceful shutdown waited for every open response, and the daemons were
//! only stopped after the server had drained, so Ctrl+C left the process
//! running for minutes. Runs when `LG_BIN` names an `lg` binary.

use std::path::Path;
use std::time::Duration;

/// The observer token the server is given.
const TOKEN: &str = "shutdown-test-observer-token";

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_stops_the_server_and_every_daemon() {
    let Ok(bin) = std::env::var("LG_BIN") else {
        eprintln!("LG_BIN is not set: skipping the shutdown check");
        return;
    };
    if !Path::new("/proc").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let agents = dir.path().join("agents");
    let config = write_config(dir.path(), port, &bin);
    let mut mud = tokio::process::Command::new(env!("CARGO_BIN_EXE_mud"))
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .env("MUD_SHUTDOWN_TEST_TOKEN", TOKEN)
        .stdout(std::fs::File::create(dir.path().join("mud.log")).unwrap())
        .stderr(std::fs::File::create(dir.path().join("mud.err")).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let log = || std::fs::read_to_string(dir.path().join("mud.log")).unwrap_or_default();

    // Wake each daemon once through the observer's macro-tick button, so
    // all three are running and connected to /mcp.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let pid_files: Vec<_> = ["marta", "torin", "vich"]
        .iter()
        .map(|e| agents.join(e).join("lg.pid"))
        .collect();
    let token = TOKEN;
    while !pid_files.iter().all(|p| p.exists()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemons never started:\n{}",
            log()
        );
        for who in ["marta", "torin", "vich"] {
            let _ = http
                .post(format!(
                    "http://127.0.0.1:{port}/observer/macro-tick/{who}?token={token}"
                ))
                .send()
                .await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let pids: Vec<u32> = pid_files
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap().trim().parse().unwrap())
        .collect();
    // Let each daemon open its MCP session.
    tokio::time::sleep(Duration::from_secs(2)).await;
    // `serve` runs the Steward: a pass reads each running daemon, and the
    // observer shows what it read.
    wait_for_a_steward_pass(&http, port, &dir.path().join("mud.log")).await;

    let interrupted = tokio::process::Command::new("kill")
        .arg("-INT")
        .arg(mud.id().unwrap().to_string())
        .status()
        .await
        .unwrap();
    assert!(interrupted.success());
    let exited = tokio::time::timeout(Duration::from_secs(10), mud.wait()).await;
    assert!(
        exited.is_ok(),
        "mud serve still running 10 s after Ctrl+C:\n{}",
        log()
    );
    for pid in pids {
        let gone = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            !Path::new(&format!("/proc/{pid}")).exists()
                || std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|s| s.split_whitespace().nth(2) == Some("Z"))
        });
        assert!(gone, "lg serve {pid} outlived the MUD:\n{}", log());
    }
}

/// Writes the `mud.toml` of the check: littleguy minds on `bin`, offline,
/// and a Steward pass every second.
#[expect(clippy::unwrap_used, reason = "a test helper fails loudly")]
fn write_config(dir: &Path, port: u16, bin: &str) -> std::path::PathBuf {
    let agents = dir.join("agents");
    let config = dir.join("mud.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[server]
bind = "127.0.0.1:{port}"
admin_token_env = "MUD_SHUTDOWN_TEST_TOKEN"
[world]
db = "{db}"
[minds]
persistent = "littleguy"
[littleguy]
bin = "{bin}"
data_dir = "{agents}"
provider = "offline"
[littleguy.steward]
every_secs = 1
[supervisor]
typing_scale = 0.0
"#,
            db = dir.join("world.db").display(),
            agents = agents.display(),
        ),
    )
    .unwrap();
    config
}

/// Waits until the observer shows what a Steward pass read of each of the
/// three daemons.
#[expect(clippy::unwrap_used, reason = "a test helper fails loudly")]
async fn wait_for_a_steward_pass(http: &reqwest::Client, port: u16, log: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let page = http
            .get(format!(
                "http://127.0.0.1:{port}/observer/entities?token={TOKEN}"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if page.matches(" ideas").count() == 3 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no Steward pass read the daemons:\n{page}\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The daemons of the Steward check: torin's fake `lg serve`, whose
/// listener counts the lines it reads.
#[derive(Debug)]
struct Counted {
    socket: std::path::PathBuf,
}

impl mud_supervisor::steward::OwnedDaemons for Counted {
    fn entities(&self) -> Vec<mud_world::EntityId> {
        vec![mud_world::EntityId::from("torin")]
    }

    fn running_socket<'a>(
        &'a self,
        _: &'a mud_world::EntityId,
    ) -> mud_supervisor::mind::BoxFuture<'a, Option<std::path::PathBuf>> {
        Box::pin(std::future::ready(Some(self.socket.clone())))
    }

    fn start_key<'a>(
        &'a self,
        _: &'a mud_world::EntityId,
    ) -> mud_supervisor::mind::BoxFuture<'a, Option<u32>> {
        Box::pin(std::future::ready(Some(1)))
    }

    fn brief(&self, _: &mud_world::EntityId) -> Option<mud_supervisor::littleguy::PersonaText> {
        None
    }

    fn settle<'a>(
        &'a self,
        _: &'a mud_world::EntityId,
        _: u64,
    ) -> mud_supervisor::mind::BoxFuture<'a, mud_supervisor::mind::Usage> {
        Box::pin(std::future::ready(mud_supervisor::mind::Usage::default()))
    }

    fn set_quiet<'a>(
        &'a self,
        _: &'a mud_world::EntityId,
        _: bool,
    ) -> mud_supervisor::mind::BoxFuture<'a, Result<(), mud_supervisor::mind::MindError>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn is_quiet(&self, _: &mud_world::EntityId) -> bool {
        false
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_steward_stops_with_the_supervisor() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("lg.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let lines = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&lines);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let counted = Arc::clone(&counted);
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let answer = if line.contains("\"plans\"") {
                    r#"{"kind":"plans","goals":[],"ideas":[]}"#
                } else if line.contains("\"notes\"") {
                    r#"{"kind":"notes","notes":[]}"#
                } else {
                    r#"{"kind":"off","message":"off"}"#
                };
                let mut stream = reader.into_inner();
                let _ = stream.write_all(format!("{answer}\n").as_bytes()).await;
            });
        }
    });
    let pack = Arc::new(mud_world::WorldPack::crypt_v01().unwrap());
    let game = mud_world::Game::new(
        mud_world::World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(mud_world::ManualClock::new(1_000_000)),
    );
    let sup = mud_supervisor::runtime::Supervisor::new(
        Arc::clone(&game),
        mud_supervisor::gate::Gate::new(Arc::clone(&game), 1, 0.0),
        std::collections::HashMap::new(),
        Arc::new(mud_supervisor::scripted::FallbackMind::from_cast(
            &pack.cast,
        )),
        mud_supervisor::runtime::SupervisorConfig::default(),
    );
    let steward = mud_supervisor::steward::Steward::new(
        Arc::clone(&sup),
        vec![Arc::new(Counted { socket })],
        None,
        mud_supervisor::steward::StewardConfig {
            every_secs: 1,
            ..Default::default()
        },
        std::collections::BTreeMap::new(),
        game.boot(),
    );
    let mut tasks = sup.spawn(Duration::from_millis(50));
    let steward_tasks = steward.spawn();
    let handles: Vec<_> = steward_tasks
        .iter()
        .map(tokio::task::JoinHandle::abort_handle)
        .collect();
    tasks.extend(steward_tasks);
    // The first pass runs at once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while lines.load(Ordering::SeqCst) < 3 {
        assert!(tokio::time::Instant::now() < deadline, "no pass ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stopper = tokio::spawn(mud_server::stop_on(
        shutdown.clone(),
        tasks
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect(),
        Vec::new(),
    ));
    shutdown.cancel();
    stopper.await.unwrap();
    for t in tasks {
        let _ = t.await;
    }
    assert!(
        handles.iter().all(tokio::task::AbortHandle::is_finished),
        "the Steward's tasks ended"
    );
    let at_stop = lines.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(
        lines.load(Ordering::SeqCst),
        at_stop,
        "no line after the stop"
    );
    assert!(Arc::strong_count(&steward) >= 1);
}
