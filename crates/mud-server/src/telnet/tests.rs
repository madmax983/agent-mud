use tokio::io::AsyncReadExt;

use super::*;
use crate::session::TOO_LONG;
use crate::test_support::{PW, accounts, game};

const WILL_ECHO: [u8; 3] = [IAC, WILL, ECHO];
const WONT_ECHO: [u8; 3] = [IAC, WONT, ECHO];

async fn shared_with(timeouts: Timeouts) -> Shared {
    Shared {
        game: game(),
        presence: Arc::new(Presence::default()),
        accounts: accounts().await,
        timeouts,
        sessions: tokio_util::task::TaskTracker::new(),
        shutdown: tokio_util::sync::CancellationToken::new(),
    }
}

async fn start(
    limiter: Arc<ConnLimiter>,
    shared: Shared,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, tokio::spawn(serve_limited(limiter, listener, shared)))
}

/// Sends `input` and returns every byte the server wrote before closing.
async fn talk(addr: SocketAddr, input: &[u8]) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(input).await.unwrap();
    // The end of input: a server waiting for the next line sees EOF.
    c.shutdown().await.unwrap();
    let mut out = Vec::new();
    c.read_to_end(&mut out).await.unwrap();
    out
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

/// The lines that make `name` a new character.
fn create(name: &str) -> String {
    format!("{name}\r\n{PW}\r\n{PW}\r\n")
}

/// Sends `input` to a fresh server after making ada, and returns
/// everything it wrote back before closing.
async fn exchange(input: &[u8]) -> String {
    text(&exchange_bytes(input).await)
}

/// [`exchange`], as the raw bytes (telnet commands included).
async fn exchange_bytes(input: &[u8]) -> Vec<u8> {
    let (addr, task) = start(
        ConnLimiter::new(0, 0),
        shared_with(Timeouts::default()).await,
    )
    .await;
    let mut all = create("ada").into_bytes();
    all.extend_from_slice(input);
    let out = talk(addr, &all).await;
    task.abort();
    out
}

