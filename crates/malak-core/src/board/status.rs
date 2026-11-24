use super::{Board, Owner, Table};
use crate::authority::ParticipantView;
use crate::channel::Channel;
use crate::{ChannelName, CreationId, Grant, ParticipantId, ParticipantName};
use malak_layers::StoreInfo;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub(super) struct Published {
    participants: Vec<ParticipantView>,
    channels: Vec<Arc<Channel>>,
}

/// The owner-visible state of a participant in a board run.
#[derive(Debug)]
#[non_exhaustive]
pub struct ParticipantStatus {
    /// Board-assigned participant identity.
    pub id: ParticipantId,
    /// Label assigned when this participant was enrolled.
    pub name: ParticipantName,
    /// Whether this participant has been permanently revoked.
    pub revoked: bool,
    /// Complete current grant, including its expiry if set.
    pub grant: Grant,
    /// Lowercase SHA-256 of the registered public key, when one exists.
    pub fingerprint: Option<String>,
    /// Number of active attached links.
    pub connections: usize,
    /// Time of enrolment in Unix epoch milliseconds, when recorded.
    pub enrolled_at_ms: Option<u64>,
    /// Time of the last change to the grant in Unix epoch milliseconds, when recorded.
    pub grant_changed_at_ms: Option<u64>,
    /// Time of revocation in Unix epoch milliseconds, when recorded.
    pub revoked_at_ms: Option<u64>,
}

/// The owner-visible state of a channel creation.
#[derive(Debug)]
#[non_exhaustive]
pub struct ChannelStatus {
    /// Current channel name.
    pub name: ChannelName,
    /// Identity of this creation of the name.
    pub creation: CreationId,
    /// Earliest position retained by the board.
    pub floor: u64,
    /// Next assignable position.
    pub head: u64,
    /// Whether damage has stopped this creation.
    pub stopped: bool,
    /// Last saved acknowledgement for each reader with an acknowledgement.
    pub readers: Arc<BTreeMap<ParticipantId, u64>>,
}

/// A view of a running board that does not wait for authority or store work.
#[derive(Debug)]
#[non_exhaustive]
pub struct BoardStatus {
    /// Identity of this specific board run.
    pub run: CreationId,
    /// Time this board run started, in Unix epoch milliseconds.
    pub started_at_ms: u64,
    /// Capabilities of the configured store.
    pub store: StoreInfo,
    /// The participants selected by the status query.
    pub participants: Vec<ParticipantStatus>,
    /// The current channel creations selected by the status query.
    pub channels: Vec<ChannelStatus>,
    /// The plan number of an unfinished repair, when one exists.
    pub pending_repair: Option<String>,
    /// The latest committed lead message's local session, when available.
    pub lead_session: Option<Arc<crate::LeadSession>>,
}

impl Owner {
    /// Inspect the board, optionally selecting a participant or channel.
    ///
    /// Either selector limits that type of record. The board's run, store
    /// information and pending plan are always returned.
    pub fn status(
        &self,
        participant: Option<&ParticipantId>,
        channel: Option<&ChannelName>,
    ) -> BoardStatus {
        self.board.status(participant, channel)
    }
}

impl Table {
    pub(super) fn snapshot(&self) -> Published {
        let mut participants = self.authority.status();
        participants.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        let mut channels = self.channels.values().cloned().collect::<Vec<_>>();
        channels.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Published {
            participants,
            channels,
        }
    }
}

impl Board {
    pub(super) fn publish(&self, table: &Table) {
        self.published.store(Arc::new(table.snapshot()));
    }

    pub(super) fn status(
        &self,
        participant: Option<&ParticipantId>,
        channel: Option<&ChannelName>,
    ) -> BoardStatus {
        let snapshot = self.published.load();
        let participants = snapshot
            .participants
            .iter()
            .filter(|view| channel.is_none() && participant.is_none_or(|id| *id == view.id))
            .map(|view| ParticipantStatus {
                id: view.id.clone(),
                name: view.name.clone(),
                revoked: view.revoked,
                grant: view.grant.clone(),
                fingerprint: view.fingerprint.clone(),
                connections: view.connections.load(Ordering::Acquire),
                enrolled_at_ms: view.times.enrolled_at_ms,
                grant_changed_at_ms: view.times.grant_changed_at_ms,
                revoked_at_ms: view.times.revoked_at_ms,
            })
            .collect();
        let channels = snapshot
            .channels
            .iter()
            .filter(|view| participant.is_none() && channel.is_none_or(|name| *name == view.name))
            .map(|view| {
                let readers = view.activity.readers.load_full();
                let (floor, head, stopped) = view.status();
                ChannelStatus {
                    name: view.name.clone(),
                    creation: view.creation,
                    floor,
                    head,
                    stopped,
                    readers,
                }
            })
            .collect();
        BoardStatus {
            run: CreationId::from_bytes(self.run),
            started_at_ms: self.started_at_ms,
            store: self.store.info,
            participants,
            channels,
            pending_repair: self.pending_repair.clone(),
            lead_session: self.store.lead_session.load_full(),
        }
    }
}
