# Owner status and event JSON

`malak --config PATH board status --json` returns one JSON object.
`malak --config PATH tail --json [--channel PATTERN]` streams one JSON
object per line. Both authenticate at the local owner door with the
credentials in the board's private state directory. The participant door
does not offer either operation. The owner wire protocol represents the
same fields as CBOR, with `run` in the status reply header and the status records in its
payload. The CLI adds the status JSON version and the locally recorded
running board version to the envelope. Tail metadata is in the event header.

A `tail` that receives a successful `STOP` event prints it and exits with status 0.

These shapes use `version: 1`. Fields may be added within a version.
Clients should accept unknown fields and event names. Positions and counters
are unsigned 64-bit integers. Run and creation IDs are lowercase hexadecimal
strings. `at_ms` is Unix epoch milliseconds.

## Upgrade notice owner operation

`UPGRADE_NOTICE` is the single exception to the rule that the owner door
never writes messages. It accepts the current `run`, `previous_build`,
`installed_build` and an optional `previous_entry` from the lead changelog.
Build IDs are distinct lowercase SHA-256 digests. `unknown` is accepted as
the previous build when its digest is unavailable.

The board posts a `note` to `lead` with both build IDs, the instruction to
re-read the Malak skill, and the relevant lines from its own changelog. The
stored sender is `owner` with ID `malakowner` and provenance from the owner
connection. Repeating the same request within the run uses the same message
ID. The owner door refuses requests for other destinations, caller-supplied
message content, and general `SEND`.

Reopening a board with a stored notice requires a build that recognizes the
owner sender in its journal.

## Move or restore a local board

1. Stop the source board with `malak --config OLD_CONFIG board stop` and
   wait for it to exit. Keep the private state directory and its store
   together. The CLI stores Markdown and SQLite data inside `state`, so
   move the whole directory or restore it from one consistent backup.
2. Set `state` in the destination configuration to the new absolute path.
   Keep `socket_folder` at its original absolute path, including on a
   replacement machine. The destination must be able to bind that path
   under the board's account. Preserve the private file permissions and
   ownership, including Unix user IDs used to restrict access. If the
   lead handover is outside `state`, carry it along separately and update
   `MALAK_HANDOVER` if its file path changes.
3. Start with `malak --config NEW_CONFIG board start --detach`. Check it
   with `malak --config NEW_CONFIG board status`. The saved owner
   credential and participant handovers still name the same sockets.

An owner command refuses a configuration whose `socket_folder` disagrees
with the saved owner credential. Starting an existing v0.1 board at a
different socket path is also refused. If the original path is unavailable,
a separate board at the new path requires a fresh state directory and new
participant enrolment.

Use the latest consistent backup. Restoring an older pair can bring back
revoked access and retired keys. If a restore rolls back authority, start
a fresh board and enrol participants again.

## Board status

```json
{
  "version": 1,
  "board_version": "0.1.2",
  "run": "00000000000000000000000000000001",
  "started_at_ms": 1795349000000,
  "layers": {"entrance": "local", "mechanism": "key", "store_durability": "synced"},
  "repair": null,
  "participants": [{
    "id": "lead", "name": "lead", "revoked": false,
    "grant": [null, [["**", 31]]],
    "fingerprint": null, "connections": 1,
    "enrolled_at_ms": 1795348000000, "grant_changed_at_ms": 1795348000000,
    "revoked_at_ms": null
  }],
  "channels": [{
    "name": "lead", "creation": "00000000000000000000000000000002",
    "floor": 1, "head": 12, "stopped": false, "readers": {"lead": 9}
  }]
}
```

- `board_version` is the running board's crate version or `null` when
  unavailable, including boards started by builds that predate version recording.
- `started_at_ms` identifies when this run started. Participant
  `enrolled_at_ms`, `grant_changed_at_ms` and `revoked_at_ms` are saved
  with the enrolment, grant change and revocation respectively. These
  values are Unix epoch milliseconds for display. A time can be null
  for a journal record written before the field existed. Revocation
  time is also null for an active participant.
- Derive canonical inbox names from the participant ID. The lead uses
  `lead` and other participants use `to/<id>`. An inbox may be absent.
  A participant is connected when its existing `connections` count is
  greater than zero. Count confirmed sends from the tail during the
  subscription.
