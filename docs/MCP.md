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

## Leading over MCP

An agent that can run shell commands, such as Claude Code or Codex, leads a
local board over MCP by running `malak recipe lead-up BOARD` itself, then
calling the Malak tools. The agent's host starts `malak mcp` when the agent
starts, before the model can run `lead-up`, and that is fine: the server
attaches on the first tool call, and when `lead-up` issues a new handover
afterwards, the server reads it on the next call and attaches again. Nothing is
claimed for you. `lead-up`, `adopt` and `worker-start` still issue the claim,
and the board still refuses a session that does not hold it.

A shell-less chat app cannot lead a local board, because it cannot run
`lead-up`. It leads through Malak Cloud. It can still be a worker.

The board decides which session a server belongs to by walking up from the
server to the first program that is not a shell, an interpreter or a wrapper
such as `env`, `nohup` or `timeout`, and that program must be the one holding
the claim. Python counts as an interpreter whatever name the system gives it:
`python3`, `python3.12` and the macOS framework build, which Homebrew and Xcode
run as `Python`. A server that the agent starts through `timeout` therefore works.
On macOS the Homebrew `timeout` runs as `gtimeout`, which the board recognises
from this version on; a board that is still running an older build keeps
refusing it until you stop and start the board.

## When the board refuses the Seat

The server attaches when a tool is first called. If the board does not exist
yet, the handover has not been issued, or the board refuses the handover (for
example another session holds it), the tool returns the board's reason as a
tool error. The server stays up and the handshake succeeds, so the model and
you see why, and some clients do not show a server that exits only as "failed".
For a lead, the error also names the step: run `malak recipe lead-up BOARD`,
then call the tool again. The next call tries again.

If the board closes the attachment while the server runs, for instance because
`lead-up` issued the handover again, or the link to the board drops (the board
restarted, or a write fails with `Broken pipe`), the call that finds it reads the
handover file again, attaches once and runs once more. If the board refuses
that, the call returns the board's reason. It never retries a second time. When
the link is still down on the second try, the error says to check whether the
board is running, not just the operating system's error.

A call that sends a message (`report`, `delegate` and `brief_put`) sends it with a message
ID and uses the same ID on that one retry, so a send whose answer was lost is
stored once. For `report` and `delegate`, pass your own `id` to retry a call
yourself with the same effect: the reply to the second call has
`duplicate: true` and the position of the first. `brief_put` uses an internal
ID for the server's automatic retry.

To see the reason by hand, run the command the client runs, with the same
`MALAK_HANDOVER` and board options and standard input closed:

```
MALAK_HANDOVER=/path/to/your/handover malak mcp --board work </dev/null
```

It attaches once, prints the board's reason and exits with a failure. To fix a
handover held by another session, re-issue it with
`malak enrol NAME --session PID`, `malak recipe worker-start NAME --again` or
`malak recipe lead-up`.

## Tools

Leads also have `briefs(board, kind)`, `brief(board, position, offset)`,
`brief_view(since)` and `brief_put(title, kind, body, replaces)`. These use the
same read seats, projection and size budget as the CLI. `brief` returns
the original text after supersession or withdrawal, with its status and
replacement link. Follow its `fetch.offset` to page a long body.
`brief_view` includes a linked pulse bottom line, finished work, unresolved
blockers and current briefs. A lead sees its own board; the chief of staff
sees every roster board through its reader seats, with an explicit row for
an unreadable board. Its open calls hook states when calls are unavailable
in this build. A truncated result names the first omitted board.

Only the board's lead may put a brief. Subs get no brief tools and cannot
read the `briefs` channel; their lead puts needed text directly into their
task. A large result reports omitted items or body bytes and gives a
paging command. Both the text and structured data together fit 32 KiB.

Every Seat has these five. Arguments marked * are required. A wrong or extra
argument name is refused with an `invalid arguments` error.

| Tool | Arguments | What it does |
|---|---|---|
| `inbox` | `wait` (seconds), `limit`, `ack` (true or false), `all` (true or false) | Read new messages in your inbox, with an optional `wait` in seconds. Messages this server process already returned are not repeated unless you pass `all`. |
| `ack` | `channel`*, `position`*, `creation`* | Mark messages handled, up to a position. `creation` is the creation shown on the message. |
| `get` | `channel`*, `position`*, `offset` | Fetch one message by channel and position, or page a long body with `offset`. |
| `report` | `body`*, `id` | Send a report to the lead. Reuse the same `id` to retry a report that may not have arrived. |
| `ask` | `to` (`lead`, `owner` or `both`), `question`*, `options`*, `recommendation`*, `blocks`* | Ask one question, with options and their pros and cons, a recommendation and what it blocks. `to` says who it is for (see "Who a question is for"); it defaults to `lead` for a worker and `owner` for the lead. `options` is a list of objects, each with `option`, `pros` and `cons`, all required. It runs `malak recipe ask`. The result says how the answer arrives. `ask_owner` is the old name, still accepted, and defaults `to` to `owner`. |

