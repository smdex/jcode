# Jcode ACP support (protocol version 1)

Jcode's stdio adapter uses newline-delimited JSON-RPC and the native Jcode daemon.
This document describes supported behavior, not parity claims about other agents.

| Surface | Native implementation / behavior |
| --- | --- |
| `session/new` | Creates a daemon session in the requested absolute cwd. |
| `session/list` | Lists native persisted sessions plus live adapter sessions not yet persisted, newest first. Entries have absolute cwd and updatedAt, with titles when available. Persisted metadata wins for duplicate IDs. Exact cwd filtering and opaque nextCursor pagination, 50 results per page. Debug sessions and unreadable snapshots are excluded. External-agent imports are not enumerated. |
| `session/load` | Attaches to the native session and replays user/assistant text, tool identities/inputs/results and stored images before responding. Hidden system rows are not replayed as assistant messages. |
| `session/resume` | Attaches with configuration state but without history replay. |
| `session/close`, `session/cancel` | Cancels native work and releases the adapter's session attachment on close. Does not delete persisted history. |
| Model | Standard `configOptions`, `session/set_config_option` and legacy `models` / `session/set_model`, using the daemon catalog. Initial catalog is explicitly fetched because startup history can omit it. |
| Reasoning effort | Native provider/model effort ladder, including `swarm` and `swarm-deep` when supported. No fictitious medium default is displayed when the daemon reports no effort. |
| Operation mode | `solo`, `swarm` (light fan-out) and `swarm-deep` (deep task graph), backed by native SetReasoningEffort. Both config options and legacy `modes` / `session/set_mode` are synchronized. Leaving swarm via solo selects medium effort. These are orchestration modes, not permission policies. |
| Subagent model | Session-scoped native SetSubagentModel. `inherit` clears the pin. |
| Compaction | Native reactive / proactive / semantic session compaction mode. |
| Slash commands | `/model [id]`, `/models`, `/effort [level]`, `/subagent <prompt>`, `/subagent-model [id\|inherit]`, `/skills reload`. A leading space escapes local slash interpretation. |
| Subagent progress | `/subagent` calls native RunSubagent (general type). Tool lifecycle and direct-child swarm status/output are presented as ordinary ACP tool calls with stable IDs. Jcode session/parent IDs are optional `_meta`, not invented protocol fields. |
| Thinking / usage | Live native reasoning deltas become agent_thought_chunk. Native token usage supplies existing prompt usage and context updates. |

Configuration changes are rejected while this adapter is processing a prompt,
so configuration readers do not steal events from the active streaming reader.
Session creation/attachment and configuration/slash control operations have a
30-second total timeout. Streaming model/subagent turns are not subject to this
control timeout. Partial daemon lines survive cancelled control reads.
The daemon remains authoritative and can reject unsupported provider values.
An unset or unknown native reasoning effort is omitted from configuration controls
rather than misrepresented as an effective default.

## Deliberate gaps

- No advertised ask/read-only/permission-bypass mode. Jcode has no equivalent
  enforced native policy in this adapter.
- No draft ACP v2 or draft subagent-session routing API. Child activity uses
  ordinary v1 tool calls, not a fabricated nested-session capability.
- No session-scoped MCP setup. The existing adapter accepts the mcpServers array
  for compatibility but uses Jcode's existing daemon MCP configuration.
- No advertised service-tier or transport selector. Native setters exist, but
  the history wire does not expose the provider's full allowed option set.
- No external-agent history enumeration or deletion/fork endpoint.
- Worker notifications are consumed while a turn is streaming, not by a separate
  idle-session pump. Tool error flags are not available in rendered historical
  tool rows, so replay cannot reconstruct their original failure status.

## Validation

`cargo test -p jcode --lib cli::acp` runs adapter unit tests and deterministic
JSON-RPC/native socket-pair bridge tests with captured ACP output. It checks
configuration dispatch, complete option responses, native model/effort changes,
subagent streaming, replay including images, and pagination. It does not call a
paid provider or certify interoperability with every editor client.

Primary specifications checked on October 7, 2026:

- https://agentclientprotocol.com/protocol/v1/session-setup
- https://agentclientprotocol.com/protocol/v1/session-config-options
- https://agentclientprotocol.com/protocol/v1/session-modes
- https://agentclientprotocol.com/rfds/session-list
- https://agentclientprotocol.com/rfds/session-resume
