# Malak lead changes

Add new entries at the top. Start each line with its date and keep existing lines stable so upgrades can find the previous build's place in the list.

- 2026-11-24 · Run `mailbox --plain --for lead --next` in the background. Handle its output, acknowledge lead messages, and run it again.
- 2026-11-24 · The worker mailbox points to the worker's inbox for answers and shows the worker's own question positions as they are posted.
- 2026-11-24 · Worker launchers retry a timed-out final report at least once with the same ID.
- 2026-11-24 · Malak 0.1.1 release.
- 2026-11-24 · `send` refuses empty bodies and requires `-` for piped text. Waiting reads reconnect within the original wait and plain mailbox previews recover from a stale link.
- 2026-11-24 · `recipe guest` gives cross-board guests a reply inbox and prints the send and watch commands. Run it again with the existing handover to upgrade a guest in place.
- 2026-11-24 · The reminder names only boards your chat leads or has joined as a worker. After restarting a lead chat, run `recipe lead-up` with the same board name.
- 2026-11-24 · Ask the owner with `recipe ask`, then in chat. Check on a worker by asking it on its inbox, not by inspecting its processes. `recipe workers` says when a worker has no launch record.
- 2026-11-24 · Upgrades restart running boards without a prompt. Worker launchers retry final reports with the same ID for up to 60 seconds during a restart.
- 2026-11-24 · `worker-start` and `queue` require a lead watcher on the selected board. Use `--no-watcher` for scripts and tests. `lead-up` prints the watcher and acknowledgement commands.
- 2026-11-24 · The mailbox shows each sender’s folder and flags session changes.
- 2026-11-24 · `worker-start` retries handover attachment when the board connection drops.
- 2026-11-24 · Mailbox previews recover after a lost board connection.
- 2026-11-23 · Treat handovers like SSH keys. Never use another participant’s handover.
- 2026-11-23 · Restart the lead watcher when it ends, including after a Claude Code Monitor time limit.
