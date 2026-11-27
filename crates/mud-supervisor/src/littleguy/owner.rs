//! The owner side of an `lg serve` socket.
//!
//! The supervisor starts every daemon as its own user, so littleguy takes
//! it for the daemon's owner (lg checks the peer uid) and answers the
//! owner's requests: the goals and notes it keeps, its overlays, a reload
//! of its config, and a sign-in link to its pages. This client speaks that
//! protocol: one JSON line out per connection, one response line back.
//!
//! lg answers `approvals`, `plans`, `notes`, `overlays` and
//! `overlay_show` beside its job loop, at once. Every other request here
//! (`decide`, `refine_propose`, `overlay_put`, `overlay_activate`, `reload`
//! and `ui_link`) is a loop job: it waits behind a wake chat or a heartbeat
//! tick that runs, so it gets [`LOOP_JOB_TIMEOUT`], not [`READ_TIMEOUT`].
//!
//! An `overlay_activate` names the pointer it read (`expected`, lg #122,
//! ADR 0013 of littleguy): lg moves the pointer only when no move came in
//! between, and answers `overlay_moved` otherwise. An older lg drops the
//! field and moves unchecked; its answer then has no `guarded`. Since
//! littleguy #124, `overlay_active` also names the `head` of the move it
//! reports, so the owner can see a later move away and back to the same
//! version. An older lg leaves it out.
//!
//! The requests are a closed set. There is no `approve`, no `deny` and no
//! `overlay_rollback`: the MUD never approves a tool call for an agent,
//! and a rollback is the human's veto on lg's own pages, never the MUD's.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::LG_MAX_LINE_BYTES;

/// How long a read beside lg's job loop may take.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a loop job may take: it waits behind a wake chat (120 s by
/// default) or a heartbeat tick, which may research ideas. It is a cap on
/// a wait, not a deadline lg keeps.
pub const LOOP_JOB_TIMEOUT: Duration = Duration::from_secs(300);

/// What the owner decides about a proposed or active goal. The MUD never
/// drops or reopens an idea.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDecision {
    /// Accept a proposed goal.
    Accept,
    /// Decline a proposed goal.
    Decline,
    /// Drop an active goal.
    DropGoal,
}

/// The overlay pointer that an `overlay_activate` expects.
///
/// Both fields are copied from the `overlays` answer read just before it.
/// Both keys are always on the wire, and `null` means what it means in
/// `overlays`: no active version, or no move yet. lg refuses a partial
/// pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedPointer {
    /// The active version, or `None`.
    pub active: Option<u64>,
    /// The `seq` of the last move of the pointer, or `None` before the
    /// first move. A move away and back changes it.
    pub head: Option<u64>,
}

/// Reads a field that is there, `null` included, as `Some`; a missing one
/// is `None` through `#[serde(default)]`.
fn named<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// One owner request to `lg serve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnerRequest {
    /// The tool calls that wait for an approval. The MUD config has no
    /// `[sandbox]`, so the answer is `off`.
    Approvals,
    /// The goals and ideas of the agent.
    Plans,
    /// Accept or decline a proposed goal, or drop an active one.
    Decide {
        /// What to do.
        decision: PlanDecision,
        /// The goal.
        id: i64,
    },
    /// The notes of the agent.
    Notes,
    /// The overlay versions.
    Overlays,
    /// One overlay version.
    OverlayShow {
        /// The version.
        version: u64,
    },
    /// Ask lg's proposer for an overlay from the transcript.
    RefinePropose,
    /// Store an overlay as a new, inactive version.
    OverlayPut {
        /// The overlay JSON.
        overlay: String,
    },
    /// Make one version the active one.
    OverlayActivate {
        /// The version.
        version: u64,
        /// The pointer read just before. With it, lg moves the pointer
        /// only when nothing moved since, checked in its swap transaction.
        /// Left off the wire when `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        expected: Option<ExpectedPointer>,
    },
    /// Read the config file again, and apply it.
    Reload,
    /// A one-time sign-in link to the daemon's pages.
    UiLink,
}

impl OwnerRequest {
    /// Whether lg runs this request in its job loop, after the chat or
    /// tick that runs now, and not beside the loop.
    #[must_use]
    pub const fn is_loop_job(&self) -> bool {
        !matches!(
            self,
            Self::Approvals | Self::Plans | Self::Notes | Self::Overlays | Self::OverlayShow { .. }
        )
    }

