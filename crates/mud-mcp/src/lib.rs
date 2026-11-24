//! The MCP server of the littleguy MUD.
//!
//! An agent plays one entity through four tools:
//!
//! - `mud_look`: the room, as [`Game::look`] shows it.
//! - `mud_sheet`: the character sheet, as [`Game::sheet`] shows it.
//! - `mud_events(since_seq?, limit?)`: what the entity perceived, as
//!   [`Game::events`] shows it. Without `since_seq` it reads on from a
//!   cursor the server keeps for the bearer token.
//! - `mud_command(cmd)`: one command line, run through a [`CommandGate`].
//!
//! # Entity scoping
//!
//! Each bearer token of the [`TokenRegistry`] is bound to exactly one
//! entity. An axum layer checks `Authorization: Bearer <token>` on every
//! HTTP request, answers 401 to a missing or unknown token, and puts the
//! resolved entity into the request extensions. rmcp's streamable HTTP
//! server hands the HTTP request [`Parts`](axum::http::request::Parts) of
//! each JSON-RPC message to the handler, and the handler reads the entity
//! from there, request by request. No tool takes an entity argument, and
//! the MCP session holds none: a token can act only as its own entity,
//! even on an MCP session that another token opened.
//!
//! # Mounting
//!
//! [`router`] serves MCP at [`MCP_PATH`] (`/mcp`) and works in any axum
//! router state. Merge it into an app to serve `/mcp`; nesting it under a
//! prefix `/p` serves `/p/mcp`.

mod gate;
mod server;
mod tokens;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use mud_world::Game;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

pub use gate::{CommandGate, DirectGate};
pub use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
pub use server::{COMMAND, DEFAULT_EVENTS, EVENTS, LOOK, MAX_EVENTS, SHEET, TOOLS};
pub use tokens::{TokenError, TokenRegistry};

use server::{Caller, MudServer};

/// Where [`router`] serves MCP.
pub const MCP_PATH: &str = "/mcp";

/// The streamable HTTP configuration [`router`] uses: rmcp's defaults
/// (stateful sessions, SSE keep-alive) without the `Host` allow-list.
///
/// rmcp checks `Host` against loopback names by default, against DNS
/// rebinding of unauthenticated local servers. Here every request must
/// carry a bearer token that a rebinding web page cannot know, and the
/// server is meant to run behind a public host name, so the check is off.
#[must_use]
pub fn default_config() -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default().disable_allowed_hosts()
}

/// The MCP endpoint at [`MCP_PATH`], with bearer authentication, using
/// [`default_config`].
///
/// `game` serves the views, `tokens` maps each bearer token to its entity,
/// and `command_gate` runs each `mud_command` line.
pub fn router<S>(
    game: Arc<Game>,
    tokens: Arc<TokenRegistry>,
    command_gate: Arc<dyn CommandGate>,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router_with_config(game, tokens, command_gate, default_config())
}

/// [`router`] with a streamable HTTP configuration of the caller's.
pub fn router_with_config<S>(
    game: Arc<Game>,
    tokens: Arc<TokenRegistry>,
    command_gate: Arc<dyn CommandGate>,
    config: StreamableHttpServerConfig,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let handler = MudServer {
        game,
        tokens: Arc::clone(&tokens),
        gate: command_gate,
    };
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Router::new()
        .route_service(MCP_PATH, service)
        .route_layer(middleware::from_fn_with_state(tokens, authenticate))
}

/// The token of an `Authorization` header value, `Bearer <token>`.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// Answers 401 unless the request carries a live bearer token; otherwise
/// puts its [`Caller`] into the request extensions.
async fn authenticate(
    State(tokens): State<Arc<TokenRegistry>>,
    mut request: Request,
    next: Next,
) -> Response {
    let caller = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer)
        .and_then(|token| {
            tokens.resolve(token).map(|entity| Caller {
                entity,
                token: Arc::from(token),
            })
        });
    let Some(caller) = caller else {
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    };
    request.extensions_mut().insert(caller);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::bearer;

    #[test]
    fn bearer_headers_parse() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Bearer "), None);
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("abc"), None);
    }
}
