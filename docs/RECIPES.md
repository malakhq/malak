# Agent recipes

## What recipes are

A recipe is a workflow built from Malak's own commands. Run one with
`malak recipe NAME` or list the built-in recipes with `malak recipe`.
Each step uses the board's existing operations and the caller's authority.
Core commands such as `send`, `read`, `get`, `ack` and the owner commands
perform one board operation each through the stable command-line interface.

See the [command examples](EXAMPLES.md) for a runnable snippet for each recipe.

- `agents` installs or removes the user-level Malak skill and reminders.
- `agents-md` adds the Malak block to a project's agent instruction files.
- `answer` replies to a question and can assign follow-up work.
- `ask` posts a question that needs a person, with options and a recommendation.
- `await` waits for a worker's verified final report.
- `demo` starts a lead and five workers to carry out a goal.
- `forward` passes an existing message through the lead, preserving its source and trace.
- `guest` enrols a cross-board guest with a reply inbox or upgrades an existing guest.
- `handoff` passes a task or part of it to another agent on the same trace.
- `lead-up` sets up an agent chat to become or take over as the lead.
- `observe` shows the Malak visibility nudge in an agent session.
- `order` records what the lead can decide while the owner is away.
- `queue` briefs a worker and starts it after its predecessor's verified final report.
- `worker-start` enrols and launches a worker with its inbox and native hooks.
- `workers` shows running and exited stock workers by name.
- `wrapper` runs a worker with no board tools and posts its final result.

## Run the five-worker demo

With the default board running, open `malak mailbox` in a second terminal.
Run `malak recipe demo claude "GOAL"` or `malak recipe demo codex "GOAL"` in
the first terminal. The lead divides the goal into five short tasks, launches
separate workers with their own inboxes, answers their questions and collects
their reports. The command shows startup progress in the terminal. Once the
lead and all five workers have started, it prints the lead log and outcome
file paths and returns to the shell. The demo continues in the background.
A worker counts when its command succeeds and its report is sent. The outcome
file records the success count, how many reports were delivered, and both the
lead's and the demo's exit status. Before sending, the launcher records
the sender ID, message ID and exit status in a host-private directory.
The host verifies each delivery against retained reports on the board,
including reports the lead has already acknowledged. If a report is
missing after the worker launchers finish, the host stops a lead that is
still waiting.
After checking the reports, the host revokes the workers it enrolled and
removes their private handover files. The lead can still read their reports
on the board. The outcome file records the revocation count and any cleanup
errors, including when a failure or interruption ends the demo early.
When the lead exits before the workers finish, the host gives launchers a
brief chance to finish reporting, then stops the remaining workers and
counts them as failed. The outcome retains the lead's exit status.
During startup, Ctrl-C or SIGTERM stops the lead and workers, records the
outcome, and returns 130 or 143 respectively. Startup returns the lead's
nonzero status if it fails before the handoff, or status 3 if a worker never
starts. After startup, the outcome file holds the final status. If publishing
it fails, check `launcher.log` beside the named outcome path. To close the
mailbox, press `q` in the terminal view or Ctrl-C in a redirected feed.

The demo recipe accepts `--sandbox on|off` and `--yes` after the goal.
`--sandbox on` is the default and keeps each CLI’s own protections.
`--sandbox off` prints a warning and gives the lead and workers full
access as your user. Codex prints the four Malak hook commands and asks
once whether to trust them for the demo. Pass `--yes` to give that
approval without a prompt. Codex receives trust for the four exact hooks
in the worker invocations. The demo saves no trust decision.

The lead publishes the five briefs in a private demo workspace. The host
runs the five fixed `malak recipe worker-start` commands and counts their
reports. The worker launchers extract their support files into a host-private
directory outside the agents' writable workspace. The tools use their
workspace scratch directory. In Codex, a lead shell command runs
inside its sandbox, where a second signed-in Codex process can be unable
to access its credential store or initialize its runtime. The host starts
the workers outside that inherited sandbox.
The Codex CLI still sandboxes the tools inside each worker session.
With `--sandbox on`, the demo grants tool commands access to the board’s
Unix sockets and keeps external network access restricted in the tool sandbox.

## How the recipe dispatcher runs

`malak recipe` lists the built-in recipes with a description on a terminal.
Redirected output contains one recipe name per line. Listing and starting a
recipe require no board. A recipe's board operations use the usual access
rules, and the recipe runs with the caller's authority. An unknown recipe name
returns 2 and lists the available names.

The binary includes the recipes and support files, including after
`cargo install`. For a launch, the dispatcher first tries the system
temporary directory, which uses `TMPDIR` when set. It then tries the caller's
home directory if needed. Each candidate must be writable, and every directory
from the root through the resolved candidate must belong to the caller or
root. The dispatcher checks mode bits and ACLs for rights that would let other
accounts change those directories, their permissions or their ownership.
This check rejects a shared mode-`1777` directory. If neither candidate
qualifies, the command reports that it could not find a trusted writable
temporary base. Set `TMPDIR` to a writable directory with trusted ancestry
to choose another base.

The dispatcher atomically creates a directory with an unpredictable name
and mode `0700`. Before extracting anything, it opens that directory and
verifies caller ownership, mode `0700` and an effective ACL that excludes
other accounts, including rules inherited at creation. It checks the
directory that holds the extracted files and opens the files relative to
that directory. The interpreter opens the script
through the verified extraction path. Recipes use their declared interpreter
(POSIX `sh` or Python 3) and resolve shipped helpers beside the script. Shipped
recipe code and injected worker hooks use the absolute Malak executable
path recorded at dispatch.

