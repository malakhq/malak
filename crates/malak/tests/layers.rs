//! The layers are separate traits: a store, transport and mechanism written
//! here, outside every core crate and using only `malak`'s public API, work
//! with the board.

use malak::layers::{
    Claim, Claimed, Enrolment, Handover, Identity, Link, Mechanism, Ownership, Proves, ReadLimit,
    ReadResult, Received, Record, SenderOrigin, Session, Side, Step, Store, StoreInfo, Transport,
    TransportInfo, Trim, Trimmed,
};
use malak::{
    Action, ChannelName, Durability, Error, Grade, Grant, Limits, Owner, ParticipantId, Position,
    SendRequest,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A store that keeps records in vectors and counts the appends it receives.
#[derive(Clone, Default)]
struct CountingStore {
    appends: Arc<AtomicUsize>,
    channels: Arc<Mutex<HashMap<String, BTreeMap<Position, Record>>>>,
}

impl Store for CountingStore {
    fn describe(&self) -> StoreInfo {
        StoreInfo {
            durability: Durability::Memory,
            max_record_bytes: usize::MAX,
            slow_writes: false,
        }
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        match mode {
            Claim::Take => Ok(Claimed::Taken(Ownership {
                store_id: [7; 16],
                generation: 1,
                has_records: false,
                location: None,
            })),
            Claim::Repair => Err(Error::not_supported(
                "the counting store cannot be repaired",
            )),
            Claim::Settle { .. } | Claim::Release { .. } => Ok(Claimed::Done),
        }
    }

    fn append(&self, _: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        self.appends.fetch_add(records.len(), Ordering::SeqCst);
        let mut channels = self.channels.lock().unwrap();
        let entries = channels.entry(channel.to_owned()).or_default();
        entries.extend(
            records
                .iter()
                .map(|record| (record.position, record.clone())),
        );
        Ok(())
    }

    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error> {
        let channels = self.channels.lock().unwrap();
        let records = channels.get(channel).map_or_else(Vec::new, |entries| {
            entries
                .range(from..)
                .take_while(|(position, _)| limit.before.is_none_or(|before| **position < before))
                .take(limit.records)
                .map(|(_, record)| record.clone())
                .collect()
        });
        Ok(ReadResult::Records {
            records,
            unparseable: false,
        })
    }

    fn trim(&self, _: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        let Trim::Keep(range) = trim else {
            return Err(Error::not_supported(
                "the counting store cannot be repaired",
            ));
        };
        if let Some(entries) = self.channels.lock().unwrap().get_mut(channel) {
            entries.retain(|position, _| range.contains(&position.get()));
        }
        Ok(Trimmed::Done)
    }
}

/// A transport whose links are queues of bytes in this process.
#[derive(Default)]
struct LoopbackTransport {
    pending: Mutex<HashMap<String, Arc<Mutex<VecDeque<u8>>>>>,
}

impl LoopbackTransport {
    fn link(io: Arc<Mutex<VecDeque<u8>>>) -> Link<Arc<Mutex<VecDeque<u8>>>> {
        Link {
            io,
            source: None,
            process_id: None,
            sender: SenderOrigin::transport("loopback"),
        }
    }
}

impl Transport for LoopbackTransport {
    type Connection = Arc<Mutex<VecDeque<u8>>>;

    fn describe(&self) -> TransportInfo {
        TransportInfo {
            distinguishes_sources: false,
            authenticates_ends: false,
        }
    }

    fn accept(&self, address: &str) -> Result<Link<Self::Connection>, Error> {
        let io = self.pending.lock().unwrap().get(address).cloned();
        io.map(Self::link)
            .ok_or_else(|| Error::unavailable("nothing is dialling this address"))
    }

    fn dial(&self, address: &str) -> Result<Link<Self::Connection>, Error> {
        let io = Arc::default();
        self.pending
            .lock()
            .unwrap()
            .insert(address.to_owned(), Arc::clone(&io));
        Ok(Self::link(io))
    }

    fn send(&self, link: &Self::Connection, bytes: &[u8]) -> Result<usize, Error> {
        link.lock().unwrap().extend(bytes);
        Ok(bytes.len())
    }

    fn receive(&self, link: &Self::Connection, output: &mut [u8]) -> Result<Received, Error> {
        let mut queue = link.lock().unwrap();
        let count = output.len().min(queue.len());
        for (slot, byte) in output.iter_mut().zip(queue.drain(..count)) {
            *slot = byte;
        }
        Ok(NonZeroUsize::new(count).map_or(Received::Closed, Received::Bytes))
    }
}

/// A mechanism that proves every caller and vetoes one channel.
struct VetoingMechanism;

impl Mechanism for VetoingMechanism {
    fn describe(&self) -> Proves {
        Proves {
            highest_grade: Grade::Proven,
            has_veto: true,
            key_at_enrolment: false,
        }
    }

    fn enrol(&self, _: &ParticipantId, _: &Enrolment<'_>) -> Result<Handover, Error> {
        Ok(Handover {
            private_key: None,
            public_key: None,
            contents: b"pass".to_vec(),
            record: None,
        })
    }

    fn step(&self, _: &Session<'_>, incoming: &[u8]) -> Result<Step, Error> {
        Ok(if incoming == b"pass" {
            Step::Proven {
                ends: None,
                register_key: false,
                record: None,
            }
        } else {
            Step::Denied
        })
    }

    fn allow(&self, _: &Identity<'_>, _: Action, channel: &ChannelName) -> Result<bool, Error> {
        Ok(channel.as_str() != "secret")
    }

    fn revoke(&self, _: &ParticipantId) -> Result<(), Error> {
        Ok(())
    }
}

#[test]
fn a_board_runs_send_read_and_ack_through_a_store_written_outside_the_core() {
    let store = CountingStore::default();
    let owner = Owner::in_memory(store.clone(), Limits::default()).unwrap();
    let mut grant = Grant::default();
    grant.allow("lead".parse().unwrap(), [Action::Send]);
    let worker = owner.enrol("worker", grant).unwrap();

    worker
        .send(SendRequest::note("lead", "run-1", b"checks passed"))
        .unwrap();
    let lead = owner.lead();
    let reply = lead.read(&["lead"], 1, Duration::ZERO).unwrap();
    let message = &reply.messages[0];
    lead.ack(
        "lead",
        message.receipt.creation,
        message.receipt.position.get(),
    )
    .unwrap();

    assert_eq!(&message.body[..], b"checks passed");
    assert!(store.appends.load(Ordering::SeqCst) > 0);
}

#[test]
fn a_transport_written_outside_the_core_carries_bytes_between_two_ends() {
    let transport = LoopbackTransport::default();
    let dialled = transport.dial("board").unwrap();
    let accepted = transport.accept("board").unwrap();
    let mut buffer = [0; 8];

    assert_eq!(transport.send(&dialled.io, b"hello").unwrap(), 5);
    let Received::Bytes(count) = transport.receive(&accepted.io, &mut buffer).unwrap() else {
        panic!("expected bytes");
    };

    assert_eq!(&buffer[..count.get()], b"hello");
    assert!(!transport.describe().authenticates_ends);
}

#[test]
fn a_mechanism_written_outside_the_core_proves_callers_and_vetoes_a_channel() {
    let mechanism = VetoingMechanism;
    let id: ParticipantId = format!("{:032x}", 1).parse().unwrap();
    let session = Session {
        side: Side::Board,
        public_key: &[0; 32],
        binding: &[0; 32],
        source: None,
        claimed: &id,
        record: None,
        handover: None,
    };
    let identity = Identity {
        participant: &id,
        grade: Grade::Proven,
        ends: None,
    };

    let handover = mechanism
        .enrol(
            &id,
            &Enrolment {
                allowed_place: None,
                account: None,
            },
        )
        .unwrap();
    assert!(matches!(
        mechanism.step(&session, &handover.contents).unwrap(),
        Step::Proven { .. }
    ));
    assert!(matches!(
        mechanism.step(&session, b"no").unwrap(),
        Step::Denied
    ));
    assert!(
        mechanism
            .allow(&identity, Action::Send, &"open".parse().unwrap())
            .unwrap()
    );
    assert!(
        !mechanism
            .allow(&identity, Action::Send, &"secret".parse().unwrap())
            .unwrap()
    );
}

/// A board served over a transport and a mechanism written outside the core.
#[cfg(all(
    feature = "unix",
    feature = "markdown",
    any(target_os = "linux", target_os = "macos")
))]
mod served {
    use super::*;
    use malak::layers::{Link, SenderOrigin, Source};
    use malak::mechanism::Key;
    use malak::server::{Entrance, LocalStartup, Server, StopHandle};
    use malak::stores::MarkdownStore;
    use malak_core::{HandoverFile, LocalClient};
    use malak_proto::encode_frame;
    use malak_testkit::unix_socket::SocketDir;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::sync::Condvar;
    use std::sync::atomic::AtomicBool;
    use std::thread::JoinHandle;

