//! End-to-end read scheduling and delivery over local participant links.

use super::{Fixture, TIMEOUT, field, read_channels, serve_participant};
use malak_core::{
    Action, ChannelName, Error, Grant, HandoverFile, Limits, LocalClient, SendRequest,
};
use malak_layers::{
    Enrolment, Handover, Identity, Link, Mechanism, Proves, Received, Session, Step, Transport,
    TransportInfo,
};
use malak_mechanism_key::Key;
use malak_proto::{Entrance, encode_frame, max_frame_bytes};
use malak_transport_unix::{UnixLink, UnixTransport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

#[path = "readers/wire_pressure.rs"]
mod wire_pressure;

#[test]
fn a_reader_with_a_full_queue_does_not_delay_another_reader() {
    let body_size = 32 * 1024;
    let limits = one_message_limits(body_size);
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let (board, entrances) = fixture
        .start_settings_with_limits(
            &fixture.state,
            |role, fallback| address_for(role, fallback, &address),
            Arc::new(Key),
            limits,
        )
        .unwrap();
    let slow = enrol(&fixture, &entrances, "slow", &["lead"]);
    let fast = enrol(&fixture, &entrances, "fast", &["lead"]);
    let receipts = [1, 2, 3].map(|marker| {
        board
            .lead()
            .send(SendRequest::note(
                "lead",
                &format!("record-{marker}"),
                &vec![marker; body_size],
            ))
            .unwrap()
    });
    let unix = Arc::new(UnixTransport::bind(&address).unwrap());
    let paused = PauseWrite::new(&unix);

    std::thread::scope(|tasks| {
        let slow_server = tasks.spawn(|| serve_participant(&entrances, &address, paused.clone()));
        let mut slow = connect(&unix, slow);
        let fast_server = tasks.spawn(|| serve_participant(&entrances, &address, unix.clone()));
        let mut fast = connect(&unix, fast);
        let (blocked, release) = paused.pause();
        slow.send_frame(read_channels(1, &["lead"], Some(1), 0), TIMEOUT)
            .unwrap();
        blocked.recv_timeout(TIMEOUT).unwrap();

        // The first frame is still charged to the slow link's queue. The
        // remaining space cannot hold a second frame of this body size.
        for receipt in receipts {
            let position = receipt.position.get();
            fast.send_frame(read_channels(position * 2, &["lead"], Some(1), 0), TIMEOUT)
                .unwrap();
            let message = fast.read_frame(TIMEOUT).unwrap();
            assert_eq!((message.op(), message.body()[0]), ("MSG", position as u8));
            assert_eq!(message.body().len(), body_size);
            assert_eq!(fast.read_frame(TIMEOUT).unwrap().op(), "OK");
            acknowledge(&mut fast, position * 2 + 1, "lead", receipt);
        }

        release.send(()).unwrap();
        assert_eq!(slow.read_frame(TIMEOUT).unwrap().op(), "MSG");
        assert_eq!(slow.read_frame(TIMEOUT).unwrap().op(), "OK");
        drop((slow, fast));
        let _ = slow_server.join().unwrap();
        let _ = fast_server.join().unwrap();
    });
}

#[test]
fn a_vetoed_first_message_does_not_take_every_read_turn() {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let preflight = Arc::new(AtomicBool::new(false));
    let veto = Arc::new(ReadVeto({
        let preflight = preflight.clone();
        move |channel: &ChannelName| {
            channel.as_str() != "blocked" || preflight.swap(false, Ordering::AcqRel)
        }
    }));
    let (board, entrances) = fixture
        .start_settings_with(
            &fixture.state,
            |role, fallback| address_for(role, fallback, &address),
            veto.clone(),
        )
        .unwrap();
    let handover = enrol(&fixture, &entrances, "reader", &["blocked", "ready"]);
    let lead = board.lead();
    lead.send(SendRequest::note("blocked", "first", b"vetoed"))
        .unwrap();
    lead.send(SendRequest::note(
        "blocked",
        "second",
        b"still behind the first",
    ))
    .unwrap();
    let due = ["ready-one", "ready-two", "ready-three"].map(|id| {
        (
            id,
            lead.send(SendRequest::note("ready", id, id.as_bytes()))
                .unwrap(),
        )
    });
    let unix = Arc::new(UnixTransport::bind(&address).unwrap());

    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| serve_participant(&entrances, &address, unix.clone()));
        let mut client = connect(&unix, handover);
        for (index, (body, receipt)) in due.into_iter().enumerate() {
            let rid = 3 * index as u64 + 1;
            for (rid, expected) in [(rid, None), (rid + 1, Some(body))] {
                // The call precheck must allow both channels. The mechanism
                // then vetoes each delivery from the first channel.
                preflight.store(true, Ordering::Release);
                client
                    .send_frame(
                        read_channels(rid, &["blocked", "ready"], Some(1), 0),
                        TIMEOUT,
                    )
                    .unwrap();
                if let Some(body) = expected {
                    let frame = client.read_frame(TIMEOUT).unwrap();
                    assert_eq!(
                        (frame.op(), frame.rid(), frame.body()),
                        ("MSG", rid, body.as_bytes())
                    );
                }
                let closing = client.read_frame(TIMEOUT).unwrap();
                assert_eq!((closing.op(), closing.rid()), ("OK", rid));
            }
            acknowledge(&mut client, rid + 2, "ready", receipt);
        }
        drop(client);
        let _ = server.join().unwrap();
    });
}