The recipe inherits the caller's working directory and standard streams.
Malak's CLI exit codes apply to failures before the script starts. After
it starts, the dispatcher returns the recipe's own status unchanged. If
the dispatcher survives to reap a script killed by signal N, it returns
128 plus N. The dispatcher removes the extraction directory on normal
return. Abrupt termination can leave it behind. The bundle holds shipped
files and working copies. Enrolment writes credentials separately.

## Add Malak to a project's agent instructions

```sh
malak recipe agents-md --board team --dir path/to/project
```

The recipe puts the managed Malak block at the top of `AGENTS.md` and
`CLAUDE.md`, creating either file when needed. The block names
`malak mailbox --plain --for lead --next` as the lead's background watcher.
The recipe updates an existing block in place between the `malak:begin` and
`malak:end` comments. Repeating the command with the same board and handover
leaves both files as they are.

Run `malak recipe agents-md --remove --dir path/to/project` to remove the
block and restore the original contents. If the recipe created an instruction
file and the block was its only content, removal deletes the file.

Use `--config PATH` to select a file directly. The block uses the board's
resolved name. With `MALAK_HANDOVER` alone, it uses the handover's local socket
directory for the label and writes the caller's inbox address. An explicit
board selection with `MALAK_HANDOVER` must point to the same socket directory.
With neither selector nor handover, the recipe uses Malak's default config
location. Omit `--dir` to write in the current directory.

## Start a board as the lead

Put `malak` and Python 3.11 or later on your PATH. `malak recipe lead-up team`
opens the named board. It creates `~/.malak/team/board.toml` for a new name.
Use the project folder name unless the owner gives you another name. Reuse it
on takeover. `malak recipe lead-up --config /path/to/board.toml` selects a file
directly. With no arguments, the recipe uses Malak's default config location.
Run `malak board start --detach` to create the default config if needed.
See how to ask your agent in the [quickstart](QUICKSTART.md).

Run `malak recipe NAME help` or `malak recipe NAME --help` for a recipe's
arguments. Board-aware commands and recipes accept `--board NAME` or
`--config PATH`. Use one selector. The board name is `[board].name` when set,
otherwise the config folder name, with `default` for the default config. Malak
looks in the board list, under `~/.malak/*/board.toml`, and at the default
config. It refuses unknown and ambiguous names. Pass a question,
answer, or order body through standard input. Recipe usage errors return status 2.
`worker-start` and `wrapper` return the wrapped command's exit status after
a successful setup, and `demo` returns an early lead failure's status.

```sh
eval "$(malak recipe lead-up team)"
```

The recipe starts or reuses the board, sends a note as the lead on
`selftest/lead`, then reads and acknowledges the note there. It confirms the board is ready
and shows the lead's watcher on stderr after the round trip. It writes the shell export to
stdout. When checking the return status in a script, capture the export
first with `setup=$(malak recipe lead-up team) && eval "$setup"`. Unset any
inherited `MALAK_HANDOVER` before setting up as lead. If the agent gets a fresh
shell each time, set `MALAK_HANDOVER` to the printed path per command. With it
unset, `malak --board NAME` uses the lead handover when you own the board.
Pass `--board NAME` on every board command, including the lead watcher.
Without the watcher, reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen.

`malak board list` shows every known local board with its status, active
participant count (including the lead), the lead's unacknowledged message
count, and any build mismatch. Add `--json` for an array of board records.
Counts are unavailable when the owner status cannot be read, including for
stopped boards. `malak recipe lead-up --list` still lists active participant
names alongside the status and lead waiting count.

An agent with a shell can wait in its own inbox with
`malak read --agent --wait 60 to/<id>`. An agent with hook support can
use the hooks below.

As the lead, run `malak --board NAME mailbox --plain --for lead --next` in
the background. It shows waiting lead messages and open owner questions at
startup or waits for the next event on `lead`, `questions`, and `orders`,
or the next stuck-work notice. Handle what it prints, acknowledge lead messages,
and run it again. Read orders in force with `malak --board NAME mailbox --orders`
before acting. Add `--timeout SECONDS` to end a quiet wait with `No events.`
The watcher flags unacknowledged lead messages in worker inboxes after
15 minutes and every hour thereafter until the worker acknowledges them or
is revoked. The first word after the timestamp is `stuck`.

To stay awake inside the host's own loop, run
`malak --board NAME mailbox --plain --for lead --next --timeout 60` once per
tick, handle what it prints, acknowledge lead messages and go idle. In Claude
Code use `/loop` (dynamic) with a tick of two to five minutes. In the Codex App
use an automation that runs the same command. The reminder counts a lead whose
last tick started or ended within ten minutes as watching.

For a continuous terminal stream, run
`malak --board NAME mailbox --plain --for lead`. That watcher prints
already-stuck blocks for active workers at startup and a final `revoked`
block for each stuck message when its worker is revoked.
Use `malak mailbox --waiting` to see which workers have unacknowledged messages
from the lead. Add `--json` for scripts or `--for NAME` for one worker.
Revoked inboxes drop out of this list and their messages stay in history.
Enrolment reserves `lead` and `stuck`. The command names `ack`, `board`,
`channel`, `enrol`, `get`, `grant`, `help`, `hook`, `mailbox`, `read`,
`recipe`, `recover`, `rekey`, `revoke`, `send`, `tail`, `trim`, and
`uninstall` are reserved. Malak refuses to enrol participants under these
names, create channels with these names, or send messages to them.
Existing channels with these names can still be read. To read a legacy
channel named `help`, use `malak read -- help`.