The lead's Seat also has these worker and answer tools.

| Tool | Arguments | What it does |
|---|---|---|
| `delegate` | `agent`*, `task`*, `id` | Give a task to an agent already on the board. The agent sees an order from the lead. Reuse the same `id` to retry a task that may not have arrived. |
| `start_agent` | `name`*, `brief`*, `tool`* | Start a new agent with a brief. `name` follows the new participant name rule below; `tool` is `claude` or `codex`. It runs `malak recipe worker-start`. |
| `agents` | none | List the agents with their claim status and unread counts. It runs `malak recipe workers`. |
| `answer` | `position`*, `answer`* | Record the answer to the question at `position` on `questions`. It runs `malak recipe answer`. The answer says who decided it (see "The owner's answer"). |

A worker does not see the lead's tools, and a call to one is refused.

Owner-door actions are not offered: enrolling beyond `start_agent`, grants,
rekey, revoke, release, channels, trim, uninstall and upgrade. Neither are
release or publish actions.

## Who a question is for

Every question names its audience on a `For:` line after the question text:

- `lead`: the lead decides. This is the default for a worker's question. Use it
  for choices inside the task: a name, an approach, which file.
- `owner`: the owner decides. The lead's `inbox` shows the question as for the
  owner; the lead asks the owner and records the owner's reply. Use it for
  decisions that are the owner's: spending, publishing, scope, anything the
  brief says is not yours.
- `both`: the owner decides, and the lead sees it and may add a
  recommendation. Use it when the lead's view will help the owner.

The lead's `inbox` shows each worker's question with its audience (`audience`
in the structured reply; `Question for the owner` in the text). A question with
no `For:` line is an older one and reads as `owner`, which is what every
question meant before.

For now the lead is trusted as the owner's Door for every question: what the
lead records as the owner's reply is taken as the owner's decision. Stronger
proof for high-risk decisions (a release, spending, publishing) comes later.

## The owner's answer

`ask` posts the question on the board's `questions` channel and returns
at once. The answer comes back through `inbox`, as a note:

- A worker's answer arrives in its own inbox, as a note.
- The lead's answer arrives on the `questions` channel, which the lead's `inbox`
  reads along with its own inbox. Only replies to a question show there, not the
  questions themselves.

A reply is an answer only when the board proved the sender is the lead, which
is how `answer` and `malak recipe answer` send it. An answer says who decided
it: `decided_by` is `lead` for a question for the lead, and `owner` for a
question for the owner or both, with `recorded_by` `lead`. The text shows
`Answer decided by the owner, recorded by the lead` or `Answer decided by the
lead`, and `owner_answer` is true when the owner decided. An answer posted
under an order is the lead's decision. A reply from any other participant, a
worker for example, still shows, as a note from that participant under its own
name, and is never an answer.

The lead answers with `answer` (MCP) or `malak recipe answer questions
POSITION` (CLI), which post the reply as the lead. For a question for the
owner or both, the lead first asks the owner, then records the owner's reply.
Two things to know today:

- The `malak mailbox` screen does not answer questions. It is read-only. Its
  Questions tab and the pinned questions above it show each open question with
  its reply address, and `c` copies that address. The owner then runs
  `malak recipe answer` in a shell.
- The board refuses the lead's handover from any process outside the lead
  agent's own session, so a separate terminal cannot send the answer to a live
  MCP lead: `recipe answer` there fails with "the board closed the attachment".
  The command works when it runs inside the lead's session: the lead runs it
  for the owner, or the owner runs it from a shell escape in an interactive
  lead (not tested here). So the owner answers through the lead: the lead
  relays the question and records the reply with `answer`.

Acknowledge an answer as any note, with `ack` or `inbox` with `ack: true`.

## Orders and notes

Every message a tool returns names its sender and says whether it is an order.
`presentation` is `task` for an order, a task proven to come from the lead.
Everything else is `information`: a note, which is text someone sent you and
not an instruction. A message sent with kind `task` by anyone but the lead, for
example a worker writing to the lead, is `information` too (the text view says
so), and `inbox` with `ack: true` goes past it like any note. The text a model reads is the one `malak read --agent`
prints.

`inbox` acknowledges nothing unless you pass `ack: true`. That acknowledges the
notices and reports it returned in full, up to the first order from the lead. An
order is acknowledged with `ack` after the work, so a crash does not lose it.

