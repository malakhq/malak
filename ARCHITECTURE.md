# Malak architecture

For in-process calls, the core issues a Direct handle tied to one participant. In v0.1, a participant must authenticate on a new connection when an existing connection expires. If the participant's grant has ended, the owner must extend it before the participant can attach again.

## Workspace and API

- **Tower** keeps integration traits in small crates. Malak puts store, transport, mechanism and observation contracts in `malak-layers`. The core and each implementation depend on that crate. The embedding application selects implementations. [Tower layer crate](https://tower-rs.github.io/tower/tower_layer/)
- **Quinn** drives its deterministic protocol core with input from a separate I/O layer. Malak keeps handshake, framing, connection transitions and timers in the core. The driver supplies bytes, elapsed time and write progress. Protocol tests can drive the same transitions without sockets or sleeps. [Quinn architecture](https://github.com/quinn-rs/quinn#overview) · [Sans-I/O](https://sans-io.readthedocs.io/)
- **Rustls** separates cryptographic operations from the networking driver. Malak keeps the fixed Noise exchange in the protocol domain and asks the mechanism only to prove the claimed participant within the core's checks. [Rustls design overview](https://docs.rs/rustls/latest/rustls/#design-overview)
- **Tokio** groups related packages under a virtual Cargo workspace. Malak shares a lockfile and verification commands. Each layer implementation can be built on its own once added. [Tokio manifest](https://github.com/tokio-rs/tokio/blob/master/Cargo.toml) · [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)
- Use validated value types at domain boundaries, private fields for core invariants, and explicit re-exports for public entry points. Document every public item. [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html)
- Use typed errors implementing `std::error::Error`. Handwritten implementations are sufficient for the small set of shared outcomes. A derive such as `thiserror` can remove boilerplate later while preserving the API. [thiserror design](https://github.com/dtolnay/thiserror#details)

## Domain boundaries

Names, messages and outcomes are shared value types. The core owns authority and handle identity, the call path and verbs, the protocol, the journal and record checks, recovery, and the owner door. The journal decides which records are current. Its record integrity primitives live in `malak-record`. Stores hold opaque bytes and report durability. Transports carry bytes and report peer information. Mechanisms answer whether a claimed identity is proven. Observation consumes events. The core calls each layer through its contract. The store test kit exercises the [store interface](crates/malak-layers/src/store.rs). It allows a failed append to leave a prefix of whole records and allows gaps between positions while testing write ordering, exact round trips and stale-writer rejection.

## Direct calls

The in-process board checks validity, counts the call, validates its shape and checks every named channel before looking up history. The authority lock orders owner saves and validity checks. A channel's guard orders stamps, writes and retries. The quota ledger counts both committed records and writes that can still take effect. Store workers hold no authority locks. The `malak` facade supplies the default memory store through the same interface as an embedding program supplying a custom store.

## Dependencies

- The Rust standard library and the internal dependency from `malak-core` to `malak-layers` provide the workspace and shared contracts.
- `mime` validates media types in `content_type`. The message keeps the sender's original spelling for the retry check. [Hyperium MIME parser](https://github.com/hyperium/mime)
- `bytes` lets the store pass immutable opaque records and repair candidates without copying their payloads. [Tokio Bytes](https://github.com/tokio-rs/bytes)
- `getrandom` draws store, board-run, channel-creation and participant IDs and the board’s record key from the operating system. [getrandom documentation](https://docs.rs/getrandom/0.3/getrandom/)
- `hmac` and `sha2` implement the record HMAC and body hash in `malak-record`. The same key has distinct message and witness domains. [RustCrypto HMAC](https://docs.rs/hmac/) · [RustCrypto SHA-2](https://docs.rs/sha2/)
- The CLI hook uses the same `sha2` dependency to find existing per-session receipts left by the Python recipe. [RustCrypto SHA-2](https://docs.rs/sha2/)
- `criterion` measures the record check in release builds and is a development dependency only. [Criterion](https://bheisler.github.io/criterion.rs/book/)
- `cobs` frames journal entries so replay can find a later complete entry after damaged bytes without interpreting a payload byte as a delimiter. `sha2` supplies the running hash and the checksum. [COBS documentation](https://docs.rs/cobs/)
- `snow` implements the fixed Noise IK exchange in `malak-proto`, with only the crypto features for X25519, ChaCha20-Poly1305 and BLAKE2s. `zeroize` clears the wrapper's private key and handover buffers on drop. [Snow features](https://docs.rs/crate/snow/0.10.0/source/Cargo.toml.orig) · [Noise protocol](https://noiseprotocol.org/noise.html) · [zeroize](https://docs.rs/zeroize/)
- `minicbor` reads CBOR primitives. Malak enforces definite lengths, bounded nesting, text keys and unique keys before a decoded frame can reach the connection state machine. [minicbor decoder](https://docs.rs/minicbor/latest/minicbor/decode/struct.Decoder.html)
- `parking_lot` gives the core a timed, owned channel guard. The core hands that guard to a store worker so its write can settle after the caller's deadline. Authority saves register ahead of new validity checks. [parking_lot synchronization primitives](https://docs.rs/parking_lot/0.12.5/parking_lot/)
- `arc-swap` publishes immutable owner status snapshots. A status request can read them while the authority save lock is held. [ArcSwap](https://docs.rs/arc-swap/)
- `fs4` checks available space on the board's volume. A memory board uses the volume it started on. [Available-space API](https://docs.rs/fs4/latest/fs4/fn.available_space.html)

- `nix` reads Unix peer credentials and bounds kernel socket buffers. The transport returns these facts through the layer contract. Entrance admission remains in the core. [Unix socket options](https://docs.rs/nix/latest/nix/sys/socket/sockopt/)
- `smallvec` holds small record metadata and the single-channel read plan inline. The same code handles larger metadata and multi-channel reads. [SmallVec](https://docs.rs/smallvec/)
- `strsim` supplies Levenshtein distance for inbox typo suggestions in board authority and the owner's inventory-only creation lookup. It was already in the lockfile, and avoids maintaining two edit-distance algorithms. Board-side suggestions keep Direct, socket and MCP clients consistent without a grant-discovery API. Only inboxes the caller's send/read/get/ack grants cover contribute candidates; owner inventory already includes every participant. Resolved calls do not compute distances. [strsim](https://docs.rs/strsim/0.11.1/strsim/fn.levenshtein.html)
- The state folder uses descriptor-relative access and a directory lock. The macOS adapter checks extended ACLs on the open descriptor and requests the full flush for durable state. [Nix file operations](https://docs.rs/nix/latest/nix/fcntl/) · [Apple ACL retrieval](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/acl_get_fd.3.html) · [Apple full flush](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fcntl.2.html)
- Handover ACLs use Darwin extended ACLs and Linux POSIX ACLs on open file descriptors. A separate recipient's search access is checked against the pinned ancestors. The temporary handover gets its final read ACL before the board writes the first byte. [Darwin descriptor ACLs](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/acl_set_fd_np.3.html) · [Linux POSIX ACL layout](https://github.com/torvalds/linux/blob/master/include/uapi/linux/posix_acl_xattr.h)
- Unix sender tracing reads Linux procfs and uses the existing `libc` bindings from Nix for macOS libproc. [Linux procfs](https://docs.kernel.org/filesystems/proc.html) · [Apple libproc](https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/wrappers/libproc/libproc.h)
- `rustix` exposes the exclusive rename on Linux and macOS. A new handover is first written and synced at a private sibling, then published without overwriting an existing destination. [Rustix rename flags](https://docs.rs/rustix/latest/rustix/fs/fn.renameat_with.html)
- `data-encoding` supplies canonical encodings for the Markdown store. File names use one case for case-insensitive volumes. Record lines use base64 to keep the marker out of stored payloads. [data-encoding](https://docs.rs/data-encoding/)

## Markdown repair

A repair claim enumerates the store's channels and scans every record marker independently. A rewrite compares the approved input digest, verifies the exact encoded bytes to keep, and syncs a set-aside copy before replacing the channel.

While the board is stopped, `malak recover` reports the approved inputs and the plan number. `recover --apply NUMBER` repeats the look before accepting a new plan. The accepted plan records the store ID, place and each channel's rewrite progress. `recover --withdraw NUMBER` opens only the journal.

At first initialization the core records the Markdown store's local place in the private journal. The store identifies it from the already open directory descriptor using the filesystem's UUID or its nonzero `f_fsid` when a UUID is unavailable, plus the directory's inode number. Local repair requires both identities to match and the owner to pass `--owner-vouches-same-volume` on each `--apply` to confirm that no copy or in-place restore has occurred. An interrupted local repair remains pending across board startup. Boards created before the local place was recorded use host attestation for repair. [Darwin volume attributes](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/getattrlist.2) · [Linux filesystem UUID ioctl](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h) · [Descriptor-based filesystem ID](https://www.man7.org/linux/man-pages/man2/statfs.2.html)

The optional `board.store_place_socket` supplies a host mark bound to the filesystem UUID and directory inode. It also permits automatic continuation of a matching accepted plan on startup.

Set `store_place_socket` to the absolute path of a trusted Unix stream socket outside the board's state directory. The socket and its peer must belong to root or the board account. The service receives one ASCII line in the format `malak-place-v1 UUID DEVICE INODE`, with a lowercase hexadecimal UUID and decimal device and inode numbers. The requesting machine's device number identifies the live attachment to the host. The service replies with `malak-place-v1 MARK`, where the mark is 64 lowercase hexadecimal characters and nonzero, or with `not-established`. Both lines end in a newline.

The service's attachment history lives outside the volume's rollback domain. It vouches only when it can verify that this is the same attachment and that no in-place restore has occurred since the mark was issued. It must invalidate the mark before a restore or other uncertain transition, and coordinate those transitions with active repairs. It issues a different mark for a new attachment once continuity is established. When configured, missing or inconclusive host evidence permits a look. Application requires the host to attest again.

## Local command line

The `malak` binary composes the Unix transport, the Key mechanism and the Markdown store. It reads `--config PATH` or the per-user `malak/board.toml` under `XDG_CONFIG_HOME` or `~/.config`. Each config file and its ancestors are checked through open descriptors. An example configuration for the current same-account setup follows. The directories' parents must exist and only the board account and system administrator may change them.

```toml
[board]
state = "/path/to/malak/state"
socket_folder = "/path/to/malak/doors"
sync = true
```

Run `malak board start --config PATH` in the foreground. Starting it again with the same configuration reopens the saved state and credentials. Owner commands use the same config and read the private owner credential from the state folder. `malak enrol NAME --out FILE` prints the assigned ID and output path. Use `malak grant ID lead:send to/ID:read,get,ack` to give the worker access to the lead's inbox and its own. Set `MALAK_HANDOVER` explicitly for each participant command. The lead handover defaults to `lead.handover` in the state folder. Use `--creation` with `ack` to identify the creation ID returned by `read`. Message JSON uses one object per line, with each body encoded as an array of bytes.

In the same-account setup, any program running as the board account can also read its credentials and use the owner door. This setup trusts the lead and all other programs sharing that account.

For a separate lead account, set `board.lead_account` or pass `--lead-account USER` each time the board starts. Supply `--lead-handover` at a path outside the private state folder. The board account must control the handover and socket ancestors, and the lead account must be able to traverse those ancestors. The lead entrance directory admits the board and the named lead account. The owner entrance and state remain private to the board. The general entrance admits local connections and authenticates them separately. Use `enrol --account USER` for a participant's handover, and `--only-from USER` to separately bind the participant's key to that source account.

To run the two real-account tests, set `MALAK_TEST_SECOND_ACCOUNT` to a separate unprivileged OS account. The test runner needs passwordless `sudo -n -u` access to run the probe as that account. Set `MALAK_TEST_SHARED_ROOT` if the second account cannot traverse the board account's home. Only the board account or the system administrator may be able to change the supplied directory or its ancestors. The second account needs permission to traverse them, and the resulting socket path must fit the Unix socket path limit. The default test run marks the two account tests as ignored with the variable name in the reason.

The CLI uses `clap` for argument parsing, `serde` and `toml` to read the config, and `serde_json` for structured output. `getrandom` provides generated message IDs, and `nix` reads the OS descriptor limit and resolves local accounts. [Clap](https://docs.rs/clap/) · [Serde](https://serde.rs/) · [TOML parsing](https://docs.rs/toml/latest/toml/fn.from_str.html) · [Serde JSON](https://docs.rs/serde_json/) · [Nix resource limits](https://docs.rs/nix/latest/nix/sys/resource/fn.getrlimit.html)

The third-account entrance test additionally reads `MALAK_TEST_THIRD_ACCOUNT`. All three accounts must be distinct. The board test runner runs the probe as that account with `sudo -n -u`. The shared root must let both test accounts traverse the path to the fixture.

## SQLite persistence

Set `board.persistence = "sqlite"` to keep history in the private `sqlite`
directory inside the state folder. The state journal continues to decide
which records are current. SQLite holds the opaque payload and its position
under the exact channel key. The store reports `synced` and uses WAL,
`synchronous=FULL` and macOS full flushes for commits and checkpoints.
`board.sync` controls Markdown writes. SQLite repair claims return
`not_supported` in v0.1.
[SQLite WAL](https://www.sqlite.org/wal.html) · [SQLite sync settings](https://www.sqlite.org/pragma.html#pragma_synchronous)

The store holds an OS lock on its dedicated directory while claimed. An
immediate SQLite transaction checks the persisted writer generation and
changes the rows and append boundary together. Claims and settles checkpoint
the WAL. Front trims preserve the append boundary, and tail cuts lower it.
The database and its sidecars must live on a local filesystem that honors
locking and sync. `rusqlite` uses the bundled SQLite build so the supported
platforms use the same SQLite behavior.
[Rusqlite connection API](https://docs.rs/rusqlite/0.41.0/rusqlite/struct.Connection.html)

## Agent bridges

`malak mcp` serves the four participant verbs over stdio JSON-RPC. Configure
the agent host to start that command with the participant's
`MALAK_HANDOVER`. The bridge attaches once and keeps its connection across
tool calls. After a connection failure, the next call can reconnect with the
current handover. Retrying a send with its original ID preserves the board's
deduplication behavior.

The `read` and `get` tools return structured message fields with a JSON text
fallback. For scripts, `malak read --agent` and `malak get --agent` render a
bounded text view. Add `--json` to read the same presentation as fields. The
message label identifies verified task provenance. When the body is longer than
the display budget, the result gives its byte size and a fetch offset. An
agent should acknowledge only positions that it has handled after reading.
[MCP stdio](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#stdio) · [MCP tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)

## Mailbox

The core's `board/status.rs` publishes owner snapshots, including the
reader cursors updated after a saved acknowledgement. `board/events.rs`
and `board/local/owner/` carry body-free activity across the owner door.
The owner CLI exposes that data as documented JSON. [Owner API](docs/OWNER-API.md)

The CLI's `mailbox/model.rs` folds those records into lanes, a waiting
list and a bounded feed using plain values. `mailbox/render.rs` draws
the pinned panes and formats the same event lines for stdout. `mailbox.rs`
owns the connections, clocks and terminal loop. It uses the existing
participant door for optional previews with the viewer's handover.

`ratatui` supplies terminal layout and buffered rendering. Its Crossterm
backend handles raw mode, key events and resizing. The dependency is
limited to the CLI's supported local platforms. [Ratatui backends](https://ratatui.rs/concepts/backends/) · [Crossterm events](https://docs.rs/crossterm/0.29.0/crossterm/event/)

The CLI uses the workspace's Rustix dependency to measure the terminal
attached to standard error when standard output is redirected.

`mailbox/who.rs` renders the lifecycle roster and uses the same participant
GET path to find a bounded historical last-message time. The core saves
each participant timestamp with its existing journal change. Replay
accepts the previous entry shapes and leaves missing times unknown.
The CLI uses `time` to show timestamps in the local timezone, with an explicit UTC offset.
[Time crate](https://docs.rs/time/0.3.55/time/)

## Firecracker transport

The Linux guest uses an AF_VSOCK stream with host CID 2. Firecracker forwards
guest-initiated connections to a host Unix listener at `uds_path_PORT`. For
host-initiated connections, Malak reads the VMM's bounded CONNECT reply before
passing the remaining bytes to the existing Noise and framing code. Unix and
vsock links share the same stream send, receive and close implementation.
[Firecracker vsock](https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md) · [Linux vsock](https://man7.org/linux/man-pages/man7/vsock.7.html)

A Firecracker runner provisions the durable board with `LocalStartup` using
Unix addresses for the owner and lead and `vsock:2:PORT` as the guest-visible
general address. The runner captures the foreground VMM child with
`Vmm::capture`, opens a distinct `VsockTransport::bind_verified` route
for that launch, and passes accepted links to the general entrance.
Unix peer credentials and the VMM's live pidfd identify the verified route.
A VM-specific handover from `LocalEntrances::enrol_for_vm` binds its key to
that launch's ID in the state journal. The runner installs the handover
privately in the guest. Guest `malak` and `malak mcp` clients select vsock
from its address. The runner owns each route's socket directory and removes
its listener when the VM ends.

## Mailbox sender shares

An optional rule limits each sender's messages and record bytes that are
still due to the designated reader. On an inbox, that reader is its owner.
A shared channel can name a reader explicitly. Configure the bounds for
a channel or pattern in the board configuration.

```toml
[[board.sender_shares]]
pattern = "lead"
channel_messages = 1000
sender_messages = 100
sender_bytes = 1048576
```

These numbers illustrate an explicit rule. An ACK by the designated reader
immediately frees the sender's share. The history still follows age and
front-only count retention. At the count limit a new send needs a prefix
that the designated reader has acknowledged or that has expired. A sender
can retry a stored ID even when its share is full. The CLI exits 3 on
`share_full` and `inbox_full`. MCP returns the same reason and owner tail
shows a denied send. The admission and replay paths use the existing
channel write section and saved reader position.
