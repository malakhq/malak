# Set up Malak as the agent

Use the project folder name as the board name only when the chat's working
folder is the owner's project and the owner has given you no other name. If
the chat is open elsewhere, ask the owner for the board name. A takeover uses
the previous lead's board name. Keep one lead chat per board.

## 1. Become the lead

Check `malak --version` and `python3 --version`. You need Malak on `PATH` and
Python 3.11 or later. Set `board` to the name the owner gave you, or use
`board=$(basename "$PWD")` if the chat is in the project folder. Then run
this in your shell.

```sh
unset MALAK_HANDOVER
setup=$(malak recipe lead-up -- "$board") && eval "$setup"
```

The recipe creates the board on first use and reuses it on takeover. Expect
`Lead check passed`. For a new board, send a test note and check for a receipt.

```sh
malak --board "$board" send lead 'Malak setup test. Lead ready.'
```

Pass `--board "$board"` on every board command. In a fresh shell, set
`board` to the same name and set `MALAK_HANDOVER` to the path printed by the
recipe. With `MALAK_HANDOVER` unset, Malak can find the lead handover from the
selected board when you can read its state as its owner. On takeover, handle
the unacknowledged lead inbox before sending a setup note or testing a worker.
Never use another participant's handover. Ask the owner if you need a
handover or need to change roles.

In a read-only Codex session, Malak can fail to create the board files.
On macOS, `bind Unix listener` with `Operation not permitted` can mean the
sandbox is blocking the board socket. Tell the owner which step was blocked
and ask them to start the chat with this command and paste the prompt again.

```sh
codex --sandbox workspace-write -c sandbox_workspace_write.network_access=true
```

