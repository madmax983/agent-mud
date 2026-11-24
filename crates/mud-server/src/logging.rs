//! The server's log subscriber.
//!
//! tungstenite traces every frame it reads and writes (`Received message
//! {message}`, `Sending frame: {frame:?}`), and those frames carry
//! passwords and resume tokens. Whatever `MUD_LOG` asks for, the
//! `tungstenite` and `tokio_tungstenite` targets never pass below INFO.
//! `init` installs `LogTracer`, so their `log` records go through the same
//! filter.

use tracing::Subscriber;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::{EnvFilter, FilterExt as _, LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;

/// The filter that keeps wire frames out of every log.
#[must_use]
pub fn wire_quiet() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_target("tungstenite", LevelFilter::INFO)
        .with_target("tokio_tungstenite", LevelFilter::INFO)
}

/// A subscriber that logs `spec` (an `EnvFilter` directive) to `writer`,
/// never with wire frames.
pub fn subscriber<W>(spec: &str, writer: W) -> impl Subscriber + Send + Sync + 'static
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let filter = EnvFilter::new(spec).and(wire_quiet());
    tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_filter(filter),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A writer into a shared buffer.
    #[derive(Clone, Default)]
    pub struct Buffer(pub Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    #[test]
    fn wire_traces_never_pass() {
        let buf = Buffer::default();
        let _g = tracing::subscriber::set_default(subscriber("trace", buf.clone()));
        tracing::trace!(target: "tungstenite::protocol", "Received message hunter2");
        tracing::trace!(target: "tokio_tungstenite", "Sending frame: hunter2");
        tracing::info!(target: "tungstenite::protocol", "a handshake");
        tracing::trace!(target: "mud::auth", "a trace line");
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("a handshake"), "{text}");
        assert!(text.contains("a trace line"), "{text}");
    }
}
