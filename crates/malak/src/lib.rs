//! An embeddable message board for a lead and the participants that work for it.
//!
//! The board is a small router. It knows who is who, passes messages between
//! them with four verbs (`send`, `read`, `get` and `ack`), and records what each
//! participant has seen. A program that embeds it gets a direct handle for each
//! participant and needs no socket, command line or other process.
//!
//! ```
//! use malak::{Action, Grant, SendRequest};
//! use std::time::Duration;
//!
//! let owner = malak::in_memory()?;
//! let lead = owner.lead();
//! let mut grant = Grant::default();
//! grant.allow("lead".parse()?, [Action::Send]);
//! let worker = owner.enrol("worker", grant)?;
//!
//! worker.send(SendRequest::note("lead", "run-1", b"checks passed"))?;
//! let reply = lead.read(&["lead"], 1, Duration::ZERO)?;
//! let message = &reply.messages[0];
//! assert_eq!(message.sender, *worker.id());
//! lead.ack("lead", message.receipt.creation, message.receipt.position.get())?;
//! # Ok::<(), malak::Error>(())
//! ```
//!
//! `docs/EMBEDDING.md` in the repository walks through a lead and two helpers in
//! one process; `crates/malak/examples/team.rs` is the program it runs.
//!
//! # The public API
//!
//! Everything this page lists is the public API of the `malak` crate:
//!
//! - [`Owner`] and [`Handle`]: the board's administrative capability and one
//!   participant's capability. [`Owner::enrol`] creates a participant. The
//!   verbs are [`Handle::send`], [`Handle::read`], [`Handle::get`] and
//!   [`Handle::ack`].
//! - [`Owner::status`] and [`Owner::stop`]: read the board's state, and stop it
//!   cleanly. Dropping the [`Owner`] and every [`Handle`] ends an in-process board.
//! - The request, reply and name types the verbs use, such as [`SendRequest`],
//!   [`Message`], [`Grant`] and [`Limits`].
//! - [`layers`]: the four layer traits (store, transport, mechanism and
//!   observer) and the types they exchange, for programs that supply their own.
//! - [`stores`], [`transport`] and [`mechanism`]: the built-in layers. The
//!   memory store is always present; the others come with the `markdown`,
//!   `sqlite`, `unix` and `key` features.
//! - [`server`] (the `unix` feature): serves a board over any transport and
//!   mechanism. `crates/malak/examples/socket_board.rs` serves one over Unix
//!   sockets.
//! - [`client`]: attaches to a served board with a handover and offers `send`,
//!   `read`, `get` and `ack`. `crates/malak/examples/socket_client.rs` talks to
//!   the board above.
//!
//! # Stability
//!
//! There is no stability promise until 1.0. The public API may change in any
//! 0.x release. Every change to it is listed in `CHANGELOG.md` and visible in
//! `docs/public-api.txt`, the exact listing of this crate's public API. CI
//! compares the crate with that file (`scripts/public_api.py`), so a change is
//! always deliberate and recorded.
//!
//! Items marked `#[doc(hidden)]` and every other `malak-*` crate are internal:
//! the command line uses them. Behaviour, the wire protocol and the on-disk
//! formats follow the specification, not this crate's version.
//!
//! # Features
//!
//! The default build (`cli`) is the `malak` command and everything it uses. A
//! program that embeds the board turns the defaults off and picks layers:
//!
//! ```toml
//! malak = { version = "0.1", default-features = false, features = ["markdown"] }
//! ```

pub use malak_core::{
    Action, ChannelName, ChannelPattern, CreationId, Durability, Error, GetReply, Grade, Grant,
    GrantRule, Handle, Header, HeaderValue, Kind, Limits, Message, MessageId, Outcome, Owner,
    ParticipantId, ParticipantName, Position, ProcessIdentity, ReadChannel, ReadReply, Receipt,
    RetentionRule, SendRequest, SenderShareRule, SenderTrace, SessionProcess, TraceId,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use malak_core::{
    BoardStatus, ChannelStatus, Claim, ClaimEvent, ClaimState, LeadSession, ParticipantStatus,
};

/// The four layer interfaces and the types they exchange.
///
/// A layer can be replaced without changing the router: implement the trait and
/// hand it to the board.
pub mod layers {
    pub use malak_layers::{
        Candidate, Claim, Claimed, ClaimedSession, Enrolment, Event, EventKind, Handover, Identity,
        Interest, KeptRecord, Link, Mechanism, Observer, Ownership, Proves, ReadLimit, ReadResult,
        Received, Record, SenderOrigin, SenderPeer, Session, Side, Source, Step, Store, StoreInfo,
        StoreLocation, StorePlace, Transport, TransportInfo, Trim, Trimmed,
    };
}

/// The built-in stores.
pub mod stores {
    pub use malak_store_memory::MemoryStore;

    /// Markdown files under a private state folder (the `markdown` feature).
    #[cfg(all(feature = "markdown", any(target_os = "linux", target_os = "macos")))]
    pub use malak_store_markdown::MarkdownStore;

    /// SQLite database file (the `sqlite` feature).
    #[cfg(all(feature = "sqlite", any(target_os = "linux", target_os = "macos")))]
    pub use malak_store_sqlite::SqliteStore;
}

/// The built-in transport (the `unix` feature).
#[cfg(all(feature = "unix", any(target_os = "linux", target_os = "macos")))]
pub mod transport {
    pub use malak_transport_unix::UnixTransport;
}

#[cfg(all(feature = "unix", any(target_os = "linux", target_os = "macos")))]
pub mod server;

/// Connect to a served board as a participant or the lead.
///
/// A [`Client`](client::Client) attaches with a handover over any
/// [`Transport`](layers::Transport) and offers the four verbs with the types
/// [`Handle`] uses.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod client {
    pub use malak_core::ParticipantClient as Client;
}

/// The built-in mechanism (the `key` feature).
#[cfg(all(feature = "key", any(target_os = "linux", target_os = "macos")))]
pub mod mechanism {
    pub use malak_mechanism_key::Key;
}

/// Start an in-process board using default limits and the memory store.
pub fn in_memory() -> Result<Owner, Error> {
    Owner::in_memory(stores::MemoryStore::default(), Limits::default())
}

/// Create a new Direct board with Markdown storage under its private state folder.
///
/// When `sync` is enabled, successful writes reach stable storage before returning.
#[cfg(all(feature = "markdown", any(target_os = "linux", target_os = "macos")))]
pub fn create_durable(state: impl AsRef<std::path::Path>, sync: bool) -> Result<Owner, Error> {
    let state = state.as_ref();
    let store = stores::MarkdownStore::new(state.join("messages"), sync);
    Owner::create_durable(store, Limits::default(), state)
}

/// Reopen an existing Direct board and its original Markdown store.
///
/// The board verifies the witness before opening the journal for new writes.
#[cfg(all(feature = "markdown", any(target_os = "linux", target_os = "macos")))]
pub fn restart_durable(state: impl AsRef<std::path::Path>, sync: bool) -> Result<Owner, Error> {
    let state = state.as_ref();
    let store = stores::MarkdownStore::new(state.join("messages"), sync);
    Owner::restart_durable(store, Limits::default(), state)
}
