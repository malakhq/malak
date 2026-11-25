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
    /// Current claim for a session-bound handover, including its history.
    pub claim: Option<Arc<crate::Claim>>,
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
                claim: view.claim.clone(),
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

/// How many inbox messages are listed by name before the rest are only counted.
const LISTED_UNREAD: usize = 5;

/// What a participant left in its inbox without acknowledging it.
pub(super) struct Unread {
    /// How many messages the participant has not acknowledged.
    pub(super) count: u64,
    /// The first few, with their positions and senders.
    pub(super) listed: Vec<(u64, Option<ParticipantName>)>,
}

impl Board {
    /// Find the inbox messages a participant has not acknowledged.
    ///
    /// A message counts as unread until the reader acknowledges it, whether or
    /// not a read has already delivered it.
    pub(super) fn unread(self: &Arc<Self>, id: &ParticipantId) -> Unread {
        let none = Unread {
            count: 0,
            listed: Vec::new(),
        };
        let name = id.inbox();
        let Some(channel) = self.table.check().channels.get(&name).cloned() else {
            return none;
        };
        let (floor, head, _) = channel.status();
        let acknowledged = channel.activity.readers.load().get(id).copied();
        let first = acknowledged.map_or(floor, |position| floor.max(position + 1));
        if first >= head {
            return none;
        }
        let listed = (first..head)
            .take(LISTED_UNREAD)
            .map(|position| (position, self.sender_at(&channel, position)))
            .collect();
        Unread {
            count: head - first,
            listed,
        }
    }

    fn sender_at(
        self: &Arc<Self>,
        channel: &Arc<Channel>,
        position: u64,
    ) -> Option<ParticipantName> {
        match self.fetch(channel.clone(), crate::Position::try_from(position).ok()?) {
            Ok((crate::GetReply::Message(message), _)) => Some(message.sender_name),
            _ => None,
        }
    }
}

impl Unread {
    /// Words for an exit report, such as `, 2 unread (lead #3, lead #4)`, or
    /// nothing when the inbox was left empty.
    pub(super) fn describe(&self) -> String {
        if self.count == 0 {
            return String::new();
        }
        let mut list = self
            .listed
            .iter()
            .map(|(position, sender)| match sender {
                Some(sender) => format!("{sender} #{position}"),
                None => format!("#{position}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let more = self.count - self.listed.len() as u64;
        if more > 0 {
            list.push_str(&format!(", and {more} more"));
        }
        format!(", {} unread ({list})", self.count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, Grant, Limits, SendRequest};
    use malak_store_memory::MemoryStore;
    use std::time::Duration;

    struct Worker {
        owner: Owner,
        handle: crate::Handle,
    }

    impl Worker {
        fn new() -> Self {
            let owner = Owner::in_memory(MemoryStore::default(), Limits::default()).unwrap();
            let mut grant = Grant::default();
            grant.allow("**".parse().unwrap(), [Action::Read, Action::Ack]);
            let handle = owner.enrol("worker", grant).unwrap();
            Self { owner, handle }
        }

        fn task(&self, id: &str) -> crate::Receipt {
            let inbox = self.handle.id().inbox();
            let request = SendRequest::note(inbox.as_str(), id, b"task");
            self.owner.lead().send(request).unwrap()
        }

        fn unread(&self) -> Unread {
            self.owner.board.unread(self.handle.id())
        }
    }

    #[test]
    fn an_empty_inbox_has_nothing_unread() {
        let worker = Worker::new();

        assert_eq!(worker.unread().describe(), "");
    }

    #[test]
    fn unacknowledged_messages_are_listed_with_sender_and_position() {
        let worker = Worker::new();
        let (first, second) = (worker.task("one"), worker.task("two"));

        let unread = worker.unread();

        assert_eq!(unread.count, 2);
        assert_eq!(
            unread.describe(),
            format!(
                ", 2 unread (lead #{}, lead #{})",
                first.position, second.position
            )
        );
    }

    #[test]
    fn a_message_that_was_read_but_not_acknowledged_is_still_unread() {
        let worker = Worker::new();
        worker.task("one");
        let inbox = worker.handle.id().inbox();

        let read = worker
            .handle
            .read(&[inbox.as_str()], 10, Duration::ZERO)
            .unwrap();

        assert_eq!(read.messages.len(), 1);
        assert_eq!(worker.unread().count, 1);
    }

    #[test]
    fn acknowledged_messages_are_not_counted() {
        let worker = Worker::new();
        let (first, second) = (worker.task("one"), worker.task("two"));
        let inbox = worker.handle.id().inbox();
        worker
            .handle
            .read(&[inbox.as_str()], 1, Duration::ZERO)
            .unwrap();
        worker
            .handle
            .ack(inbox.as_str(), first.creation, first.position.get())
            .unwrap();

        let unread = worker.unread();

        assert_eq!(
            unread.describe(),
            format!(", 1 unread (lead #{})", second.position)
        );
    }

    #[test]
    fn a_long_list_names_the_first_few_and_counts_the_rest() {
        let worker = Worker::new();
        for id in ["a", "b", "c", "d", "e", "f", "g"] {
            worker.task(id);
        }

        let text = worker.unread().describe();

        assert!(text.starts_with(", 7 unread (lead #"), "{text}");
        assert!(text.ends_with(", and 2 more)"), "{text}");
    }
}
