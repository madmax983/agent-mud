# ADR 0005: One MCP endpoint, one entity per bearer token

Status: accepted

## Decision

`mud-mcp` serves MCP over streamable HTTP (rmcp 3.x, the transport
littleguy's client speaks) at `/mcp`, merged into the autumn-web app.

- Four tools only: `mud_look`, `mud_sheet`, `mud_events(since_seq?,
  limit?)`, `mud_command(cmd)`. No tool takes an entity argument.
- An axum layer resolves `Authorization: Bearer <token>` to an entity on
  every request and returns 401 otherwise. The handler reads the caller
  from the request parts rmcp forwards, so a session opened with one token
  and continued with another acts as the second; a token can never act as
  another entity.
- Tokens are 256-bit random hex, never logged, revocable per token or per
  entity.
- `mud_events` without `since_seq` reads a cursor kept per token.
- `mud_command` runs through a `CommandGate`; the server passes the Turing
  typing gate, so remote agents get the same latency as in-process ones.

## Consequences

Any MCP client (littleguy, Claude Code, a test harness) can play a
character with nothing but its token and the URL.
