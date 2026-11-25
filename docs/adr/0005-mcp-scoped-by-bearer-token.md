# ADR 0005: One MCP endpoint, one entity per bearer token

Status: accepted

## Decision

`mud-mcp` serves MCP over streamable HTTP (rmcp 3.x, the transport
littleguy's client speaks) at `/mcp`, merged into the autumn-web app.

- Six tools only: `mud_look`, `mud_sheet`, `mud_events(since_seq?,
  limit?)`, `mud_command(cmd)`, and `mud_wake_me(minutes, note)` /
  `mud_cancel_wake(id)` (ADR 0008). No tool takes an entity argument.
- The wake tools need a supervised Persistent or Turing entity with a
  mind (`Supervisor::can_book`). `tools/list` stays six tools (a
  littleguy agent is Persistent or Turing), and any other token (an
  ambient NPC, a mob, a human, an entity no mind plays) is answered
  `{"ok":false,"error_code":"no_wakes"}`, in words that name no tier.
  In-process minds follow the same rule: `tool_specs()` is the four world
  tools, `wake_tool_specs()` the two wake tools, and only Persistent and
  Turing minds (harvest, littleguy, scripted) get the second set; ambient
  minds (plugin-agent, and harvest for an ambient NPC) get four tools.
- An axum layer resolves `Authorization: Bearer <token>` to an entity on
  every request and returns 401 otherwise. The handler reads the caller
  from the request parts rmcp forwards, so a session opened with one token
  and continued with another acts as the second; a token can never act as
  another entity.
- Tokens are 256-bit random hex, never logged, revocable per token or per
  entity.
- `mud_events` without `since_seq` reads a cursor kept per token. Tokens
  live in memory, so a restart issues new ones; each starts at its agent's
  replay floor (ADR 0011, phase 4): the last event a committed wake of the
  agent consumed. So after a restart `mud_events` hands out nothing that
  wake consumed and skips nothing after it, while what the agent read but
  no wake committed comes again.
- `mud_command` runs through a `CommandGate`; the server passes the Turing
  typing gate, so remote agents get the same latency as in-process ones.

## Consequences

Any MCP client (littleguy, Claude Code, a test harness) can play a
character with nothing but its token and the URL.