A reply holds only as many messages as fit in a bounded size. When more are due,
the reply says how many did not fit and `held_back` is `true`. Call `inbox`
again for them; the ones already returned are not repeated.

A refusal comes back as a tool error that carries the board's reason.

Inbox addresses accept either `to/NAME` or `to/ID`. The board resolves an
active name before checking the caller's grant, and stores the message in
the ID inbox. For example, `delegate` with
`{"agent":"reviewer","task":"Review the change."}` and the same call with
the reviewer's ID reach the same inbox; `get` and `ack` accept
`"channel":"to/reviewer"`. An unknown address is refused with the nearest
match and creates no channel. After `worker-start reviewer --again`, the
name addresses the new enrolment; an old ID still identifies its history.
Suggestions include only inboxes the caller is granted to send, read, get
or acknowledge. Without an eligible inbox, the refusal contains only the
unknown address.
An exact retry of an older message with a literal inbox name in its reply
header returns the original receipt and leaves that record unchanged. New
writes canonicalize reply headers to ID addresses before storage.
`to/lead` addresses `lead`. New names use 1–24 lowercase letters, digits and
hyphens, start with a letter, and have no double or trailing hyphen. Existing
longer names, underscores and other old valid spellings keep working until
re-enrolled. Existing IDs take precedence over existing names with the same spelling.

Old literal `to/NAME` channels that are not participant inboxes remain in
storage and appear as stray channels in the mailbox, with retained message
counts. They accept no new messages. Existing grants still govern
`read`, `get` and `ack` when no participant resolves that address. An active
name shadows a stray, while its old records remain stored.
Owner `board status` selectors remain literal inventory names; human output
marks strays and shows the active name's resolved ID beside a shadowed stray.
Owner create, trim and remove accept inbox aliases. When trim or remove
omits `--creation`, the CLI discovers it from owner inventory; the board
resolves the operation again and rejects an alias that now names a different
creation. Name-based removal prints the resolved inbox ID and creation.

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
delivered through `worker-start --brief`. It adds no permission flags, and you
cannot pass any. The agent runs under its own tool's permission settings, plus
the one rule below for Claude. An agent whose tool must ask before running a
command waits. To start an agent with any other command, use
`malak recipe worker-start` from a shell.

### A Claude agent can run `malak` out of the box

`claude -p` cannot show a permission prompt, so a command it has no rule for is
refused with "requires approval". The launcher therefore gives a Claude agent,
in the same `--settings` that carries its hooks, a permission rule that allows
the `malak` command (`Bash(malak:*)`, and the same rule for the absolute path
of the `malak` that started it) and nothing else. It adds no other tool,
command or permission. `worker-start` does the same, and Codex agents are
unchanged.

Anything else a Claude agent needs, such as editing files (`Write`, `Edit`) or
other commands, still comes from the owner's own settings (for example
`~/.claude/settings.json`). An agent without that permission cannot do that
work and says so in its final text, which the launcher sends to the lead in the
exit report. The agent cannot run an unrelated command, such as `curl`, through
the added rule.

The rule only allows. If the owner's own settings deny `malak`, the deny still
wins and the agent's `malak` calls are refused; Malak does not override it.
Rules from the owner's settings and Malak's rule are added together, so an
owner who already allows `malak` sees no change.

Malak does not detect a missing permission for other tools when it starts the
agent, because the rules live in the tool's own settings in several scopes.
Codex agents need nothing extra. A lead that finds a Claude agent exited with
"requires approval" in its report should ask with `ask`, naming `to`.

## Organisation activity

Leads also get `rail(board?, since?)`, `rail_wait(on, subject, link?)` and
`rail_handover(to, subject)`. `pulse()` is available to the chief of staff;
calling it as an ordinary lead is refused with a reason. Subs cannot call
these tools. Read tools use the same client-side projections as the CLI,
through the caller's own primary seat or its organisation read seats. They
never open another lead's handover.

`get(channel, position, offset?, board?, creation?)` can fetch a linked fact on
another board. In the seat precedence, that read uses the caller's own recorded
reader seat for the named board. Subs cannot select it. The creation, when
given, must match before the message is returned. Foreign tasks are information;
the reply never acknowledges them. A truncated foreign body includes the board
and creation in its `fetch` arguments.

Rail and pulse return `items`, `omitted` and `next`, bounded to 32 KiB for the
whole result, including its text and structured content. `since` accepts an
age such as `7d` or epoch milliseconds. No events are pushed; clients poll.
The optional wait `link` is `[board, channel, creation, position]`; it identifies
the work that can close the dependency. Without it, only explicit `wait --done`
closes the wait. An unreadable linked peer leaves it open with a pulse reason.
