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
    let config = dir.path().join("mud.toml");
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
[supervisor]
typing_scale = 0.0
"#,
            db = dir.path().join("world.db").display(),
            agents = agents.display(),
        ),
    )
    .unwrap();
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
