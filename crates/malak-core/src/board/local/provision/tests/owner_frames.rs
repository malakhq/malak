//! Observe the actual replies and tail frames from an authenticated owner link.

use super::*;
use crate::frame_fields::Fields;
use crate::journal_entry::StartupEntry;
use malak_journal::StateFolder;
use malak_layers::{Identity, Proves, Session, Step};
use malak_proto::encode_frame;

const CONTENTS: &[u8] = b"private fixture mechanism data";

struct KeyWithHandover;

impl Mechanism for KeyWithHandover {
    fn describe(&self) -> Proves {
        Key.describe()
    }

    fn enrol(&self, id: &ParticipantId, request: &Enrolment<'_>) -> Result<Handover, Error> {
        let mut value = Key.enrol(id, request)?;
        value.contents = CONTENTS.to_vec();
        Ok(value)
    }

    fn step(&self, session: &Session<'_>, incoming: &[u8]) -> Result<Step, Error> {
        Key.step(session, incoming)
    }

    fn allow(
        &self,
        who: &Identity<'_>,
        action: Action,
        channel: &crate::ChannelName,
    ) -> Result<bool, Error> {
        Key.allow(who, action, channel)
    }

    fn revoke(&self, id: &ParticipantId) -> Result<(), Error> {
        Key.revoke(id)
    }
}

#[test]
fn owner_replies_and_events_keep_private_credentials_off_the_wire() {
    let fixture = Fixture::with_mechanism(Arc::new(KeyWithHandover));
    let state = fixture.root.path().join("state");
    let path = fixture.root.path().join("worker");
    let path_text = path.to_str().unwrap();
    let address = OwnerCredential::read(&state).unwrap().address().to_owned();
    let unix = Arc::new(UnixTransport::bind(&address).unwrap());
    let serve = || {
        let link = unix.accept(&address).unwrap();
        let _ = fixture
            .entrances
            .open(Entrance::Owner, unix.clone(), link)
            .unwrap()
            .serve_owner();
    };

    let (frames, enrolled, rekeyed) = std::thread::scope(|tasks| {
        let watcher = tasks.spawn(serve);
        let mut tail =
            LocalClient::owner(unix.clone(), OwnerCredential::read(&state).unwrap()).unwrap();
        let publisher = tasks.spawn(serve);
        let mut owner =
            LocalClient::owner(unix.clone(), OwnerCredential::read(&state).unwrap()).unwrap();
        tail.send_frame(encode_frame(1, "TAIL", 0, |_| {}, &[]).unwrap(), WAIT)
            .unwrap();
        let ping = tail
            .owner_call(encode_frame(2, "PING", 0, |_| {}, &[]).unwrap(), 2)
            .unwrap();
        assert_eq!(ping.op(), "OK");

        let enrolled_reply = owner
            .owner_call(
                encode_frame(
                    1,
                    "ENROL",
                    3,
                    |header| {
                        header.str("name").unwrap().str("worker").unwrap();
                        header.str("path").unwrap().str(path_text).unwrap();
                        header.str("grant").unwrap();
                        crate::grant::write_grant(header, &Grant::default());
                    },
                    &[],
                )
                .unwrap(),
                1,
            )
            .unwrap();
        assert_eq!(keys(enrolled_reply.header()), ["id", "op", "path", "rid"]);
        assert!(enrolled_reply.body().is_empty());
        assert_eq!(
            Fields::new(&enrolled_reply)
                .unwrap()
                .required("path")
                .unwrap(),
            path_text
        );
        let enrolled = HandoverFile::read(&path).unwrap();
        assert!(enrolled.mechanism_data() == CONTENTS);
        let expected = fingerprint(&enrolled);

        let rekeyed_reply = owner
            .owner_call(
                encode_frame(
                    2,
                    "REKEY",
                    3,
                    |header| {
                        header
                            .str("id")
                            .unwrap()
                            .str(enrolled.id().as_str())
                            .unwrap();
                        header.str("fingerprint").unwrap().str(&expected).unwrap();
                        header.str("path").unwrap().str(path_text).unwrap();
                    },
                    &[],
                )
                .unwrap(),
                2,
            )
            .unwrap();
        assert_eq!(keys(rekeyed_reply.header()), ["op", "path", "rid"]);
        assert!(rekeyed_reply.body().is_empty());
        assert_eq!(
            Fields::new(&rekeyed_reply)
                .unwrap()
                .required("path")
                .unwrap(),
            path_text
        );
        let rekeyed = HandoverFile::read(&path).unwrap();
        assert!(rekeyed.mechanism_data() == CONTENTS);

        let status = owner
            .owner_call(
                encode_frame(
                    3,
                    "STATUS",
                    1,
                    |header| {
                        header
                            .str("id")
                            .unwrap()
                            .str(rekeyed.id().as_str())
                            .unwrap();
                    },
                    &[],
                )
                .unwrap(),
                3,
            )
            .unwrap();
        assert_eq!(keys(status.header()), ["op", "rid", "run"]);
        check_status_schema(status.body(), &fingerprint(&rekeyed));

        let events = [("ENROL", "target"), ("REKEY", "id")].map(|(op, who)| {
            let event = tail.read_frame(WAIT).unwrap();
            let mut fields = vec![
                "at_ms",
                "event",
                "missed",
                "op",
                "operation",
                "outcome",
                "rid",
                "sender_trace",
                "version",
                who,
            ];
            fields.sort_unstable();
            assert_eq!(keys(event.header()), fields);
            assert_eq!((event.op(), event.rid()), ("EVENT", 1));
            assert_eq!(
                Fields::new(&event).unwrap().required("operation").unwrap(),
                op
            );
            assert!(event.body().is_empty());
            event
        });
        drop((tail, owner));
        watcher.join().unwrap();
        publisher.join().unwrap();
        let [enrol_event, rekey_event] = events;
        (
            [
                enrolled_reply,
                rekeyed_reply,
                status,
                enrol_event,
                rekey_event,
            ],
            enrolled,
            rekeyed,
        )
    });

    fixture
        .owner
        .stop(fixture.owner.status(None, None).run)
        .unwrap();
    let journal = StateFolder::acquire_existing(&state)
        .unwrap()
        .inspect()
        .unwrap();
    let StartupEntry::Keys(keys) =
        StartupEntry::decode(journal.entries().next().unwrap().1).unwrap()
    else {
        panic!("expected the startup keys");
    };
    for frame in frames {
        for secret in [
            CONTENTS,
            enrolled.private_key(),
            rekeyed.private_key(),
            keys.board.private(),
            keys.owner.private(),
            keys.record.as_slice(),
        ] {
            for data in [frame.header(), frame.body()] {
                assert!(
                    !data.windows(secret.len()).any(|bytes| bytes == secret),
                    "credential bytes in an owner frame"
                );
            }
        }
    }
}

