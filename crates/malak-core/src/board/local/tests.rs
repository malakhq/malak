//! Local entrance opening against real Unix links.

use crate::{
    Grade, Grant, Limits, LocalConfig, LocalEntrances, LocalOpening, Owner, ParticipantId,
};
use bytes::Bytes;
use malak_layers::{Error, Link, Mechanism, Received, Source, Transport};
use malak_mechanism_key::Key;
use malak_proto::{
    Binding, Encrypted, Entrance, Frame, FrameDecoder, Initiator, KeyPair, OPENING_BYTES,
    OwnerOperation, REPLY_BYTES, SegmentDecoder, SegmentKind, TAG_BYTES, attach_request,
    encode_frame, frame_prefix,
};
use malak_store_memory::MemoryStore;
use malak_testkit::unix_socket::SocketDir;
use malak_transport_unix::{UnixLink, UnixTransport};
use minicbor::{Decoder, Encoder};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

mod mechanism_rules;
mod rate_events;
mod tail_pressure;
mod wire_duplex;

struct Fixture {
    transport: Arc<UnixTransport>,
    client: Link<UnixLink>,
    server: Option<Link<UnixLink>>,
    board: Owner,
    entrances: LocalEntrances,
    board_key: [u8; 32],
    owner_key: KeyPair,
    lead_key: KeyPair,
    _socket: SocketDir,
}

impl Fixture {
    fn new(allowed_uid: u32) -> Self {
        Self::with_mechanism(allowed_uid, Arc::new(Key))
    }

    fn with_mechanism(allowed_uid: u32, mechanism: Arc<dyn Mechanism>) -> Self {
        Self::with_limits(allowed_uid, mechanism, Limits::default())
    }

    fn with_limits(allowed_uid: u32, mechanism: Arc<dyn Mechanism>, limits: Limits) -> Self {
        let socket = SocketDir::new().unwrap();
        let address = socket.address();
        let transport = Arc::new(UnixTransport::bind(&address).unwrap());
        let client = transport.dial(&address).unwrap();
        let server = transport.accept(&address).unwrap();
        let board = Owner::in_memory(MemoryStore::default(), limits).unwrap();
        let board_pair = KeyPair::generate().unwrap();
        let board_key = *board_pair.public();
        let owner_key = KeyPair::generate().unwrap();
        let lead_key = KeyPair::generate().unwrap();
        let entrances = board
            .local_entrances(
                LocalConfig {
                    board_key: board_pair,
                    board_user: allowed_uid,
                    owner_key: *owner_key.public(),
                    lead_user: allowed_uid,
                    lead_key: *lead_key.public(),
                },
                mechanism,
            )
            .unwrap();
        Self {
            transport,
            client,
            server: Some(server),
            board,
            entrances,
            board_key,
            owner_key,
            lead_key,
            _socket: socket,
        }
    }

    fn opening(&self, entrance: Entrance, caller: &KeyPair) -> Initiator {
        let (initiator, opening) = self.opening_bytes(entrance, caller);
        self.send(SegmentKind::Opening, &opening);
        initiator
    }

    fn opening_bytes(
        &self,
        entrance: Entrance,
        caller: &KeyPair,
    ) -> (Initiator, [u8; OPENING_BYTES]) {
        Initiator::open(
            Binding {
                version: 1,
                entrance,
            },
            caller.private(),
            &self.board_key,
        )
        .unwrap()
    }

    fn reconnect(&mut self) {
        let address = self._socket.address();
        self.client = self.transport.dial(&address).unwrap();
        self.server = Some(self.transport.accept(&address).unwrap());
    }

    fn send(&self, kind: SegmentKind, segment: &[u8]) {
        let prefix = SegmentDecoder::prefix(kind, segment).unwrap();
        let packet = [prefix.as_slice(), segment].concat();
        let mut sent = 0;
        while sent < packet.len() {
            sent += self
                .transport
                .send(&self.client.io, &packet[sent..])
                .unwrap();
        }
    }

    fn receive(&self, mut destination: &mut [u8]) {
        while !destination.is_empty() {
            let Received::Bytes(count) = self
                .transport
                .receive(&self.client.io, destination)
                .unwrap()
            else {
                panic!("server closed the link");
            };
            destination = &mut destination[count.get()..];
        }
    }

    fn open(&mut self, entrance: Entrance) -> Result<LocalOpening<UnixTransport>, Error> {
        self.entrances.open(
            entrance,
            self.transport.clone(),
            self.server.take().unwrap(),
        )
    }

    fn protected(&mut self, entrance: Entrance) -> (LocalOpening<UnixTransport>, Encrypted) {
        let key = match entrance {
            Entrance::Owner => &self.owner_key,
            Entrance::Lead => &self.lead_key,
            Entrance::General => panic!("fixture has no general key"),
        };
        let initiator = self.opening(entrance, key);
        self.finish_opening(entrance, initiator)
    }

