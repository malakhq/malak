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

Use a fresh default board from the lead's own account.

```sh
malak board start --detach
```

Open the mailbox in a second terminal.

```sh
malak mailbox
```

In the first terminal, run the demo and watch it in the mailbox.

```sh
malak recipe demo claude "Give a one-word fruit for each of five colors."
```

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
The command shows startup progress, prints the lead log and outcome file
paths, and returns once all five workers have started. The outcome file
records the lead and demo exit statuses and the worker success count when
the demo finishes.

The mailbox shows the five worker inboxes and activity as workers pick up
tasks and report back. The first reports can take about a minute to arrive.
Press `a` in the terminal view to show or hide archived participants, or
`q` to quit. Press Ctrl-C to stop a redirected feed. Run
`malak read lead questions` to see messages that are still waiting for the lead.

## Make your agent the lead

Open Claude Code or Codex in your project and paste this into the chat.

> Become the Malak lead for this project.

The agent runs `malak recipe lead-up NAME` with the project folder name unless
you give it a board name. It keeps `malak --board NAME mailbox --plain --for lead`
running and passes `--board NAME` on each board command.
Without the watcher, reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen.
Run `malak --board NAME recipe agents-md` from the project
directory to give every chat there Malak instructions.

### Take over as lead

Ask a new Claude Code or Codex chat in the same project.

> Take over as the Malak lead for this project.

The new chat uses the same board name and runs `malak --board NAME read lead`
to read unacknowledged messages first. It then checks `questions`, orders in
force, waiting workers, and traces for active work on that board. Keep one lead
chat for each board.

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
