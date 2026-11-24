use super::*;
use malak_core::{GetReply, Position};
use malak_proto::Frame;
use std::sync::{Barrier, OnceLock};

#[test]
fn a_thousand_overlapping_wire_retries_share_one_stored_receipt() {
    const LINKS: usize = 4;
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let mut limits = Limits::default();
    limits.calls_per_minute = 1_300;
    let (owner, entrances) = fixture
        .start_settings_with_limits(
            &fixture.state,
            |role, default| {
                if role == Entrance::General {
                    address.clone()
                } else {
                    default.to_owned()
                }
            },
            Arc::new(Key),
            limits,
        )
        .unwrap();
    let path = fixture.root.path().join("worker");
    entrances
        .enrol("worker", allow([malak_core::Action::Send]), &path, None)
        .unwrap();
    let unix = Arc::new(UnixTransport::bind(&address).unwrap());
    let start = Barrier::new(LINKS);
    let saved = OnceLock::new();

    let originals = std::thread::scope(|scope| {
        let connect = || {
            LocalClient::participant(
                unix.clone(),
                HandoverFile::read(&path).unwrap(),
                Some(Arc::new(Key)),
            )
            .unwrap()
        };
        let server = || serve_participant(&entrances, &address, unix.clone());
        let mut clients = Vec::new();
        for _ in 0..LINKS {
            scope.spawn(server);
            clients.push(connect());
        }

        clients
            .into_iter()
            .map(|mut client| {
                let start = &start;
                let saved = &saved;
                scope.spawn(move || {
                    start.wait();
                    let mut originals = 0;
                    for id in 2..=251 {
                        let reply = send(&mut client, id, "note", "tag", b"body");
                        if reply.op() == "ERR" {
                            assert_eq!(field(&reply, "outcome").str().unwrap(), "unavailable");
                            continue;
                        }
                        assert_eq!(reply.op(), "OK");
                        assert_eq!(field(&reply, "position").u64().unwrap(), 1);
                        let creation = field(&reply, "creation").str().unwrap();
                        assert_eq!(saved.get_or_init(|| creation.to_owned()), creation);
                        assert_eq!(field(&reply, "durability").str().unwrap(), "restart");
                        originals += usize::from(!field(&reply, "duplicate").bool().unwrap());
                    }
                    originals
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|client| client.join().unwrap())
            .sum::<usize>()
    });

    assert_eq!(originals, 1);
    let lead = owner.lead();
    let GetReply::Message(message) = lead.get("lead", 1).unwrap() else {
        panic!("the retried message was not stored");
    };
    assert_eq!(message.receipt.position, Position::FIRST);
    assert_eq!(&message.receipt.creation.to_string(), saved.get().unwrap());
    assert!(matches!(
        lead.get("lead", 2).unwrap(),
        GetReply::NeverWritten { .. }
    ));

    std::thread::scope(|scope| {
        scope.spawn(|| serve_participant(&entrances, &address, unix.clone()));
        let mut client = LocalClient::participant(
            unix.clone(),
            HandoverFile::read(&path).unwrap(),
            Some(Arc::new(Key)),
        )
        .unwrap();
        for (id, (kind, header, body)) in [
            ("report", "tag", b"body".as_slice()),
            ("note", "changed", b"body".as_slice()),
            ("note", "tag", b"changed".as_slice()),
        ]
        .into_iter()
        .enumerate()
        {
            let reply = send(&mut client, id as u64 + 2, kind, header, body);
            assert_eq!(reply.op(), "ERR");
            assert_eq!(field(&reply, "outcome").str().unwrap(), "refused");
        }
    });
}

fn send(
    client: &mut LocalClient<UnixTransport>,
    rid: u64,
    kind: &str,
    header: &str,
    body: &[u8],
) -> Frame {
    let request = encode_frame(
        rid,
        "SEND",
        4,
        |out| {
            out.str("channel").unwrap().str("lead").unwrap();
            out.str("id").unwrap().str("retried").unwrap();
            out.str("kind").unwrap().str(kind).unwrap();
            out.str("headers")
                .unwrap()
                .map(1)
                .unwrap()
                .str("x-label")
                .unwrap()
                .str(header)
                .unwrap();
        },
        body,
    )
    .unwrap();
    client.send_frame(request, TIMEOUT).unwrap();
    let reply = client.read_frame(TIMEOUT).unwrap();
    assert_eq!(reply.rid(), rid);
    reply
}