fn has(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[tokio::test]
async fn a_full_server_turns_new_clients_away() {
    let (addr, task) = start(
        ConnLimiter::new(1, 0),
        shared_with(Timeouts::default()).await,
    )
    .await;
    let mut first = TcpStream::connect(addr).await.unwrap();
    let mut greeting = [0u8; 16];
    first.read_exact(&mut greeting).await.unwrap();
    let out = text(&talk(addr, b"").await);
    assert!(out.contains("The server is full"), "{out}");
    task.abort();
}

#[tokio::test]
async fn a_telnet_player_makes_an_account_and_comes_back() {
    let (addr, task) = start(
        ConnLimiter::new(0, 0),
        shared_with(Timeouts::default()).await,
    )
    .await;
    let first = text(
        &talk(
            addr,
            format!("{}say hello\r\nquit\r\n", create("ada")).as_bytes(),
        )
        .await,
    );
    assert!(
        first.contains("Welcome to Harrow's Rest.\r\nWhat is your name?\r\n"),
        "{first}"
    );
    assert!(first.contains("Nobody here is called ada. Choose a password to make a new character (8 or more characters):"), "{first}");
    assert!(first.contains("Type it again:"), "{first}");
    assert!(
        first.contains(
            "Welcome, ada. Your password is the only way back to this character; keep it."
        ),
        "{first}"
    );
    assert!(first.contains("Town Square"), "{first}");
    assert!(first.contains("You say, \"hello\""), "{first}");
    assert!(first.contains("Farewell."), "{first}");
    assert!(!first.contains(PW), "the password is never written back");
    let wrong = text(&talk(addr, b"ada\r\nnot-my-password\r\n\r\n").await);
    assert!(wrong.contains("Wrong password."), "{wrong}");
    let back = text(&talk(addr, format!("ada\r\n{PW}\r\nquit\r\n").as_bytes()).await);
    assert!(back.contains("Password:"), "{back}");
    assert!(back.contains("Town Square"), "{back}");
    let kestrel = text(&talk(addr, format!("kestrel\r\n{PW}\r\n\r\n").as_bytes()).await);
    assert!(
        kestrel.contains("Password:") && kestrel.contains("Wrong password."),
        "{kestrel}"
    );
    task.abort();
}

#[tokio::test]
async fn password_prompts_turn_echo_off_and_on() {
    let out = exchange_bytes(b"quit\r\n").await;
    let prompt = has(&out, b"Choose a password").unwrap();
    let off = has(&out, &WILL_ECHO).unwrap();
    let on = has(&out, &WONT_ECHO).unwrap();
    assert!(prompt < off && off < on, "{}", text(&out));
    assert!(has(&out, b"What is your name?\r\n").unwrap() < off);
    // Two secret prompts, two pairs; after the last answer, echo is on.
    assert_eq!(out.windows(3).filter(|w| *w == WILL_ECHO).count(), 2);
    assert_eq!(out.windows(3).filter(|w| *w == WONT_ECHO).count(), 2);
}

#[tokio::test]
async fn a_pipelined_name_and_password_log_in() {
    let (addr, task) = start(
        ConnLimiter::new(0, 0),
        shared_with(Timeouts::default()).await,
    )
    .await;
    talk(addr, format!("{}quit\r\n", create("ada")).as_bytes()).await;
    let out = text(&talk(addr, format!("ada\r\n{PW}\r\nlook\r\nquit\r\n").as_bytes()).await);
    assert_eq!(
        out.matches("Town Square").count(),
        2,
        "greeting and look: {out}"
    );
    task.abort();
}

#[tokio::test]
async fn a_password_keeps_its_inner_spaces() {
    let (addr, task) = start(
        ConnLimiter::new(0, 0),
        shared_with(Timeouts::default()).await,
    )
    .await;
    let pw = "correct horse battery";
    talk(addr, format!("ada\r\n{pw}\r\n{pw}\r\nquit\r\n").as_bytes()).await;
    let squeezed = text(&talk(addr, b"ada\r\ncorrecthorsebattery\r\n\r\n").await);
    assert!(squeezed.contains("Wrong password."), "{squeezed}");
    let ok = text(&talk(addr, format!("ada\r\n{pw}\r\nquit\r\n").as_bytes()).await);
    assert!(ok.contains("Town Square"), "{ok}");
    task.abort();
}

#[tokio::test]
async fn a_cr_nul_line_ends_a_password() {
    let out = exchange(b"").await;
    assert!(out.contains("Town Square"));
    let (addr, task) = start(
        ConnLimiter::new(0, 0),
        shared_with(Timeouts::default()).await,
    )
    .await;
    let out = text(
        &talk(
            addr,
            format!("bo\r\0{PW}\r\0{PW}\r\0say hi\r\0quit\r\0").as_bytes(),
        )
        .await,
    );
    assert!(out.contains("Welcome, bo."), "{out}");
    assert!(out.contains("You say, \"hi\""), "{out}");
    task.abort();
}

#[tokio::test]
async fn a_bare_cr_ends_a_line_and_crlf_still_works() {
    // One byte per read, so the LF after a CR arrives in a later fill.
    let (mut tx, rx) = tokio::io::duplex(1);
    let writer = tokio::spawn(async move {
        tx.write_all(b"one\rtwo\r\nthree\nfour\r\0five\r")
            .await
            .unwrap();
    });
    let mut r = LineReader::new(rx);
    let line = |s: &str| Some(Input::Line(s.to_owned()));
    for want in ["one", "two", "three", "four", "five"] {
        assert_eq!(r.next_line().await.unwrap(), line(want));
    }
    writer.await.unwrap();
    assert!(r.next_line().await.unwrap().is_none());
    let mut r = LineReader::new(&b"\r\n\r\nx"[..]);
    assert_eq!(r.next_line().await.unwrap(), line(""));
    assert_eq!(r.next_line().await.unwrap(), line(""));
    assert_eq!(r.next_line().await.unwrap(), line("x"));
}

#[tokio::test]
async fn an_overlong_password_turns_echo_back_on() {
    let (addr, task) = start(
        ConnLimiter::new(0, 0),
        shared_with(Timeouts::default()).await,
    )
    .await;
    let mut input = b"kestrel\r\n".to_vec();
    input.resize(input.len() + MAX_LINE + 5, b'p');
    input.extend_from_slice(b"\r\n");
    let out = talk(addr, &input).await;
    let off = has(&out, &WILL_ECHO).unwrap();
    let on = has(&out, &WONT_ECHO).unwrap();
    let refused = has(&out, b"That line is too long; nothing was done.").unwrap();
    assert!(off < on && on < refused, "{}", text(&out));
    assert!(
        text(&out).ends_with("What is your name?\r\n"),
        "{}",
        text(&out)
    );
    task.abort();
}

#[tokio::test]
async fn a_slow_drip_login_is_cut_at_the_total_deadline() {
    let ms = Some(std::time::Duration::from_millis(300));
    let (addr, task) = start(
        ConnLimiter::new(0, 0),
        shared_with(Timeouts {
            login: ms,
            idle: None,
        })
        .await,
    )
    .await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    for chunk in [&b"ad"[..], b"a\r\n", b"lan", b"tern", b"-and"] {
        // Once the deadline passes the server is gone; later writes may fail.
        let _ = c.write_all(chunk).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let mut out = Vec::new();
    c.read_to_end(&mut out).await.unwrap();
    let out = text(&out);
    assert!(out.contains("Too slow. Farewell."), "{out}");
    assert!(!out.contains("Town Square"), "{out}");
    task.abort();
}

#[tokio::test]
async fn telnet_negotiation_and_stray_bytes_do_not_drop_the_client() {
    // IAC WILL NAWS, IAC WILL TTYPE (as PuTTY sends on connect), a NAWS
    // subnegotiation whose height is 10 (a raw '\n' byte), then the name;
    // later IAC IP (Ctrl-C) and a Latin-1 byte mid-line.
    let mut input = vec![0xFF, 0xFB, 0x1F, 0xFF, 0xFB, 0x18];
    input.extend_from_slice(&[0xFF, 0xFA, 0x1F, 0, 80, 0, 10, 0xFF, 0xF0]);
    input.extend_from_slice(create("bo").as_bytes());
    input.extend_from_slice(&[0xFF, 0xF4]);
    input.extend_from_slice(b"say caf\xE9\r\nsay still here\r\nquit\r\n");
    let (addr, task) = start(
        ConnLimiter::new(0, 0),
        shared_with(Timeouts::default()).await,
    )
    .await;
    let out = text(&talk(addr, &input).await);
    assert!(out.contains("Town Square"), "{out}");
    assert!(out.contains("You say, \"caf\u{FFFD}\""), "{out}");
    assert!(out.contains("You say, \"still here\""), "{out}");
    assert!(out.contains("Farewell."), "{out}");
    task.abort();
}

#[tokio::test]
async fn an_endless_line_is_refused_and_the_session_goes_on() {
    // More than the whole old 16 MiB stream budget, in one line.
    let mut input = b"say ".to_vec();
    input.resize(input.len() + (17 << 20), b'a');
    input.extend_from_slice(b"\r\nsay still here\r\nquit\r\n");
    let out = exchange(&input).await;
    // The long line is refused once and never runs, cut or whole.
    assert_eq!(out.matches(TOO_LONG).count(), 1, "{out}");
    assert!(!out.contains("You say, \"aaa"), "{out}");
    assert!(out.contains("You say, \"still here\""), "{out}");
    assert!(out.contains("Farewell."), "{out}");
}

#[tokio::test]
async fn an_overlong_line_during_a_password_change_never_leaves_echo_on() {
    // `password`, then a line over the limit at `Current password:`, then
    // what the player types next with echo back on.
    let mut input = b"password\r\n".to_vec();
    input.resize(input.len() + MAX_LINE + 5, b'x');
    input.extend_from_slice(b"\r\nsay my-current-secret\r\nquit\r\n");
    let out = exchange_bytes(&input).await;
    let refused = has(&out, TOO_LONG.as_bytes()).unwrap();
    let tail = &out[refused..];
    // Echo is on after the refusal, so no secret prompt is open: the
    // change is cancelled and the next line is an ordinary command.
    assert!(has(tail, &WILL_ECHO).is_none(), "{}", text(tail));
    assert!(
        has(tail, b"Password change cancelled.").is_some(),
        "{}",
        text(tail)
    );
    assert!(has(tail, b"New password").is_none(), "{}", text(tail));
    assert!(
        has(tail, b"You say, \"my-current-secret\"").is_some(),
        "{}",
        text(tail)
    );
}

#[tokio::test]
async fn the_line_reader_strips_telnet_and_bounds_lines() {
    let mut input = vec![b'h', 0xFF, 0xFF, b'i', b'\r', b'\n'];
    input.extend_from_slice(&[0xFF, 0xFA, 0x18, b'\n', 0xFF, 0xFF, 0xFF, 0xF0]);
    input.extend_from_slice(&[0xFF, 0xF1, b'x']);
    input.resize(input.len() + 3 * MAX_LINE_BYTES, b'y');
    input.extend_from_slice(b"\nlast");
    let mut r = LineReader::new(&input[..]);
    let line = |s: &str| Some(Input::Line(s.to_owned()));
    assert_eq!(r.next_line().await.unwrap(), line("h\u{FFFD}i"));
    assert_eq!(r.next_line().await.unwrap(), Some(Input::TooLong));
    assert_eq!(r.next_line().await.unwrap(), line("last"));
    assert!(r.next_line().await.unwrap().is_none());
    let mut r = LineReader::new(&[0xFF, 0xFB, 0x01][..]);
    assert!(r.next_line().await.unwrap().is_none());
}

#[tokio::test]
async fn the_line_limit_counts_characters_not_bytes() {
    // 1,024 three-byte characters: 3 KiB, but within the limit.
    let wide = "\u{4E2D}".repeat(MAX_LINE);
    let input = format!("{wide}\n{wide}\u{4E2D}\nok\n");
    let mut r = LineReader::new(input.as_bytes());
    assert_eq!(r.next_line().await.unwrap(), Some(Input::Line(wide)));
    assert_eq!(r.next_line().await.unwrap(), Some(Input::TooLong));
    assert_eq!(r.next_line().await.unwrap(), Some(Input::Line("ok".into())));
}

#[tokio::test]
async fn a_wide_say_under_the_parser_limit_arrives_whole() {
    let said = "\u{4E2D}".repeat(400);
    let out = exchange(format!("say {said}\r\nquit\r\n").as_bytes()).await;
    assert!(out.contains(&format!("You say, \"{said}\"")), "{out}");
}

#[tokio::test]
async fn a_silent_client_is_closed_and_its_slot_freed() {
    let ms = Some(std::time::Duration::from_millis(150));
    let (addr, task) = start(
        ConnLimiter::new(1, 0),
        shared_with(Timeouts {
            login: ms,
            idle: None,
        })
        .await,
    )
    .await;
    // Silent, but still connected.
    let mut silent = TcpStream::connect(addr).await.unwrap();
    let mut out = Vec::new();
    silent.read_to_end(&mut out).await.unwrap();
    let out = text(&out);
    assert!(out.contains("Too slow"), "{out}");
    let out = text(&talk(addr, format!("{}quit\r\n", create("ada")).as_bytes()).await);
    assert!(out.contains("Town Square"), "{out}");
    task.abort();
}

#[tokio::test]
async fn an_idle_player_is_closed() {
    let ms = Some(std::time::Duration::from_millis(100));
    let (addr, task) = start(
        ConnLimiter::new(1, 0),
        shared_with(Timeouts {
            login: None,
            idle: ms,
        })
        .await,
    )
    .await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(create("ada").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    c.read_to_end(&mut out).await.unwrap();
    let out = text(&out);
    assert!(out.contains("Town Square"), "{out}");
    assert!(out.contains("Idle too long"), "{out}");
    task.abort();
}

#[tokio::test]
async fn one_address_cannot_take_every_slot() {
    let limiter = ConnLimiter::new(10, 1);
    let (addr, task) = start(Arc::clone(&limiter), shared_with(Timeouts::default()).await).await;
    let mut first = TcpStream::connect(addr).await.unwrap();
    let mut greeting = [0u8; 16];
    first.read_exact(&mut greeting).await.unwrap();
    let out = text(&talk(addr, b"").await);
    assert!(
        out.contains("Too many connections from your address"),
        "{out}"
    );
    assert_eq!(limiter.open(), 1);
    drop(first);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while limiter.open() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slot was never freed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    task.abort();
}

#[tokio::test]
async fn a_changed_password_ends_another_telnet_session_on_revalidate() {
    let mut shared = shared_with(Timeouts::default()).await;
    shared.accounts = crate::test_support::accounts_with(|c| c.revalidate_secs = 5).await;
    let (addr, task) = start(ConnLimiter::new(0, 0), shared.clone()).await;
    talk(addr, format!("{}quit\r\n", create("ada")).as_bytes()).await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(format!("ada\r\n{PW}\r\n").as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let key = mud_accounts::NameKey::parse("ada").unwrap();
    shared
        .accounts
        .admin_signout(&key, shared.game.now_ms() + 10)
        .await
        .unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(8), c.read_to_end(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert!(
        text(&out).contains("Your account changed elsewhere. Farewell."),
        "{}",
        text(&out)
    );
    task.abort();
}
