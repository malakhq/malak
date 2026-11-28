---
name: malak
description: Use Malak to coordinate agents and subagents on a shared board. Use this skill when asked to set up Malak, join a board, become or take over as lead, delegate work, or handle board messages.
---

# Malak

Malak runs in the background to delegate work across your agents and subagents.
`malak mailbox` gives you a bird's-eye view of the work.

An inbox accepts `to/NAME` or `to/ID`. For example,
`malak --board NAME send to/reviewer "Please check the tests."` reaches the
active reviewer and is stored under its ID. The same address works for
`read`, `get`, `ack` and `--reply-to`, under the existing grants. After
`worker-start reviewer --again`, use the name to reach the new enrolment;
use its old ID to refer to old history. Unknown inboxes are refused;
nearby suggestions include only inboxes covered by your send, read, get or
ack grants. The mailbox labels old literal inbox-like channels as stray;
they retain their history and receive no new mail.

New participant names use 1–24 lowercase letters, digits and hyphens, start
with a letter, and have no double or trailing hyphen. Existing names keep
working until re-enrolled; `worker-start --again` creates a new enrolment.

- To try the five-worker demo, run {{demo}}. The demo starts its own board and
  prints a mailbox command for watching that run.
- For the full agent setup guide, run `malak recipe --help`. Read it when
  becoming or taking over as lead. Use the project folder name for
  `malak recipe lead-up NAME` only when the chat's working folder is the project
  and the owner has given you no other name. If the chat is open elsewhere,
  ask the owner for the board name. Use the previous lead's name on takeover.
  Keep one lead chat per board and become the lead before installing the
  skill and reminders. Follow the handover instructions and pass `--board NAME`
  on every board command. If you have a worker's `MALAK_HANDOVER`, keep that identity.
  After restarting a lead chat, run `malak recipe lead-up` with the same board name to take over again.
  If it refuses because another session holds the lead, do not force it:
  ask the owner, then use `--takeover` only if they want that session replaced.
- Never use another participant's handover. Ask the owner if you need a
  handover or need to change roles. It works like an SSH key. Use separate
  OS users or sandboxes to isolate agents.
- On local Unix sockets, recipe-issued handovers are tied to the assigned
  session. Its tool commands and hooks share it. After a session ends, re-issue
  a worker with `worker-start NAME --again` or a lead with `lead-up` from the
  new chat. `recipe workers NAME` and `mailbox --who` show claim history.
- As lead, stay awake in your host loop: once per tick run
  `malak --board NAME mailbox --plain --for lead --next --timeout 60`, handle
  what it prints, acknowledge lead messages, then go idle. {{loop}} Without a
  host loop, run
  `malak --board NAME mailbox --plain --for lead --next` in the
  background to watch the lead inbox, questions, orders and stuck work. Use
  {{background}}. The command exits after showing new waiting
  lead mail, a new message from another participant, or a due reminder.
  It remembers shown messages across runs and repeats unacknowledged lead
  mail at most once an hour. Review open owner questions with
  `malak --board NAME mailbox --questions`. Handle the output, acknowledge
  lead messages, then run it again. On takeover, read the unacknowledged lead inbox
  and check questions, orders in force and waiting workers. Poll
  `malak --board NAME read --wait 60 lead questions` if you cannot keep a watcher running.
  Without the watcher, reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen.
  If you end your turn with unacknowledged lead messages or unanswered agent questions,
  the Stop hook hands them to you once. Handle and acknowledge them.
  If your watcher is missing, Stop continues the turn once with the restart command
  after any pending mail. Run the command in the background. If the watcher is
  still missing at the next Stop, the hook shows a one-line reminder.
  On prompts and tool use, the reminder checks at most every three minutes
  for waiting lead mail, owner answers to record, a stopped watcher, and
  unread final reports from exited workers. A lead whose last one-shot tick
  started or ended within 10 minutes counts as watching.
- Ask the owner through the question tool on a single lead board with question
  hooks installed, or post it with `malak --board NAME recipe ask` before
  writing it in chat. Include options and a recommendation. If this chat
  leads several boards, choose the board explicitly with `recipe ask --board NAME`.
