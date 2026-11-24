use super::*;
use crate::{HandoverFile, LocalClient, frame_fields::Fields};
use malak_layers::{Event, EventKind};
use malak_testkit::tasks;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const WAIT: Duration = Duration::from_secs(3);
const BURST: usize = 16_000;

#[test]
fn a_full_unread_tail_does_not_block_send_and_reports_what_it_missed() {
    tasks::spawn(exercise_unread_tail)
        .wait(Duration::from_secs(15))
        .expect("tail-pressure scenario did not finish");
}

fn exercise_unread_tail() {
    let mut fixture = Fixture::new(uid());
    let address = fixture._socket.address();
    let (owner, mut caller) = fixture.protected(Entrance::Owner);
    let tail = tasks::spawn(move || owner.serve_owner());
    fixture.send_protected(
        &mut caller,
        &encode_frame(1, "TAIL", 0, |_| {}, &[]).unwrap(),
    );
    fixture.send_protected(
        &mut caller,
        &encode_frame(2, "PING", 0, |_| {}, &[]).unwrap(),
    );
    let ping = fixture.receive_frame(&mut caller);
    assert_eq!((ping.op(), ping.rid()), ("OK", 2));

    let mut marker = Event::new(EventKind::Stored);
    marker.channel = Some("test/events".parse().unwrap());
    for _ in 0..BURST {
        fixture.board.board.events.emit(marker.clone());
    }

    let handover = HandoverFile::new(
        Binding {
            version: 1,
            entrance: Entrance::Lead,
        },
        address.clone(),
        fixture.board_key,
        ParticipantId::lead(),
        String::new(),
        Zeroizing::new(Vec::new()),
        Zeroizing::new(*fixture.lead_key.private()),
    )
    .unwrap();
    let participant = tasks::spawn({
        let transport = fixture.transport.clone();
        let entrances = fixture.entrances.clone();
        move || {
            let link = transport.accept(&address).unwrap();
            let _ = entrances
                .open(Entrance::Lead, transport, link)
                .unwrap()
                .attach_participant()
                .unwrap()
                .serve();
        }
    });
    let mut lead = LocalClient::participant(fixture.transport.clone(), handover, None).unwrap();
    let send = encode_frame(
        1,
        "SEND",
        2,
        |fields| {
            fields.str("channel").unwrap().str("lead").unwrap();
            fields.str("id").unwrap().str("while-tail-is-full").unwrap();
        },
        b"the participant still works",
    )
    .unwrap();
    lead.send_frame(send, WAIT).unwrap();
    assert_eq!(lead.read_frame(WAIT).unwrap().op(), "OK");

    let missed = (0..BURST).find_map(|_| {
        let frame = fixture.receive_frame(&mut caller);
        assert_eq!((frame.op(), frame.rid()), ("EVENT", 1));
        Fields::new(&frame)
            .unwrap()
            .number("missed")
            .unwrap()
            .filter(|n| *n > 0)
    });
    assert!(missed.is_some());

    let cancel = encode_frame(
        3,
        "CANCEL",
        1,
        |fields| {
            fields.str("target").unwrap().u64(1).unwrap();
        },
        &[],
    )
    .unwrap();
    fixture.send_protected(&mut caller, &cancel);
    for rid in [1, 3] {
        let reply = (0..BURST)
            .map(|_| fixture.receive_frame(&mut caller))
            .find(|frame| frame.op() != "EVENT")
            .expect("tail cancellation reply missing");
        assert_eq!((reply.op(), reply.rid()), ("OK", rid));
    }

    drop((lead, fixture));
    participant.wait(WAIT).expect("lead link did not shut down");
    assert!(
        tail.wait(WAIT)
            .expect("owner link did not shut down")
            .is_err()
    );
}

#[test]
fn disconnecting_a_tail_does_not_wait_for_its_blocked_writer() {
    tasks::spawn(exercise_disconnect)
        .wait(WAIT * 3)
        .expect("owner disconnect scenario timed out");
}

fn exercise_disconnect() {
    use std::sync::mpsc;

    let mut fixture = Fixture::new(uid());
    let (entered, paused) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let transport = Arc::new(HeldWrite {
        unix: fixture.transport.clone(),
        gate: std::sync::Mutex::new(None),
    });
    let initiator = fixture.opening(Entrance::Owner, &fixture.owner_key);
    let opening = fixture
        .entrances
        .open(
            Entrance::Owner,
            transport.clone(),
            fixture.server.take().unwrap(),
        )
        .unwrap();
    let mut caller = fixture.finish_reply(initiator);
    let mut owner = opening.owner_requests().unwrap();
    fixture.send_protected(
        &mut caller,
        &encode_frame(1, "TAIL", 0, |_| {}, &[]).unwrap(),
    );
    fixture.send_protected(
        &mut caller,
        &encode_frame(2, "PING", 0, |_| {}, &[]).unwrap(),
    );
    let server = tasks::spawn(move || {
        let request = owner.next_request().unwrap();
        owner.dispatch(request)
    });
    assert_eq!(fixture.receive_frame(&mut caller).rid(), 2);

    *transport.gate.lock().unwrap() = Some((entered, resume));
    fixture
        .board
        .board
        .events
        .emit(Event::new(EventKind::Stored));
    paused
        .recv_timeout(WAIT)
        .expect("tail writer did not enter the transport");
    drop(fixture);

    let outcome = server
        .wait(WAIT)
        .expect("owner waited for the blocked writer after disconnect");
    assert!(outcome.is_err());
    release.send(()).unwrap();
}