    const WAIT: Duration = Duration::from_secs(5);
    const PHRASE: &[u8] = b"open sesame";

    /// One direction of a pipe: bytes and whether the writer has closed it.
    #[derive(Default)]
    struct Pipe {
        state: Mutex<(VecDeque<u8>, bool)>,
        changed: Condvar,
    }

    impl Pipe {
        fn close(&self) {
            self.state.lock().unwrap().1 = true;
            self.changed.notify_all();
        }
    }

    struct End {
        incoming: Arc<Pipe>,
        outgoing: Arc<Pipe>,
        reported: AtomicBool,
    }

    /// A transport whose links are blocking in-memory pipes; the accepting end
    /// reports the account that owns this test as its source.
    struct PipeTransport {
        uid: u32,
        waiting: Mutex<HashMap<String, VecDeque<Link<Arc<End>>>>>,
        arrived: Condvar,
    }

    impl PipeTransport {
        fn new(uid: u32) -> Self {
            Self {
                uid,
                waiting: Mutex::default(),
                arrived: Condvar::new(),
            }
        }

        fn link(
            incoming: &Arc<Pipe>,
            outgoing: &Arc<Pipe>,
            source: Option<Source>,
        ) -> Link<Arc<End>> {
            Link {
                io: Arc::new(End {
                    incoming: incoming.clone(),
                    outgoing: outgoing.clone(),
                    reported: AtomicBool::new(false),
                }),
                source,
                process_id: None,
                sender: SenderOrigin::transport("pipe"),
            }
        }
    }