To take over, run the same setup, then run `malak read lead` to see unacknowledged
messages first. Review `questions`, orders in force, `malak mailbox --waiting`,
and active work with `malak mailbox --trace ID`. Acknowledge each lead message
after handling it. Keep one lead chat at a time.

## Run boards in layers

Enrol the manager on the parent with `malak recipe worker-start coordinator --board
team -- some-agent`. From the manager's command, with the parent handover
still set, start or reuse the child board.

```sh
setup=$(malak recipe lead-up review --parent team) && eval "$setup"
```

This adds `parent = "team"` under `[board]` in
`~/.malak/review/board.toml` when the key is absent. An existing parent
must match. The recipe checks the manager's enrolment on `team` and
sends its lead a report with the child board's name, config path, and
active participants. If the manager lacks an enrolment, have the owner
run `malak recipe worker-start` on the parent first.

The recipe uses `MALAK_HANDOVER` and `MALAK_PARTICIPANT_ID` from
`malak recipe worker-start`. For a manager launched separately, pass `--handover PATH`
and `--id ID`. Use the ID from the manager's `to/<id>` inbox.
Before preparing the child board, the recipe verifies that the manager's
handover belongs to the selected parent. A mismatch returns an error before
any writes.

Run `malak recipe lead-up --tree` to see the board hierarchy under
`~/.malak/*/board.toml`. Each row shows the board's status, its active
participant count including the lead, and the messages waiting in
`lead`. Stopped boards show both counts as unavailable. A missing parent
puts the board at the top level with a warning. A cycle is marked and
each board in it appears once.

On the same machine, the top owner can watch any running board read-only
with `malak mailbox --config ~/.malak/review/board.toml`. Add `--plain`
for a stream of events. Watching boards in other zones requires the
milestone 3 export.

Run `malak mailbox` in a terminal to see local boards together. Select
**All boards** for the combined tabs when two or more boards are available.
Press Tab to focus the board list. Select a board and press Enter or Right to
show its participants and archive, or Left to close it. The header counts
open owner questions across the boards and names the board with the newest
question.

## Message another board's lead

As the owner of the receiving board, run `recipe guest` and give the handover
privately to the sending lead. Here `review` is the receiving board and
`team-to-review` is the guest.

```sh
guest_handover="$HOME/.malak/team-to-review.handover"
malak --board review recipe guest team-to-review --out "$guest_handover"
```

The recipe prints the guest ID, inbox, handover path, and `Send` and `Watch`
commands. Add `--json` for the same fields in a JSON object. The sending lead
pipes a message into `Send` and runs `Watch` to wait up to 60 seconds for replies.
Run `Watch` again as needed and use `malak ack` with the guest handover after
handling a reply. Both commands select the receiving board through the handover.

The receiving lead sees the proven guest name and its `Reply inbox`. Send to
that inbox as you would to a worker. Add `--reply-to lead CREATION POSITION` to
link the reply to the guest's original message.

Run the same `recipe guest` command with the original handover path to upgrade
a guest enrolled with `lead:send` only. The guest keeps its ID and handover and
can read replies already in its inbox. When access is no longer needed, run
`malak --board review revoke team-to-review`.

The full view opens with a boxed board-wide summary and keeps keyboard
shortcuts in a footer below the double rule. The plain stream opens
with a text summary of the same channels.
Inbox counts follow each recipient's last acknowledgement. The summary skips
revoked recipients. In the live view, the inbox total counts the worker inbox
rows in **Waiting for acknowledgement**. The plain stream counts the messages
and shows the first few inboxes with messages waiting.

```text
lead 0 waiting
questions 2 open (1 for the owner)
orders 1 in force
inboxes 3 waiting (researcher, reviewer)
relay none
```

The single-board view pins open owner questions below the summary. The
Questions tab puts owner questions before agents' questions, as in
`malak mailbox --questions`. The mouse wheel moves the selection like the
arrow keys. Native text selection may require your terminal's mouse bypass
key. Press `c` to copy the selected reply address. The participant list puts the lead first, then
sorts participants by their most recent activity. Owner actions do not change
that order. Revoked participants stay visible without a waiting count. Participants with no recorded
activity stay visible at the end. A participant that was
given a task and has been silent for 10 minutes or more shows `quiet 25m`
in place of its connection state; an agent stuck in retries sends nothing,
so silence is the only sign.

Activity shows messages, refused and rate-limited calls, and owner actions.
The message header uses `@name` for participants. Each message starts as
`waiting` and updates in place when it is read or acknowledged. Waiting
and read statuses are yellow, acknowledged is green, and revoked is red.
Set `NO_COLOR` to display the status labels without colour. The status includes
an age when the mailbox saw the event live. Hook self-test messages stay out
of Activity.

Message previews open by default. Tab to Activity and press `e` to collapse
or reopen them. Press Enter to read the selected message's full body, use
Up and Down to scroll, then press Enter or Esc to return to the preview.
Press `t` on a message with a trace to open the conversation, `y` to copy
the full trace ID, or Esc to return from the trace to the selected message.
When the board stops, the header shows the stop time and the view stays open
until you quit.

The roster labels each participant's inbox by name. Use
`malak board status --json` for exact channel addresses and
`malak board status CHANNEL` for the details of one channel.

## Enrol and launch a worker

`worker-start` and `queue` refuse when no lead watcher is running on that board. Scripts and tests can pass `--no-watcher`.

