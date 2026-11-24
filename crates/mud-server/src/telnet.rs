//! Plain-text clients: telnet, netcat, MUD clients.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::config::Timeouts;
use crate::session::{Line, Presence, Session};
use mud_world::Game;

/// The longest line a telnet client may send, in characters. A longer
/// line is read to its end, discarded and refused.
const MAX_LINE: usize = 1_024;

/// The bytes kept of one line: room for [`MAX_LINE`] characters of four
/// UTF-8 bytes each.
const MAX_LINE_BYTES: usize = MAX_LINE * 4;

/// What the server says to a line over [`MAX_LINE`].
const TOO_LONG: &str = "That line is too long; nothing was done.";

/// One line from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Input {
    /// A line within the limit.
    Line(String),
    /// A line over [`MAX_LINE`] characters.
    TooLong,
}

/// Telnet "interpret as command".
const IAC: u8 = 0xFF;
/// Subnegotiation begin.
const SB: u8 = 0xFA;
/// Subnegotiation end.
const SE: u8 = 0xF0;
/// The first option verb (`WILL`); `WONT`, `DO` and `DONT` follow it.
const WILL: u8 = 0xFB;
/// The last option verb.
const DONT: u8 = 0xFE;

/// Where the reader is in the telnet byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Telnet {
    /// Plain text.
    Data,
    /// After an `IAC`.
    Iac,
    /// After `IAC WILL|WONT|DO|DONT`, waiting for the option byte.
    Option,
    /// Inside `IAC SB … IAC SE`.
    Sub,
    /// After an `IAC` inside a subnegotiation.
    SubIac,
}

impl Telnet {
    /// Steps over one byte; returns it if it is text.
    const fn feed(&mut self, b: u8) -> Option<u8> {
        let (next, text) = match (*self, b) {
            (Self::Data, IAC) => (Self::Iac, None),
            (Self::Data, _) => (Self::Data, Some(b)),
            (Self::Iac, IAC) => (Self::Data, Some(IAC)),
            (Self::Iac, SB) => (Self::Sub, None),
            (Self::Iac, WILL..=DONT) => (Self::Option, None),
            (Self::Iac | Self::Option, _) | (Self::SubIac, SE) => (Self::Data, None),
            (Self::Sub, IAC) => (Self::SubIac, None),
            (Self::Sub | Self::SubIac, _) => (Self::Sub, None),
        };
        *self = next;
        text
    }
}

/// Reads lines from a telnet client: strips telnet commands, decodes
/// each line as UTF-8 (replacing invalid bytes), and refuses a line over
/// [`MAX_LINE`] characters without holding more than [`MAX_LINE_BYTES`]
/// of it. Cancel-safe: a partial line survives a dropped `next_line`
/// future.
struct LineReader<R> {
    inner: BufReader<R>,
    telnet: Telnet,
    line: Vec<u8>,
    pending: bool,
    overflow: bool,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    fn new(r: R) -> Self {
        Self {
            inner: BufReader::new(r),
            telnet: Telnet::Data,
            line: Vec::new(),
            pending: false,
            overflow: false,
        }
    }

    fn take_line(&mut self) -> Input {
        self.pending = false;
        let overflow = std::mem::take(&mut self.overflow);
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        if overflow || text.chars().count() > MAX_LINE {
            Input::TooLong
        } else {
            Input::Line(text)
        }
    }

    /// The next line, or `None` at the end of the stream.
    async fn next_line(&mut self) -> std::io::Result<Option<Input>> {
        loop {
            let buf = self.inner.fill_buf().await?;
            if buf.is_empty() {
                return Ok(self.pending.then(|| self.take_line()));
            }
            let mut used = 0;
            let mut done = false;
            for &b in buf {
                used += 1;
                match self.telnet.feed(b) {
                    Some(b'\n') => {
                        done = true;
                        break;
                    }
                    Some(c) => {
                        self.pending = true;
                        if self.line.len() < MAX_LINE_BYTES {
                            self.line.push(c);
                        } else {
                            self.overflow = true;
                        }
                    }
                    None => {}
                }
            }
            self.inner.consume(used);
            if done {
                return Ok(Some(self.take_line()));
            }
        }
    }
}

