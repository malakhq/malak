//! Attachment and delivery races with authority saved on the running board.

use super::*;
use crate::{ChannelName, GetReply, SendRequest};
use malak_layers::{Identity, Proves, Session, Side, Step};
use malak_proto::encode_frame;
use parking_lot::Mutex;

#[derive(Default)]
struct PausedProof {
    attachment: Mutex<Option<Pause>>,
    delivery: Mutex<Option<(usize, Pause)>>,
}

impl Mechanism for PausedProof {
    fn describe(&self) -> Proves {
        Proves {
            has_veto: true,
            ..Key.describe()
        }
    }

    fn enrol(&self, who: &ParticipantId, hints: &Enrolment<'_>) -> Result<Handover, Error> {
        Key.enrol(who, hints)
    }

    fn step(&self, session: &Session<'_>, incoming: &[u8]) -> Result<Step, Error> {
        if session.side == Side::Board {
            let paused = self.attachment.lock().take();
            if let Some(pause) = paused {
                pause.reach();
            }
        }
        Key.step(session, incoming)
    }

    fn allow(
        &self,
        who: &Identity<'_>,
        action: Action,
        channel: &ChannelName,
    ) -> Result<bool, Error> {
        if action == Action::Read {
            let paused = {
                let mut delivery = self.delivery.lock();
                match delivery.as_mut() {
                    Some((0, _)) => delivery.take().map(|(_, pause)| pause),
                    Some((before, _)) => {
                        *before -= 1;
                        None
                    }
                    None => None,
                }
            };
            if let Some(pause) = paused {
                pause.reach();
            }
        }
        Key.allow(who, action, channel)
    }

    fn revoke(&self, who: &ParticipantId) -> Result<(), Error> {
        Key.revoke(who)
    }
}

struct Pause {
    reached: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

impl Pause {
    fn pair() -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (reached, waiting) = mpsc::channel();
        let (resume, released) = mpsc::channel();
        (
            Self {
                reached,
                resume: released,
            },
            waiting,
            resume,
        )
    }

    fn reach(self) {
        self.reached.send(()).unwrap();
        self.resume.recv_timeout(WAIT).unwrap();
    }
}

#[test]
fn a_key_replaced_while_proof_is_in_flight_cannot_finish_attachment() {
    let mechanism = Arc::new(PausedProof::default());
    let fixture = Fixture::with_mechanism(mechanism.clone());
    let id = fixture.enrol("worker");
    let path = fixture.root.path().join("worker");
    let old = HandoverFile::read(&path).unwrap();
    let previous = fixture.saved_fingerprint(&id);
    let (pause, waiting, resume) = Pause::pair();
    *mechanism.attachment.lock() = Some(pause);
    let address = old.address().to_owned();
    let unix = Arc::new(UnixTransport::bind(&address).unwrap());

    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| fixture.accept(&unix, &address));
        let (transport, old) = (unix.clone(), old);
        let client =
            tasks.spawn(move || LocalClient::participant(transport, old, Some(Arc::new(Key))));
        waiting.recv_timeout(WAIT).unwrap();

        fixture.entrances.rekey(&id, &previous, &path).unwrap();

        resume.send(()).unwrap();
        assert!(server.join().unwrap().is_err());
        assert!(client.join().unwrap().is_err());
    });
}

#[test]
fn an_attached_key_cannot_start_a_call_after_replacement_is_saved() {
    let fixture = Fixture::new();
    let id = fixture.enrol_with("worker", [Action::Send]);
    let path = fixture.root.path().join("worker");
    let previous = fixture.saved_fingerprint(&id);
    let old = HandoverFile::read(&path).unwrap();
    let address = old.address().to_owned();
    let unix = Arc::new(UnixTransport::bind(&address).unwrap());
    let (saved, close) = fixture.pause_after_saving();

    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| fixture.accept(&unix, &address).unwrap().serve());
        let mut client = LocalClient::participant(unix.clone(), old, Some(Arc::new(Key))).unwrap();
        let key = tasks.spawn(|| fixture.entrances.rekey(&id, &previous, &path));
        saved.recv_timeout(WAIT).unwrap();

        let send = encode_frame(
            7,
            "SEND",
            2,
            |header| {
                header.str("channel").unwrap().str("lead").unwrap();
                header.str("id").unwrap().str("after-rekey").unwrap();
            },
            b"retired",
        )
        .unwrap();
        if client.send_frame(send, WAIT).is_ok() {
            let closed = client.read_frame(Duration::from_secs(2)).unwrap_err();
            assert!(closed.to_string().contains("link closed"), "{closed}");
        }
        assert!(matches!(
            fixture.owner.lead().get("lead", 1).unwrap(),
            GetReply::NeverWritten { .. }
        ));

        close.send(()).unwrap();
        key.join().unwrap().unwrap();
        drop(client);
        let _ = server.join().unwrap();
    });
}

#[test]
fn waiting_reads_yield_no_messages_after_revocation_or_key_replacement_is_saved() {
    for replace_key in [false, true] {
        let mechanism = Arc::new(PausedProof::default());
        let fixture = Fixture::with_mechanism(mechanism.clone());
        let id = fixture.enrol_with("worker", [Action::Read]);
        let path = fixture.root.path().join("worker");
        let previous = fixture.saved_fingerprint(&id);
        let old = HandoverFile::read(&path).unwrap();
        let address = old.address().to_owned();
        let unix = Arc::new(UnixTransport::bind(&address).unwrap());
        let (pause, delivery, resume_delivery) = Pause::pair();
        *mechanism.delivery.lock() = Some((1, pause));

        std::thread::scope(|tasks| {
            let server = tasks.spawn(|| fixture.accept(&unix, &address).unwrap().serve());
            let mut client =
                LocalClient::participant(unix.clone(), old, Some(Arc::new(Key))).unwrap();
            let read = encode_frame(
                7,
                "READ",
                2,
                |header| {
                    header
                        .str("channels")
                        .unwrap()
                        .array(1)
                        .unwrap()
                        .str("lead")
                        .unwrap();
                    header.str("wait_ms").unwrap().u64(5_000).unwrap();
                },
                &[],
            )
            .unwrap();
            client.send_frame(read, WAIT).unwrap();
            fixture
                .owner
                .lead()
                .send(SendRequest::note("lead", "due", b"held at delivery"))
                .unwrap();
            delivery.recv_timeout(WAIT).unwrap();

            let key_change = if replace_key {
                let (saved, close) = fixture.pause_after_saving();
                let key = tasks.spawn(|| fixture.entrances.rekey(&id, &previous, &path));
                saved.recv_timeout(WAIT).unwrap();
                Some((key, close))
            } else {
                fixture.owner.revoke(&id).unwrap();
                None
            };
            resume_delivery.send(()).unwrap();
            let closed = client.read_frame(Duration::from_secs(2)).unwrap_err();
            assert!(closed.to_string().contains("link closed"), "{closed}");
            if let Some((key, close)) = key_change {
                close.send(()).unwrap();
                key.join().unwrap().unwrap();
            }
            drop(client);
            let _ = server.join().unwrap();
        });
    }
}