#[test]
fn both_channels_deliver_every_due_position_across_reconnect_and_restart() {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let start = || fixture.start_on(Entrance::General, &address).unwrap();
    let (board, entrances) = start();
    let handover = || HandoverFile::read(fixture.root.path().join("reader")).unwrap();
    let channels = ["alpha", "beta"];
    enrol(&fixture, &entrances, "reader", &channels);
    let receipts = channels.map(|channel| {
        [1, 2, 3].map(|position| {
            board
                .lead()
                .send(SendRequest::note(
                    channel,
                    &format!("message-{position}"),
                    channel.as_bytes(),
                ))
                .unwrap()
        })
    });
    let unix = Arc::new(UnixTransport::bind(&address).unwrap());
    let session = |entrances: &malak_core::LocalEntrances, rounds: &[(usize, usize)]| {
        std::thread::scope(|tasks| {
            let server = tasks.spawn(|| serve_participant(entrances, &address, unix.clone()));
            let mut reader = connect(&unix, handover());
            for (index, &(channel, position)) in rounds.iter().enumerate() {
                let rid = 2 + 2 * index as u64;
                let receipt = receipts[channel][position - 1];
                reader
                    .send_frame(read_channels(rid, &channels, Some(1), 0), TIMEOUT)
                    .unwrap();
                let message = reader.read_frame(TIMEOUT).unwrap();
                assert_eq!(
                    (message.op(), message.rid(), message.body()),
                    ("MSG", rid, channels[channel].as_bytes())
                );
                assert_eq!(
                    field(&message, "position").u64().unwrap(),
                    receipt.position.get()
                );
                let closing = reader.read_frame(TIMEOUT).unwrap();
                assert_eq!((closing.op(), closing.rid()), ("OK", rid));
                acknowledge(&mut reader, rid + 1, channels[channel], receipt);
            }
            drop(reader);
            let _ = server.join().unwrap();
        });
    };
    session(&entrances, &[(0, 1)]);
    session(&entrances, &[(1, 1)]);
    let old_run = board.status(None, None).run;
    board.stop(old_run).unwrap();
    drop((board, entrances));

    let (board, entrances) = start();
    assert_ne!(board.status(None, None).run, old_run);
    session(&entrances, &[(0, 2), (1, 2), (0, 3), (1, 3)]);
    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| serve_participant(&entrances, &address, unix.clone()));
        let mut reader = connect(&unix, handover());
        reader
            .send_frame(read_channels(2, &channels, Some(1), 0), TIMEOUT)
            .unwrap();
        assert_eq!(reader.read_frame(TIMEOUT).unwrap().op(), "OK");
        drop(reader);
        let _ = server.join().unwrap();
    });
}

fn one_message_limits(body_size: usize) -> Limits {
    let mut limits = Limits::default();
    limits.body_bytes = body_size;
    limits.read_reply_bytes = max_frame_bytes(body_size).unwrap();
    limits.outgoing_message_bytes = limits.read_reply_bytes;
    assert!(limits.outgoing_message_bytes < 2 * body_size);
    limits
}

fn address_for(role: Entrance, fallback: &str, general: &str) -> String {
    if role == Entrance::General {
        general
    } else {
        fallback
    }
    .to_owned()
}

fn enrol(
    fixture: &Fixture,
    entrances: &malak_core::LocalEntrances,
    name: &str,
    channels: &[&str],
) -> HandoverFile {
    let path = fixture.root.path().join(name);
    let mut grant = Grant::default();
    for channel in channels {
        grant.allow(channel.parse().unwrap(), [Action::Read, Action::Ack]);
    }
    entrances.enrol(name, grant, &path, None).unwrap();
    HandoverFile::read(path).unwrap()
}