    fn finish_opening(
        &mut self,
        entrance: Entrance,
        initiator: Initiator,
    ) -> (LocalOpening<UnixTransport>, Encrypted) {
        let opening = self.open(entrance).unwrap();
        (opening, self.finish_reply(initiator))
    }

    fn finish_reply(&self, initiator: Initiator) -> Encrypted {
        let mut reply = [0; 2 + REPLY_BYTES];
        self.receive(&mut reply);
        initiator.finish(&reply[2..]).unwrap()
    }

    fn send_protected(&self, caller: &mut Encrypted, plaintext: &[u8]) {
        let mut protected = vec![0; plaintext.len() + TAG_BYTES];
        let len = caller.seal(plaintext, &mut protected).unwrap();
        self.send(SegmentKind::Protected, &protected[..len]);
    }

    fn receive_protected_segment(&self) -> Vec<u8> {
        let mut prefix = [0; 2];
        self.receive(&mut prefix);
        let mut protected = vec![0; u16::from_be_bytes(prefix) as usize];
        self.receive(&mut protected);
        protected
    }

    fn receive_frame(&self, caller: &mut Encrypted) -> Frame {
        let protected = self.receive_protected_segment();
        let mut plaintext = vec![0; protected.len() - TAG_BYTES];
        caller.open(&protected, &mut plaintext).unwrap();
        FrameDecoder::new(16 * 1024, None)
            .unwrap()
            .pull(&mut Bytes::from(plaintext))
            .unwrap()
            .unwrap()
    }

    fn owner_request(
        &self,
        owner: &mut super::LocalOwner<UnixTransport>,
        caller: &mut Encrypted,
        request: Bytes,
    ) -> Frame {
        self.send_protected(caller, &request);
        let request = owner.next_request().unwrap();
        owner.dispatch(request).unwrap();
        self.receive_frame(caller)
    }

    fn provision(&self, name: &str, key: [u8; 32]) -> ParticipantId {
        let handle = self.board.enrol(name, Grant::default()).unwrap();
        self.board.board.table.save().authority.provision_fixture(
            handle.id(),
            Some(key),
            Some(Source::LocalUser(uid())),
        );
        handle.id().clone()
    }

    fn registered(
        &mut self,
        name: &str,
    ) -> (ParticipantId, LocalOpening<UnixTransport>, Encrypted) {
        let key = KeyPair::generate().unwrap();
        let id = self.provision(name, *key.public());
        let initiator = self.opening(Entrance::General, &key);
        let (opening, caller) = self.finish_opening(Entrance::General, initiator);
        (id, opening, caller)
    }

    fn start_proof(
        &mut self,
        name: &str,
    ) -> (ParticipantId, LocalOpening<UnixTransport>, Encrypted) {
        let (id, opening, mut caller) = self.registered(name);
        let request = attach_request(5, id.as_str(), b"opening bytes").unwrap();
        self.send_protected(&mut caller, &request);
        (id, opening, caller)
    }

    fn assert_closed(&self) {
        let mut byte = [0];
        assert_eq!(
            self.transport.receive(&self.client.io, &mut byte).unwrap(),
            Received::Closed
        );
    }
}

fn uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

#[test]
fn owner_ping_succeeds_without_an_attach() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(2)
        .unwrap()
        .str("op")
        .unwrap()
        .str("PING")
        .unwrap()
        .str("rid")
        .unwrap()
        .u64(8)
        .unwrap();
    let header = encoder.into_writer();
    let prefix = frame_prefix(header.len(), 0).unwrap();
    fixture.send_protected(&mut caller, &[prefix.as_slice(), &header].concat());

    let request = owner.next_request().unwrap();
    assert!(owner.reply_if_immediate(&request).unwrap());

    let reply = fixture.receive_frame(&mut caller);
    assert_eq!((reply.op(), reply.rid()), ("OK", 8));
}