    impl Transport for PipeTransport {
        type Connection = Arc<End>;

        fn describe(&self) -> TransportInfo {
            TransportInfo {
                distinguishes_sources: true,
                authenticates_ends: false,
            }
        }

        fn accept(&self, address: &str) -> Result<Link<Self::Connection>, Error> {
            let mut waiting = self.waiting.lock().unwrap();
            loop {
                if let Some(link) = waiting.get_mut(address).and_then(VecDeque::pop_front) {
                    return Ok(link);
                }
                waiting = self.arrived.wait(waiting).unwrap();
            }
        }

        fn dial(&self, address: &str) -> Result<Link<Self::Connection>, Error> {
            let (to_board, to_caller) = (Arc::<Pipe>::default(), Arc::<Pipe>::default());
            let board = Self::link(&to_board, &to_caller, Some(Source::LocalUser(self.uid)));
            self.waiting
                .lock()
                .unwrap()
                .entry(address.to_owned())
                .or_default()
                .push_back(board);
            self.arrived.notify_all();
            Ok(Self::link(&to_caller, &to_board, None))
        }

        fn send(&self, link: &Self::Connection, bytes: &[u8]) -> Result<usize, Error> {
            if bytes.is_empty() {
                link.outgoing.close();
                link.incoming.close();
                return Ok(0);
            }
            let mut state = link.outgoing.state.lock().unwrap();
            if state.1 {
                return Err(Error::unavailable("the pipe is closed"));
            }
            state.0.extend(bytes);
            link.outgoing.changed.notify_all();
            Ok(bytes.len())
        }

