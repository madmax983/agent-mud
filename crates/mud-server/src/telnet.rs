//! Plain-text clients: telnet, netcat, MUD clients.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::session::{Line, Presence, Session};
use mud_world::Game;

/// The longest line a telnet client may send.
const MAX_LINE: usize = 1_024;

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
    let mut lines = BufReader::new(r.take(1 << 24)).lines();
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
                let line: String = line.chars().take(MAX_LINE).collect();
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

    use super::*;

    #[tokio::test]
    async fn a_telnet_player_logs_in_and_looks_around() {
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
        c.write_all(b"ada\r\nsay hello\r\nquit\r\n").await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("What is your name?"));
        assert!(out.contains("Town Square"));
        assert!(out.contains("You say, \"hello\""));
        assert!(out.contains("Farewell."));
        task.abort();
    }
}
