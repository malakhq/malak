use crate::{ChannelName, ChannelPattern, CreationId, Error, Outcome, ParticipantId, Position};
use malak_layers::{Event, EventKind, SenderPeer, SenderTrace, SessionProcess};
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Instant, SystemTime};

mod failures;

const WATCHERS: usize = 4;
const QUEUE_LIMIT: usize = 1_000;

#[derive(Default)]
pub(crate) struct Events {
    watchers: Mutex<Watchers>,
    failures: Arc<failures::Failures>,
    first_senders: Mutex<HashMap<ParticipantId, Option<Arc<SenderPeer>>>>,
}

#[derive(Default)]
struct Watchers {
    entries: Vec<Weak<Watcher>>,
    closed: bool,
}

fn session_change(
    baseline: Option<&SenderPeer>,
    session: Option<&SessionProcess>,
) -> Option<SessionProcess> {
    let baseline = baseline?;
    let earlier = baseline.trace.session.as_ref()?;
    let session = session?.identity;
    // A wrapper already below the baseline session can become the session
    // itself if that ancestor exits and init adopts the wrapper.
    (session != earlier.identity && !baseline.ancestry.contains(&session)).then(|| earlier.clone())
}

#[cfg(test)]
mod sender_tests {
    use super::*;
    use malak_layers::ProcessIdentity;

    #[test]
    fn adoption_by_init_does_not_flag_the_same_worker() {
        let worker = process(32, 103);
        let mut baseline = SenderPeer::transport("unix");
        baseline.ancestry.push(worker.identity);
        baseline.trace.session = Some(process(21, 99));

        assert_eq!(session_change(Some(&baseline), Some(&worker)), None);
    }

    #[test]
    fn a_reused_pid_with_a_new_start_time_is_a_session_change() {
        let mut baseline = SenderPeer::transport("unix");
        baseline.trace.session = Some(process(21, 99));
        let replacement = process(21, 200);

        assert_eq!(
            session_change(Some(&baseline), Some(&replacement)),
            baseline.trace.session
        );
    }

    fn process(pid: u32, start_time: u64) -> SessionProcess {
        SessionProcess {
            identity: ProcessIdentity { pid, start_time },
            program: "worker".into(),
        }
    }
}

pub(super) enum OwnerTarget<'a> {
    Name(Option<&'a str>),
    Participant(Option<&'a str>),
    Channel(Option<&'a str>, Option<(CreationId, Position)>),
    Run(Option<CreationId>),
}

pub(super) struct Watcher {
    pattern: Option<ChannelPattern>,
    queue: Mutex<Queue>,
    changed: Condvar,
}

pub(super) struct TailWriter(Arc<Watcher>);

#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    missed: u64,
    phase: Phase,
    writing: bool,
}

#[derive(Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Open,
    Draining,
    Closed,
}

impl Events {
    pub(super) fn start(self: &Arc<Self>) -> Result<(), Error> {
        self.failures.start(Arc::downgrade(self))
    }

    pub(super) fn failure(
        &self,
        participant: &ParticipantId,
        operation: &'static str,
        channel: impl FnOnce() -> Option<ChannelName>,
        error: &Error,
    ) {
        if error.outcome() == Outcome::Refused || error.is_limit() {
            let event = failed_call(participant, operation, channel(), error);
            self.failures.record(event, error.is_limit());
        }
    }

    pub(crate) fn stored(
        &self,
        channel: &crate::channel::Channel,
        sender: &ParticipantId,
        position: Position,
        kind: crate::Kind,
        at: SystemTime,
        peer: Option<Arc<SenderPeer>>,
    ) {
        let mut event = Event::new(EventKind::Stored);
        event.operation = Some("send");
        event.participant = Some(sender.clone());
        {
            let mut first = self.first_senders.lock();
            let baseline = first.entry(sender.clone()).or_insert_with(|| peer.clone());
            if let Some(peer) = &peer {
                event.earlier_session =
                    session_change(baseline.as_deref(), peer.trace.session.as_ref());
            }
        }
        event.sender_trace = Some(peer.as_ref().map_or_else(
            || SenderTrace::transport("in_process"),
            |peer| peer.trace.clone(),
        ));
        event.channel = Some(channel.name.clone());
        event.record = Some((channel.creation, position));
        event.outcome = Some(Outcome::Done);
        event.at = at;
        event.message_kind = Some(kind.as_str());
        self.emit(event);
    }

    pub(super) fn earlier_session(
        &self,
        sender: &ParticipantId,
        session: Option<&SessionProcess>,
    ) -> Option<SessionProcess> {
        let session = session?;
        let first = self.first_senders.lock();
        let baseline = first.get(sender)?.as_deref();
        session_change(baseline, Some(session))
    }