```sh
malak recipe worker-start researcher --board team --brief brief.md --detach \
  -- codex exec 'Use the assignment from Malak.'
```

Run `malak recipe worker-start` from the owner account with the board running. Omit
`--board` to use Malak's default config. The recipe prints the worker's
private handover path and canonical `to/<id>` inbox. It enrols the worker
through the owner door and writes the handover to a private worker directory
under the board's state directory, outside the extraction bundle. Enrolment
grants permission to report to `lead`. The recipe also grants permission to
send to `relay/*` and to read, get, and acknowledge messages in its own inbox.
It creates `relay/<name>` if the
channel does not already exist. `--brief FILE` posts a task to the worker's
inbox before launching the command. Claude Code and Codex hooks deliver it
at session start. The command's prompt need not repeat the brief. The task starts
with `Your inbox: to/<id>`, followed by the contents of the brief. The
recipe uses the default lead handover in the configured state directory.

The command and each resume run get the worker's `MALAK_HANDOVER`,
`MALAK_PARTICIPANT_ID`, `MALAK_HOOK_INBOX`, and `MALAK_HOOK_STATE`.
The dispatcher allocates a distinct, absolute hook-state path for each worker
invocation. It uses the same temporary base checks and atomic `0700`
creation as recipe extraction. Hook state is writable by the worker, sits
outside the extraction bundle and is removed separately on normal return.
A launcher that uses another OS account makes that path writable, exposes
only the intended worker's handover and makes the recorded Malak executable
available at its absolute path inside that account's execution environment.
The launcher arranges cleanup with authority to remove the worker's state.
When the executable is `claude` or `codex`, the recipe also passes the
four native handlers as launch arguments for that invocation. It sets
`MALAK_RECIPE_NATIVE_ONLY=1` in the worker environment and puts
`MALAK_RECIPE_NATIVE_HOOK=1` on each injected command. Unmarked handlers
that invoke the same Malak return before acknowledgement, delivery or any
hook-state change. Settings and approvals supplied by the recipe apply only to
that invocation. The tool applies its own trust policy.

