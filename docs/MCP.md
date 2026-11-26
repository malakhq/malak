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
files exist, and `malak recipe agents --remove` takes it out.

A worker gets the MCP tools only when the `malak` server is in its own tool's
configuration, by `malak recipe agents` or by hand. `worker-start` and
`start_agent` do not add it. They set `MALAK_HANDOVER` to the worker's own
handover in the worker's environment, so a `malak mcp` server that the worker's
tool starts, and that receives that environment, serves the worker's Seat. A
worker whose tool has no such server has no MCP tools and uses the `malak`
command line, which every worker has.

Treat the handover like an SSH key. Use only the handover assigned to your
session. A handover bound to a session works from the agent that started the
server and from nothing else, and the board refuses it from any other process.
No tool returns a handover, a key or a token.

## When the board refuses the Seat

The board can refuse the handover at startup, for example when another session
holds it. `malak mcp` still completes the client's handshake and offers its
tools, and every tool then returns the board's reason as a tool error, so the
model and you see why. Some clients show only that a server is "failed" when it
exits, which is why the server stays up.

To see the reason by hand, run the command the client runs, with the same
`MALAK_HANDOVER` and board options and standard input closed:

```
MALAK_HANDOVER=/path/to/your/handover malak mcp --board work </dev/null
```

It prints the board's reason and exits with a failure. To fix a handover held by
another session, re-issue it with `malak enrol NAME --session PID`,
`malak recipe worker-start NAME --again` or `malak recipe lead-up`.

## Tools

Every Seat has these five.

| Tool | What it does |
|---|---|
| `inbox` | Read new messages in your inbox, with an optional `wait` in seconds. Messages this server process already returned are not repeated unless you pass `all`. |
| `ack` | Mark messages handled, up to a position. |
| `get` | Fetch one message by channel and position, or page a long body with `offset`. |
| `report` | Send a report to the lead. |
| `ask_owner` | Ask the owner one question, with options and their pros and cons, a recommendation and what it blocks. It runs `malak recipe ask`. The result says how the answer arrives. |

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

## The owner's answer

`ask_owner` posts the question on the board's `questions` channel and returns
at once. The answer comes back through `inbox`, as a note:

- A worker's answer arrives in its own inbox, as a note.
- The lead's answer arrives on the `questions` channel, which the lead's `inbox`
  reads along with its own inbox. Only replies to a question show there, not the
  questions themselves.

A reply is the owner's answer only when it is marked `owner_answer` (the text
shows `Owner answer yes`). The mark means the board proved the sender is the
lead, which is how `malak recipe answer` sends the owner's answer from the owner's
terminal. A reply from any other participant, a worker for example, still shows,
as a note from that participant under its own name, and is never the owner's
answer. The board refuses the lead's own handover from the owner's shell while
the lead agent holds it, so an owner who answers as another participant gets a
reply the model reads as a note, not as the owner's decision.

The owner answers with `malak recipe answer questions POSITION` from the
command line, the mailbox or a door with proof, the same as for any agent.
Acknowledge an answer as any note, with `ack` or `inbox` with `ack: true`.

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

An MCP lead must be a long-lived agent. The tools answer one call at a time and
send nothing on their own, so a lead that ends its turn stops watching and
sees no more reports. A one-shot `claude -p` lead does that: in the trial it
waited a few minutes, then ended its turn with a summary, and an owner answer
that came later found nobody. Run the lead in a session that keeps going, or
have it call `inbox` with a wait in a loop with a rule for when to stop.

A lead that exits and starts again loses nothing. The new server process
returns every message the lead had not acknowledged, tasks included, and then
only new ones. Acknowledge with `ack` after the work, as the instructions say.

## Starting agents

`start_agent` runs `claude -p` or `codex exec` with a short prompt and the brief
delivered through `worker-start --brief`. It adds no permission flags, so the
agent runs under its own tool's permission settings. An agent whose tool must
ask before running a command waits. To start an agent with any other command,
use `malak recipe worker-start` from a shell.

### A Claude agent needs permission to run `malak`

`claude -p` cannot show a permission prompt, and it ignores the permission
rules of a project's `.claude/settings.json` until that folder is trusted
(it prints `Ignoring ... permissions.allow entries ... this workspace has not
been trusted`). A Claude agent started without permission to run `malak`
has every `malak` call refused with "requires approval". It cannot ask the
owner or report, and says so only in its final text, which the launcher sends to
the lead in the exit report.

Give it both of these before the lead starts one:

- User-level permission for the `malak` command, in `~/.claude/settings.json`:

  ```json
  { "permissions": { "allow": ["Bash(malak:*)"] } }
  ```

- The folder the agent starts in trusted: open `claude` once in it and accept
  the trust prompt.

Malak does not detect a missing permission when it starts the agent, because
the rules live in the tool's own settings in several scopes. Codex agents need
nothing extra. A lead that finds a Claude agent exited with "requires
approval" in its report should ask the owner with `ask_owner`.