    pub(super) fn mailbox_full(
        &self,
        channel: &ChannelName,
        sender: &ParticipantId,
        error: &Error,
    ) {
        self.emit(failed_call(sender, "send", Some(channel.clone()), error));
    }

    pub(crate) fn damaged(&self, channel: &ChannelName, creation: CreationId, position: Position) {
        let mut event = Event::new(EventKind::StoreDamaged);
        event.channel = Some(channel.clone());
        event.record = Some((creation, position));
        event.outcome = Some(Outcome::Unavailable);
        self.emit(event);
    }

    pub(crate) fn tail_cut(
        &self,
        channel: &ChannelName,
        creation: CreationId,
        position: Position,
        tampered: bool,
    ) {
        let kind = if tampered {
            EventKind::TailTampered
        } else {
            EventKind::TailCut
        };
        self.record(kind, "startup_cut", None, channel, creation, Some(position));
    }

    pub(super) fn owner(
        &self,
        operation: &'static str,
        target: OwnerTarget<'_>,
        error: Option<&Error>,
    ) {
        self.owner_from(
            operation,
            target,
            error,
            &SenderTrace::transport("in_process"),
        );
    }

    pub(super) fn owner_from(
        &self,
        operation: &'static str,
        target: OwnerTarget<'_>,
        error: Option<&Error>,
        sender_trace: &SenderTrace,
    ) {
        let mut event = Event::new(EventKind::Owner);
        event.sender_trace = Some(sender_trace.clone());
        event.operation = Some(operation);
        event.outcome = Some(Outcome::Done);
        if let Some(error) = error {
            event.outcome = Some(error.outcome());
            event.reason = Some(
                match error.outcome() {
                    Outcome::Refused => "owner_request_refused",
                    Outcome::Unavailable => "owner_request_unavailable",
                    Outcome::NotSupported => "owner_request_not_supported",
                    Outcome::Done => unreachable!("unsuccessful owner operation"),
                }
                .into(),
            );
        }
        match target {
            OwnerTarget::Name(name) => {
                event.owner_target = name
                    .and_then(|name| name.parse::<crate::ParticipantName>().ok())
                    .map(|name| name.to_string());
            }
            OwnerTarget::Participant(id) => {
                event.participant = id.and_then(|id| id.parse().ok());
            }
            OwnerTarget::Channel(channel, record) => {
                event.channel = channel.and_then(|channel| channel.parse().ok());
                event.record = record;
            }
            OwnerTarget::Run(run) => event.owner_target = run.map(|run| run.to_string()),
        }
        self.emit(event);
    }

    pub(super) fn stream(&self, pattern: Option<ChannelPattern>) -> Result<TailWriter, Error> {
        self.subscribe(pattern, true).map(TailWriter)
    }

    #[cfg(test)]
    pub(super) fn watch(&self, pattern: Option<ChannelPattern>) -> Result<Arc<Watcher>, Error> {
        self.subscribe(pattern, false)
    }

    fn subscribe(
        &self,
        pattern: Option<ChannelPattern>,
        writing: bool,
    ) -> Result<Arc<Watcher>, Error> {
        let mut watchers = self.watchers.lock();
        if watchers.closed {
            return Err(Error::unavailable("board has stopped"));
        }
        watchers
            .entries
            .retain(|watcher| watcher.strong_count() != 0);
        if watchers.entries.len() >= WATCHERS {
            return Err(Error::unavailable("no owner tail place is available"));
        }
        let watcher = Arc::new(Watcher {
            pattern,
            queue: Mutex::new(Queue {
                writing,
                ..Queue::default()
            }),
            changed: Condvar::new(),
        });
        watchers.entries.push(Arc::downgrade(&watcher));
        Ok(watcher)
    }

    pub(super) fn flush(&self, deadline: Instant) {
        let entries = {
            let mut watchers = self.watchers.lock();
            watchers.closed = true;
            std::mem::take(&mut watchers.entries)
        };
        let watchers: Vec<_> = entries
            .into_iter()
            .filter_map(|entry| entry.upgrade())
            .collect();
        for watcher in &watchers {
            watcher.begin_drain();
        }
        for watcher in watchers {
            watcher.wait_for_writer(deadline);
        }
    }

    pub(super) fn record(
        &self,
        kind: EventKind,
        operation: &'static str,
        participant: Option<&ParticipantId>,
        channel: &ChannelName,
        creation: CreationId,
        position: Option<Position>,
    ) {
        let mut event = Event::new(kind);
        event.operation = Some(operation);
        event.participant = participant.cloned();
        event.channel = Some(channel.clone());
        event.record = position.map(|position| (creation, position));
        event.outcome = Some(Outcome::Done);
        self.emit(event);
    }