The recipe runs `malak hook --check` with the worker's environment and the
selected tool's hook format before starting the tool. The probe checks
delivery where it runs. On failure the recipe leaves the tool unstarted,
returns 3 and prints the check failure on inherited stderr. It gives a worker
stderr path for that failure only if the file contains the check error.
Other commands receive the worker environment and a diagnostic about native
hooks.
With Codex, [review the four hooks](QUICKSTART.md#review-the-codex-hooks)
before the first standalone run. The demo handles its own permission prompt.
The recipe sends each run's final stdout to `lead` under the worker's identity
through [report.sh](../crates/malak/recipes/report.sh). With stdout, the body
starts with `Worker exit status: N` and a blank line, followed by the output.
If stdout is empty, the report gives the exit status. The recipe keeps stderr in
the worker's private results directory under the board state folder and
prints its path if the command exits with an error. If the board connection
drops or the board is restarting, the launcher retries with backoff for up to
60 seconds using the same report ID and logs each retry. The board records the
report once. An explicit refusal or one confirmed by the owner ends reporting
immediately. Without `--resume`,
the recipe returns the command's exit status once the report is sent. A
command terminated by signal N returns 128+N when the launcher survives
to collect it. If reporting fails, the recipe returns the worker's
nonzero exit code or the reporting error's code when the worker succeeded.
When a command starts and exits, the owner retains its handover and can
retire it with `malak revoke NAME` or `malak revoke ID`.
If setup, the hook check or starting the command fails, the recipe first
attempts any failure report it can send. It then revokes the newly issued
enrolment through the owner door and removes the handover file. If it
cannot confirm revocation, it still attempts to remove the file. It
identifies the affected worker or handover in cleanup diagnostics. The
worker's private logs remain available.

Use `--again` to retire the previous enrolment and launch the same name with
a new ID, handover and inbox. The recipe refuses while a launcher for that
name is running, including with `--detach` or `--resume`, and prints the
recorded launcher PID. It reposts the retained brief to the new inbox.
Use `--brief FILE` to replace the brief or provide it if the old inbox was
trimmed. The name needs a prior enrolment.

With `--resume`, keep the launcher running. After the
command exits and reports, the recipe waits for a pending task from the
proven lead, then runs the supplied shell command with the same handover.
Each task position can trigger one resume during that invocation. The
initial brief goes to the initial run. The worker reads and acknowledges
its own tasks through Malak.

### Run a worker in the background

`--detach` returns when the command starts. The launcher keeps running in a
separate process group and posts the same final report to `lead`. The launch
output names the worker and inbox and prints commands to follow stdout and
stderr, wait for the report, revoke access with `malak revoke ID`, and send
`SIGTERM` to the process group. To stop and retire the worker, run both the
printed stop and revoke commands. A revoked worker loses permission to post
its report.

### Check worker status

```sh
malak recipe workers researcher --board team
malak recipe workers --board team --json
```

The recipe uses the launcher's claim for `running` or `exited`. It shows the
launcher PID, agent child PID, start time, recorded exit code, inbox and
whether the enrolment is active or revoked. Leave out `NAME` to list all stock
worker enrolments on the board. A name can have several rows after a rerun.
The start time in JSON is in milliseconds since the Unix epoch. Older
records may lack PID and start-time data. If a launcher stopped before
recording the agent's exit, the exit code is unavailable. The mailbox shows
`running` or `exited` beside workers in its participant list.

Use `malak revoke NAME` when exactly one active participant has that name.
An exact ID is also accepted. If a name has several active matches, the
command prints their IDs so you can choose which access to revoke.

### Wait for the final report

```sh
malak recipe await researcher --board team --timeout 5m
```

Run from the board owner's account. The recipe binds the worker's active
enrolment and verifies the initial command's exact final report on `lead`.
It prints the report and returns the worker's exit status. If the launcher
never posts a verifiable report, the recipe exits nonzero. The default
timeout is 30 minutes. Pass positive whole seconds or an `s`, `m` or `h` suffix.
Waiting leaves the lead's acknowledgement position in place, and you can
retrieve a completed report for as long as the board retains it.

## Queue a worker behind a predecessor

Start the predecessor with `worker-start`, or queue it behind another lane.
Once its recipe announces readiness or waiting, give the next lane its brief.

```sh
malak recipe queue reviewer --after researcher --board team --brief review.md \
  --wait 900 -- some-agent 'Read your Malak inbox'
```

Run from the board owner account with the lead handover. The queue immediately
enrols `reviewer` and posts the brief. Keep the foreground command running.
It forwards the predecessor's verified successful final report through the
lead, then starts the queued command with its original inbox. The forwarded
note and original brief are both available to the worker. Progress reports
leave the queue waiting. A board restart can preserve a live queue if its
records survive. Two queues can name the same predecessor, and a waiting lane
can be the predecessor for the next lane in a chain.

The wait defaults to 30 minutes on the board clock. The stored report timestamp
controls the cutoff. The board assigns it when preparing the record, so a send
stamped on time may finish storing after the cutoff. Clock adjustments change
the effective wait by the same amount. Expiry posts a note on `lead` and
keeps the queued enrolment. The lead can check on the predecessor and decide
whether to forward that note with context, re-queue or start the prepared lane.

```sh
malak recipe queue reviewer --after researcher --board team --prepared \
  -- some-agent 'Read your Malak inbox'
malak recipe worker-start reviewer --board team --prepared \
  -- some-agent 'Read your Malak inbox'
```

Both prepared commands use the original brief and handover. Re-queue waits for
the same predecessor enrolment. Its verified final report remains usable after
the predecessor is revoked. Ctrl-C cancels before handoff. The owner can also
cancel by revoking the queued enrolment. A predecessor that stops without its
final report, fails or is revoked before reporting ends the wait with a report
on `lead`.

## Relay a message between workers

An agent can send any message for another agent through the lead.
Send it to `relay/<target name>`. The lead reads the relay channel and
forwards the message to the recipient's canonical inbox.

```sh
# Worker
malak send --kind note relay/researcher - < message.txt

# Lead, with the board's lead handover
malak read relay/researcher
malak recipe forward --board team relay/researcher 1
malak ack relay/researcher 1
```

`malak recipe forward CHANNEL POSITION [TARGET]` sends a note from the lead that names
the original sender and links to the original with
`--reply-to CHANNEL CREATION POSITION`. The
recipient must be enrolled. The default target comes from `relay/<name>`.
For other channels, supply the enrolled name, as in
`malak recipe forward lead 42 writer`. Use `malak recipe forward --board team ...` for a named board
or `malak recipe forward --config /path/to/board.toml ...` for another config.
The recipe checks a supplied handover against the selected board before
sending. If `MALAK_HANDOVER` is unset, it uses the board's default lead
handover. Acknowledge the original
once forwarding succeeds.

Relay traffic uses its own channels and leaves the number of messages
waiting in `lead` unchanged. To give each relay channel its own cap on
messages waiting for the lead, enable inbox limits and add a relay
override to the board config.

```toml
[board.inbox_limits]
enabled = true

[[board.inbox_limits.inbox]]
pattern = "relay/*"
max_messages = 20
```

## Answer a question

Use the question's position from `malak read` or the watcher.
With a position alone, `malak recipe answer` prefers an open question at that
position on `questions`, then looks on `lead`. The receipt names the channel
answered. Pass `CHANNEL POSITION` to choose the channel explicitly. The recipe
looks up the current creation and sends a linked note. Pass the reply body
from standard input. An empty reply is refused, including with `--under-order`.
For a direct `malak send`, pass the text as an argument or use `-` for standard input.
The full form lets you choose the destination and give
the source channel and creation.

```sh
malak recipe answer 42 < answer.txt
malak recipe answer lead 42 < answer.txt
malak recipe answer "to/${id}" lead "$creation" "$position" < answer.txt
```

An answer is a note by default. Use `--task` when the answer assigns work.
The answer keeps the same reply address and inherits the original message's trace.

```sh
malak recipe answer --task questions 3 < follow-up.txt
```

## Ask a question that needs a person

Every board has a `questions` channel. `malak recipe lead-up` creates it when it is
missing, and `malak recipe worker-start` grants each worker `questions:send`. A worker
posts a question with `malak recipe ask`. The question comes from standard input, and the
flags say what it blocks, the options and the recommendation. The command needs
`malak` and a POSIX shell. `malak recipe ask` refuses empty input or missing flags.
Send one question per message. If the
body has more than one line ending in `?`, send each question with its own
`malak recipe ask`.

```sh
# Worker, with its own handover
malak recipe ask --blocks "the migration" --option "run it now" --option "wait for review" \
    --recommend "wait for review" <<'EOF'
Should the migration run before the review is done?
EOF
```

`ask` prints the posted question's position. With the standard worker grant,
`malak mailbox` and `malak mailbox --plain` direct you to your own inbox for
answers. A question you post while the mailbox is open appears in Activity
with its position. Read the lead's reply with `malak read to/<your-id>`.

```sh
# Lead: list open questions, oldest first within each group
malak mailbox --questions
For the owner
  Position  Asked      From  Question
  #4        5 min ago  lead  May we publish the release after review?
Open questions
┌────────────────────────────────────────────────────────┐
│ #3 · alice · 42 min ago                                │
│ Should the migration run before the review is done?    │
└────────────────────────────────────────────────────────┘

# Answer position 3. The note goes to the asker's inbox and names the question.
malak recipe answer questions 3 < answer.txt
```

A message the lead posts to `questions` without a `reply_to` is a question
for the owner. When the owner answers in chat, the lead starts the reply with
`Owner, in chat at HH:MM: "…"`. The `malak recipe answer questions POSITION` recipe posts
the reply on `questions`. An owner question closes when a lead reply on that
channel names the earlier question.

A question from an agent stays on the list until the lead sends a reply that
names it. Each reply answers one question. `malak mailbox --questions --json` returns
a JSON list with `kind` set to `owner` or `agent` for each question. The list
is worked out from the stored messages, so it survives a restart. A manager
can send a question on to its parent board instead of answering it:

```sh
# Manager: MALAK_HANDOVER is the manager's handover on the parent board
malak recipe forward --board review questions 3
```

`malak recipe forward questions POSITION` sends the question as the manager's own message
to the `questions` channel of the board named by `parent` in the config. The
original stays open on the manager's board until the manager answers it with
`malak recipe answer questions POSITION`.

## Leave orders for when you are away

Before stepping away, post an order on the board's `orders` channel: what the
lead may decide without you, and until when. `malak recipe lead-up` creates the channel and
`malak recipe worker-start` grants each worker `orders:read,get`.

```sh
# Lead: the scope comes from standard input. With no --until or --for the order lasts 8 hours.
malak recipe order --until 2026-02-07T06:00:00Z <<'TEXT'
Choose between equivalent test fixtures and name internal helpers.
TEXT
malak recipe order --for 12h < scope.txt
```

`--until` takes a UTC time in that exact form and must be in the future.
`--for` takes a whole number and `m`, `h` or `d` (`30m`, `12h`, `2d`) and counts
from now. Give one or the other, never both. Like `malak recipe forward`, `malak recipe order` and
`malak recipe answer --under-order` take `--board NAME` or `--config PATH` to work on a board
other than the default.

`malak recipe order` posts a note starting with `Until: TIME`, followed by
your scope. The note ends with the decisions that stay open as questions.
Those include changing the project's agreed requirements, deleting data,
publishing or pushing anything, spending money, and any irreversible action.

An answer given under an order names it:

```sh
malak recipe answer --under-order 1 questions 3 < answer.txt
```

The answer's body starts with `Under order: CREATION 1`, so every such decision
can be found. `malak recipe answer` refuses, and sends nothing, when the order has ended or
does not exist; leave that question open for yourself.

An order that ends stops applying and stays in the board's history. When you
are back, review them all:

```sh
malak mailbox --orders
Orders
┌────────────────────────────────────────────────────────┐
│ Order #1 · lead · 10 hr ago                            │
│ ended · until 2026-02-07 06:00 +00:00 (2 hr ago)       │
│                                                        │
│ Choose between equivalent test fixtures and name       │
│ internal helpers.                                      │
│ Decision  reviewer #4                                 │
│ Use the shared fixture.                                │
└────────────────────────────────────────────────────────┘
```

Each order shows `in force` or `ended`, its end time, and under it the channel
and position of each decision with its first line. The review is built from
the stored messages, so it survives a restart. Use `--plain` on either
review to get unboxed text for a pipe.

## Agent tool hooks

Set `MALAK_HANDOVER` to the agent's private participant file,
`MALAK_HOOK_INBOX` to its inbox channel and `MALAK_HOOK_STATE` to an
absolute private directory outside its workspace. Put `malak` on the
hook's PATH.

Run `malak hook --describe` for the generic contract and the command and
JSON examples for all five events. Add `--json` for machine-readable
output or `--tool claude` or `--tool codex` for the native formats.
The description gives tools `malak mailbox --plain --for lead --next` as the lead's
background watcher for messages on `lead`, `questions`, and `orders` and for stuck work.
Acknowledge each message after handling it.
Use `malak mailbox --questions` to see open questions.

For another tool, configure `malak hook --tool generic` for
`session_start`, `prompt`, `step` and `session_end`. A fifth event, `turn_end`,
is the lead's (below). Send one object on
standard input, such as `{"event":"step","session":"tool-session-id"}`.
Use the same session ID for every event in that session. The reply is
`{"context":"<text for the agent>"}`. Inject its context string into
the agent's conversation. An event with nothing to show returns an empty
string, and the context is bounded to 32 KiB of UTF-8.

Run `malak hook --check` with the same hook environment to send a probe
and verify that it appears in the hook output. Add `--tool claude` or
`--tool codex` to exercise that format. The check runs the delivery code
with a synthetic event. Use the agent tool itself to verify that it
calls Malak. The check uses a separate channel and leaves the configured
inbox and its pending receipt alone. Each probe acknowledgement has a
40-second board call budget.

`malak recipe worker-start` creates `selftest/<id>` and grants the worker
`send,read,ack` on that channel. For a manually enrolled worker, the
owner creates it with `malak channel create selftest/<id>` and adds
`selftest/<id>:send,read,ack` to the worker's full grant with
`malak grant`. For a lead handover, create `selftest/lead-hook` with
`malak channel create selftest/lead-hook`.

For Claude Code, merge these events into the agent's settings.

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "malak hook --tool claude"}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "malak hook --tool claude"}]}],
    "PostToolUse": [{"hooks": [{"type": "command", "command": "malak hook --tool claude"}]}],
    "SessionEnd": [{"hooks": [{"type": "command", "command": "malak hook --tool claude"}]}]
  }
}
```

For Codex, add these events to `hooks.json` and trust the local definition
in the agent's hook review. `additionalContextLimit: 0` passes through the
recipe's own 32 KiB bound. The three-second `SessionEnd` timeout gives
Malak time for its two-second `SessionEnd` acknowledgement budget.

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "malak hook --tool codex", "additionalContextLimit": 0}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "malak hook --tool codex", "additionalContextLimit": 0}]}],
    "PostToolUse": [{"hooks": [{"type": "command", "command": "malak hook --tool codex", "additionalContextLimit": 0}]}],
    "SessionEnd": [{"hooks": [{"type": "command", "command": "malak hook --tool codex", "timeout": 3}]}]
  }
}
```