#[test]
fn a_live_owner_link_refuses_a_second_change_while_the_first_waits_to_save() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let mut fixture = Fixture::new(uid());
    let first = fixture.board.enrol("first", Grant::default()).unwrap();
    let second = fixture.board.enrol("second", Grant::default()).unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let board = fixture.board.board.clone();
    let forced = AtomicBool::new(false);
    let revoke = |rid, id: &ParticipantId| {
        encode_frame(
            rid,
            "REVOKE",
            1,
            |fields| {
                fields.str("id").unwrap().str(id.as_str()).unwrap();
            },
            &[],
        )
        .unwrap()
    };

    std::thread::scope(|tasks| {
        let authority = board.table.save();
        let server = tasks.spawn(move || opening.serve_owner());
        let (finished, wait) = std::sync::mpsc::channel::<()>();
        let (transport, client, forced) = (&fixture.transport, &fixture.client.io, &forced);
        let timer = tasks.spawn(move || {
            if wait.recv_timeout(Duration::from_secs(5)).is_err() {
                forced.store(true, Ordering::Release);
                let _ = transport.send(client, &[]);
            }
        });
        fixture.send_protected(&mut caller, &revoke(1, first.id()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !board.table.save_waiting() {
            assert!(
                Instant::now() < deadline,
                "first request did not reach the save"
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        fixture.send_protected(
            &mut caller,
            &encode_frame(2, "PING", 0, |_| {}, &[]).unwrap(),
        );
        let ping = fixture.receive_frame(&mut caller);
        assert_eq!((ping.op(), ping.rid()), ("OK", 2));
        fixture.send_protected(&mut caller, &revoke(3, second.id()));
        fixture.assert_closed();

        drop(authority);
        finished.send(()).unwrap();
        timer.join().unwrap();
        assert!(
            !forced.load(Ordering::Acquire),
            "second request did not close the live link"
        );
        assert!(server.join().unwrap().is_err());
    });

    assert!(fixture.board.handle(first.id()).is_err());
    assert!(fixture.board.handle(second.id()).is_ok());
}

#[test]
fn owner_status_answers_while_the_authority_is_locked_for_a_save() {
    let mut fixture = Fixture::new(uid());
    let caller = fixture.opening(Entrance::Owner, &fixture.owner_key);
    let board = fixture.board.board.clone();
    let save = board.table.save();
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::scope(|tasks| {
        tasks.spawn(|| {
            let (opening, mut caller) = fixture.finish_opening(Entrance::Owner, caller);
            let mut owner = opening.owner_requests().unwrap();
            let request = encode_frame(1, "STATUS", 0, |_| {}, &[]).unwrap();
            let reply = fixture.owner_request(&mut owner, &mut caller, request);
            done.send(reply).unwrap();
        });
        let reply = wait.recv_timeout(std::time::Duration::from_secs(2));
        drop(save);
        let reply = reply.unwrap();
        assert_eq!((reply.op(), reply.rid()), ("OK", 1));
        let fields = crate::frame_fields::Fields::new(&reply).unwrap();
        assert_eq!(
            fields.required("run").unwrap(),
            crate::CreationId::from_bytes(board.run).to_string()
        );
    });
}

#[test]
fn owner_status_reports_the_current_channel_floor_and_head() {
    let fixture = Fixture::new(uid());
    let owner = &fixture.board;
    let receipt = owner
        .lead()
        .send(crate::SendRequest::note("lead", "one", b"body"))
        .unwrap();
    owner.trim("lead", receipt.creation, 2).unwrap();

    let status = owner.status(None, Some(&"lead".parse().unwrap()));

    assert_eq!(status.channels.len(), 1);
    let channel = &status.channels[0];
    assert_eq!(
        (
            channel.creation,
            channel.floor,
            channel.head,
            channel.stopped
        ),
        (receipt.creation, 2, 2, false)
    );
}

#[test]
fn an_owner_stop_with_an_earlier_run_id_is_refused() {
    let mut fixture = Fixture::new(uid());
    let mut earlier = fixture.board.board.run;
    earlier[0] ^= 1;
    let earlier = crate::CreationId::from_bytes(earlier).to_string();
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    let request = encode_frame(
        1,
        "STOP",
        1,
        |fields| {
            fields.str("run").unwrap().str(&earlier).unwrap();
        },
        &[],
    )
    .unwrap();

    let reply = fixture.owner_request(&mut owner, &mut caller, request);

    assert_eq!(reply.op(), "ERR");
    let fields = crate::frame_fields::Fields::new(&reply).unwrap();
    assert_eq!(fields.required("outcome").unwrap(), "refused");
    assert!(!fixture.entrances.stopped());
}

#[test]
fn repeated_owner_revoke_leaves_the_participant_revoked() {
    let mut fixture = Fixture::new(uid());
    let participant = fixture.board.enrol("alice", Grant::default()).unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();

    for rid in 1..=2 {
        let request = encode_frame(
            rid,
            "REVOKE",
            1,
            |fields| {
                fields
                    .str("id")
                    .unwrap()
                    .str(participant.id().as_str())
                    .unwrap();
            },
            &[],
        )
        .unwrap();
        let reply = fixture.owner_request(&mut owner, &mut caller, request);
        assert_eq!((reply.op(), reply.rid()), ("OK", rid));
        assert!(fixture.board.handle(participant.id()).is_err());
    }
}

#[test]
fn repeated_owner_create_leaves_the_same_creation() {
    use crate::frame_fields::Fields;

    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    let mut creations = Vec::new();

    for rid in 1..=2 {
        let request = encode_frame(
            rid,
            "CHANNEL_CREATE",
            1,
            |fields| {
                fields.str("channel").unwrap().str("tasks").unwrap();
            },
            &[],
        )
        .unwrap();
        let reply = fixture.owner_request(&mut owner, &mut caller, request);
        assert_eq!((reply.op(), reply.rid()), ("OK", rid));
        let fields = Fields::new(&reply).unwrap();
        assert_eq!(fields.get("created").unwrap().bool().unwrap(), rid == 1);
        creations.push(fields.required("creation").unwrap().to_owned());
    }
    assert_eq!(creations[0], creations[1]);
}

#[test]
fn repeating_owner_remove_leaves_a_new_creation_in_place() {
    use crate::frame_fields::Fields;

    let mut fixture = Fixture::new(uid());
    let (old, _) = fixture.board.create_channel("tasks").unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    let remove = |rid| {
        encode_frame(
            rid,
            "CHANNEL_REMOVE",
            2,
            |fields| {
                fields.str("channel").unwrap().str("tasks").unwrap();
                fields
                    .str("creation")
                    .unwrap()
                    .str(&old.to_string())
                    .unwrap();
            },
            &[],
        )
        .unwrap()
    };

    let reply = fixture.owner_request(&mut owner, &mut caller, remove(1));
    assert!(
        Fields::new(&reply)
            .unwrap()
            .get("removed")
            .unwrap()
            .bool()
            .unwrap()
    );
    let (new, _) = fixture.board.create_channel("tasks").unwrap();
    let repeated = fixture.owner_request(&mut owner, &mut caller, remove(2));

    assert_ne!(old, new);
    let fields = Fields::new(&repeated).unwrap();
    assert!(!fields.get("removed").unwrap().bool().unwrap());
    assert_eq!(fields.required("creation").unwrap(), new.to_string());
    assert_eq!(fixture.board.create_channel("tasks").unwrap(), (new, false));
}

#[test]
fn repeated_owner_trim_returns_the_saved_floor() {
    use crate::SendRequest;
    use crate::frame_fields::Fields;

    let mut fixture = Fixture::new(uid());
    let (creation, _) = fixture.board.create_channel("tasks").unwrap();
    fixture
        .board
        .lead()
        .send(SendRequest::note("tasks", "sample", b""))
        .unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();

    for rid in 1..=2 {
        let request = encode_frame(
            rid,
            "TRIM",
            3,
            |fields| {
                fields.str("channel").unwrap().str("tasks").unwrap();
                fields
                    .str("creation")
                    .unwrap()
                    .str(&creation.to_string())
                    .unwrap();
                fields.str("position").unwrap().u64(2).unwrap();
            },
            &[],
        )
        .unwrap();
        let reply = fixture.owner_request(&mut owner, &mut caller, request);
        assert_eq!((reply.op(), reply.rid()), ("OK", rid));
        assert_eq!(
            Fields::new(&reply).unwrap().number("floor").unwrap(),
            Some(2)
        );
    }
}

#[test]
fn an_owner_tail_streams_stored_metadata_and_can_be_cancelled() {
    use crate::SendRequest;
    use crate::frame_fields::Fields;

    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    fixture.send_protected(
        &mut caller,
        &encode_frame(
            1,
            "TAIL",
            1,
            |fields| {
                fields.str("pattern").unwrap().str("tasks").unwrap();
            },
            &[],
        )
        .unwrap(),
    );
    fixture.send_protected(
        &mut caller,
        &encode_frame(2, "PING", 0, |_| {}, &[]).unwrap(),
    );

    std::thread::scope(|threads| {
        let running = threads.spawn(|| {
            let request = owner.next_request().unwrap();
            owner.dispatch(request).unwrap();
        });
        let ping = fixture.receive_frame(&mut caller);
        assert_eq!((ping.op(), ping.rid()), ("OK", 2));
        let lead = fixture.board.lead();
        lead.send(SendRequest::note("other", "sample", b"ignored"))
            .unwrap();
        let receipt = lead
            .send(SendRequest::note("tasks", "sample", b"secret"))
            .unwrap();

        let event = fixture.receive_frame(&mut caller);
        assert_eq!((event.op(), event.rid()), ("EVENT", 1));
        let fields = Fields::new(&event).unwrap();
        assert_eq!(fields.required("event").unwrap(), "stored");
        assert_eq!(fields.required("channel").unwrap(), "tasks");
        assert_eq!(
            fields.number("position").unwrap(),
            Some(receipt.position.get())
        );
        assert!(event.body().is_empty());

        fixture.send_protected(
            &mut caller,
            &encode_frame(
                3,
                "CANCEL",
                1,
                |fields| {
                    fields.str("target").unwrap().u64(1).unwrap();
                },
                &[],
            )
            .unwrap(),
        );
        for rid in [1, 3] {
            let reply = fixture.receive_frame(&mut caller);
            assert_eq!((reply.op(), reply.rid()), ("OK", rid));
        }
        running.join().unwrap();
    });
}

#[test]
fn tail_metadata_tracks_delivery_and_acknowledgement_positions() {
    use malak_layers::EventKind;

    let fixture = Fixture::new(uid());
    let lead = fixture.board.lead();
    let receipt = lead
        .send(crate::SendRequest::note("tasks", "one", b"body"))
        .unwrap();
    let watcher = fixture.board.board.events.watch(None).unwrap();

    lead.read(&["tasks"], 1, std::time::Duration::ZERO).unwrap();
    lead.ack("tasks", receipt.creation, receipt.position.get())
        .unwrap();

    for expected in [EventKind::Read, EventKind::Acked] {
        let (event, missed) = watcher.next().unwrap();
        assert_eq!(event.kind, expected);
        assert_eq!(event.record, Some((receipt.creation, receipt.position)));
        assert_eq!(event.participant.as_ref(), Some(lead.id()));
        assert!(event.body.is_none());
        assert_eq!(missed, 0);
    }
}

#[test]
fn every_changing_owner_opcode_emits_one_event_on_shape_refusal() {
    let mut fixture = Fixture::new(uid());
    let watcher = fixture.board.board.events.watch(None).unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();

    for (index, operation) in [
        "ENROL",
        "GRANT",
        "REVOKE",
        "REKEY",
        "TRIM",
        "CHANNEL_CREATE",
        "CHANNEL_REMOVE",
        "STOP",
    ]
    .into_iter()
    .enumerate()
    {
        let reply = fixture.owner_request(
            &mut owner,
            &mut caller,
            encode_frame(index as u64 + 1, operation, 0, |_| {}, &[]).unwrap(),
        );
        assert_eq!(reply.op(), "ERR");
        let (event, _) = watcher.next().unwrap();
        assert_eq!(event.operation, Some(operation));
        assert_eq!(event.outcome, Some(crate::Outcome::Refused));
        assert!(event.body.is_none());
    }
    fixture
        .board
        .lead()
        .send(crate::SendRequest::note("tasks", "marker", b""))
        .unwrap();

    assert_eq!(
        watcher.next().unwrap().0.kind,
        malak_layers::EventKind::Stored
    );
}

#[test]
fn a_saved_revoke_emits_an_event_when_its_reply_cannot_be_written() {
    let mut fixture = Fixture::new(uid());
    let participant = fixture.board.enrol("alice", Grant::default()).unwrap();
    let watcher = fixture.board.board.events.watch(None).unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    fixture.send_protected(
        &mut caller,
        &encode_frame(
            1,
            "REVOKE",
            1,
            |fields| {
                fields
                    .str("id")
                    .unwrap()
                    .str(participant.id().as_str())
                    .unwrap();
            },
            &[],
        )
        .unwrap(),
    );
    let request = owner.next_request().unwrap();
    drop(fixture.client);

    assert!(owner.dispatch(request).is_err());
    assert!(fixture.board.handle(participant.id()).is_err());
    let (event, _) = watcher.next().unwrap();
    assert_eq!(event.operation, Some("REVOKE"));
    assert_eq!(event.participant.as_ref(), Some(participant.id()));
    assert_eq!(event.outcome, Some(crate::Outcome::Done));
}

#[test]
fn repeating_a_grant_with_a_lost_reply_replaces_the_entire_intervening_grant() {
    use crate::{Action, grant::write_grant};

    let mut fixture = Fixture::new(uid());
    let worker = fixture.board.enrol("worker", Grant::default()).unwrap();
    let id = worker.id();
    let mut original = Grant::default();
    original.allow("lead".parse().unwrap(), [Action::Read]);
    original.allow("tasks".parse().unwrap(), [Action::Send]);
    let request = encode_frame(
        1,
        "GRANT",
        2,
        |fields| {
            fields.str("id").unwrap().str(id.as_str()).unwrap();
            fields.str("grant").unwrap();
            write_grant(fields, &original);
        },
        &[],
    )
    .unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    fixture.send_protected(&mut caller, &request);
    let pending = owner.next_request().unwrap();
    fixture.reconnect();
    assert!(owner.dispatch(pending).is_err());
    drop(owner);
    assert_eq!(
        fixture.board.status(Some(id), None).participants[0]
            .grant
            .rules(),
        original.rules()
    );
    let mut intervening = Grant::default();
    intervening.allow("lead".parse().unwrap(), [Action::Ack]);
    fixture.board.replace_grant(id, intervening).unwrap();

    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    let reply = fixture.owner_request(&mut owner, &mut caller, request);

    assert_eq!((reply.op(), reply.rid()), ("OK", 1));
    assert_eq!(
        fixture.board.status(Some(id), None).participants[0]
            .grant
            .rules(),
        original.rules()
    );
}

#[test]
fn the_owner_protocol_rejects_participant_operations() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    fixture.send_protected(&mut caller, &attach_request(5, "lead", &[]).unwrap());

    let request = owner.next_request().unwrap();
    assert_eq!(request.operation, OwnerOperation::Unsupported);
    assert!(owner.reply_if_immediate(&request).unwrap());
    let reply = fixture.receive_frame(&mut caller);
    let mut header = Decoder::new(reply.header());
    let outcome = (0..header.map().unwrap().unwrap())
        .find_map(|_| {
            if header.str().unwrap() == "outcome" {
                Some(header.str().unwrap())
            } else {
                header.skip().unwrap();
                None
            }
        })
        .unwrap();

    assert_eq!(
        (reply.op(), reply.rid(), outcome),
        ("ERR", 5, "not_supported")
    );
}

#[test]
fn the_owner_door_refuses_general_sends() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    let send = encode_frame(
        1,
        "SEND",
        3,
        |out| {
            out.str("channel").unwrap().str("lead").unwrap();
            out.str("id").unwrap().str("an-owner-send").unwrap();
            out.str("kind").unwrap().str("note").unwrap();
        },
        b"a general message",
    )
    .unwrap();

    let reply = fixture.owner_request(&mut owner, &mut caller, send);

    assert_eq!(reply.op(), "ERR");
    assert_eq!(
        crate::frame_fields::Fields::new(&reply)
            .unwrap()
            .required("outcome")
            .unwrap(),
        "not_supported"
    );
}

#[test]
fn the_upgrade_notice_rejects_a_caller_selected_destination() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    let run = crate::CreationId::from_bytes(fixture.board.board.run).to_string();
    let request = encode_frame(
        1,
        "UPGRADE_NOTICE",
        4,
        |out| {
            for (key, value) in [
                ("run", run.as_str()),
                ("previous_build", "unknown"),
                (
                    "installed_build",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
                ("channel", "questions"),
            ] {
                out.str(key).unwrap().str(value).unwrap();
            }
        },
        &[],
    )
    .unwrap();

    let reply = fixture.owner_request(&mut owner, &mut caller, request);

    assert_eq!(
        crate::frame_fields::Fields::new(&reply)
            .unwrap()
            .required("outcome")
            .unwrap(),
        "refused"
    );
}

#[test]
fn retrying_the_same_owner_upgrade_notice_keeps_one_message() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    let mut owner = opening.owner_requests().unwrap();
    let run = crate::CreationId::from_bytes(fixture.board.board.run).to_string();

    for rid in [1, 2] {
        let request = encode_frame(
            rid,
            "UPGRADE_NOTICE",
            3,
            |out| {
                out.str("run").unwrap().str(&run).unwrap();
                out.str("previous_build").unwrap().str("unknown").unwrap();
                out.str("installed_build")
                    .unwrap()
                    .str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    .unwrap();
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            fixture.owner_request(&mut owner, &mut caller, request).op(),
            "OK"
        );
    }

    let status = fixture.board.status(None, Some(&"lead".parse().unwrap()));
    assert_eq!(status.channels[0].head, 2);
}

#[test]
fn a_lead_link_cannot_open_the_owner_request_stream() {
    let mut fixture = Fixture::new(uid());
    let (opening, _) = fixture.protected(Entrance::Lead);

    assert!(opening.owner_requests().is_err());
    fixture.assert_closed();
}

#[test]
fn owner_entrance_closes_before_reading_from_a_different_uid() {
    let mut fixture = Fixture::new(uid() ^ 1);

    assert!(fixture.open(Entrance::Owner).is_err());
    fixture.assert_closed();
}

#[test]
fn lead_entrance_closes_before_reading_from_a_different_uid() {
    let mut fixture = Fixture::new(uid() ^ 1);

    assert!(fixture.open(Entrance::Lead).is_err());
    fixture.assert_closed();
}

#[test]
fn owner_entrance_gives_no_reply_to_a_lead_key() {
    let mut fixture = Fixture::new(uid());
    fixture.opening(Entrance::Owner, &fixture.lead_key);

    assert!(fixture.open(Entrance::Owner).is_err());
    fixture.assert_closed();
}

#[test]
fn the_owner_door_refuses_recorded_lead_and_general_handshakes() {
    for entrance in [Entrance::Lead, Entrance::General] {
        let mut fixture = Fixture::new(uid());
        let key = KeyPair::generate().unwrap();
        if entrance == Entrance::General {
            fixture.provision("worker", *key.public());
        }
        let registered = if entrance == Entrance::Lead {
            &fixture.lead_key
        } else {
            &key
        };
        let (initiator, recorded) = fixture.opening_bytes(entrance, registered);
        fixture.send(SegmentKind::Opening, &recorded);
        drop(fixture.finish_opening(entrance, initiator));
        fixture.reconnect();

        fixture.send(SegmentKind::Opening, &recorded);

        assert!(fixture.open(Entrance::Owner).is_err());
        fixture.assert_closed();
    }
}

#[test]
fn general_entrance_gives_no_reply_to_an_unregistered_key() {
    let mut fixture = Fixture::new(uid());
    fixture.opening(Entrance::General, &KeyPair::generate().unwrap());

    assert!(fixture.open(Entrance::General).is_err());
    fixture.assert_closed();
}

#[test]
fn general_entrance_refuses_the_lead_key() {
    let mut fixture = Fixture::new(uid());
    fixture.opening(Entrance::General, &fixture.lead_key);

    assert!(fixture.open(Entrance::General).is_err());
    fixture.assert_closed();
}

#[test]
fn lead_entrance_refuses_an_opening_bound_to_the_owner_entrance() {
    let mut fixture = Fixture::new(uid());
    fixture.opening(Entrance::Owner, &fixture.lead_key);

    assert!(fixture.open(Entrance::Lead).is_err());
    fixture.assert_closed();
}

#[test]
fn a_changed_protected_segment_closes_the_lead_link() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Lead);
    let mut protected = [0; TAG_BYTES + 1];
    caller.seal(b"x", &mut protected).unwrap();
    protected[0] ^= 1;

    fixture.send(SegmentKind::Protected, &protected);

    assert!(opening.attach_participant().is_err());
    fixture.assert_closed();
}