    pub(super) fn emit(&self, event: Event) {
        debug_assert!(event.body.is_none());
        self.watchers.lock().entries.retain(|watcher| {
            let Some(watcher) = watcher.upgrade() else {
                return false;
            };
            if watcher.pattern.as_ref().is_some_and(|pattern| {
                !event
                    .channel
                    .as_ref()
                    .is_some_and(|channel| pattern.matches(channel))
            }) {
                return true;
            }
            let mut queue = watcher.queue.lock();
            if queue.phase == Phase::Open {
                if queue.events.len() == QUEUE_LIMIT {
                    queue.missed = queue.missed.saturating_add(1);
                } else {
                    queue.events.push_back(event.clone());
                    watcher.changed.notify_one();
                }
            }
            true
        });
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.failures.stop();
    }
}

fn failed_call(
    participant: &ParticipantId,
    operation: &'static str,
    channel: Option<ChannelName>,
    error: &Error,
) -> Event {
    let mut event = Event::new(EventKind::Denied);
    event.participant = Some(participant.clone());
    event.operation = Some(operation);
    event.channel = channel;
    let mut reason = error.to_string();
    reason.truncate(reason.floor_char_boundary(256));
    event.outcome = Some(error.outcome());
    event.reason = Some(reason);
    event
}

impl Watcher {
    pub(super) fn next(&self) -> Option<(Event, u64)> {
        let mut queue = self.queue.lock();
        while queue.phase != Phase::Closed {
            if let Some(event) = queue.events.pop_front() {
                return Some((event, std::mem::take(&mut queue.missed)));
            }
            if queue.phase == Phase::Draining {
                break;
            }
            self.changed.wait(&mut queue);
        }
        None
    }

    pub(super) fn close(&self) {
        self.queue.lock().phase = Phase::Closed;
        self.changed.notify_all();
    }

    fn begin_drain(&self) {
        let mut queue = self.queue.lock();
        if queue.phase == Phase::Open {
            queue.phase = Phase::Draining;
            self.changed.notify_all();
        }
    }

    fn wait_for_writer(&self, deadline: Instant) {
        let mut queue = self.queue.lock();
        while queue.writing && queue.phase == Phase::Draining {
            if self.changed.wait_until(&mut queue, deadline).timed_out() {
                break;
            }
        }
    }
}

impl TailWriter {
    pub(super) fn watcher(&self) -> Arc<Watcher> {
        self.0.clone()
    }