        fn receive(&self, link: &Self::Connection, output: &mut [u8]) -> Result<Received, Error> {
            let mut state = link.incoming.state.lock().unwrap();
            loop {
                if !state.0.is_empty() {
                    let count = output.len().min(state.0.len());
                    for (slot, byte) in output.iter_mut().zip(state.0.drain(..count)) {
                        *slot = byte;
                    }
                    return Ok(Received::Bytes(NonZeroUsize::new(count).unwrap()));
                }
                if state.1 {
                    return if link.reported.swap(true, Ordering::SeqCst) {
                        Err(Error::unavailable("the pipe is closed"))
                    } else {
                        Ok(Received::Closed)
                    };
                }
                state = link.incoming.changed.wait(state).unwrap();
            }
        }
    }

    /// A mechanism that proves a caller by a shared phrase, on both sides.
    struct Passphrase(&'static [u8]);

    impl Mechanism for Passphrase {
        fn describe(&self) -> Proves {
            Key.describe()
        }

        fn enrol(&self, id: &ParticipantId, hints: &Enrolment<'_>) -> Result<Handover, Error> {
            Key.enrol(id, hints)
        }

        fn step(&self, session: &Session<'_>, incoming: &[u8]) -> Result<Step, Error> {
            Ok(match session.side {
                Side::Participant => Step::More(self.0.to_vec()),
                Side::Board if incoming == PHRASE => Step::Proven {
                    ends: None,
                    register_key: false,
                    record: None,
                },
                Side::Board => Step::Denied,
            })
        }

        fn allow(&self, _: &Identity<'_>, _: Action, _: &ChannelName) -> Result<bool, Error> {
            Ok(true)
        }

        fn revoke(&self, _: &ParticipantId) -> Result<(), Error> {
            Ok(())
        }
    }

    /// A served board with the lead and one worker enrolled.
    struct Served {
        owner: Owner,
        stop: StopHandle,
        transport: Arc<PipeTransport>,
        runner: Option<JoinHandle<Result<(), Error>>>,
        lead: PathBuf,
        worker: PathBuf,
        _root: SocketDir,
    }

    impl Served {
        fn start() -> Self {
            let root = SocketDir::new().unwrap();
            let uid = std::fs::metadata(root.path()).unwrap().uid();
            let transport = Arc::new(PipeTransport::new(uid));
            let addresses = ["unix:/pipe/o", "unix:/pipe/l", "unix:/pipe/p"];
            let (owner, server) = Server::create(
                MarkdownStore::new(root.path().join("store"), false),
                Limits::default(),
                root.path().join("state"),
                LocalStartup {
                    owner_address: addresses[0].into(),
                    lead_address: addresses[1].into(),
                    general_address: addresses[2].into(),
                    lead_handover: root.path().join("lead"),
                    mechanism_name: "passphrase".into(),
                },
                Arc::new(Passphrase(PHRASE)),
                uid,
            )
            .unwrap();
            let mut grant = Grant::default();
            grant.allow("lead".parse().unwrap(), [Action::Send]);
            let worker = root.path().join("worker");
            server
                .entrances()
                .enrol("worker", grant, &worker, None)
                .unwrap();
            for (entrance, address) in [Entrance::Owner, Entrance::Lead, Entrance::General]
                .into_iter()
                .zip(addresses)
            {
                server
                    .listen(entrance, transport.clone(), address.into(), 4)
                    .unwrap();
            }
            server.ready();
            let stop = server.stop_handle();
            Self {
                owner,
                stop,
                transport,
                runner: Some(std::thread::spawn(move || server.run())),
                lead: root.path().join("lead"),
                worker,
                _root: root,
            }
        }

        fn worker(&self, phrase: &'static [u8]) -> Result<LocalClient<PipeTransport>, Error> {
            LocalClient::participant(
                self.transport.clone(),
                HandoverFile::read(&self.worker).unwrap(),
                Some(Arc::new(Passphrase(phrase))),
            )
        }

        fn lead(&self) -> LocalClient<PipeTransport> {
            LocalClient::participant(
                self.transport.clone(),
                HandoverFile::read(&self.lead).unwrap(),
                None,
            )
            .unwrap()
        }
    }

    impl Drop for Served {
        fn drop(&mut self) {
            self.stop.stop();
            let _ = self.runner.take().map(JoinHandle::join);
        }
    }

    /// Read the replies to one request: any messages, then its outcome.
    fn replies(client: &mut LocalClient<PipeTransport>) -> Vec<(String, Vec<u8>)> {
        let mut frames = Vec::new();
        loop {
            let frame = client.read_frame(WAIT).unwrap();
            let done = frame.op() != "MSG";
            frames.push((frame.op().to_owned(), frame.body().to_vec()));
            if done {
                return frames;
            }
        }
    }

    fn send_report(
        client: &mut LocalClient<PipeTransport>,
        rid: u64,
        body: &[u8],
    ) -> Vec<(String, Vec<u8>)> {
        let frame = encode_frame(
            rid,
            "SEND",
            2,
            |out| {
                out.str("channel").unwrap().str("lead").unwrap();
                out.str("id").unwrap().str("report").unwrap();
            },
            body,
        )
        .unwrap();
        client.send_frame(frame, WAIT).unwrap();
        replies(client)
    }

    fn read_lead(client: &mut LocalClient<PipeTransport>, rid: u64) -> Vec<(String, Vec<u8>)> {
        let frame = encode_frame(
            rid,
            "READ",
            2,
            |out| {
                out.str("channels")
                    .unwrap()
                    .array(1)
                    .unwrap()
                    .str("lead")
                    .unwrap();
                out.str("wait_ms").unwrap().u64(0).unwrap();
            },
            &[],
        )
        .unwrap();
        client.send_frame(frame, WAIT).unwrap();
        replies(client)
    }

    fn ack_lead(
        client: &mut LocalClient<PipeTransport>,
        rid: u64,
        creation: &str,
    ) -> Vec<(String, Vec<u8>)> {
        let frame = encode_frame(
            rid,
            "ACK",
            3,
            |out| {
                out.str("channel").unwrap().str("lead").unwrap();
                out.str("position").unwrap().u64(1).unwrap();
                out.str("creation").unwrap().str(creation).unwrap();
            },
            &[],
        )
        .unwrap();
        client.send_frame(frame, WAIT).unwrap();
        replies(client)
    }

    #[test]
    fn a_worker_proved_by_an_outside_mechanism_reaches_the_lead_over_an_outside_transport() {
        let served = Served::start();
        let mut worker = served.worker(PHRASE).unwrap();
        let mut lead = served.lead();

        let sent = send_report(&mut worker, 1, b"checks passed");
        let read = read_lead(&mut lead, 1);
        let creation = served
            .owner
            .lead()
            .read(&["lead"], 1, Duration::ZERO)
            .unwrap()
            .messages[0]
            .receipt
            .creation
            .to_string();
        let acked = ack_lead(&mut lead, 2, &creation);
        let again = read_lead(&mut lead, 3);

        assert_eq!(sent, [("OK".to_owned(), vec![])]);
        assert_eq!(read[0], ("MSG".to_owned(), b"checks passed".to_vec()));
        assert_eq!(acked, [("OK".to_owned(), vec![])]);
        assert_eq!(again, [("OK".to_owned(), vec![])]);
    }

    #[test]
    fn a_worker_whose_proof_the_outside_mechanism_denies_is_refused_at_attach() {
        let served = Served::start();

        let refused = served.worker(b"wrong guess");

        assert!(refused.is_err());
    }

    #[test]
    fn stopping_the_board_ends_the_run() {
        let mut served = Served::start();

        served.stop.stop();

        assert!(served.runner.take().unwrap().join().unwrap().is_ok());
    }
}