#[test]
fn the_lead_attaches_as_proven_after_a_fragmented_request() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Lead);
    let request = attach_request(7, "lead", &[]).unwrap();
    fixture.send_protected(&mut caller, &request[..3]);
    fixture.send_protected(&mut caller, &request[3..]);

    let attached = opening.attach_participant().unwrap();

    assert_eq!(attached.id(), &ParticipantId::lead());
    assert_eq!(attached.grade(), Grade::Proven);
    let reply = fixture.receive_frame(&mut caller);
    assert_eq!((reply.op(), reply.rid()), ("OK", 7));
}

#[test]
fn the_lead_key_cannot_attach_as_a_different_participant() {
    let mut fixture = Fixture::new(uid());
    let participant = fixture.board.enrol("other", Grant::default()).unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Lead);
    fixture.send_protected(
        &mut caller,
        &attach_request(1, participant.id().as_str(), &[]).unwrap(),
    );

    assert!(opening.attach_participant().is_err());
    fixture.assert_closed();
}

#[test]
fn an_owner_link_cannot_attach_a_participant() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Owner);
    fixture.send_protected(&mut caller, &attach_request(1, "lead", &[]).unwrap());

    assert!(opening.attach_participant().is_err());
    fixture.assert_closed();
}

#[test]
fn extending_an_expired_grant_cannot_revive_a_lead_link() {
    let mut fixture = Fixture::new(uid());
    let (opening, mut caller) = fixture.protected(Entrance::Lead);
    fixture.send_protected(&mut caller, &attach_request(1, "lead", &[]).unwrap());
    let mut attached = opening.attach_participant().unwrap();
    fixture.receive_frame(&mut caller);
    let mut expired = Grant::default();
    expired.set_end(Some(UNIX_EPOCH));
    fixture.board.replace_grant(attached.id(), expired).unwrap();
    fixture
        .board
        .replace_grant(attached.id(), Grant::default())
        .unwrap();

    assert!(attached.check_valid().is_err());
    fixture.assert_closed();
}

