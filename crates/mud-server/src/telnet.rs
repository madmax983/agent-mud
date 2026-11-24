//! Plain-text clients: telnet, netcat, MUD clients.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::session::{Line, Presence, Session};
use mud_world::Game;

/// The longest line a telnet client may send, in bytes. The rest of a
/// longer line is read and discarded.
const MAX_LINE: usize = 1_024;

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

/// Reads lines from a telnet client: strips telnet commands, keeps at
/// most [`MAX_LINE`] bytes of each line, and decodes what is left as
/// UTF-8, replacing invalid bytes. Cancel-safe: a partial line survives a
/// dropped `next_line` future.
struct LineReader<R> {
    inner: BufReader<R>,
    telnet: Telnet,
    line: Vec<u8>,
    pending: bool,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    fn new(r: R) -> Self {
        Self {
            inner: BufReader::new(r),
            telnet: Telnet::Data,
            line: Vec::new(),
            pending: false,
        }
    }

    fn take_line(&mut self) -> String {
        self.pending = false;
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8_lossy(&line).into_owned()
    }

    /// The next line, or `None` at the end of the stream.
    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
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
                        if self.line.len() < MAX_LINE {
                            self.line.push(c);
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
) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = LineReader::new(r);
    w.write_all(b"Welcome to Harrow's Rest. What is your name?\r\n")
        .await?;
    let session = loop {
        let Some(name) = lines.next_line().await? else {
            return Ok(());
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
            line = lines.next_line() => {
                let Some(line) = line? else { break };
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

/// Serves telnet clients on `listener` until the task is aborted.
pub async fn serve(listener: TcpListener, game: Arc<Game>, presence: Arc<Presence>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let (g, p) = (Arc::clone(&game), Arc::clone(&presence));
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, g, p).await {
                        tracing::debug!(%peer, error = %e, "telnet session ended");
                    }
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
    async fn an_endless_line_is_cut_and_the_session_goes_on() {
        // More than the whole old 16 MiB stream budget, in one line.
        let mut input = b"ada\r\nsay ".to_vec();
        input.resize(input.len() + (17 << 20), b'a');
        input.extend_from_slice(b"\r\nsay still here\r\nquit\r\n");
        let out = exchange(&input).await;
        // The cut line still runs, once, as one (too long) command.
        assert_eq!(out.matches("too long").count(), 1, "{out}");
        assert!(out.contains("You say, \"still here\""), "{out}");
        assert!(out.contains("Farewell."), "{out}");
    }

    #[tokio::test]
    async fn the_line_reader_strips_telnet_and_bounds_lines() {
        let mut input = vec![b'h', 0xFF, 0xFF, b'i', b'\r', b'\n'];
        input.extend_from_slice(&[0xFF, 0xFA, 0x18, b'\n', 0xFF, 0xFF, 0xFF, 0xF0]);
        input.extend_from_slice(&[0xFF, 0xF1, b'x']);
        input.resize(input.len() + 3 * MAX_LINE, b'y');
        input.extend_from_slice(b"\nlast");
        let mut r = LineReader::new(&input[..]);
        assert_eq!(r.next_line().await.unwrap().unwrap(), "h\u{FFFD}i");
        let long = r.next_line().await.unwrap().unwrap();
        assert_eq!(long.len(), MAX_LINE);
        assert!(long.starts_with("xyy"));
        assert_eq!(r.next_line().await.unwrap().unwrap(), "last");
        assert!(r.next_line().await.unwrap().is_none());
        let mut r = LineReader::new(&[0xFF, 0xFB, 0x01][..]);
        assert!(r.next_line().await.unwrap().is_none());
    }
}
