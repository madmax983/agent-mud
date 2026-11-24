//! Plain-text clients: telnet, netcat, MUD clients.
//!
//! Before a password prompt the server sends `IAC WILL ECHO`, so a telnet
//! client stops echoing what the player types; after the next input it
//! sends `IAC WONT ECHO`. A client that switches to character mode while
//! echo is off may end lines with `CR NUL`; a CR ends a line, and a LF or
//! NUL right after it is swallowed. Telnet never remembers a login.

use std::net::SocketAddr;
use std::sync::Arc;

use mud_accounts::{Accounts, Peer, Secret};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::config::Timeouts;
use crate::limits::{ConnLimiter, bucket};
use crate::login::{Dialogue, Next};
use crate::session::{Line, LineKind, Presence, Session, Transport};
use mud_world::Game;

/// The longest line a telnet client may send, in characters. A longer
/// line is read to its end, discarded and refused.
const MAX_LINE: usize = 1_024;

/// The bytes kept of one line: room for [`MAX_LINE`] characters of four
/// UTF-8 bytes each.
const MAX_LINE_BYTES: usize = MAX_LINE * 4;

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
/// "I will not".
const WONT: u8 = 0xFC;
/// The echo option.
const ECHO: u8 = 0x01;
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
    /// The last text byte was a CR that ended a line.
    after_cr: bool,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    fn new(r: R) -> Self {
        Self {
            inner: BufReader::new(r),
            telnet: Telnet::Data,
            line: Vec::new(),
            pending: false,
            overflow: false,
            after_cr: false,
        }
    }

    fn take_line(&mut self) -> Input {
        self.pending = false;
        let overflow = std::mem::take(&mut self.overflow);
        let line = std::mem::take(&mut self.line);
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
                let Some(c) = self.telnet.feed(b) else {
                    continue;
                };
                let after_cr = std::mem::take(&mut self.after_cr);
                match c {
                    b'\n' | 0 if after_cr => {}
                    b'\r' => {
                        self.after_cr = true;
                        done = true;
                        break;
                    }
                    b'\n' => {
                        done = true;
                        break;
                    }
                    c => {
                        self.pending = true;
                        if self.line.len() < MAX_LINE_BYTES {
                            self.line.push(c);
                        } else {
                            self.overflow = true;
                        }
                    }
                }
            }
            self.inner.consume(used);
            if done {
                return Ok(Some(self.take_line()));
            }
        }
    }
}

/// Writes `lines`; returns whether the last one asked for a secret, after
/// which echo is off.
async fn write_lines(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    lines: &[Line],
) -> std::io::Result<bool> {
    let mut secret = false;
    for l in lines {
        // Control characters from other players never reach the terminal.
        let clean: String = l
            .text
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        w.write_all(clean.replace('\n', "\r\n").as_bytes()).await?;
        w.write_all(b"\r\n").await?;
        secret = matches!(l.kind, LineKind::Secret(_));
    }
    if secret {
        w.write_all(&[IAC, WILL, ECHO]).await?;
    }
    Ok(secret)
}

/// Turns echo back on once a secret answer has arrived (or the client
/// went away).
async fn echo_on(w: &mut tokio::net::tcp::OwnedWriteHalf, off: &mut bool) -> std::io::Result<()> {
    if std::mem::take(off) {
        w.write_all(&[IAC, WONT, ECHO]).await?;
        w.write_all(b"\r\n").await?;
    }
    Ok(())
}

/// What one telnet connection shares with the rest of the server.
#[derive(Debug, Clone)]
pub struct Shared {
    /// The world.
    pub game: Arc<Game>,
    /// Open connections per player.
    pub presence: Arc<Presence>,
    /// The accounts.
    pub accounts: Arc<Accounts>,
    /// The timeouts.
    pub timeouts: Timeouts,
}