#[test]
fn an_unresponsive_link_closes_at_its_opening_deadline() {
    let mut fixture = Fixture::new(uid());

    assert!(fixture.open(Entrance::Lead).is_err());

    fixture.assert_closed();
}

#[test]
fn revoking_closes_an_attached_unix_link_even_when_it_is_idle() {
    let mut fixture = Fixture::new(uid());
    let key = KeyPair::generate().unwrap();
    let id = fixture.provision("worker", *key.public());
    let initiator = fixture.opening(Entrance::General, &key);
    let (opening, mut caller) = fixture.finish_opening(Entrance::General, initiator);
    fixture.send_protected(&mut caller, &attach_request(1, id.as_str(), &[]).unwrap());
    let _attached = opening.attach_participant().unwrap();
    fixture.receive_frame(&mut caller);

    fixture.board.revoke(&id).unwrap();

    fixture.assert_closed();
}

#[test]
fn a_registered_key_attaches_on_the_general_entrance() {
    let mut fixture = Fixture::new(uid());
    let (id, opening, mut caller) = fixture.registered("helper");
    fixture.send_protected(&mut caller, &attach_request(2, id.as_str(), &[]).unwrap());

    let attached = opening.attach_participant().unwrap();

    assert_eq!(attached.id(), &id);
    assert_eq!(attached.grade(), Grade::Proven);
    assert_eq!(fixture.receive_frame(&mut caller).op(), "OK");
}

