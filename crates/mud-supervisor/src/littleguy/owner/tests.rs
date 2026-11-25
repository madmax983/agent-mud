//! The owner client against a scripted socket.

use std::sync::{Arc, Mutex};

use tokio::net::UnixListener;

use super::*;

/// A daemon that answers each connection's line with the next scripted
/// answer, after its delay, and records every line it read.
fn scripted(
    path: &Path,
    answers: Vec<(Duration, &'static str)>,
) -> (Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&lines);
    let task = tokio::spawn(async move {
        for (delay, answer) in answers {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            seen.lock().unwrap().push(line.trim_end().to_owned());
            tokio::time::sleep(delay).await;
            let mut stream = reader.into_inner();
            let _ = stream.write_all(answer.as_bytes()).await;
            let _ = stream.write_all(b"\n").await;
        }
    });
    (lines, task)
}

const SOON: Duration = Duration::from_secs(5);

/// Each request, the exact line it sends, and an answer lg gives.
fn protocol_cases() -> Vec<(OwnerRequest, &'static str, &'static str)> {
    vec![
        (
            OwnerRequest::Approvals,
            r#"{"kind":"approvals"}"#,
            r#"{"kind":"off","message":"no call waits for approval in this daemon"}"#,
        ),
        (
            OwnerRequest::Plans,
            r#"{"kind":"plans"}"#,
            r#"{"kind":"plans","goals":[{"id":1,"status":"proposed","title":"Sell ore","why":"coin","check_every_minutes":60,"next_check_ms":null,"idea":2,"notes":[{"at_ms":5,"text":"asked"}]}],"ideas":[{"id":2,"title":"Ore","body":"b","created_ms":3,"notes":[]}],"answered":[]}"#,
        ),
        (
            OwnerRequest::Decide {
                decision: PlanDecision::Accept,
                id: 1,
            },
            r#"{"kind":"decide","decision":"accept","id":1}"#,
            r#"{"kind":"plans","goals":[],"ideas":[]}"#,
        ),
        (
            OwnerRequest::Decide {
                decision: PlanDecision::DropGoal,
                id: 4,
            },
            r#"{"kind":"decide","decision":"drop_goal","id":4}"#,
            r#"{"kind":"error","message":"4 is not an active goal"}"#,
        ),
        (
            OwnerRequest::Notes,
            r#"{"kind":"notes"}"#,
            r#"{"kind":"notes","notes":[{"name":"ore","text":"Ada sells","updated_ms":9}]}"#,
        ),
        (
            OwnerRequest::Overlays,
            r#"{"kind":"overlays"}"#,
            r#"{"kind":"overlays","active":1,"entries":[{"version":1,"author":"operator:mud","created_at":"2026-11-25T00:00:00Z","active":true,"parts":["guardrails"]}]}"#,
        ),
        (
            OwnerRequest::OverlayShow { version: 1 },
            r#"{"kind":"overlay_show","version":1}"#,
            r#"{"kind":"overlay","entry":{"version":1,"author":"operator:mud","created_at":"t","active":true,"parts":[]},"overlay":"{}"}"#,
        ),
        (
            OwnerRequest::RefinePropose,
            r#"{"kind":"refine_propose"}"#,
            r#"{"kind":"proposed","version":3,"overlay":"{\"guardrails\":\"- y\"}"}"#,
        ),
        (
            OwnerRequest::OverlayPut {
                overlay: r#"{"guardrails":"- x"}"#.into(),
            },
            r#"{"kind":"overlay_put","overlay":"{\"guardrails\":\"- x\"}"}"#,
            r#"{"kind":"overlay_stored","version":2}"#,
        ),
        (
            OwnerRequest::OverlayActivate { version: 2 },
            r#"{"kind":"overlay_activate","version":2}"#,
            r#"{"kind":"overlay_active","version":2,"previous":null}"#,
        ),
        (
            OwnerRequest::Reload,
            r#"{"kind":"reload"}"#,
            r#"{"kind":"reloaded","applied":["memory","heartbeat"],"restart":[]}"#,
        ),
        (
            OwnerRequest::UiLink,
            r#"{"kind":"ui_link"}"#,
            r#"{"kind":"connect_link","link":"http://127.0.0.1:4123/login?token=t"}"#,
        ),
    ]
}

