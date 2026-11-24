//! Shared types and extension points for Malak layers.

mod names;
mod outcome;
mod values;

pub use names::{ChannelName, ChannelPattern, MessageId, ParticipantId, ParticipantName};
pub use outcome::{Error, Outcome};
pub use values::{Action, CreationId, Durability, Grade, Position};

mod mechanism;
mod observe;
mod sender;
mod store;
mod transport;

pub use mechanism::{Enrolment, Handover, Identity, Mechanism, Proves, Session, Side, Step};
pub use observe::{Event, EventKind, Interest, Observer};
pub use sender::{
    ClaimedSession, ProcessIdentity, SenderOrigin, SenderPeer, SenderTrace, SessionProcess,
};
pub use store::{
    Candidate, Claim, Claimed, KeptRecord, Ownership, ReadLimit, ReadResult, Record, Store,
    StoreInfo, StoreLocation, StorePlace, Trim, Trimmed,
};
pub use transport::{Link, Received, Source, Transport, TransportInfo};