If the board's config or state is outside the writable workspace, ask the
owner to add those directories with `--add-dir PATH` or approve the Malak
command that needs access. [Codex network access](https://learn.chatgpt.com/docs/agent-approvals-security#network-isolation)

`lead-up` binds the handover to the chat that runs it on a local Unix socket.
Take over from a new chat by running `lead-up` there. The worker launcher binds
the command it starts. Use `malak recipe workers NAME` or `malak mailbox --who`
to check the claimed session and see refused attempts. A person in a separate
terminal can use the owner mailbox to inspect status.

The current lead session can release a worker's claim with
`malak release NAME` when it has owner access. Other owner releases require
a terminal and typed confirmation. The handover needs to be re-issued
after release.

## 2. Install the skill and reminders

Run `malak recipe agents` from a terminal and confirm the prompt. When the
owner has approved installation and you are running without a terminal,
use `malak recipe agents --yes`. Use `--dry-run` to preview the changes.
The recipe installs the Malak skill and prompt reminders for the user in
existing Claude Code and Codex configuration directories. Each tool gets one
fixed set of hook entries with no matcher: `SessionStart`, `UserPromptSubmit`,
`SubagentStart`, `SubagentStop`, `PreToolUse`, `PostToolUse`, `Stop` and
`SessionEnd`. Every entry runs `malak recipe observe --reminder`, which decides
from the payload and says nothing unless it has something to say. A new
behaviour is a new branch there, shipped by `malak upgrade`, and never a new
entry. That is how a lead about to end its turn with unacknowledged lead
messages or unanswered agent questions is handed them once and asked to handle
and acknowledge them first.
On prompts and tool use, the reminder checks at most every three minutes and
shows commands for waiting work. It also records questions from Claude Code's
`AskUserQuestion`, Codex's Plan-mode `request_user_input`, and Codex's
`request_user_input_async` on the chat's single lead board. It quotes the
owner's answer when the tool returns it. For the async tool, record the
reply with `recipe answer` when the owner answers. The reminder gives the
command when Codex supplies a reply linked to that question.

In Codex CLI, open `/hooks` and press Enter on `SessionStart`, `UserPromptSubmit`,
`SubagentStart`, `SubagentStop`, `PreToolUse`, `PostToolUse`, `Stop` or `SessionEnd`. Select the Malak command from
`hooks.json`, review it and press `t` to trust it. Press Esc and repeat
for the other events. In Codex desktop use **Settings > Hooks**. After
`malak upgrade` changes the set, open Codex threads are asked to reread their
config through the app-server and need no restart, but approval stays with
the owner; an agent never approves hooks for itself. Claude Code chats need
one restart for the new set.
If the owner also wants a one-time notice in already open local Codex
app-server chats, add `--notify-open`.

Run `malak recipe agents --remove` to remove the installed skill and
reminders. `malak uninstall` removes them too.

When you run `malak upgrade`, it refreshes the installed integration if its
skill or hook definitions changed. With the Codex integration installed, open
Codex app-server threads receive the upgrade notice. If the hooks changed,
restart existing Claude Code chats or review the hooks in Codex.

## 3. Start the lead watcher

Start this command in the background before launching the test worker. It
follows the lead inbox, questions, orders and stuck workers.
Without the watcher, reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen.

```sh
malak --board "$board" mailbox --plain --for lead --next
```

Set `MALAK_HANDOVER` in the watcher command itself. In Claude Code, use
background Bash. In Codex, start the command with a short yield, keep the
command session ID, and check its output as you work. The watcher exits after
showing new waiting lead mail, a new message from another participant, or a
due reminder. It remembers shown messages across runs and repeats
unacknowledged lead mail at most once an hour. Review open owner questions
with `malak --board "$board" mailbox --questions`. Handle what the watcher
prints, acknowledge lead messages, and run it again. It reconnects through a
brief board restart.

To stay awake inside the host's own loop, run the watcher once per tick:

```sh
malak --board "$board" mailbox --plain --for lead --next --timeout 60
```

In Claude Code, run it from `/loop` (dynamic) with a tick of two to five
minutes. In the Codex App, make an automation that runs the same command. Each
tick handles what the command prints, acknowledges lead messages and goes idle.
The reminder counts a lead whose last tick started or ended within ten minutes
as watching.

For continuous output in a terminal, use
`malak --board "$board" mailbox --plain --for lead`.

If your tool cannot keep a background command running, poll at the start of
each turn and after each step. Use the same handover and board name as above.

```sh
malak --board "$board" read --wait 60 lead questions
```

Before acting, read the orders marked `in force` with `malak --board "$board" mailbox --orders`.
Check again when a new order arrives. Handle lead messages in order and
acknowledge through the last one you handled. Use the creation and position of
that last message. On takeover, the read shows what is still unacknowledged.

```sh
malak --board "$board" read lead
malak --board "$board" ack --creation CREATION lead POSITION
```

## 4. Test a worker message

`worker-start` and `queue` refuse when no lead watcher is running on that board. Scripts and tests can pass `--no-watcher`.
`worker-start --brief FILE` posts a task to the worker's inbox. Claude Code
and Codex hooks deliver it at session start. The command's prompt need not
repeat the brief. Use a short shell worker to check delivery by reading
the inbox explicitly and posting a report.

```sh
test_worker="setup-check-$(date +%s)"
test_dir=$(mktemp -d)
printf 'Test files %s\n' "$test_dir"
printf '%s\n' 'Malak setup test. Please reply.' > "$test_dir/brief"
cat > "$test_dir/worker.sh" <<'SH'
set -eu
message=$(malak --board "$MALAK_SETUP_BOARD" read --ack "$MALAK_TASK_INBOX")
printf '%s\n' "$message" | grep -Fq 'Malak setup test. Please reply.'
printf '%s\n' 'Malak setup test. Worker received its message.'
SH
MALAK_SETUP_BOARD="$board" malak --board "$board" recipe worker-start "$test_worker" \
    --detach --brief "$test_dir/brief" -- sh "$test_dir/worker.sh"
malak --board "$board" recipe await "$test_worker" --timeout 30s
malak --board "$board" recipe workers "$test_worker"
```

Confirm that `Worker received its message.` appears in the report. The launch
output includes the worker's inbox, process group, and commands to follow
and stop it. Keep the test files path and the worker's ID from `to/ID` for cleanup.

## 5. Ask the owner a test question

Use the question tool with the hooks installed on a single lead board, or
run `recipe ask` before writing the question in chat. If this chat leads
several boards, pass the intended board explicitly to `recipe ask`.

```sh
printf '%s\n' 'Which check should the Malak setup test use?' |
    malak --board "$board" recipe ask --blocks 'the test handoff' --option 'delivery' \
        --option 'visibility' --recommend 'delivery'
malak --board "$board" mailbox --questions
```

Expect a question receipt and the question under **For the owner**. When the
watcher shows a question you posted for the owner or a worker's question only
the owner can answer, put it to the owner in chat right away. Include the
options and the recommendation. The owner can also see their questions at
the top of `malak mailbox`. Record the owner's reply with the question's
position from the watcher or the question receipt.

```sh
printf '%s\n' 'Owner, in chat at HH:MM: "…"' |
    malak --board "$board" recipe answer questions POSITION
```

## 6. Try an urgent notification

When a question is urgent and blocks work, post it on `questions` and try to
get the owner's attention through your agent tool's own notification feature.
For this test, if the feature is available, try a notification that says
“Malak setup test. The question is at the top of your mailbox.” In a
noninteractive session, describe what you would try in an interactive chat.
Continue the setup either way. In your final reply, include one line about
the test notification. Say what you tried, what you would try, or that your
tool has no notification feature.

## 7. Set a test order

```sh
printf '%s\n' 'For this setup test, the lead may choose which test check to run.' |
    malak --board "$board" recipe order --for 10m
malak --board "$board" mailbox --orders
```

Expect an order receipt and an active order with that scope. Tell the owner
they can see it in the Orders tab of `malak mailbox` or with
`malak --board "$board" mailbox --orders`.

## 8. Clean up and explain what the owner saw

Use the worker name and replace `ID` and `TEST_DIR` with the worker ID and
test files path from step 4.

```sh
malak --board "$board" revoke "$test_worker"
malak --board "$board" board status ID
rm -f "TEST_DIR/brief" "TEST_DIR/worker.sh"
rmdir "TEST_DIR"
```

Expect the test worker to be revoked. `revoke` also accepts the ID, which is
required if several active participants have the same name. Use
`worker-start NAME --again` to retire an earlier enrolment once its launcher
exits. Keep the board and your watcher running for the rest of the session.
If you are polling, keep using the poll in step 3.

For cross-board messages, use [`recipe guest`](RECIPES.md#message-another-boards-lead)
to send as a guest. Use the printed commands to watch for replies and
acknowledge each reply after handling it. A registered guest handover also
brings replies into your home lead inbox and watcher. Revoke guest access
when done.

Give the owner a few plain lines on how Malak helps their project. Use what
you saw in the project and the test. Mention how they can see delegated
work, read worker replies, answer blocking questions, and set the scope for
decisions while they are away. Tell them how you received the test reply.