async fn handle(stream: TcpStream, from: SocketAddr, shared: Shared) -> std::io::Result<()> {
    let peer = bucket(Some(from.ip())).map_or(Peer::Shared, Peer::Addr);
    let (r, mut w) = stream.into_split();
    let mut lines = LineReader::new(r);
    let mut dialogue = Dialogue::new(
        Arc::clone(&shared.game),
        Arc::clone(&shared.accounts),
        Transport::Telnet,
        peer,
    );
    w.write_all(b"Welcome to Harrow's Rest.\r\n").await?;
    let mut echo_off = write_lines(&mut w, &dialogue.greeting().lines).await?;
    let deadline = shared.timeouts.login_deadline();
    let auth = loop {
        let Some(next) = Timeouts::until(deadline, lines.next_line()).await else {
            echo_on(&mut w, &mut echo_off).await?;
            w.write_all(b"Too slow. Farewell.\r\n").await?;
            return Ok(());
        };
        echo_on(&mut w, &mut echo_off).await?;
        let reply = match next? {
            None => return Ok(()),
            Some(Input::TooLong) => dialogue.too_long(),
            Some(Input::Line(line)) => {
                let Some(r) =
                    Timeouts::until(deadline, dialogue.answer(Secret::new(line), false)).await
                else {
                    w.write_all(b"Too slow. Farewell.\r\n").await?;
                    return Ok(());
                };
                r
            }
        };
        echo_off = write_lines(&mut w, &reply.lines).await?;
        match reply.next {
            Next::Ask => {}
            Next::Close(_) => {
                echo_on(&mut w, &mut echo_off).await?;
                return Ok(());
            }
            Next::Done(auth) => break *auth,
        }
    };
    let (session, greet) = match Session::login(
        Arc::clone(&shared.game),
        Arc::clone(&shared.presence),
        Arc::clone(&shared.accounts),
        auth,
        peer,
        Transport::Telnet,
    ) {
        Ok(s) => s,
        Err(e) => {
            w.write_all(format!("{e}. Farewell.\r\n").as_bytes())
                .await?;
            return Ok(());
        }
    };
    echo_off = write_lines(&mut w, &greet).await?;
    play(&mut lines, &mut w, &shared, session, echo_off).await
}

/// A logged-in telnet player: lines in, events out, and a revalidation
/// tick.
async fn play(
    lines: &mut LineReader<tokio::net::tcp::OwnedReadHalf>,
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    shared: &Shared,
    mut session: Session,
    mut echo_off: bool,
) -> std::io::Result<()> {
    let mut events = shared.game.subscribe();
    let every =
        std::time::Duration::from_secs(u64::from(shared.accounts.config().revalidate_secs.max(1)));
    let mut recheck = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    loop {
        tokio::select! {
            line = Timeouts::within(shared.timeouts.idle, lines.next_line()) => {
                let Some(line) = line else {
                    echo_on(w, &mut echo_off).await?;
                    w.write_all(b"Idle too long. Farewell.\r\n").await?;
                    break;
                };
                echo_on(w, &mut echo_off).await?;
                let out = match line? {
                    None => break,
                    Some(Input::TooLong) => session.too_long(),
                    Some(Input::Line(line)) => session.input(Secret::new(line)).await,
                };
                echo_off = write_lines(w, &out.lines).await?;
                if out.close.is_some() {
                    echo_on(w, &mut echo_off).await?;
                    break;
                }
            }
            ev = events.recv() => match ev {
                Ok(ev) => {
                    if let Some(l) = session.render(&ev) {
                        write_lines(w, &[l]).await?;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = recheck.tick() => match session.revalidate().await {
                Ok(true) => {}
                Ok(false) => {
                    echo_on(w, &mut echo_off).await?;
                    w.write_all(b"Your account changed elsewhere. Farewell.\r\n").await?;
                    break;
                }
                Err(e) => tracing::warn!(target: "mud::auth", error = %e, "a session could not be revalidated"),
            },
        }
    }
    drop(session);
    Ok(())
}

/// Serves telnet clients on `listener` until the task is aborted, with
/// the default limits.
pub async fn serve(listener: TcpListener, shared: Shared) {
    serve_limited(
        ConnLimiter::new(crate::MAX_CONNECTIONS, crate::MAX_CONNECTIONS_PER_ADDRESS),
        listener,
        shared,
    )
    .await;
}

/// Serves telnet clients while `limiter` admits them.
///
/// The limiter may be shared with the web terminal. A client over a limit
/// is told so and dropped; a client that does not finish its login within
/// the login deadline, or stays silent past the idle timeout, is closed,
/// freeing its slot.
pub async fn serve_limited(limiter: Arc<ConnLimiter>, listener: TcpListener, shared: Shared) {
    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let permit = match limiter.try_acquire(Some(peer.ip())) {
                    Ok(p) => p,
                    Err(refusal) => {
                        let _ = stream.write_all(format!("{refusal}\r\n").as_bytes()).await;
                        continue;
                    }
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, peer, shared).await {
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
mod tests;
