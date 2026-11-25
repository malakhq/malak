# Embedding a Malak board

A Rust program can run a Malak board inside itself. There is no socket, no
command line and no second process: the program owns the board and gives each
participant a handle. The router is the same one the `malak` command runs, with
the same rules for who may do what.

There is no stability promise until 1.0. [`public-api.txt`](public-api.txt)
lists the exact public API, and [CHANGELOG.md](../CHANGELOG.md) lists each
change to it.

## Add the crate

Turn off the default features so the build is the board and nothing else.

```toml
[dependencies]
malak = { version = "0.1", default-features = false }
```

| Feature | Adds |
|---|---|
| `markdown` | The Markdown store and `malak::create_durable` / `malak::restart_durable` |
| `sqlite` | The SQLite store |
| `unix` | The Unix socket transport |
| `key` | The key mechanism |
| `cli` (default) | The `malak` command, mailbox screen and recipes, with all of the above |

The board alone, with the memory store, links to about 0.5 MB in a
size-optimised build (`scripts/board-size` measures it and CI keeps it under a
budget). It needs no Python.

## The model

- `malak::in_memory()` starts a board and returns an `Owner`, the program's
  administrative capability.
- `owner.lead()` returns the lead's `Handle`. `owner.enrol(name, grant)` enrols a
  participant and returns its `Handle`. A grant says which channels the
  participant may send to, read, get from and acknowledge.
- A `Handle` has the four verbs: `send`, `read`, `get` and `ack`. Every call is
  checked against the caller's grant and the board's limits.
- A new board holds the lead's inbox, `lead`. Enrolling a participant creates
  its inbox, `to/<id>`. Any other channel, such as `questions` or `orders`, you
  create with `owner.create_channel`. Delegation is a message on a channel.

## A lead and two helpers

[`crates/malak/examples/team.rs`](../crates/malak/examples/team.rs) runs the
whole flow in one process:

```sh
cargo run -p malak --example team --no-default-features
```

It follows these steps.

1. **Enrol.** Each helper gets a grant for `lead`, `questions` and, once it
   exists, its own inbox. A new participant's inbox name is known only after
   `enrol`, so the example adds the inbox rule with `owner.replace_grant`.
2. **Send.** The lead sends each helper a `Kind::Task` in its inbox.
3. **Read and ack.** A helper calls `read` with a wait, then `ack` with the
   message's `receipt.creation` and `receipt.position`. Reading does not
   acknowledge a message; `ack` does.
4. **Report.** One helper sends a `Kind::Report` to `lead`.
5. **Ask.** The other helper sends a note to `questions`. The channel has to
   exist, so the program creates it first with `owner.create_channel`.
6. **Answer.** The lead replies on `questions` with a `reply_to` header that
   names the question's channel, creation and position. The helper finds it by
   that header.
7. **Stop.** `owner.status(None, None).run` names the board's run, and
   `owner.stop(run)` stops it and releases its store. Dropping the `Owner` and
   every `Handle` also ends an in-process board.

The program asserts each result, and CI runs it, so the guide cannot drift from
the code.

## Durable boards

With the `markdown` feature, `malak::create_durable(folder, sync)` keeps the
board's history in a private state folder, and `malak::restart_durable` reopens
it after verifying the witness. The memory store keeps nothing across restarts.

## Your own store

The four layers are traits in `malak::layers`: `Store`, `Transport`, `Mechanism`
and `Observer`. `Owner::in_memory(store, limits)` and `Owner::create_durable`
accept any `Store`. A store implements five functions. The router checks every
grant and limit before it calls the store, so a store never checks them.

## Serve the board over a transport

With the `unix` feature, `malak::server::Server` accepts links for a board
over any `Transport` and proves callers with any `Mechanism`. It adds no layer:
the transport moves bytes, the mechanism proves identity, and the router is the
one the `malak` command runs.

1. `Server::create(store, limits, state, startup, mechanism, lead_uid)` makes a
   durable board and returns its `Owner` and the `Server`. `startup` names the
   owner, lead and general addresses and where the lead's credentials go.
2. `server.listen(entrance, transport, address, capacity)` starts one thread
   that accepts links at an entrance. A link beyond `capacity`, or one that
   arrives before `server.ready()`, is dropped.
3. `server.ready()` lets the board serve what the listeners accept, and
   `server.run()` blocks until the owner stops the board, a `StopHandle` is
   used, or a listener fails.

[`examples/socket_board.rs`](../crates/malak/examples/socket_board.rs) serves a
board over Unix sockets in about thirty lines, and `tests/layers.rs` serves one
over an in-memory transport and a passphrase mechanism written outside the
core. The server's size is tracked by `scripts/board-size`.

## Connect to a served board

`malak::client::Client` is the other end of a served board. `Client::connect(transport,
path, mechanism)` reads a handover file, dials the address in it, performs the
handshake and attaches. Pass the handover's mechanism for a participant and `None` for
the lead. The client then offers `send`, `read`, `get` and `ack` with the types
`Handle` uses, so code written against an in-process board moves to a served one by
changing how it connects. A failed call closes the connection; connect again to
continue.

[`examples/socket_client.rs`](../crates/malak/examples/socket_client.rs) attaches to
the board `socket_board` serves, and `tests/layers.rs` does the same over an
in-memory transport and a passphrase mechanism, with both ends written against the
public API.

## What is not public yet

The constructors that the server uses, `create_local` and the `Local*` types, stay
`#[doc(hidden)]`. The owner door has no public client: the `malak` command is its
client.
