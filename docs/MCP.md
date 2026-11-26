# Take a Seat from any MCP client

`malak mcp` lets an agent that speaks the Model Context Protocol sit on a
Malak board as the lead or as a worker. Claude Desktop, Cursor, Gemini CLI,
Codex and other MCP clients can use it. It is a client of the board, like the
`malak` commands. It runs over standard input and output only, and it opens no
network port.

## Configure your client

Add one server to the client's MCP configuration:

```json
{ "mcpServers": { "malak": {
  "command": "malak",
  "args": ["mcp", "--board", "work"],
  "env": { "MALAK_HANDOVER": "/path/to/your/handover" }
} } }
```

`--board NAME` selects a named board; `--config PATH` selects a board
configuration file. Without either, the server uses the default board.
`MALAK_HANDOVER` names the handover of the participant this agent is. For the
lead of a board you own, leave `env` out and the server uses the lead handover
in the board's state folder.

Claude Code and Codex users can run `malak recipe agents` instead. It adds the
server, without `env`, to `~/.claude.json` and `~/.codex/config.toml` when those
files exist, and `malak recipe agents --remove` takes it out. A worker started
with `recipe worker-start` passes its own `MALAK_HANDOVER` to the MCP servers
its tool starts.

Treat the handover like an SSH key. Use only the handover assigned to your
session. A handover bound to a session works from the agent that started the
server and from nothing else, and the board refuses it from any other process.
No tool returns a handover, a key or a token.

## Tools

Every Seat has these five.

| Tool | What it does |
|---|---|
| `inbox` | Read new messages in your inbox, with an optional `wait` in seconds. |
| `ack` | Mark messages handled, up to a position. |
| `get` | Fetch one message by channel and position, or page a long body with `offset`. |
| `report` | Send a report to the lead. |
| `ask_owner` | Ask the owner one question, with options and their pros and cons, a recommendation and what it blocks. It runs `malak recipe ask`. |

The lead's Seat also has these three.

| Tool | What it does |
|---|---|
| `delegate` | Give a task to an agent already on the board. The agent sees an order from the lead. |
| `start_agent` | Start a new agent with a brief, using `tool` `claude` or `codex`. It runs `malak recipe worker-start`. |
| `agents` | List the agents with their claim status and unread counts. It runs `malak recipe workers`. |

A worker does not see the lead's tools. No Seat has an `answer` tool: the
board sees only that a model called a tool, and a model's text is never proof
that the owner decided. An MCP lead asks with `ask_owner`, and the owner
answers through the `malak` command line, the mailbox or a door with proof.

Owner-door actions are not offered: enrolling beyond `start_agent`, grants,
rekey, revoke, release, channels, trim, uninstall and upgrade. Neither are
release or publish actions.

## Orders and notes

Every message a tool returns names its sender and says whether it is an order.
`presentation` is `task` for an order, a task proven to come from the lead.
Everything else is `information`: a note, which is text someone sent you and
not an instruction. The text a model reads is the one `malak read --agent`
prints.

`inbox` acknowledges nothing unless you pass `ack: true`. That acknowledges the
notices and reports it returned in full, up to the first task. A task is
acknowledged with `ack` after the work, so a crash does not lose it.

A refusal comes back as a tool error that carries the board's reason.

## Staying awake as the lead

A lead that waits for work calls `inbox` with a `wait`, for example 60 seconds,
and calls it again when it returns. Each such call counts as a watch tick on the
board, the same as `malak mailbox --plain --for lead --next --timeout 60`. A
lead that keeps calling `inbox` counts as watching. One that stops does not, and
`start_agent` then refuses with the lead-watcher message from `worker-start`.

## Starting agents

`start_agent` runs `claude -p` or `codex exec` with a short prompt and the brief
delivered through `worker-start --brief`. It adds no permission flags, so the
agent runs under its own tool's permission settings. An agent whose tool must
ask before running a command waits. To start an agent with any other command,
use `malak recipe worker-start` from a shell.