fn connect(unix: &Arc<UnixTransport>, handover: HandoverFile) -> LocalClient<UnixTransport> {
    LocalClient::participant(unix.clone(), handover, Some(Arc::new(Key))).unwrap()
}

fn acknowledge(
    client: &mut LocalClient<UnixTransport>,
    rid: u64,
    channel: &str,
    receipt: malak_core::Receipt,
) {
    client
        .send_frame(ack_frame(rid, channel, receipt), TIMEOUT)
        .unwrap();
    let reply = client.read_frame(TIMEOUT).unwrap();
    assert_eq!((reply.op(), reply.rid()), ("OK", rid));
}

fn ack_frame(rid: u64, channel: &str, receipt: malak_core::Receipt) -> bytes::Bytes {
    encode_frame(
        rid,
        "ACK",
        3,
        |out| {
            out.str("channel").unwrap().str(channel).unwrap();
            out.str("position")
                .unwrap()
                .u64(receipt.position.get())
                .unwrap();
            out.str("creation")
                .unwrap()
                .str(&receipt.creation.to_string())
                .unwrap();
        },
        &[],
    )
    .unwrap()
}

type Gate = (mpsc::Sender<()>, mpsc::Receiver<()>);

struct PauseWrite {
    unix: Arc<UnixTransport>,
    next: Mutex<Option<Gate>>,
    received: Mutex<usize>,
    received_changed: Condvar,
}

impl PauseWrite {
    fn new(unix: &Arc<UnixTransport>) -> Arc<Self> {
        Arc::new(Self {
            unix: unix.clone(),
            next: Mutex::new(None),
            received: Mutex::new(0),
            received_changed: Condvar::new(),
        })
    }

    fn pause(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, blocked) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        assert!(
            self.next
                .lock()
                .unwrap()
                .replace((entered, resume))
                .is_none()
        );
        (blocked, release)
    }

    fn received_bytes(&self) -> usize {
        *self.received.lock().unwrap()
    }

    fn wait_for_bytes(&self, expected: usize) {
        let (received, _) = self
            .received_changed
            .wait_timeout_while(self.received.lock().unwrap(), TIMEOUT, |bytes| {
                *bytes < expected
            })
            .unwrap();
        assert!(
            *received >= expected,
            "board did not read the expected bytes"
        );
    }
}

impl Transport for PauseWrite {
    type Connection = UnixLink;

    fn describe(&self) -> TransportInfo {
        self.unix.describe()
    }

    fn accept(&self, address: &str) -> Result<Link<UnixLink>, Error> {
        self.unix.accept(address)
    }

    fn dial(&self, address: &str) -> Result<Link<UnixLink>, Error> {
        self.unix.dial(address)
    }

    fn send(&self, link: &UnixLink, bytes: &[u8]) -> Result<usize, Error> {
        if !bytes.is_empty() {
            let pause = self.next.lock().unwrap().take();
            if let Some((entered, resume)) = pause {
                entered.send(()).unwrap();
                resume
                    .recv()
                    .map_err(|_| Error::unavailable("fixture did not resume its writer"))?;
            }
        }
        self.unix.send(link, bytes)
    }

    fn receive(&self, link: &UnixLink, out: &mut [u8]) -> Result<Received, Error> {
        // Feed one byte at a time so this probe can distinguish protocol
        // admission from the socket reading ahead into its segment buffer.
        let received = self.unix.receive(link, &mut out[..1])?;
        if let Received::Bytes(count) = received {
            *self.received.lock().unwrap() += count.get();
            self.received_changed.notify_all();
        }
        Ok(received)
    }
}

struct ReadVeto<F>(F);

impl<F: Fn(&ChannelName) -> bool + Send + Sync> Mechanism for ReadVeto<F> {
    fn describe(&self) -> Proves {
        Proves {
            has_veto: true,
            ..Key.describe()
        }
    }

    fn enrol(
        &self,
        who: &malak_core::ParticipantId,
        request: &Enrolment<'_>,
    ) -> Result<Handover, Error> {
        Key.enrol(who, request)
    }

    fn step(&self, session: &Session<'_>, incoming: &[u8]) -> Result<Step, Error> {
        Key.step(session, incoming)
    }

    fn allow(
        &self,
        who: &Identity<'_>,
        action: Action,
        channel: &ChannelName,
    ) -> Result<bool, Error> {
        if action == Action::Read {
            return Ok((self.0)(channel));
        }
        Key.allow(who, action, channel)
    }

    fn revoke(&self, who: &malak_core::ParticipantId) -> Result<(), Error> {
        Key.revoke(who)
    }
}