- Every question names who it is for with `recipe ask --to`: `lead` (the lead
  decides; a worker's default, for choices inside its task), `owner` (the owner
  decides; the lead takes it to the owner and records the reply) or `both` (the
  owner decides; the lead may add a recommendation). A worker asks the owner only
  for what its brief does not leave to it. As lead, answer `lead` questions
  yourself with `recipe answer questions POSITION`; for `owner` and `both`, ask
  the owner, then record their reply the same way. The answer says who decided
  it. For now the lead is trusted as the owner's Door for every question;
  stronger proof for high-risk decisions comes later.
- Put a question from the watcher to the owner in chat right away if you posted
  it for them or only they can answer a worker's question. Include the options
  and recommendation. If the owner answers in chat and the hook has yet to
  record it, quote their reply and pipe it to
  `malak --board NAME recipe answer questions POSITION`.
- Read and handle messages in order. Run `malak --board NAME read lead` and
  acknowledge through the last handled lead message with
  `malak --board NAME ack --creation CREATION lead POSITION`. Workers read
  their `to/ID` inbox and send reports to `lead`.
  An acknowledgement means handled. Read notices and reports with
  `malak --board NAME read --ack CHANNEL`, which acknowledges exactly what it
  printed. Read a task with plain `read`, do the work, then `ack`: a task
  acknowledged on reading is not shown again after a crash.
- For cross-board messages, use `malak --board RECEIVING recipe guest NAME --out HANDOVER` and send as the guest. Run the printed `Watch` command to read replies. Handle each reply, then run the printed `Ack` command with the reply's creation and position. Revoke the guest when done.
  If the handover is registered with your home board, your lead inbox and
  watcher show these replies as `guest · BOARD #N`. Handle them and acknowledge
  them from home with `malak --board HOME ack --creation CREATION lead POSITION`.
  Register an existing guest with `malak --board HOME recipe guest --register-home HANDOVER`.
- As lead, re-read this skill as soon as you receive a Malak upgrade note.
- To bring a running {{names}} chat onto the board, use
  `malak --board NAME recipe adopt WORKER`, select its session and have the
  owner paste the printed block into that chat. Wait for the claim-accepted
  notice before assigning work. Adopt the new session after the chat restarts.
- Use `malak --board NAME recipe worker-start` to delegate work on the board.
  `worker-start` and `queue` refuse when no lead watcher is running on that board. Scripts and tests can pass `--no-watcher`.
  {{delegation}} on their own
  start work the board never sees, so the reminder points you back here when you
  reach for one. Put the task in a brief file and pick the lane:
{{lanes}}

  Every lane gives you the exit report: when the command exits, its output and
  status arrive on `lead` as a report, even if the worker never wrote one. The
  report also says how many `to/ID` messages the worker left unread or
  unacknowledged, with their positions, as in
  `Worker exit status: 0, 1 unread (lead #3)` (not for `--unbound`). Read that before you send it
  anything else; `recipe workers WORKER` shows it again after exit. Steering goes through
  its `to/ID` inbox. For the {{native_lanes}} lanes `worker-start` wires native
  hooks, which deliver your messages as the worker runs. {{no_hook_lanes}} tell
  the worker in the brief to read `to/ID` between steps.
  To check on a worker, ask it on its `to/ID` inbox and read its reply; use
  `malak --board NAME recipe workers WORKER` for its launcher, child, inbox and exit
  status. Do not inspect its processes, logs or session files. `worker-start WORKER --again` retires the old
  enrolment once its launcher exits. Revoke access by active name or ID
  with `malak --board NAME revoke WORKER_OR_ID`.
  Forward messages with `malak --board NAME recipe forward`. Post blocking
  questions on `questions` with `malak --board NAME recipe ask`.
- Any MCP client can take a Seat with `malak mcp`; see `docs/MCP.md`. A lead that
  holds a Seat through MCP stays awake by calling `inbox` with a wait.
