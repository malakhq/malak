use crate::{
    ChannelName, CreationId, Durability, Grant, MessageId, ParticipantId, ParticipantName, Position,
};
use malak_record::Stamp;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) struct TailCut {
    pub(crate) last: Option<Position>,
    pub(crate) unparseable: bool,
    pub(crate) tampered: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl TailCut {
    pub(crate) fn has_bytes(self) -> bool {
        self.last.is_some() || self.unparseable
    }
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) enum Entry<'a> {
    Keys {
        record: &'a [u8; 32],
        board: &'a malak_proto::KeyPair,
        owner: &'a malak_proto::KeyPair,
        store: [u8; 16],
        location: Option<&'a malak_layers::StoreLocation>,
    },
    Lead {
        creation: CreationId,
        enrolled_at_ms: Option<u64>,
    },
    LocalLead {
        creation: CreationId,
        enrolled_at_ms: Option<u64>,
        key: &'a [u8; 32],
        uid: u32,
    },
    LocalEnrol {
        participant: &'a ParticipantId,
        name: &'a ParticipantName,
        grant: &'a Grant,
        inbox: CreationId,
        key: &'a [u8; 32],
        uid: u32,
        place: Option<&'a malak_layers::Source>,
        mechanism: &'a str,
        record: Option<&'a [u8]>,
        enrolled_at_ms: u64,
    },
    SocketUpdate {
        participant: &'a ParticipantId,
        generation: u64,
        key: Option<[u8; 32]>,
        record: Option<&'a [u8]>,
    },
    Complete,
    Run([u8; 16]),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    WitnessHanded(malak_journal::Checkpoint),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    WitnessPublished {
        at: malak_journal::Checkpoint,
        position: Position,
    },
    Enrol {
        participant: &'a ParticipantId,
        name: &'a ParticipantName,
        grant: &'a Grant,
        inbox: CreationId,
        enrolled_at_ms: u64,
    },
    Grant {
        participant: &'a ParticipantId,
        grant: &'a Grant,
        generation: u64,
        changed_at_ms: u64,
    },
    Revoke {
        participant: &'a ParticipantId,
        revoked_at_ms: u64,
    },
    Rekey {
        participant: &'a ParticipantId,
        generation: u64,
        key: [u8; 32],
        record: Option<&'a [u8]>,
    },
    Create {
        name: &'a ChannelName,
        creation: CreationId,
        creator: &'a ParticipantId,
    },
    Reserve {
        creation: CreationId,
        sender: &'a ParticipantId,
        position: Position,
        day: u64,
        bytes: u64,
    },
    Head {
        creation: CreationId,
        position: Position,
        stamp: Stamp,
        new_stamp: bool,
        bytes_kept: u64,
        record_bytes: u64,
        day: u64,
        sender: &'a ParticipantId,
        id: &'a MessageId,
        digest: &'a crate::record_fields::RetryDigest,
        durability: Durability,
        trace: Option<crate::TraceId>,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        lead_session: Option<&'a crate::LeadSession>,
    },
    Trim {
        creation: CreationId,
        floor: u64,
        bytes_kept: u64,
    },
    TrimDone(CreationId),
    Remove(CreationId),
    RemoveDone(CreationId),
    Cut {
        creation: CreationId,
        head: u64,
        attempt: u64,
        tail: Option<TailCut>,
    },
    CutDone {
        creation: CreationId,
        released: bool,
    },
    Damage {
        creation: CreationId,
        position: Position,
    },
    Ack {
        creation: CreationId,
        participant: &'a ParticipantId,
        position: Position,
    },
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod encode;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod start;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod location;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use start::{SavedKeys, StartupEntry};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod replay;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use replay::{Change, SavedHead};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod repair;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use repair::{
    AcceptedPlace, AcceptedPlan, Phase, PlanNumber, PlannedChannel, RepairChange,
};

pub(crate) const fn durability_code(durability: Durability) -> u8 {
    match durability {
        Durability::Memory => 0,
        Durability::Restart => 1,
        Durability::Synced => 2,
    }
}
