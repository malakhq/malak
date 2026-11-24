# CLI JSON output

The participant commands below write their results to stdout. Errors use
stderr and the usual exit status. A successful GET miss has exit status 0.
A successful streaming READ always ends with a channel summary. If READ
fails partway through, earlier message lines can already be on stdout.

## Send receipts

`malak send --json` emits one object.

```json
{"id":"job-1","creation":"00000000000000000000000000000001","position":1,"durability":"restart","duplicate":false,"trace":"a1b2"}
```

`id` is the message ID, `creation` is the channel creation, and `position`
is a positive integer. Durability is `memory`, `restart` or `synced`.
`duplicate` is true on a successful retry. The optional `trace` is the
original trace, including its original case. Legacy receipts omit it.

## Messages and read summaries

`malak get CHANNEL POSITION --json` emits one message object on a hit.
`malak read CHANNEL... --json` emits one such object per line and ends
with a single object containing `channels`, even when there are no
messages.

```json
{"id":"job-1","channel":"lead","sender":"lead","sender_name":"lead","grade":"proven","kind":"task","creation":"00000000000000000000000000000001","position":1,"durability":"restart","sent_at":[1795350000,0],"headers":{"trace":"a1b2"},"body":[79,75],"text":"OK"}
{"channels":[{"channel":"lead","creation":"00000000000000000000000000000001","voids":0},{"channel":"absent","voids":0}]}
```

- `sent_at` contains Unix seconds and nanoseconds. `grade` is `proven`
  or `link_only`. `kind` is `task`, `report` or `note`.
- `headers` maps keys to strings, except `reply_to`, which is an array
  containing channel, creation and position. `lost_before`, when present,
  counts void positions before the message.
- `body` is an array of integer bytes from 0 through 255. `text` is present
  exactly when the full body is valid UTF-8.
- `sender_trace` is the board's observation of the sending connection. It
  includes `transport` (`unix`, `vsock` or `in_process` for the built-in
  paths). Unix messages can also have `pid`, `program`, `cwd` and `session`.
  A session has `pid`, `program` and `start_time`. Start time is in ticks
  since boot on Linux and microseconds since the Unix epoch on macOS.
  `earlier_session`, when present, has the same shape and identifies the
  differing session that sent the participant's first message in the
  running board. Unavailable details are omitted. Older records omit
  `sender_trace` altogether.
- Each channel summary includes `channel` and `voids`. A present channel
  also includes `creation`. `trimmed_before` appears when the read passes
  trimmed history and gives the first position retention has kept.

## Get misses

A miss emits one object with these fields.

| Result | Fields |
| --- | --- |
| `absent` | `result`, `channel` |
| `never_written`, `void` | `result`, `channel`, `creation`, `position` |
| `trimmed` | `result`, `channel`, `creation`, `position`, `trimmed_before` |

`position` is the requested position. `trimmed_before` is the retention floor.

## Agent presentation

Adding `--agent` makes READ and GET emit one bounded JSON object with a
`messages` array. READ includes the same `channels` array beside
`messages`. GET includes the miss fields beside an empty `messages` array
when the requested position has no message.

An agent message includes `sender`, `sender_name`, `grade`, `kind`,
`channel`, `creation`, `position`, and optional `trace`, `sender_trace`
and `lost_before` fields. `presentation` is `task` for a proven task from the lead,
or `information` otherwise. Its displayed `body` is a string.
`body_encoding` is `utf8` or `hex` and `body_bytes` is the original
byte length. When shortened, the `fetch` object gives `tool` (`get`),
`channel`, `position` and the next byte `offset`.

`--offset BYTES` requires `--agent` and selects a byte offset in the
first returned body. Later READ messages start at byte zero.
For UTF-8, the offset must fall on a character boundary. Use the
addressed `get --agent --offset` command to continue a shortened body.

## Other JSON commands

- `enrol --json` returns one object with the participant `id` and
  the written `handover` path, both strings.
- `board status --json` returns `board_version` as a crate version string
  or `null` when unavailable. The numeric `version` field remains the
  JSON format version. Version mismatch warnings go to standard error.
  See the [owner JSON formats](OWNER-API.md) for the status and `tail --json` shapes.
- `mailbox --questions --json` returns an array of open questions.
  Each entry carries `kind` (`owner` or `agent`), integer `position`,
  `sender`, integer Unix milliseconds `at_ms` and string `first_line`.
- `mailbox --waiting --json` returns one object with a `waiting` array.
  Each entry has a `participant` name, a canonical `inbox` and a
  `messages` array. Each message has an integer `position` and string
  `kind`. The `waiting` array is empty when no active worker inbox has an
  unacknowledged lead message. Add `--for NAME` to filter by participant.
- `mailbox --trace ID --json` prints one message object per line and exits.
  It uses the `read --json` message fields and orders matching retained
  messages by `sent_at`, then channel and position. Trace IDs are matched
  exactly. An empty result leaves standard output empty. Notices go to
  standard error.
- `hook --describe --json` returns one object with `environment`,
  `context_byte_limit`, `events` and `monitor`.