#[tokio::test]
async fn owner_requests_speak_the_socket_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let cases = protocol_cases();
    let (lines, task) = scripted(
        &sock,
        cases.iter().map(|(_, _, a)| (Duration::ZERO, *a)).collect(),
    );
    let mut answers = Vec::new();
    for (req, _, _) in &cases {
        answers.push(request(&sock, req, SOON).await.unwrap());
    }
    task.await.unwrap();
    let sent = lines.lock().unwrap().clone();
    let want: Vec<&str> = cases.iter().map(|(_, l, _)| *l).collect();
    assert_eq!(sent, want, "the exact wire lines");
    for ((req, line, _), answer) in cases.iter().zip(&answers) {
        assert!(line.contains(req.kind()), "{line}");
        assert!(!matches!(answer, OwnerResponse::Other), "{answer:?}");
    }
    assert!(matches!(&answers[0], OwnerResponse::Off { .. }));
    let OwnerResponse::Plans { goals, ideas, .. } = &answers[1] else {
        panic!("{:?}", answers[1]);
    };
    assert_eq!(goals[0].title, "Sell ore");
    assert_eq!(goals[0].notes[0].text, "asked");
    assert_eq!(ideas[0].id, 2);
    assert!(
        matches!(&answers[3], OwnerResponse::Error { message } if message.contains("not an active goal"))
    );
    assert!(matches!(&answers[4], OwnerResponse::Notes { notes } if notes[0].name == "ore"));
    assert!(
        matches!(&answers[5], OwnerResponse::Overlays { active: Some(1), entries } if entries[0].parts == ["guardrails"])
    );
    assert!(matches!(
        &answers[6],
        OwnerResponse::Overlay { entry, .. } if entry.version == 1
    ));
    assert!(matches!(
        &answers[7],
        OwnerResponse::Proposed { version: 3, .. }
    ));
    assert_eq!(answers[8], OwnerResponse::OverlayStored { version: 2 });
    assert_eq!(
        answers[9],
        OwnerResponse::OverlayActive {
            version: 2,
            previous: None
        }
    );
    assert_eq!(
        answers[10],
        OwnerResponse::Reloaded {
            applied: vec!["memory".into(), "heartbeat".into()],
            restart: vec![],
            profile: vec![],
        }
    );
    assert!(matches!(&answers[11], OwnerResponse::ConnectLink { .. }));
}

