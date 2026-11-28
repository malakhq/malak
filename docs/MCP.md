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
blockers, open calls (including default blue digest calls) and current briefs. A lead sees its own board; the top lead
sees every roster board through its reader seats, with an explicit row for
an unreadable board. Digest contents default to briefs, finished work, blockers and calls, configurable. A truncated result names the first omitted board.

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
| `answer` | `position`*, `answer` or `decision` | Answer what the lead knows, or record the top decision named by `decision` (four-part link). The linked note supplies the authoritative words and attribution. Writes the question record, the asker's inbox and decided/unblocked rail events. |

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

A proven answer from the asking board's lead records who decided it. Each level
answers what it knows. A lead proposes a call when it cannot answer; the top lead
sets its real level and decides what needs the owner. Topics are hints, with no
fixed list of owner-only topics. The owner speaks to the top lead.

The top lead uses `decide_call` for its own decision and
`relay_owner_decision` for the owner's actual words. Both write `Type: decision`
on the top board's `calls`. The asking lead records the returned link with
`answer` and `decision: ["top", "calls", "CREATION", POSITION]`. Readers validate
the sender, top board, source creation and exact question link, or a proven
merge; the linked note's text is displayed, never the lead's copy. Earlier
decisions remain visible and the latest proven top decision wins. An invalid or
relinked record stays text and leaves the call open. A sub has no top-board
reader seat; it trusts its own proven lead's record and sees the attribution,
source link and `source_checked_by: "lead"`. A reader that verifies through
its own Seat sees `source_checked_by: "self"`.

Without a top lead, the lead records the owner's words directly with
`Decided by: owner, recorded by lead, no top lead`. An ordinary answer from
the lead inside an organisation says `Decided by: lead`. The mailbox is read-only.
A bound lead handover works only inside its lead session; a separate owner
terminal cannot impersonate that session to send an answer.

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

## Calls tools

All links are `[board, channel, creation, position]`, never handover paths. Subs
have none of these tools; they use `ask`, which accepts no level or topic.

| Tool | Arguments | Role and behavior |
|---|---|---|
| `calls` | `level`, `all` | Top lead across the roster, or lead for its own board. Lists proposal, effective level, every original, all askers, and linked decisions. |
| `raise_call` | `position`, `level`, `topic` | Asking board's lead proposes a level and writes blocked on rail. |
| `call_level` | `question`, `level`, `topic` | Top lead sets the real level up or down, even without a lead proposal. |
| `call_merge` | `questions` (array of links) | Board lead for its own calls; top lead across boards. |
| `decide_call` | `question`, `body` | Top lead writes a decision attributed to top lead. |
| `relay_owner_decision` | `question`, `body` | Top lead relays the owner's words with explicit attribution. |

Example: `raise_call` with `{"position":1,"level":"red","topic":"release"}`;
then `call_level` with `{"question":["build","questions","CREATION",1],"level":"orange"}`.
The topic never grants authority. Record a returned decision with
`answer` and `{"position":1,"decision":["top","calls","CREATION",2]}`.

The defaults are red within one watcher tick, orange listed, blue digest only;
all are configurable using `calls.levels` and `calls.digest` (see
[the recipe guide](RECIPES.md#calls-settings)). Agent watchers continue once for
an open proposal or effective level configured with `delivery: tick`. Top notes
naming a lead's board reach that lead through its own top seat. The open-source
build has no push Door; the effective delivery hook is available for one.

Recorded decision views include `source_checked_by`: `"self"` when the reader
validated the top link through its own Seat, or `"lead"` when a sub trusts the
record from its own proven lead. Subs receive no reader Seat. `decided_by` and
`recorded_by` remain separate fields, and attribution headers are stripped from
the shown answer body. A record from any other or unproven sender stays text.
