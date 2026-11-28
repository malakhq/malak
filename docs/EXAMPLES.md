# Malak command examples

Use `malak COMMAND help` or `malak COMMAND --help` to see the command's
arguments and example. For a nested command, use `malak help board start`.

Run each example from a fresh scratch directory with a disposable default
board. Put Malak and Python 3.11 or later on your PATH, then open an
isolated shell:

```sh
scratch=$(mktemp -d "$HOME/malak-example.XXXXXX") &&
(
    unset MALAK_HANDOVER MALAK_TASK_INBOX MALAK_TASK_CREATION MALAK_TASK_POSITION MALAK_TASK_TRACE
    cd "$scratch" && HOME="$scratch" XDG_CONFIG_HOME="$scratch/.config" TMPDIR="$scratch" sh
)
```

In that shell, run `malak board start --detach` before the example unless
the example is for `board start` or needs no board. Use a new scratch
shell for each example so message positions and participant names start
fresh. If the board is still running, stop it with `malak board stop`
before leaving. For live examples that ask for a second terminal, use
the same scratch directory and environment there. The five-worker demo
also requires Claude Code on your PATH and signed in within the scratch
environment.

## `malak send`

Give a nonempty body as an argument, or pass `-` to read it from standard input.

```sh
# Send two notes to lead and read them back.
malak send lead "Build finished."
printf '%s\n' 'A second note.' | malak send lead -
malak read lead
```

## `malak read`

If the link drops during `--wait`, Malak reconnects within the original wait window.

```sh
# Read an unseen note and acknowledge it after printing.
malak send lead "Ready for review."
malak read --ack lead
```

Use `--ack` for notices and reports. For a task, run plain `malak read` and
`malak ack` once the work is done: an acknowledged task is not shown again
after a crash.

## `malak get`

```sh
# Retrieve the first message by position, then see it still waiting to be read.
malak send lead "Keep this note."
malak get lead 1
malak read lead
```

## `malak ack`

```sh
# Acknowledge the first message after reading it, then check the inbox.
malak send lead "Please confirm."
malak read lead && malak ack lead 1
malak read lead
```

## `malak rail`

```sh
# Record a dependency and read the board's recent rail.
malak recipe rail wait --on build 'shared contract'
malak rail --since 24h --json
```

## `malak pulse`

Run this from the chief of staff's established top-board session.

```sh
# Read the organisation's current pulse as chief of staff.
malak pulse --json
```

## `malak board list`

```sh
# List known boards and their lead inbox counts as JSON.
malak board list --json
```

## `malak board start`

```sh
# Start a disposable board in the background and wait until it is ready.
malak board start --detach
```

## `malak board status`

```sh
# Show the running board, its version and its channels.
malak board status
```

The summary shows the version and build ID. If either differs from the
CLI's, the CLI prints a restart hint to standard error.

## `malak board stop`

```sh
# Stop the disposable board and release its store.
malak board stop
```

## `malak board stop --all`

```sh
# Stop all of the account's discoverable boards.
malak board stop --all
```

## `malak upgrade`

```sh
# From a Malak checkout, preview an upgrade using that source.
malak upgrade --path . --dry-run
```

The plan shows the current and target versions, each board to restart and its
connected workers. Cargo produces the local build ID during installation.
Run `malak upgrade` to install the latest release tag and restart running
boards without a prompt.

## `malak uninstall`

```sh
# Preview data deletion with board roots and any paths that stay.
malak uninstall --dry-run --delete-data
# Show individual handovers and managed paths in the preview.
malak uninstall --dry-run --delete-data --verbose
```

The short deletion preview shows one block per board, followed by `Will delete`
and, when applicable, `Will keep`. This board has a notes file in its socket
folder.

```text
Board data for lanes
  Config    ~/.malak/lanes/board.toml
  State     ~/.malak/lanes/st
  Sockets   ~/.malak/lanes/d
  Contains  42 worker handovers, 6 demo workspaces
  Status    Would stop board.
  Run       633b691f10fe98ebeda3d79acdcf3d04
Will delete
  ~/.malak/lanes/st
  ~/.malak/lanes/board.toml
  ~/.malak/boards.jsonl
Malak will also delete managed entries from the folders listed below.
Will keep
  ~/.malak/lanes/d (directory contains other entries)
  ~/.malak/lanes/d/notes.txt (outside Malak's managed data)
```

## `malak enrol`

```sh
# Give Alice a private handover and use it to send as Alice.
malak enrol alice --grant lead:send --out alice.key
MALAK_HANDOVER="$PWD/alice.key" malak send lead "Hello from Alice."
```

## `malak grant`

```sh
# Replace Alice's grant with permission to send to lead.
id=$(malak enrol alice --out alice.key --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["id"])')
malak grant "$id" lead:send
MALAK_HANDOVER="$PWD/alice.key" malak send lead "Grant works."
```

## `malak rekey`