#[tokio::test]
async fn kept_and_approvals_parse() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let (_, task) = scripted(
        &sock,
        vec![
            (Duration::ZERO, r#"{"kind":"delta","text":"pre"}"#),
            (Duration::ZERO, r#"{"kind":"kept"}"#),
            (
                Duration::ZERO,
                r#"{"kind":"approvals","entries":[{"id":3,"tool":"mud__mud_command","text":"say hi"}]}"#,
            ),
        ],
    );
    // A preview line alone is no answer: the daemon then closed.
    let e = request(&sock, &OwnerRequest::RefinePropose, SOON)
        .await
        .unwrap_err();
    assert!(matches!(e, OwnerError::Io(_)), "{e}");
    assert_eq!(
        request(&sock, &OwnerRequest::RefinePropose, SOON)
            .await
            .unwrap(),
        OwnerResponse::Kept
    );
    let r = request(&sock, &OwnerRequest::Approvals, SOON)
        .await
        .unwrap();
    assert!(matches!(r, OwnerResponse::Approvals { entries } if entries[0].id == 3));
    task.await.unwrap();
}

#[tokio::test]
async fn an_unknown_answer_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let (_, task) = scripted(
        &sock,
        vec![
            (
                Duration::ZERO,
                r#"{"kind":"patches","repo":"x","patches":[]}"#,
            ),
            (Duration::ZERO, "not json"),
            (Duration::ZERO, r#"{"kind":"plans","goals":"many"}"#),
            (Duration::ZERO, r"{}"),
        ],
    );
    for _ in 0..4 {
        let e = request(&sock, &OwnerRequest::Plans, SOON)
            .await
            .unwrap_err();
        assert!(matches!(e, OwnerError::Malformed(_)), "{e}");
    }
    task.await.unwrap();
    let e = request(&dir.path().join("none.sock"), &OwnerRequest::Plans, SOON)
        .await
        .unwrap_err();
    assert!(matches!(e, OwnerError::Io(_)), "{e}");
}

#[tokio::test]
async fn a_silent_daemon_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let (lines, task) = scripted(
        &sock,
        vec![(
            Duration::from_secs(30),
            r#"{"kind":"plans","goals":[],"ideas":[]}"#,
        )],
    );
    let e = request(&sock, &OwnerRequest::Plans, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert_eq!(e, OwnerError::Timeout);
    assert_eq!(lines.lock().unwrap().len(), 1, "the line was sent");
    task.abort();
}

#[tokio::test]
async fn a_slow_loop_job_answers_on_its_connection() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let (_, task) = scripted(
        &sock,
        vec![(
            Duration::from_secs(2),
            r#"{"kind":"overlay_active","version":4,"previous":3}"#,
        )],
    );
    let started = std::time::Instant::now();
    let r = request(
        &sock,
        &OwnerRequest::OverlayActivate { version: 4 },
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(
        r,
        OwnerResponse::OverlayActive {
            version: 4,
            previous: Some(3)
        }
    );
    task.await.unwrap();
}

#[tokio::test]
async fn a_request_longer_than_lg_reads_is_never_sent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let (lines, task) = scripted(&sock, vec![(Duration::ZERO, r#"{"kind":"kept"}"#)]);
    let big = OwnerRequest::OverlayPut {
        overlay: "x".repeat(LG_MAX_LINE_BYTES),
    };
    let e = request(&sock, &big, SOON).await.unwrap_err();
    assert!(
        matches!(e, OwnerError::Malformed(ref m) if m.contains("overlay_put")),
        "{e}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(lines.lock().unwrap().is_empty(), "no byte was sent");
    task.abort();
}

/// Every request the client can build, by an exhaustive match: a new
/// variant does not compile until it is listed here.
fn every_request() -> Vec<OwnerRequest> {
    let all = [
        OwnerRequest::Approvals,
        OwnerRequest::Plans,
        OwnerRequest::Decide {
            decision: PlanDecision::Accept,
            id: 1,
        },
        OwnerRequest::Decide {
            decision: PlanDecision::Decline,
            id: 1,
        },
        OwnerRequest::Decide {
            decision: PlanDecision::DropGoal,
            id: 1,
        },
        OwnerRequest::Notes,
        OwnerRequest::Overlays,
        OwnerRequest::OverlayShow { version: 1 },
        OwnerRequest::RefinePropose,
        OwnerRequest::OverlayPut {
            overlay: "{}".into(),
        },
        OwnerRequest::OverlayActivate { version: 1 },
        OwnerRequest::Reload,
        OwnerRequest::UiLink,
    ];
    for r in &all {
        match r {
            OwnerRequest::Approvals
            | OwnerRequest::Plans
            | OwnerRequest::Decide { .. }
            | OwnerRequest::Notes
            | OwnerRequest::Overlays
            | OwnerRequest::OverlayShow { .. }
            | OwnerRequest::RefinePropose
            | OwnerRequest::OverlayPut { .. }
            | OwnerRequest::OverlayActivate { .. }
            | OwnerRequest::Reload
            | OwnerRequest::UiLink => {}
        }
    }
    all.to_vec()
}

#[test]
fn the_owner_client_can_neither_approve_deny_nor_roll_back() {
    for r in every_request() {
        let v = serde_json::to_value(&r).unwrap();
        let kind = v["kind"].as_str().unwrap();
        assert_eq!(kind, r.kind());
        for refused in ["approve", "deny", "overlay_rollback"] {
            assert_ne!(kind, refused, "{v}");
        }
        if let Some(d) = v.get("decision").and_then(Value::as_str) {
            assert!(
                ["accept", "decline", "drop_goal"].contains(&d),
                "never drop or reopen an idea: {d}"
            );
        }
    }
}