    pub(super) fn run(
        self,
        mut send: impl FnMut(Event, u64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        while let Some((event, missed)) = self.0.next() {
            send(event, missed)?;
        }
        Ok(())
    }
}

impl Drop for TailWriter {
    fn drop(&mut self) {
        self.0.queue.lock().writing = false;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_damage_is_reported_once_and_survives_replacing_the_bytes() {
        use crate::{Limits, Owner, SendRequest};
        use malak_layers::{Claim, Claimed, ReadLimit, ReadResult, Store};
        use malak_store_markdown::MarkdownStore;
        use malak_testkit::unix_socket::SocketDir;
        use std::time::{Duration, Instant};

        for missing in 0..3 {
            let root = SocketDir::new().unwrap();
            let state = root.path().join("state");
            let path = root.path().join("records");
            let store = || MarkdownStore::new(&path, true);
            let restart = || Owner::restart_durable(store(), Limits::default(), &state).unwrap();
            let board = Owner::create_durable(store(), Limits::default(), &state).unwrap();
            let lead = board.lead();
            let receipts = ["first", "middle", "last"].map(|id| {
                lead.send(SendRequest::note("lead", id, id.as_bytes()))
                    .unwrap()
            });
            lead.send(SendRequest::note("other", "unaffected", b"unaffected"))
                .unwrap();
            board.stop(board.status(None, None).run).unwrap();
            drop((lead, board));
            let receipt = receipts[missing];
            let name = malak_record::store_name(&"lead".parse().unwrap(), receipt.creation);
            let damaged = store();
            let Claimed::Taken(ownership) = damaged.claim(Claim::Take).unwrap() else {
                panic!("expected exclusive store claim");
            };
            let ReadResult::Records {
                records,
                unparseable: false,
            } = damaged
                .read(
                    &name,
                    Position::FIRST,
                    ReadLimit {
                        records: 4,
                        before: None,
                    },
                )
                .unwrap()
            else {
                panic!("expected the three original records");
            };
            let mut remaining = records.clone();
            remaining.remove(missing);
            replace_records(&damaged, ownership.generation, &name, &remaining);
            drop(damaged);

            let board = restart();
            let watcher = board.board.events.watch(None).unwrap();
            board.board.sweep_retention();

            {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut queue = watcher.queue.lock();
                while queue.events.is_empty() {
                    assert!(!watcher.changed.wait_until(&mut queue, deadline).timed_out());
                }
                let damage = queue.events.pop_front().unwrap();
                assert_eq!(damage.kind, EventKind::StoreDamaged);
                assert_eq!(damage.record, Some((receipt.creation, receipt.position)));
                assert_eq!(damage.outcome, Some(Outcome::Unavailable));
                assert!(damage.body.is_none());
            }
            let lead = board.lead();
            let result = lead.get("lead", receipt.position.get());
            assert_eq!(result.unwrap_err().outcome(), Outcome::Unavailable);
            assert!(watcher.queue.lock().events.is_empty());
            assert!(matches!(
                lead.get("other", 1).unwrap(),
                crate::GetReply::Message(_)
            ));
            board.stop(board.status(None, None).run).unwrap();
            drop((lead, board));
            let restored = store();
            let Claimed::Taken(ownership) = restored.claim(Claim::Take).unwrap() else {
                panic!("expected exclusive store claim");
            };
            replace_records(&restored, ownership.generation, &name, &records);
            drop(restored);

            let board = restart();
            let failure = board
                .lead()
                .get("lead", receipt.position.get())
                .unwrap_err();
            assert_eq!(failure.outcome(), Outcome::Unavailable);
            assert!(failure.to_string().contains("stopped for damage"));
        }
    }

    fn replace_records(
        store: &impl malak_layers::Store,
        generation: u64,
        channel: &str,
        records: &[malak_layers::Record],
    ) {
        use malak_layers::{Trim, Trimmed};
        assert_eq!(
            store.trim(generation, channel, Trim::Keep(1..1)).unwrap(),
            Trimmed::Done
        );
        store.append(generation, channel, records).unwrap();
    }

    #[test]
    fn direct_owner_changes_emit_one_event_each_with_their_outcomes() {
        use crate::{Grant, Limits, Owner};
        use malak_store_memory::MemoryStore;

        let board = Owner::in_memory(MemoryStore::default(), Limits::default()).unwrap();
        let watcher = board.board.events.watch(None).unwrap();

        let participant = board.enrol("worker", Grant::default()).unwrap();
        assert!(board.enrol("worker", Grant::default()).is_err());
        board
            .replace_grant(participant.id(), Grant::default())
            .unwrap();
        board.revoke(participant.id()).unwrap();
        let (creation, _) = board.create_channel("jobs").unwrap();
        board.trim("jobs", creation, 1).unwrap();
        board.remove_channel("jobs", creation).unwrap();
        board.stop(board.status(None, None).run).unwrap();

        let queue = watcher.queue.lock();
        let owner_events = queue
            .events
            .iter()
            .filter(|event| event.kind == EventKind::Owner);
        let operations = owner_events
            .clone()
            .map(|event| (event.operation, event.outcome))
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            [
                (Some("ENROL"), Some(Outcome::Done)),
                (Some("ENROL"), Some(Outcome::Refused)),
                (Some("GRANT"), Some(Outcome::Done)),
                (Some("REVOKE"), Some(Outcome::Done)),
                (Some("CHANNEL_CREATE"), Some(Outcome::Done)),
                (Some("TRIM"), Some(Outcome::Done)),
                (Some("CHANNEL_REMOVE"), Some(Outcome::Done)),
                (Some("STOP"), Some(Outcome::Done)),
            ]
        );
        assert!(owner_events.clone().all(|event| event.body.is_none()));
        assert_eq!(
            owner_events.clone().nth(2).unwrap().participant.as_ref(),
            Some(participant.id())
        );
        assert_eq!(
            owner_events.clone().nth(5).unwrap().record,
            Some((creation, 1.try_into().unwrap()))
        );
    }

    #[test]
    fn a_full_tail_queue_drops_new_events_and_reports_the_count() {
        let events = Events::default();
        let watcher = events.watch(None).unwrap();
        for index in 0..QUEUE_LIMIT + 3 {
            let mut event = Event::new(EventKind::Stored);
            event.count = index as u64;
            events.emit(event);
        }

        let (first, missed) = watcher.next().unwrap();
        assert_eq!((first.count, missed), (0, 3));
        for expected in 1..QUEUE_LIMIT {
            let (event, missed) = watcher.next().unwrap();
            assert_eq!((event.count, missed), (expected as u64, 0));
        }
    }

    #[test]
    fn a_fifth_watcher_cannot_take_a_tail_place() {
        let events = Events::default();
        let mut watchers = (0..WATCHERS)
            .map(|_| events.watch(None).unwrap())
            .collect::<Vec<_>>();

        assert!(events.watch(None).is_err());
        watchers.pop();
        assert!(events.watch(None).is_ok());
    }
}