    /// How long to wait for the answer: [`LOOP_JOB_TIMEOUT`] for a loop
    /// job, [`READ_TIMEOUT`] for a read.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        if self.is_loop_job() {
            LOOP_JOB_TIMEOUT
        } else {
            READ_TIMEOUT
        }
    }

    /// The `kind` the request carries on the wire.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Approvals => "approvals",
            Self::Plans => "plans",
            Self::Decide { .. } => "decide",
            Self::Notes => "notes",
            Self::Overlays => "overlays",
            Self::OverlayShow { .. } => "overlay_show",
            Self::RefinePropose => "refine_propose",
            Self::OverlayPut { .. } => "overlay_put",
            Self::OverlayActivate { .. } => "overlay_activate",
            Self::Reload => "reload",
            Self::UiLink => "ui_link",
        }
    }
}

/// One call that waits for an approval.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApprovalEntry {
    /// The request number.
    pub id: i64,
    /// The tool.
    pub tool: String,
    /// What the call does.
    pub text: String,
}

/// One note of a goal or an idea.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlanNote {
    /// When, in epoch milliseconds (lg's clock, never the game's).
    pub at_ms: i64,
    /// The text, which the agent wrote.
    pub text: String,
}

/// One proposed or active goal.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GoalEntry {
    /// Its number.
    pub id: i64,
    /// `proposed` or `active`.
    pub status: String,
    /// A short name.
    pub title: String,
    /// Why it matters.
    pub why: String,
    /// How often it is checked, in minutes.
    #[serde(default)]
    pub check_every_minutes: Option<i64>,
    /// When the next check is due, in epoch milliseconds.
    #[serde(default)]
    pub next_check_ms: Option<i64>,
    /// The idea it came from.
    #[serde(default)]
    pub idea: Option<i64>,
    /// The newest notes, oldest first.
    #[serde(default)]
    pub notes: Vec<PlanNote>,
}

/// One open or answered idea.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IdeaEntry {
    /// Its number.
    pub id: i64,
    /// A short name.
    pub title: String,
    /// What it is.
    pub body: String,
    /// When it was filed, in epoch milliseconds.
    #[serde(default)]
    pub created_ms: i64,
    /// The newest research notes, oldest first.
    #[serde(default)]
    pub notes: Vec<PlanNote>,
}

/// One note of the agent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NoteEntry {
    /// Its name.
    pub name: String,
    /// The note.
    pub text: String,
    /// When it last changed, in epoch milliseconds.
    #[serde(default)]
    pub updated_ms: i64,
}

/// One overlay version.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OverlayEntry {
    /// The version.
    pub version: u64,
    /// Who wrote it, such as `operator:mud` or `operator:lg-refine`.
    pub author: String,
    /// When it was stored, in RFC 3339.
    pub created_at: String,
    /// Whether the agent runs on it.
    pub active: bool,
    /// The parts it sets, such as `guardrails`.
    #[serde(default)]
    pub parts: Vec<String>,
}

/// The answer of `lg serve` to an owner request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnerResponse {
    /// The calls that wait for an approval.
    Approvals {
        /// Each call, oldest first.
        entries: Vec<ApprovalEntry>,
    },
    /// The goals and ideas.
    Plans {
        /// The active goals, then the proposed ones.
        goals: Vec<GoalEntry>,
        /// The open ideas.
        ideas: Vec<IdeaEntry>,
        /// The newest answered ideas.
        #[serde(default)]
        answered: Vec<IdeaEntry>,
    },
    /// The notes.
    Notes {
        /// Each note.
        notes: Vec<NoteEntry>,
    },
    /// The overlay versions.
    Overlays {
        /// The active version, if any.
        active: Option<u64>,
        /// The `seq` of the last move of the pointer, read with `active`
        /// from one row. lg #122 always names it: `Some(None)` before the
        /// first move. An lg before #122 never does: `None`. Only a named
        /// head guards an activate.
        #[serde(default, deserialize_with = "named")]
        head: Option<Option<u64>>,
        /// Each version, oldest first.
        entries: Vec<OverlayEntry>,
    },
    /// One overlay version.
    Overlay {
        /// The version.
        entry: OverlayEntry,
        /// The overlay JSON.
        overlay: String,
    },
    /// The proposer stored an inactive overlay.
    Proposed {
        /// The stored version.
        version: u64,
        /// The overlay JSON.
        overlay: String,
    },
    /// The proposer kept the active overlay.
    Kept,
    /// An overlay was stored, inactive.
    OverlayStored {
        /// The version.
        version: u64,
    },
    /// The agent runs on `version` from its next op.
    OverlayActive {
        /// The active version.
        version: u64,
        /// The version that was active before, if any.
        previous: Option<u64>,
        /// lg checked the `expected` pointer of the request, so the move
        /// was exactly from it. An lg before #122 never says so.
        #[serde(default)]
        guarded: bool,
        /// The `seq` of the move this answer reports (littleguy #124), or
        /// the head as it stands when the version was already active. lg
        /// never names the head of a later move. It is `None` from an lg
        /// before #124, before the first move, and when lg cannot tell
        /// that the head belongs to this move.
        #[serde(default)]
        head: Option<u64>,
    },
    /// The pointer moved after the read that `expected` copied, so lg did
    /// not move it now: nothing changed.
    OverlayMoved {
        /// The version the request named.
        version: u64,
        /// The active version now, if any.
        active: Option<u64>,
        /// The `seq` of the last move now, if any.
        head: Option<u64>,
    },
    /// The daemon read its config again.
    Reloaded {
        /// The sections that changed and apply now.
        applied: Vec<String>,
        /// The sections that changed and apply at the next start.
        restart: Vec<String>,
        /// The sections that shape the agent profile.
        #[serde(default)]
        profile: Vec<String>,
    },
    /// A one-time sign-in link.
    ConnectLink {
        /// The link.
        link: String,
    },
    /// What the request asked for is not set up in this daemon.
    Off {
        /// What is off.
        message: String,
    },
    /// The request was refused, or the daemon failed.
    Error {
        /// What went wrong.
        message: String,
    },
    /// A kind this client does not know. [`request`] answers it as
    /// [`OwnerError::Malformed`].
    #[serde(other)]
    Other,
}

