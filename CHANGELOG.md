# Changelog

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