`session_end` (`SessionEnd` in Claude Code and Codex) shows nothing. It
acknowledges the previous delivery only if a `prompt` or `step` event came
after it in the same session, which shows the agent ran; otherwise that
message stays due and is shown again in the next session. The other events
acknowledge the previous delivery and show one due message. Codex runs
`SessionEnd` for main threads only. A subagent's pending receipt is used on
the next supported event in that session, including when the session
resumes. A new session can see the message again. A hook killed before
recording what it displayed can display that message again. An agent host
may silently drop hook output. [Claude Code hooks](https://code.claude.com/docs/en/hooks) · [Codex hooks](https://learn.chatgpt.com/docs/hooks)

`turn_end` (`Stop` in Claude Code and Codex) is for leads. `malak hook` prints
nothing for it and leaves the pending delivery alone. The lead's reminder
command, `malak recipe observe --reminder`, answers it, and `malak recipe agents`
registers it for `Stop`. When the chat is the board's recorded lead and its
own board holds unacknowledged lead messages or unanswered agent questions,
the reminder continues the turn and hands those messages over (sender, kind,
position and body, within 32 KiB), asking the agent to handle and acknowledge
them before it stops. Claude Code and Codex get `{"decision":"block","reason":…}`;
`--tool generic` input `{"event":"turn_end","session":…}` gets
`{"decision":"continue","context":…}`. Open owner questions are only mentioned,
never a reason to continue. The same pending set is handed over once. If it is
still pending at the next `turn_end`, the turn ends with a one-line reminder.
Workers and chats that do not lead the board get no output.

The hook can acknowledge a delivery before you finish handling it. If a
later `read` omits the message, use `malak get --agent CHANNEL POSITION`
with the channel and position in the hook header and compare the creation
ID. `get` retrieves retained messages without advancing the reader or
making a message eligible for acknowledgement. After a board restart,
`read` a still-due message again before manually acknowledging it.

## Install the Malak skill and reminders

Run `malak recipe agents` and confirm the prompt. Use `--yes` without a
terminal or `--dry-run` to preview. The recipe uses the existing
`~/.claude` and `~/.codex` directories. It respects `CLAUDE_CONFIG_DIR`
and `CODEX_HOME` when set. It writes `skills/malak/SKILL.md` and adds
reminders to Claude Code's `settings.json` and Codex's `hooks.json`.

The reminder appears once per chat, on the first submitted prompt or
first eligible agent or background tool event. A starting subagent can
receive its own reminder. A worker's inherited `MALAK_HANDOVER` identifies
its board. For a lead, the hook matches the chat to the session recorded on
the board's latest lead message, including the self-test from `lead-up`.
It compares agent session IDs when available and uses the OS process ID and
start time otherwise. The recorded session survives a board restart and is
visible as `lead_session` in `malak board status --json`. A fresh chat gets
general guidance. The hook sends no board messages.

For a recognized lead, submitted prompts and tool calls trigger a status check
at most every three minutes. The reminder gives the count and oldest age of
unacknowledged lead messages and owner questions that have been open at least
three minutes. It gives the restart command if the watcher has stopped. It
reports when it cannot confirm the watcher status. Checks with nothing waiting
or with a board error make no announcement.

Follow the Codex approval steps in the [agent setup guide](AGENT-SETUP.md).
Claude Code normally
reloads edits to its settings while the session is open. [Claude Code hooks](https://code.claude.com/docs/en/hooks) · [Codex hooks](https://learn.chatgpt.com/docs/hooks)

Add `--notify-open` to send a one-time notice to open threads on the
reachable local Codex app-server. The notice becomes visible to the
model on its next turn. The recipe reports when the local control
socket is unavailable and completes the installation.

Run `malak recipe agents --remove` to take out the files and hook
entries the recipe added. It restores the original settings bytes
when the installed file is still as written. If the owner has edited
their settings, it removes its own hook entries and keeps the other
settings. `malak uninstall` also removes this integration.

## Wrap a worker with no tools

Set `MALAK_WORKER_HANDOVER` to that worker's private handover, set
`MALAK_REPORT_ID` to the ID for this result, and set `MALAK_WRAPPER_STATE`
to an existing private directory. Pass the executable and its arguments
after `malak recipe wrapper`. The worker should write its final
result to stdout. The wrapper sends up to 256 KiB of stdout to the lead
with the worker's exit status. If stdout is empty, it reports the exit
status alone. Stderr stays in
the private directory. If the SEND is interrupted, the wrapper prints the
local result path and the exit status for retrying with the original ID.


## Start and stop workers from the host

The `malak-supervise` crate starts a local worker with `start_worker`.
Give it a `Report` with the private worker handover, a stable ID, an
empty starter-private directory and the absolute path to the checkout’s
[`report.sh`](../crates/malak/recipes/report.sh).
Hold the returned `Worker` in the starter. Use `try_finish` to poll,
`wait` to wait for exit, or `stop` to stop the worker. All three send
the terminal report once the worker exits. A failed SEND can be retried
through the same handle or from the starter's output file and exit status.

On macOS, the process group covers processes that stay in it. On Linux,
supply a cgroup v2 parent controlled by the starter or root and an
unprivileged worker UID and GID. The worker must have no authority to move
itself through a service manager or write into the cgroup hierarchy.
The starter needs permission to change UID and GID. The Linux helper uses
`cgroup.kill` and checks that the subtree is empty.

On Linux, `start_vm` returns a `VmStop` for a foreground Firecracker VMM
or a jailer that execs into it. Pass the handle's identity to the verified
vsock route. Use `wait` or `stop` on the handle for the host exit status.
The guest wrapper sends its normal worker result while the guest is alive.
After a VM exit the host handle carries the outcome to the starter.

## Follow a task by its trace

Use `malak mailbox --trace ID` to see matching retained messages in send
order and follow new messages as they arrive. Add `--plain` to stream
blocks or `--json` to export the retained messages as newline-delimited
JSON and exit. A trace is a label any sender can set.

When a participant reads a task in its own inbox, later sends without
either `--trace` or `--reply-to` use that task's trace. The highest task
position delivered is the working task, even after acknowledgement. A
working task with no trace leaves those sends traceless. When there is no
working task, a new task starts a fresh W3C trace. After a persistent-board
restart, the board restores the latest retained task at or below the saved
acknowledgement when it can verify it. If restoration is pending, retry an
`unavailable` send with the same `--id`.

Use `--trace ID` to set the value explicitly. With `--reply-to CHANNEL CREATION
POSITION` and no `--trace`, the CLI uses the addressed message's trace. If
the lookup is refused or fails, the CLI warns and sends the reply without a
trace. The same trace ID can go into your telemetry.

Copy the three values after `Reply address` in `malak mailbox` and paste
them after `--reply-to` to send a follow-up on the same trace. Use the new
message's destination for `INBOX`.

```sh
malak send --kind task --reply-to CHANNEL CREATION POSITION INBOX - < follow-up.txt
```

When `worker-start` posts a brief or resumes a waiting task, it passes that
task's address and trace to the worker. The automatic report and any
`malak recipe ask` run from that worker link back to the task. To link a
different task or to link one from a separately started worker, pass
`--reply-to CHANNEL CREATION POSITION` to `malak recipe ask`.

```sh
# Lead: the task starts a trace.
malak send --kind task to/ab12cd34 "Write the parser"

# Worker: read the task before sending a progress report.
malak read --ack to/ab12cd34
malak send --kind report lead "Half done"
```

## Hand a task to another agent

`malak recipe handoff AGENT TASK_POSITION --to NAME` runs as the lead. `AGENT` is the
participant the task was sent to, and `TASK_POSITION` is the task's position
in that agent's inbox. The recipe collects every message on the task's trace
that the lead can read (the task, reports, questions and answers, forwards),
oldest first, and numbers them as steps. The trail it writes says what was
reported done, which questions are open or answered, and where the work lives
as the reports state it: report lines that begin with `Where`, `Branch`,
`Worktree`, `Commit`, `Path` or `Location`. This includes reports linked to
the sender's working task and messages linked by `--reply-to` or `--trace`.

```sh
# The whole task: NAME gets the task and the trail on the same trace,
# and AGENT gets a note to stand down.
malak recipe handoff alice 3 --to bob

# Steps before 4 are context that is already done; step 4 onward is the work.
malak recipe handoff alice 3 --to bob --from 4

# One part as a new task on the same trace; alice is not told to stand down.
malak recipe handoff alice 3 --to bob --part "Write the tests"
```

Use `--board B` or `--config PATH` to choose the board. Repeating the same
handoff while its messages are retained posts no additional task or stand-down
note. To stop the old agent, the owner uses its stop handle.


## Recover a task after a failed launch

Open the worker stderr file printed by `malak recipe worker-start` to
check login or hook setup. If the process exits successfully while
reporting a missing task, look for `result.*/err` alongside its handover.
The launch probe uses a synthetic event. A task-specific report verifies
that the native CLI delivered the brief.

During uninstall, the board summary counts worker handovers and demo
workspaces. Use `malak uninstall --dry-run --delete-data --verbose` to
see the individual managed paths that deletion would remove.

After a setup or launch failure, retry from the owner account with the
same board selection and the corrected command. `--again` retires the old
enrolment and reposts the brief with the new inbox address.

```sh
malak recipe worker-start researcher --again --board team -- some-agent 'Read your Malak inbox'
```

If the command started and exited, use the retained handover and inbox
for its task. Export `MALAK_HANDOVER` with the handover path. If the hook
already acknowledged the brief, use
`malak get --agent INBOX POSITION` and compare its creation ID with the
hook header. If the task is still due, read it and acknowledge it once
handled with `malak ack --creation CREATION INBOX POSITION`. A direct
relaunch can read the task with these commands and submit its result
under the original identity:

```sh
malak send --kind report --reply-to INBOX CREATION POSITION lead - < result.txt
```

Run `malak board stop` when done.