#[test]
fn stop_drains_a_queued_event_and_stop_before_closing_the_tail() {
    tasks::spawn(|| {
        let mut fixture = Fixture::new(uid());
        let (mut caller, release, server) = pause_tail_writer(&mut fixture);
        let run = fixture.board.status(None, None).run;
        let (finished, completion) = mpsc::channel();

        std::thread::scope(|threads| {
            let board = &fixture.board;
            threads.spawn(move || {
                board.stop(run).unwrap();
                finished.send(()).unwrap();
            });
            assert_eq!(
                completion.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout),
                "board stopped while a tail write was still pending"
            );
            release.send(()).unwrap();
            let queued = fixture.receive_frame(&mut caller);
            let stop = fixture.receive_frame(&mut caller);
            assert_eq!(
                Fields::new(&queued).unwrap().required("event").unwrap(),
                "stored"
            );
            let fields = Fields::new(&stop).unwrap();
            assert_eq!(fields.required("operation").unwrap(), "STOP");
            assert_eq!(fields.required("outcome").unwrap(), "done");
            completion.recv_timeout(WAIT).unwrap();
        });

        fixture.assert_closed();
        assert!(server.wait(WAIT).unwrap().is_err());
    })
    .wait(WAIT * 2)
    .expect("tail drain scenario timed out");
}

#[test]
fn stop_closes_a_tail_whose_writer_stays_blocked_past_the_deadline() {
    tasks::spawn(|| {
        let mut fixture = Fixture::new(uid());
        let (_, release, server) = pause_tail_writer(&mut fixture);
        let run = fixture.board.status(None, None).run;
        let started = Instant::now();

        fixture.board.stop(run).unwrap();

        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(2), "stop took {elapsed:?}");
        fixture.assert_closed();
        assert!(server.wait(WAIT).unwrap().is_err());
        release.send(()).unwrap();
    })
    .wait(WAIT * 2)
    .expect("stalled tail prevented shutdown");
}

fn pause_tail_writer(
    fixture: &mut Fixture,
) -> (Encrypted, mpsc::Sender<()>, tasks::Task<Result<(), Error>>) {
    let (entered, paused) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let transport = Arc::new(HeldWrite {
        unix: fixture.transport.clone(),
        gate: std::sync::Mutex::new(None),
    });
    let initiator = fixture.opening(Entrance::Owner, &fixture.owner_key);
    let owner = fixture
        .entrances
        .open(
            Entrance::Owner,
            transport.clone(),
            fixture.server.take().unwrap(),
        )
        .unwrap();
    let mut caller = fixture.finish_reply(initiator);
    let server = tasks::spawn(move || owner.serve_owner());
    for (rid, op) in [(1, "TAIL"), (2, "PING")] {
        fixture.send_protected(&mut caller, &encode_frame(rid, op, 0, |_| {}, &[]).unwrap());
    }
    assert_eq!(fixture.receive_frame(&mut caller).rid(), 2);

    *transport.gate.lock().unwrap() = Some((entered, resume));
    fixture
        .board
        .board
        .events
        .emit(Event::new(EventKind::Stored));
    paused
        .recv_timeout(WAIT)
        .expect("tail writer did not stall");
    (caller, release, server)
}

type WriteGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

struct HeldWrite {
    unix: Arc<UnixTransport>,
    gate: std::sync::Mutex<Option<WriteGate>>,
}

impl Transport for HeldWrite {
    type Connection = UnixLink;

    fn describe(&self) -> malak_layers::TransportInfo {
        self.unix.describe()
    }

    fn accept(&self, address: &str) -> Result<Link<UnixLink>, Error> {
        self.unix.accept(address)
    }

    fn dial(&self, address: &str) -> Result<Link<UnixLink>, Error> {
        self.unix.dial(address)
    }

    fn send(&self, socket: &UnixLink, bytes: &[u8]) -> Result<usize, Error> {
        if !bytes.is_empty() {
            let gate = self.gate.lock().unwrap().take();
            if let Some((entered, resume)) = gate {
                entered.send(()).unwrap();
                let _ = resume.recv_timeout(WAIT * 2);
            }
        }
        self.unix.send(socket, bytes)
    }

    fn receive(&self, socket: &UnixLink, bytes: &mut [u8]) -> Result<Received, Error> {
        self.unix.receive(socket, bytes)
    }
}