#[test]
fn a_registered_participant_cannot_claim_another_participants_id() {
    let mut fixture = Fixture::new(uid());
    let other_key = KeyPair::generate().unwrap();
    let other = fixture.provision("second", *other_key.public());
    let (_, opening, mut caller) = fixture.registered("first");
    fixture.send_protected(
        &mut caller,
        &attach_request(3, other.as_str(), &[]).unwrap(),
    );

    assert!(opening.attach_participant().is_err());
    fixture.assert_closed();
}

struct TwoRounds;

impl Mechanism for TwoRounds {
    fn describe(&self) -> malak_layers::Proves {
        Key.describe()
    }

    fn enrol(
        &self,
        id: &ParticipantId,
        hints: &malak_layers::Enrolment<'_>,
    ) -> Result<malak_layers::Handover, Error> {
        Key.enrol(id, hints)
    }

    fn step(
        &self,
        session: &malak_layers::Session<'_>,
        incoming: &[u8],
    ) -> Result<malak_layers::Step, Error> {
        match incoming {
            b"opening bytes" => Ok(malak_layers::Step::More(b"challenge".to_vec())),
            b"challenge answer" => Key.step(session, incoming),
            _ => Ok(malak_layers::Step::Denied),
        }
    }

    fn allow(
        &self,
        who: &malak_layers::Identity<'_>,
        action: crate::Action,
        channel: &crate::ChannelName,
    ) -> Result<bool, Error> {
        Key.allow(who, action, channel)
    }

