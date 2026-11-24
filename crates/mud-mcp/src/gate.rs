//! [`CommandGate`]: the one door `mud_command` goes through.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use mud_world::{CommandResult, EntityId, Game};

/// Runs the command lines of `mud_command`.
///
/// The MCP server never calls [`Game::execute`] itself: every line goes
/// through a gate, so a supervisor can pace an agent (typing latency),
/// queue its lines, or cancel them. The entity is the one the bearer token
/// is bound to; a gate must run the line as that entity and no other.
pub trait CommandGate: Send + Sync + 'static {
    /// Runs one command line for `entity`; a supervisor may delay it
    /// (typing latency) or cancel it. The result goes back to the caller
    /// as the result of `mud_command`.
    fn run(&self, entity: EntityId, line: String) -> BoxFuture<'static, CommandResult>;
}

/// A gate that runs each line at once through [`Game::execute`].
#[derive(Clone)]
pub struct DirectGate(pub Arc<Game>);

impl DirectGate {
    /// A gate onto `game`.
    #[must_use]
    pub const fn new(game: Arc<Game>) -> Self {
        Self(game)
    }
}

impl fmt::Debug for DirectGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DirectGate")
    }
}

impl CommandGate for DirectGate {
    fn run(&self, entity: EntityId, line: String) -> BoxFuture<'static, CommandResult> {
        let game = Arc::clone(&self.0);
        Box::pin(async move { game.execute(&entity, &line) })
    }
}