- `channels[].readers` maps reader IDs to their last saved acknowledgements
  for this creation. An absent reader has no saved acknowledgement.
- `head` is the next assignable position. The retained positional backlog
  is `head - max(floor, ack + 1)`, saturating at zero and using zero for a
  missing ack. Positions that recovery marked void can fall within this
  range.
- With an optional target, status returns only the matching participant or
  channel. It still includes the run, layer and repair fields.

## Tail events

```json
{"version":1,"at_ms":1795350000000,"event":"stored","operation":"send","id":"lead","channel":"to/e314bd29556a40d9a500d976f8177ebf6","creation":"00000000000000000000000000000002","position":4,"kind":"task","outcome":"done","missed":0}
{"version":1,"at_ms":1795350001000,"event":"acked","operation":"ack","id":"lead","channel":"lead","creation":"00000000000000000000000000000002","position":10,"outcome":"done","missed":0}
{"version":1,"at_ms":1795350002000,"event":"denied","operation":"send","id":"e314bd29556a40d9a500d976f8177ebf6","channel":"lead","outcome":"unavailable","reason":"share_full (2 of 2 sender places, 512 of 1048576 sender bytes, 2 messages waiting for the reader). Wait for the reader, then retry with the same message ID.","count":1,"missed":0}
```

Every event has `version`, `event`, `at_ms` and `missed`. The remaining fields
appear when applicable. `id` is the acting participant or the participant
target of an owner action. `channel` identifies the destination for a send.
`creation` qualifies `position`. A store's `kind` is `task`, `note` or
`report`. An ack is emitted
when its saved cursor advances.

`stored` and `owner` events carry `sender_trace` with the same shape as
[message JSON](CLI-JSON.md#messages-and-read-summaries). The board gets these
details from the accepting transport. For an `owner` event the source is
the owner connection, while `id`, when present, is still the target participant.
`earlier_session` is available for a `stored` event when its sender's session
differs from the first session that sent a message as that participant in the run.

`outcome` is `done`, `refused`, `unavailable` or `not_supported`.
`reason` is an extensible string of up to 256 UTF-8 bytes. Existing reasons
can be human-readable. Consumers should display unfamiliar values and
escape them for the output medium. `denied` and `limit_hit` retain the
board's per-participant coalescing. `count` is the number of calls in the
batch, and the other fields describe the last call of that kind. If its
target channel could be validated, it appears as `channel`. A refusal at
a limit can contribute to both batches.

`missed` counts events dropped by the watcher queue since that watcher last
received an event. Refresh from status on a gap. A snapshot gives current
state, and the stream covers activity during the subscription. There is no
event replay. Use separate authenticated connections for a tail and status
requests. The owner tail provides no message bodies and no private keys.

When `malak board stop` succeeds, the board queues an owner event with
`operation: STOP`, `outcome: done` and the run ID as `target`. The board gives
all live tails one shared second to write their queued events to their
connections, then closes the connections even if a tail is still blocked.
The usual channel filters and queue limits apply during shutdown.

A viewer can optionally use the participant door for previews. Use the
participant's `MALAK_HANDOVER` for a normal `get` on the event's channel and
position. Display a short escaped preview only after a successful reply
whose creation matches the event or status entry. Omit previews when there
is no handover or the participant cannot retrieve the message.

The mailbox renders the saved cursor and retained position range even when
it cannot retrieve the message. Its sent count starts when the tail opens.
The full-screen view counts missed events explicitly and refreshes the
snapshot each second. The live renderer shows the board event time in UTC. An authorized
preview can supply the original timestamp for a waiting message.

`malak mailbox --who` shows the run start, enrolment, grant change,
revocation, last message and grant expiry in UTC. With a terminal it
updates as events arrive. With piped stdout it prints one roster. It
accepts `--for NAME`. The last-message column uses the latest store
observed from the participant during the subscription. For participants
with no observed store, it can look back through at most 200 positions per
channel. That lookup uses the participant handover and considers channels
the listed participant can send to and the viewer can get. Missing or
inaccessible times appear as a dash.