    fn revoke(&self, id: &ParticipantId) -> Result<(), Error> {
        Key.revoke(id)
    }
}

#[test]
fn a_multiround_proof_finishes_over_unix() {
    let mut fixture = Fixture::with_mechanism(uid(), Arc::new(TwoRounds));
    let (id, opening, mut caller) = fixture.start_proof("two_rounds");

    let attached = std::thread::scope(|scope| {
        let board = scope.spawn(|| opening.attach_participant());
        let challenge = fixture.receive_frame(&mut caller);
        assert_eq!(challenge.op(), "STEP");
        assert_eq!(challenge.body(), b"challenge");
        let answer = malak_proto::attach_step(6, b"challenge answer").unwrap();
        fixture.send_protected(&mut caller, &answer);

        board.join().unwrap().unwrap()
    });

    assert_eq!(attached.id(), &id);
    let reply = fixture.receive_frame(&mut caller);
    assert_eq!((reply.op(), reply.rid()), ("OK", 6));
}

#[test]
fn a_key_revoked_between_steps_cannot_finish_attachment() {
    let mut fixture = Fixture::with_mechanism(uid(), Arc::new(TwoRounds));
    let (id, opening, mut caller) = fixture.start_proof("revoked");

    std::thread::scope(|scope| {
        let board = scope.spawn(|| opening.attach_participant());
        assert_eq!(fixture.receive_frame(&mut caller).op(), "STEP");
        fixture.board.revoke(&id).unwrap();
        let answer = malak_proto::attach_step(6, b"challenge answer").unwrap();
        fixture.send_protected(&mut caller, &answer);

        assert!(board.join().unwrap().is_err());
    });
    fixture.assert_closed();
}