fn keys(bytes: &[u8]) -> Vec<&str> {
    let mut input = minicbor::Decoder::new(bytes);
    let mut names = Vec::new();
    for _ in 0..input.map().unwrap().unwrap() {
        names.push(input.str().unwrap());
        input.skip().unwrap();
    }
    assert_eq!(input.position(), bytes.len());
    names.sort_unstable();
    names
}

fn check_status_schema(body: &[u8], fingerprint: &str) {
    assert_eq!(
        keys(body),
        [
            "channels",
            "layers",
            "participants",
            "repair",
            "started_at_ms"
        ]
    );
    let mut input = minicbor::Decoder::new(body);
    for _ in 0..input.map().unwrap().unwrap() {
        if input.str().unwrap() != "participants" {
            input.skip().unwrap();
            continue;
        }
        assert_eq!(input.array().unwrap(), Some(1));
        let start = input.position();
        input.skip().unwrap();
        let participant = &body[start..input.position()];
        assert_eq!(
            keys(participant),
            [
                "connections",
                "enrolled_at_ms",
                "fingerprint",
                "grant",
                "grant_changed_at_ms",
                "id",
                "name",
                "revoked",
                "revoked_at_ms"
            ]
        );
        let mut fields = minicbor::Decoder::new(participant);
        let field = (0..fields.map().unwrap().unwrap()).find_map(|_| {
            if fields.str().unwrap() == "fingerprint" {
                return Some(fields.str().unwrap());
            }
            fields.skip().unwrap();
            None
        });
        assert_eq!(field, Some(fingerprint));
        return;
    }
    panic!("status omitted participants");
}