/// An owner request that got no usable answer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OwnerError {
    /// No answer within the time given.
    #[error("lg serve did not answer in time")]
    Timeout,
    /// The socket could not be reached, or closed early.
    #[error("lg serve: {0}")]
    Io(String),
    /// The request or the answer is not what the protocol allows.
    #[error("lg serve answered something unexpected: {0}")]
    Malformed(String),
}

/// Sends `req` over the owner socket at `socket` and reads its answer.
///
/// A loop job (`decide`, `overlay_put`, `overlay_activate`, `reload`,
/// `refine_propose`, `ui_link`) answers when lg's loop runs it, on this
/// connection: `timeout` should cover a chat or a tick that runs ahead of
/// it ([`OwnerRequest::timeout`]).
///
/// # Errors
/// [`OwnerError::Timeout`] when nothing came in `timeout`,
/// [`OwnerError::Io`] when the socket fails, and
/// [`OwnerError::Malformed`] for a request longer than lg reads or an
/// answer this client does not know.
pub async fn request(
    socket: &Path,
    req: &OwnerRequest,
    timeout: Duration,
) -> Result<OwnerResponse, OwnerError> {
    let mut line = serde_json::to_string(req).map_err(|e| OwnerError::Malformed(e.to_string()))?;
    line.push('\n');
    if line.len() > LG_MAX_LINE_BYTES {
        return Err(OwnerError::Malformed(format!(
            "a {} request of {} bytes is longer than lg accepts",
            req.kind(),
            line.len()
        )));
    }
    tokio::time::timeout(timeout, exchange(socket, &line))
        .await
        .map_err(|_| OwnerError::Timeout)?
}

/// One line out, the response line back.
async fn exchange(socket: &Path, line: &str) -> Result<OwnerResponse, OwnerError> {
    let io = |e: std::io::Error| OwnerError::Io(format!("{}: {e}", socket.display()));
    let mut stream = UnixStream::connect(socket).await.map_err(io)?;
    stream.write_all(line.as_bytes()).await.map_err(io)?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    loop {
        buf.clear();
        if reader.read_line(&mut buf).await.map_err(io)? == 0 {
            return Err(OwnerError::Io("the daemon closed the socket".into()));
        }
        let v: Value =
            serde_json::from_str(buf.trim()).map_err(|e| OwnerError::Malformed(e.to_string()))?;
        let kind = v["kind"].as_str().unwrap_or_default().to_owned();
        // A streamed piece is a preview; the response line comes after.
        if matches!(kind.as_str(), "delta" | "restart") {
            continue;
        }
        return match serde_json::from_value(v) {
            Ok(OwnerResponse::Other) => Err(OwnerError::Malformed(format!("kind {kind:?}"))),
            Ok(r) => Ok(r),
            Err(e) => Err(OwnerError::Malformed(format!("{kind}: {e}"))),
        };
    }
}

#[cfg(test)]
mod tests;
