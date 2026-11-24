//! A local board for passing messages between a lead and its participants.
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

pub use malak_core::{
    Action, ChannelName, ChannelPattern, CreationId, Durability, Error, GetReply, Grade, Grant,
    GrantRule, Handle, Header, HeaderValue, Kind, Limits, Message, MessageId, Outcome, Owner,
    ParticipantId, ParticipantName, Position, ProcessIdentity, ReadChannel, ReadReply, Receipt,
    RetentionRule, SendRequest, SenderShareRule, SenderTrace, SessionProcess, TraceId,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use malak_core::{BoardStatus, ChannelStatus, LeadSession, ParticipantStatus};

/// Start an in-process board using default limits and the memory store.
pub fn in_memory() -> Result<Owner, Error> {
    Owner::in_memory(
        malak_store_memory::MemoryStore::default(),
        Limits::default(),
    )
}

/// Create a new Direct board with Markdown storage under its private state folder.
///
/// When `sync` is enabled, successful writes reach stable storage before returning.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn create_durable(state: impl AsRef<std::path::Path>, sync: bool) -> Result<Owner, Error> {
    let state = state.as_ref();
    let store = malak_store_markdown::MarkdownStore::new(state.join("messages"), sync);
    Owner::create_durable(store, Limits::default(), state)
}

/// Reopen an existing Direct board and its original Markdown store.
///
/// The board verifies the witness before opening the journal for new writes.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn restart_durable(state: impl AsRef<std::path::Path>, sync: bool) -> Result<Owner, Error> {
    let state = state.as_ref();
    let store = malak_store_markdown::MarkdownStore::new(state.join("messages"), sync);
    Owner::restart_durable(store, Limits::default(), state)
}
