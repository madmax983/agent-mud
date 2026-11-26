# Architecture decision records

ADRs are numbered in merge order; design docs say "the accounts ADR", not a
number, and the number is assigned when the ADR merges.

| ADR | Decision |
|---|---|
| [0001](0001-the-server-owns-physics.md) | The server owns physics; minds only type commands |
| [0002](0002-the-supervisor-owns-lifecycle.md) | The supervisor owns lifecycle; littleguy owns minds |
| [0003](0003-attention-router-and-anti-chatter.md) | Event-driven wakes, anti-chatter, and budget breakers |
| [0004](0004-turing-pc-friction.md) | Hidden agent PCs pass by friction, not prose |
| [0005](0005-mcp-scoped-by-bearer-token.md) | One MCP endpoint, one entity per bearer token |
| [0006](0006-verified-kernels.md) | Verus proves the kernels; a mirror test keeps them honest |
| [0007](0007-connection-admission.md) | One limiter admits every player connection |
| [0008](0008-wakes-an-agent-books.md) | Wakes an agent books for itself |
| [0009](0009-accounts-live-outside-the-world.md) | Accounts live outside the world |
| [0010](0010-sandboxed-rhai-scripts.md) | Sandboxed Rhai scripts: data in, data out |
| [0011](0011-supervisor-state-lives-in-the-world-store.md) | Supervisor state lives in the world store |
| [0012](0012-downed-not-limbo.md) | Downed, not limbo: coming to wakes the mind, and death costs more |
| [0013](0013-shops-keep-their-stock-and-retreat-is-a-walk.md) | Shops keep their stock in a fixture, and a retreat is a walk |
| [0014](0014-a-wake-ends-when-its-mind-does.md) | A wake ends when its mind does, and owes only what it did not read |

Planned work that these decisions shape is in [the roadmap](../roadmap.md).