```sh
# Replace Alice's key and use the new handover to send.
id=$(malak enrol alice --grant lead:send --out alice.key --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["id"])')
malak rekey "$id" --out alice-new.key
MALAK_HANDOVER="$PWD/alice-new.key" malak send lead "New key works."
```

## `malak release`

The current lead session can release a worker's claim through the owner door.

```sh
# Release a worker claim from the current lead session.
eval "$(malak recipe lead-up)"
malak enrol alice --session "$$" --out alice.handover
malak release alice
```

## `malak revoke`

```sh
# Revoke Alice's access and show her revoked status.
id=$(malak enrol alice --out alice.key --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["id"])')
malak revoke alice
malak board status "$id"
```

## Message another board's lead

With both local boards running, use the receiving board's name in place of
`review`. Give the guest handover privately to the sending lead. Send as
`team-to-review` and use the printed `Watch` command to read replies in the
guest inbox. The last command revokes access after the exchange.

```sh
guest_handover="$HOME/.malak/team-to-review.handover"
malak --board review recipe guest team-to-review --out "$guest_handover"
printf '%s\n' 'The team build is ready for review.' |
    MALAK_HANDOVER="$guest_handover" malak send lead -
malak --board review revoke team-to-review
```

## `malak channel create`

```sh
# Create a channel named updates and send its first note.
malak channel create updates
malak send updates "First update."
```

## `malak channel remove`

```sh
# Create a scratch channel and remove it.
malak channel create scratch
malak channel remove scratch
```

## `malak trim`

```sh
# Discard history before position 2 and show the retained message.
malak send lead "Old note." && malak send lead "Keep this note."
malak trim lead 2
malak read lead
```

## `malak tail`

```sh
# Follow lead events. Send a note to lead in a second terminal. Press Ctrl-C to exit.
malak tail --channel lead
```

## `malak recover`

```sh
# Inspect the stopped Markdown board for damaged channels.
malak board stop
malak recover
```

## `malak hook`

On the scratch board, acknowledge the lead's claim notice before posting the
note the hook will deliver.

```sh
# Deliver a waiting note as JSON context to a generic agent session.
setup=$(malak recipe lead-up) && eval "$setup"
malak read --ack --wait 60 lead && malak send lead "Check the build."
printf '%s\n' '{"event":"session_start","session":"example"}' | MALAK_HOOK_INBOX=lead MALAK_HOOK_STATE="$PWD/hook-state" malak hook --tool generic
```

## `malak mcp`

An MCP client starts this command and talks to it over stdio. Here standard
input is closed, so the server starts as the lead and stops at once.

```sh
# Serve the lead's Seat over stdio; set MALAK_HANDOVER to serve a worker's Seat.
setup=$(malak recipe lead-up) && eval "$setup"
malak mcp </dev/null
```

## `malak mailbox`

```sh
# Open the live mailbox to see participants and activity.
# Press a to show or hide archived participants and q to quit.
malak mailbox
```

## `malak mailbox --plain`

```sh
# Stream activity as text. Send a note to lead in a second terminal. Press Ctrl-C to exit.
malak mailbox --plain
```

## `malak mailbox --next`

```sh
# Wait one second for a lead event, or show waiting mail immediately.
malak mailbox --plain --for lead --next --timeout 1
```

## `malak mailbox --for`

```sh
# Open the mailbox filtered to Alice. Press q to quit.
malak enrol alice --out alice.key
malak mailbox --for alice
```

## `malak mailbox --who`

```sh
# Show when Alice was enrolled and granted access. Press q to quit on a terminal.
malak enrol alice --out alice.key
malak mailbox --who
```

## `malak mailbox --questions`

```sh
# Post a question and list it while it is still open.
malak send questions "Which build should ship?"
malak mailbox --questions
```

## `malak mailbox --orders`

```sh
# Give the lead permission for one hour and show the order in force.
printf '%s\n' 'The lead may choose the test order.' | malak recipe order --for 1h
malak mailbox --orders
```

## `malak mailbox --waiting`

```sh
# Send Alice a task and show that her inbox is waiting for acknowledgement.
id=$(malak enrol alice --out alice.key --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["id"])')
malak send --kind task "to/$id" "Review the build."
malak mailbox --waiting
```

## `malak mailbox --trace`

```sh
# Follow the history of a known trace. Press Ctrl-C to exit.
malak send --kind task --trace 0123456789abcdef0123456789abcdef lead "Run the checks."
malak mailbox --trace 0123456789abcdef0123456789abcdef --plain
```

## `malak recipe`

```sh
# List the recipes shipped with Malak. No board is needed.
malak recipe
```

## `malak recipe adopt`

```sh
# Use the PID of a running Claude Code or Codex session.
malak recipe adopt researcher --session "$agent_pid"
```

## `malak recipe agents`

```sh
# Install the Malak skill and reminders for the configured agent tools.
malak recipe agents --yes
```

## `malak recipe agents-md`

```sh
# Add Malak instructions to the current project's AGENTS.md and CLAUDE.md.
malak recipe agents-md
```

## `malak recipe answer`

Giving only a position prefers an open question on `questions`, then looks
on `lead`. The receipt names the channel. Use `answer lead 1` to choose
`lead` explicitly.

