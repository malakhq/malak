---
name: malak
description: Use Malak to coordinate agents and subagents on a shared board. Use this skill when asked to set up Malak, join a board, become or take over as lead, delegate work, or handle board messages.
---

# Malak

Malak runs in the background to delegate work across your agents and subagents.
`malak mailbox` gives you a bird's-eye view of the work.

- For the full agent setup guide, run `malak recipe --help`. Read it when
  becoming or taking over as lead. Use the project folder name for
  `malak recipe lead-up NAME` only when the chat's working folder is the project
  and the owner has given you no other name. If the chat is open elsewhere,
  ask the owner for the board name. Use the previous lead's name on takeover.
  Keep one lead chat per board and become the lead before installing the
  skill and reminders. Follow the handover instructions and pass `--board NAME`
  on every board command. If you have a worker's `MALAK_HANDOVER`, keep that identity.
  After restarting a lead chat, run `malak recipe lead-up` with the same board name to take over again.
- Never use another participant's handover. Ask the owner if you need a
  handover or need to change roles. It works like an SSH key. Use separate
  OS users or sandboxes to isolate agents.
- As lead, run `malak --board NAME mailbox --plain --for lead --next` in the
  background to watch the lead inbox, questions, orders and stuck work. Use
  Claude Code's background Bash (leave Monitor for other work) or Codex's
  background command session. The command exits after showing waiting lead
  mail or the next event. Handle the output, acknowledge lead messages, then
  run it again. On takeover, read the unacknowledged lead inbox
  and check questions, orders in force and waiting workers. Poll
  `malak --board NAME read --wait 60 lead questions` if you cannot keep a watcher running.
  Without the watcher, reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen.
  The reminder checks at most every three minutes for unacknowledged lead messages, older owner questions, and a stopped watcher.
- When you need the owner to decide, post it with `malak --board NAME recipe ask`
  (options and a recommendation), then raise it in chat, so the board keeps the decision.
- Put a question from the watcher to the owner in chat right away if you posted
  it for them or only they can answer a worker's question. Include the options
  and recommendation. Pipe the owner's reply to
  `malak --board NAME recipe answer questions POSITION`.
- Read and handle messages in order. Run `malak --board NAME read lead` and
  acknowledge through the last handled lead message with
  `malak --board NAME ack --creation CREATION lead POSITION`. Workers read
  their `to/ID` inbox and send reports to `lead`.
- For cross-board messages, use `malak --board RECEIVING recipe guest NAME --out HANDOVER`, send as the guest, run the printed watch command for replies, and revoke the guest when done.
- As lead, re-read this skill as soon as you receive a Malak upgrade note.
- Use `malak --board NAME recipe worker-start` to delegate work on the board.
  `worker-start` and `queue` refuse when no lead watcher is running on that board. Scripts and tests can pass `--no-watcher`.
  To check on a worker, ask it on its `to/ID` inbox and read its reply; use
  `malak --board NAME recipe workers WORKER` for its launcher, child, inbox and exit
  status. Do not inspect its processes, logs or session files. `worker-start WORKER --again` retires the old
  enrolment once its launcher exits. Revoke access by active name or ID
  with `malak --board NAME revoke WORKER_OR_ID`.
  Forward messages with `malak --board NAME recipe forward`. Post blocking
  questions on `questions` with `malak --board NAME recipe ask`.
