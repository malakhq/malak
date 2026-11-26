# Changelog

## Unreleased

- Add a public client. The new `malak::client::Client` attaches to a served
  board from a handover file over any `Transport` and offers `send`, `read`,
  `get` and `ack` with the types `Handle` uses. `tests/layers.rs` now serves a
  board and talks to it with only the public API, and
  `examples/socket_client.rs` connects to `examples/socket_board.rs`. The wire
  protocol is unchanged. Fixes #43.
- Serve a board from the library. The new `malak::server` module (the `unix`
  feature) has `Server`, `StopHandle`, `Entrance` and `LocalStartup`:
  `Server::create` provisions a durable board, `listen` accepts links at an
  entrance over any `Transport`, and `run` blocks until the board stops. The
  accept loop, the places that cap each entrance and the ready gate moved out of
  the command line; `malak board start` now calls them. Behaviour, the wire
  protocol, on-disk formats and command output are unchanged. `tests/layers.rs`
  serves a board over a transport and a mechanism written outside the core, and
  `examples/socket_board.rs` serves one over Unix sockets. Fixes #41.
- Make the `malak` crate embeddable. A build without the default `cli` feature
  is the board alone, and `markdown`, `sqlite`, `unix` and `key` add the
  built-in layers. `scripts/board-size` keeps that build under a size budget in
  CI. The default build is unchanged.
- Move every Claude Code, Codex and generic detail of the recipes into one
  adapter table per tool (`adapter_<tool>.py`). Recipes name no tool or model,
  a new tool is a new adapter file, and the lead's worker-start hint now reads
  `-- <your agent command>` with the tool's own command as an "e.g.". The
  installed skill is filled from the adapters; its text is otherwise unchanged.
- Add `docs/EMBEDDING.md` and an in-process example, `examples/team.rs`: a lead
  and two helpers that send, read, ack, ask, answer and stop, run by CI.
- Choose the public API of the `malak` crate, add the `layers`, `stores`,
  `transport` and `mechanism` modules, and pin the API in
  `docs/public-api.txt`. CI fails on a change that does not update it. There is
  no stability promise until 1.0. Recovery and local-entrance setup are now
  `#[doc(hidden)]`.
- Register one fixed set of hook entries for Claude Code and Codex, with no
  matchers, and decide every behaviour from the payload inside
  `malak recipe observe --reminder`. The command answers events it does not use
  before any recipe starts, and a failure never blocks a tool call. After
  `malak upgrade` changes the set, open Codex threads are asked to reread their
  config; Claude Code chats need one restart. Approval stays with the owner.
- Remind the lead of the `worker-start` form before it starts work through
  Claude Code's `Agent` or `Task` tool, `codex exec` or `durative exec`. The
  skill shows lanes for Sonnet, Codex and a `durative` sandbox.
- Say why an `ack` after `mailbox --next` is refused, and name the `read` that
  gives the message to the reader.

## 0.1.2 (2026-11-25)

- Add `malak recipe adopt` to bring a running Claude Code or Codex session
  onto a local board with its own inbox and session-bound handover.
- Bind local worker, guest and lead handovers to their assigned sessions. Show
  claim status and refused attachments in the mailbox and `recipe workers`.
  Use `enrol --session PID` to bind a running session manually.
- Make `mailbox --next` skip the lead's own messages and remember which messages
  it has shown across restarts. Unacknowledged lead mail can repeat at most
  once an hour.
- Run each agent demo on a disposable board and clean up its state and
  handovers after the run. Use `malak recipe demo … --keep` to keep its board
  for inspection.
- Continue the lead's turn once with the restart command when its watcher is
  missing. If the watcher is still missing at the next Stop, show a reminder.
- Record owner questions and quoted answers from supported Claude Code and
  Codex question tools on the chat's lead board. Show short, actionable
  reminders for leads and workers.
- Print the guest inbox acknowledgement command from `recipe guest` and
  show it in the reminder when a guest reply is waiting.
- Show replies from registered guest handovers in the home lead inbox,
  mailbox watcher and turn-end hook. Route acknowledgements back through
  the guest handover. Register existing handovers with `recipe guest --register-home`.

## v0.1.1 (2026-11-24)

- Reject empty message bodies and require `-` when piping a body to `send`.
- Reconnect waiting reads within their original timeout and recover plain
  mailbox previews after a connection drops.
- Add GitHub issue and pull request templates, release notes, and private
  security reporting instructions.

## v0.1.0

Initial release for macOS and Linux.

- Run a local message board over a Unix socket with key-based participant
  authentication and channel grants.
- Send, read, retrieve and acknowledge messages through the Rust library
  or CLI. Use memory, Markdown or SQLite storage.
- Follow board activity and see waiting messages and open questions in the
  mailbox. Inspect a stopped Markdown board with `malak recover`.
- Launch participants, exchange reports and link answers to questions with
  the shell recipes. Deliver inbox messages to Claude Code and Codex with hooks.