#[test]
fn the_lead_can_attach_when_general_opening_places_are_full() {
    let mut fixture = Fixture::new(uid());
    let general = &fixture.entrances.settings.general.gate;
    let now = std::time::Instant::now();
    let _places: Vec<_> = (0..64)
        .map(|_| general.reserve(None, now).unwrap())
        .collect();
    assert!(general.reserve(None, now).is_err());
    let (opening, mut caller) = fixture.protected(Entrance::Lead);
    fixture.send_protected(&mut caller, &attach_request(1, "lead", &[]).unwrap());

    let _attached = opening.attach_participant().unwrap();
    assert_eq!(fixture.receive_frame(&mut caller).op(), "OK");
}

#[test]
fn attached_general_links_cannot_spend_the_leads_places() {
    let fixture = Fixture::new(uid());
    let general = &fixture.entrances.settings.general_attached;
    let lead = &fixture.entrances.settings.lead_attached;
    let participants: Vec<ParticipantId> = (0..64)
        .map(|n| format!("worker{n:03}").parse().unwrap())
        .collect();
    let mut places = Vec::new();
    for id in &participants {
        places.extend((0..16).map(|_| general.reserve(id).unwrap()));
        assert!(general.reserve(id).is_none());
    }
    let another = "worker064".parse().unwrap();
    assert!(general.reserve(&another).is_none());

    let lead_id = ParticipantId::lead();
    let _lead_places: Vec<_> = (0..16).map(|_| lead.reserve(&lead_id).unwrap()).collect();
    assert!(lead.reserve(&lead_id).is_none());
    places.pop();
    assert!(general.reserve(&another).is_some());
}
