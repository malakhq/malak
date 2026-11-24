# Run Malak with Claude Code or Codex

Install the [Malak CLI](../README.md#cli) and put Python 3.11 or later on
`PATH`. Install and sign in to the agent you want to run.

Run `malak recipe agents` to install the Malak skill and reminders for Claude Code and Codex.
Run `malak upgrade --dry-run` to preview an upgrade and `malak upgrade` to apply it.
An upgrade restarts running boards without a prompt. Worker launchers retry
their final reports for up to 60 seconds while the board is unreachable.

Use only the handover assigned to your session. Ask the owner if you need a
handover or need to change roles. A handover works like an SSH key. Use
separate OS users or sandboxes to isolate agents.

## With your agent

Open Claude Code or Codex in your project and paste this prompt.

> Set up Malak for me. Follow `docs/AGENT-SETUP.md` or the guide in `malak recipe --help`, then tell me how Malak helps me.

On macOS, start Codex CLI with workspace writes and network access so it
can create board files and reach Malak's local Unix socket.

```sh
codex --sandbox workspace-write -c sandbox_workspace_write.network_access=true
```

A read-only session can fail when creating the board files. A blocked socket
can report `bind Unix listener` with `Operation not permitted`. Use
`--add-dir PATH` for each board directory Codex needs to write outside the workspace.

## With the CLI

### Run the demo

Run the demo to start a fresh, dedicated board.

```sh
malak recipe demo claude "Give a one-word fruit for each of five colors."
```

To watch the demo, run the mailbox command it prints in a second terminal.

Use `malak recipe demo codex "GOAL"` to run the same flow with Codex. The
recipe shows this prompt with the path to your installed `malak`.

```text
Codex hooks for this demo
  SessionStart: MALAK_RECIPE_NATIVE_HOOK=1 /path/to/malak hook --tool codex
  UserPromptSubmit: MALAK_RECIPE_NATIVE_HOOK=1 /path/to/malak hook --tool codex
  PostToolUse: MALAK_RECIPE_NATIVE_HOOK=1 /path/to/malak hook --tool codex
  SessionEnd: MALAK_RECIPE_NATIVE_HOOK=1 /path/to/malak hook --tool codex
trust these for this session? y/N
```

- `SessionStart` checks for a waiting message when the session starts.
- `UserPromptSubmit` checks again when you send a prompt.
- `PostToolUse` checks again after a tool call.
- `SessionEnd` acknowledges the last delivery if a prompt or tool event followed it.

The first three hooks also acknowledge the previous delivery. Enter `y` to
trust these hooks for this demo. Enter `N` or press Enter to cancel. Add `--yes`
after the goal to approve the hooks and skip the prompt.

Give the lead a goal it can divide into five short tasks.
The command shows progress and prints the result in the terminal. It removes
the demo board when it finishes or you press Ctrl-C. Pass `--keep` to keep the
board, logs and outcome file. The demo prints the commands to stop and remove
the kept board. Pass `--timeout SECONDS` if you need more than five minutes.

The mailbox shows the five worker inboxes and activity as workers pick up
tasks and report back. The first reports can take about a minute to arrive.
Press `a` in the terminal view to show or hide archived participants, or
`q` to quit. Press Ctrl-C to stop a redirected feed.

## Make your agent the lead

Open Claude Code or Codex in your project and paste this into the chat.

> Become the Malak lead for this project.

The agent runs `malak recipe lead-up NAME` with the project folder name unless
you give it a board name. It runs
`malak --board NAME mailbox --plain --for lead --next` in background Bash
in Claude Code or a background command session in Codex. It handles the
output, acknowledges lead messages, and runs the command again. It passes
`--board NAME` on each board command.
Without the watcher, reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen.
To stay awake in the host's own loop instead, the lead runs
`malak --board NAME mailbox --plain --for lead --next --timeout 60` once per
tick: from `/loop` (dynamic, a tick of two to five minutes) in Claude Code, or
from an automation in the Codex App. A person can keep
`malak --board NAME mailbox --plain --for lead` running in a terminal.
Run `malak --board NAME recipe agents-md` from the project
directory to give every chat there Malak instructions.

### Take over as lead

Ask a new Claude Code or Codex chat in the same project.

> Take over as the Malak lead for this project.

The new chat uses the same board name and runs `malak --board NAME read lead`
to read unacknowledged messages first. It then checks `questions`, orders in
force, waiting workers, and traces for active work on that board. Keep one lead
chat for each board.

## Bring a running agent onto the board

Run `malak --board team recipe adopt researcher` and select the Claude Code
or Codex session by PID, program, folder and start time. From a script, pass
`--session PID`. Paste the printed block into that chat to open its inbox.
If the agent restarts, run the same recipe again and select the new session.
See the [adopt recipe](RECIPES.md#adopt-a-running-agent) for the handover and
message commands.

## Launch one worker

With the board running, start `malak mailbox --plain --for lead` in another
terminal. Save a short task to `brief.md` and launch the agent directly through
the worker recipe. The recipe enrols the worker,
posts the brief, configures the tool's Malak hooks and sends its stdout to
`lead` under the worker's identity.
`worker-start` and `queue` refuse when no lead watcher is running on that board. Scripts and tests can pass `--no-watcher`.

```sh
malak recipe worker-start researcher --brief brief.md -- \
  claude -p --no-session-persistence -- 'Handle the task from the Malak hook.'
malak read --wait 30 lead
```

For Codex, [review the hooks](#review-the-codex-hooks), then use
`codex exec --ephemeral` followed by the same prompt after `--`. Use a
trusted Git workspace for Codex or pass `--skip-git-repo-check`
if the directory is a scratch workspace. The four native hooks are the
same for a single worker and the demo.

Use `malak recipe workers researcher` to see the worker's state, PIDs, inbox
and recorded exit code. After its launcher exits, `malak recipe worker-start
researcher --again -- COMMAND` replaces its old enrolment and reuses its brief.
Use `malak revoke researcher` or `malak revoke ID` to retire access by itself.

To message another board, use [`recipe guest`](RECIPES.md#message-another-boards-lead) to send as a guest, watch the guest inbox for replies, and revoke access when done.

### Check and re-issue a handover

`malak recipe workers NAME` and `malak mailbox --who` show the claimed
session, refused attempts and claim history. When a session ends, its handover
needs to be re-issued. Run `malak recipe worker-start NAME --again` to launch a
replacement worker, or `malak recipe lead-up` from the new lead chat.

Use `malak enrol NAME --session PID --out HANDOVER` to bind a running session
manually. Bare enrolment and `enrol --unbound` both allow multiple sessions.
`malak recipe guest NAME --out HANDOVER` binds the invoking session. Pass
`--session PID` to issue the guest handover to a different running session.

Use `worker-start --unbound` for scripts that share a handover across
processes. `--resume` requires `--unbound`.

## Upgrade and uninstall

After upgrading Malak, stop the old board processes with `malak board stop --all`.
Start each board again with `malak board start --detach` and its original
`--config` if it used one. `malak board status` shows the running version and build.

Run `malak uninstall` to stop the boards, list where their data lives, and remove
the Cargo-installed binary when Cargo is available. The command prints the path
for manual removal of other binaries. On a terminal, confirm uninstall, then
choose whether to delete the listed board data. The default keeps the data.
Deletion removes Malak's managed entries and reports other files it leaves in
the board folders.

Without a terminal, run `malak uninstall --yes` to keep the data or
`malak uninstall --yes --delete-data` to delete it. Add `--dry-run` to preview
the actions without confirmation or changes.

## Review the Codex hooks

For a standalone Codex worker, run this interactive Codex session once.
Open Codex’s hook review and trust the four commands that invoke your
installed `malak` by absolute path. You can also open the review with
`/hooks`. Exit the session after review.

```sh
python3 -c '
import json, os, shlex, shutil, subprocess
from pathlib import Path

malak = str(Path(shutil.which("malak")).resolve())
description = subprocess.check_output([malak, "hook", "--describe", "--json", "--tool", "codex"])
command = json.dumps("MALAK_RECIPE_NATIVE_HOOK=1 " + shlex.join([malak, "hook", "--tool", "codex"]))
args = ["codex"]
for event in json.loads(description)["events"]:
    name = event["input"]["hook_event_name"]
    setting = "timeout=3" if name == "SessionEnd" else "additionalContextLimit=0"
    args += ["-c", f"hooks.{name}=[{{hooks=[{{type=\"command\",command={command},{setting}}}]}}]"]
os.execvp(args[0], args)
'
```

Codex stores the standalone trust decision. `malak recipe worker-start`
supplies the same four definitions to each worker invocation.
[Codex hook review](https://learn.chatgpt.com/docs/hooks#review-and-trust-hooks)

See the [agent recipes](RECIPES.md) for replies, manual hook setup and
[recovery after a failed launch](RECIPES.md#recover-a-task-after-a-failed-launch).
