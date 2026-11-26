//! The embeddable board core for Malak.

mod authority;

fn epoch_millis(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
mod board;
mod changes;
mod channel;
/// The fixed Malak upgrade notice and its lead-facing changelog.
pub mod claim_notice;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod client;
mod entrance_slots;
mod frame_fields;
mod grant;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod handover;
mod journal_entry;
mod limits;
mod link;
mod message;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod participant_client;
mod quota;
mod readers;
mod record_fields;
mod reply;
mod reserved_names;
mod retention;
mod sender;
mod storage;
pub mod upgrade_notice;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod witness;
mod workers;

pub use authority::{Claim, ClaimEvent, ClaimState};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use board::{BoardStatus, ChannelStatus, ParticipantStatus};
pub use board::{
    Handle, LocalConfig, LocalEntrances, LocalOpening, LocalOwner, LocalParticipant, Owner,
};
pub use channel::SenderShareRule;
pub use grant::{Grant, GrantRule};
pub use limits::Limits;
pub use malak_layers::{
    Action, ChannelName, ChannelPattern, CreationId, Durability, Error, Grade, MessageId, Outcome,
    ParticipantId, ParticipantName, Position, ProcessIdentity, SenderTrace, SessionProcess,
};
pub use message::{Header, HeaderValue, Kind, Message, Receipt, SendContent, SendRequest, TraceId};
pub use reply::{GetReply, ReadChannel, ReadReply};
pub use retention::RetentionRule;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use sender::LeadSession;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use board::{LocalStartup, RecoveryReport};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use client::LocalClient;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use handover::{HandoverFile, HandoverTarget, HandoverWrite, OwnerCredential};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use participant_client::ParticipantClient;

pub(crate) fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|_| Error::unavailable("could not draw board randomness"))?;
    Ok(bytes)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod enrol_kill;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod durable_kill;