async fn write_lines(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    lines: &[Line],
) -> std::io::Result<()> {
    for l in lines {
        // Control characters from other players never reach the terminal.
        let clean: String = l
            .text
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        w.write_all(clean.replace('\n', "\r\n").as_bytes()).await?;
        w.write_all(b"\r\n").await?;
    }
    Ok(())
}

async fn handle(
    stream: TcpStream,
    game: Arc<Game>,
    presence: Arc<Presence>,
    timeouts: Timeouts,
) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = LineReader::new(r);
    w.write_all(b"Welcome to Harrow's Rest. What is your name?\r\n")
        .await?;
    let session = loop {
        let Some(next) = Timeouts::within(timeouts.login, lines.next_line()).await else {
            w.write_all(b"Too slow. Farewell.\r\n").await?;
            return Ok(());
        };
        let name = match next? {
            None => return Ok(()),
            Some(Input::TooLong) => {
                w.write_all(format!("{TOO_LONG} What is your name?\r\n").as_bytes())
                    .await?;
                continue;
            }
            Some(Input::Line(name)) => name,
        };
        match Session::login(Arc::clone(&game), Arc::clone(&presence), name.trim()) {
            Ok((s, greet)) => {
                write_lines(&mut w, &greet).await?;
                break s;
            }
            Err(e) => {
                w.write_all(format!("{e}. What is your name?\r\n").as_bytes())
                    .await?;
            }
        }
    };
    let mut events = game.subscribe();
    loop {
        tokio::select! {
            line = Timeouts::within(timeouts.idle, lines.next_line()) => {
                let Some(line) = line else {
                    w.write_all(b"Idle too long. Farewell.\r\n").await?;
                    break;
                };
                let line = match line? {
                    None => break,
                    Some(Input::TooLong) => {
                        w.write_all(format!("{TOO_LONG}\r\n").as_bytes()).await?;
                        continue;
                    }
                    Some(Input::Line(line)) => line,
                };
                if line.trim().eq_ignore_ascii_case("quit") {
                    w.write_all(b"Farewell.\r\n").await?;
                    break;
                }
                write_lines(&mut w, &session.command(&line)).await?;
            }
            ev = events.recv() => match ev {
                Ok(ev) => {
                    if let Some(l) = session.render(&ev) {
                        write_lines(&mut w, &[l]).await?;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    drop(session);
    Ok(())
}

/// Serves telnet clients on `listener` until the task is aborted, with
/// the default limits.
pub async fn serve(listener: TcpListener, game: Arc<Game>, presence: Arc<Presence>) {
    serve_limited(
        crate::MAX_CONNECTIONS,
        Timeouts::default(),
        listener,
        game,
        presence,
    )
    .await;
}

/// Serves telnet clients with at most `max` open at once. A client over
/// the limit is told so and dropped; a client that stays silent past
/// `timeouts` is closed, freeing its slot.
pub async fn serve_limited(
    max: usize,
    timeouts: Timeouts,
    listener: TcpListener,
    game: Arc<Game>,
    presence: Arc<Presence>,
) {
    let slots = Arc::new(tokio::sync::Semaphore::new(max));
    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                    let _ = stream
                        .write_all(b"The server is full. Try again later.\r\n")
                        .await;
                    continue;
                };
                let (g, p) = (Arc::clone(&game), Arc::clone(&presence));
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, g, p, timeouts).await {
                        tracing::debug!(%peer, error = %e, "telnet session ended");
                    }
                    drop(permit);
                });
            }
            Err(e) => tracing::warn!(error = %e, "telnet accept failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use mud_world::{ManualClock, World, WorldPack};
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn a_full_server_turns_new_clients_away() {
        let game = Game::new(
            World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1),
            None,
            Arc::new(ManualClock::new(1)),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_limited(
            1,
            Timeouts::default(),
            listener,
            game,
            Arc::new(Presence::default()),
        ));
        let mut first = TcpStream::connect(addr).await.unwrap();
        let mut greeting = [0u8; 16];
        first.read_exact(&mut greeting).await.unwrap();
        let mut second = TcpStream::connect(addr).await.unwrap();
        let mut out = String::new();
        second.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("The server is full"), "{out}");
        task.abort();
    }

    /// Sends `input` to a fresh server and returns everything it wrote
    /// back before closing.
    async fn exchange(input: &[u8]) -> String {
        let game = Game::new(
            World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1),
            None,
            Arc::new(ManualClock::new(1)),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(
            listener,
            Arc::clone(&game),
            Arc::new(Presence::default()),
        ));
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(input).await.unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).await.unwrap();
        task.abort();
        String::from_utf8(out).unwrap()
    }

    #[tokio::test]
    async fn a_telnet_player_logs_in_and_looks_around() {
        let out = exchange(b"ada\r\nsay hello\r\nquit\r\n").await;
        assert!(out.contains("What is your name?"));
        assert!(out.contains("Town Square"));
        assert!(out.contains("You say, \"hello\""));
        assert!(out.contains("Farewell."));
    }

    #[tokio::test]
    async fn telnet_negotiation_and_stray_bytes_do_not_drop_the_client() {
        // IAC WILL NAWS, IAC WILL TTYPE (as PuTTY sends on connect), a
        // NAWS subnegotiation whose height is 10 (a raw '\n' byte), then
        // the name; later IAC IP (Ctrl-C) and a Latin-1 byte mid-line.
        let mut input = vec![0xFF, 0xFB, 0x1F, 0xFF, 0xFB, 0x18];
        input.extend_from_slice(&[0xFF, 0xFA, 0x1F, 0, 80, 0, 10, 0xFF, 0xF0]);
        input.extend_from_slice(b"ada\r\n");
        input.extend_from_slice(&[0xFF, 0xF4]);
        input.extend_from_slice(b"say caf\xE9\r\nsay still here\r\nquit\r\n");
        let out = exchange(&input).await;
        assert!(out.contains("Town Square"), "{out}");
        assert!(out.contains("You say, \"caf\u{FFFD}\""), "{out}");
        assert!(out.contains("You say, \"still here\""), "{out}");
        assert!(out.contains("Farewell."), "{out}");
    }

    #[tokio::test]
    async fn an_endless_line_is_refused_and_the_session_goes_on() {
        // More than the whole old 16 MiB stream budget, in one line.
        let mut input = b"ada\r\nsay ".to_vec();
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
        let text = "\u{4E2D}".repeat(400);
        let out = exchange(format!("ada\r\nsay {text}\r\nquit\r\n").as_bytes()).await;
        assert!(out.contains(&format!("You say, \"{text}\"")), "{out}");
    }

    /// A one-slot server with short timeouts.
    async fn tight(timeouts: Timeouts) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let game = Game::new(
            World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1),
            None,
            Arc::new(ManualClock::new(1)),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_limited(
            1,
            timeouts,
            listener,
            game,
            Arc::new(Presence::default()),
        ));
        (addr, task)
    }

    #[tokio::test]
    async fn a_silent_client_is_closed_and_its_slot_freed() {
        let ms = Some(std::time::Duration::from_millis(100));
        let (addr, task) = tight(Timeouts {
            login: ms,
            idle: None,
        })
        .await;
        let mut silent = TcpStream::connect(addr).await.unwrap();
        let mut out = String::new();
        silent.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("Too slow"), "{out}");
        let mut next = TcpStream::connect(addr).await.unwrap();
        next.write_all(b"ada\r\nquit\r\n").await.unwrap();
        let mut out = String::new();
        next.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("Town Square"), "{out}");
        task.abort();
    }

    #[tokio::test]
    async fn an_idle_player_is_closed() {
        let ms = Some(std::time::Duration::from_millis(100));
        let (addr, task) = tight(Timeouts {
            login: None,
            idle: ms,
        })
        .await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"ada\r\n").await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("Town Square"), "{out}");
        assert!(out.contains("Idle too long"), "{out}");
        task.abort();
    }
}