```sh
# Reply to questions #1, even if lead #1 exists. Read the linked answer.
malak send questions "Which build should ship?"
printf '%s\n' 'Ship the passing build.' | malak recipe answer 1
malak get questions 2
```

## `malak recipe ask`

```sh
# Ask which build to ship with two options and a recommendation.
printf '%s\n' 'Which build should ship?' | malak recipe ask --blocks 'the release' --option 'stable' --option 'preview' --recommend 'stable'
malak read questions
```

## `malak recipe await`

```sh
# Wait for the builder's own final report without acknowledging lead.
malak recipe worker-start builder --no-watcher --detach -- printf '%s\n' 'Build finished.'
malak recipe await builder --timeout 30s
```

## `malak recipe brief`

```sh
# Store text; subsequent briefs can supersede or withdraw it.
printf '%s\n' 'The retry must reuse its message ID.' | malak recipe brief put --title 'Retry contract' --kind learned
```

## `malak recipe demo`

```sh
# Start a disposable board and run five workers.
malak recipe demo claude "Name five different fruits."
```

Run the printed mailbox command in a second terminal to watch the demo.
The demo prints the result and removes its board before returning. Pass
`--keep` to retain the board, logs and outcome file.

## `malak recipe forward`

```sh
# Forward the first lead message into Alice's inbox with its source.
id=$(malak enrol alice --out alice.key --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["id"])')
malak send lead "The build is ready."
malak recipe forward lead 1 alice && malak get "to/$id" 1
```

## `malak recipe guest`

```sh
# Enrol a guest and print commands to send a note and watch for replies.
malak recipe guest team-to-review --out team-to-review.handover
```

## `malak recipe handoff`

```sh
# Pass Alice's first task to Bob on the same trace.
id=$(malak enrol alice --out alice.key --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["id"])')
malak enrol bob --out bob.key && malak send --kind task "to/$id" "Review the build."
malak recipe handoff alice 1 --to bob
```

## `malak recipe lead-up`

```sh
# In the agent shell, apply the export for a configured default board.
setup=$(malak recipe lead-up) && eval "$setup"
printf 'Lead handover: %s\n' "$MALAK_HANDOVER"
```

## `malak recipe observe`

```sh
# Simulate an agent session starting without a board and print the reminder.
printf '%s\n' '{"session_id":"example","hook_event_name":"SessionStart","source":"startup"}' | MALAK_OBSERVE=1 malak recipe observe
```

## `malak recipe org`

```sh
# Create a top board whose lead key remains unclaimed with the owner.
malak recipe org setup
malak --board top board status --json
```

## `malak recipe order`

```sh
# Record what the lead can decide over the next hour.
printf '%s\n' 'The lead may choose the test order.' | malak recipe order --for 1h
malak read orders
```

## `malak recipe rail`

```sh
# Track a dependency until its subject is done.
malak recipe rail wait --on build 'shared contract'
malak recipe rail wait --done 'shared contract'
```

## `malak recipe queue`

```sh
# Start a reviewer after the researcher's verified final report.
printf '%s\n' 'Review the build.' > review.txt
malak recipe worker-start researcher --no-watcher -- printf '%s\n' 'Research finished.'
malak recipe queue reviewer --no-watcher --after researcher --brief review.txt -- printf '%s\n' 'Review finished.'
```

## `malak recipe worker-start`

```sh
# Post the builder's brief and start the command in the background.
printf '%s\n' 'Build the project.' > brief.txt
malak recipe worker-start builder --no-watcher --brief brief.txt --detach -- printf '%s\n' 'Build finished.'
malak recipe await builder
```

The launch output includes commands to follow the logs, wait for the report,
revoke the enrolment, and send `SIGTERM` to the worker's process group.

## `malak recipe workers`

```sh
# Look up the builder after its command has exited.
malak recipe worker-start builder --no-watcher -- printf '%s\n' 'Build finished.'
malak recipe workers builder
```

## `malak briefs`

```sh
# Store a learned brief and list it through your own seat.
printf '%s\n' 'Reuse the message ID.' | malak recipe brief put --title 'Retry contract' --kind learned
malak briefs --kind learned --json
```

## `malak brief get`

```sh
# The original text remains available after supersession.
printf '%s\n' 'Original text.' | malak recipe brief put --title 'Contract' --kind spec
printf '%s\n' 'Updated text.' | malak recipe brief put --title 'Contract' --kind spec --replaces 1
malak brief get malak 1
```

## `malak brief view`

```sh
# A linked digest for this board.
malak brief view --since 24h --json
```

## `malak recipe wrapper`

```sh
# Run a command and post its stdout as Alice's report.
malak enrol alice --grant lead:send --out alice.key
MALAK_WORKER_HANDOVER="$PWD/alice.key" MALAK_REPORT_ID=wrapper-example MALAK_WRAPPER_STATE="$PWD" malak recipe wrapper -- printf '%s\n' 'Build finished.'
malak read lead
```
